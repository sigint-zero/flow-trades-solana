//! Meteora DLMM (`LBUZKh…`) exact-input quoting: the liquidity-book bin walk.
//!
//! A DLMM pair is a ladder of constant-price bins. Bin `id` prices X in Y at
//! `(1 + bin_step/1e4)^id` (Q64.64, the program's own square-and-multiply
//! `pow`) and holds `amount_x` / `amount_y` of market-maker liquidity plus, on
//! limit-order pairs, resting orders on one side. A swap fills the active bin
//! at its price, then steps one bin in the swap direction (X→Y: down, Y→X: up)
//! until the input is spent, moving to the next 70-bin array that the pair's
//! bitmap marks as holding liquidity.
//!
//! The fee is re-priced in every bin with liquidity: base
//! `base_factor·bin_step·10·10^power` plus variable
//! `ceil(ctrl·(va·bin_step)²/1e11)` (per 1e9, capped at 10 %). The volatility
//! accumulator `va` grows with the distance walked from `index_reference`,
//! which the swap resets from the time since the pair's last update
//! (`update_references`). Fee off the input: `ceil(in·rate/1e9)` for a partial
//! bin, `ceil(filled·rate/(1e9−rate))` on top of a drained one.
//!
//! Everything mirrors the program's integer arithmetic, rounding included
//! (reference: MeteoraAg `dlmm-sdk` `commons::quote`). Hot-path rules as in
//! `quote::clmm`: no RPC, bins come from
//! [`BINS`] — an LbPair snapshot fetched in the SAME `getMultipleAccounts` as
//! its bin arrays, so a walk never mixes two slots — filled off the quote path
//! by `pool::bins`.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;

use super::clmm::{mul_div_ceil, mul_div_floor};
use crate::constants::METEORA_DLMM_PROG_ID;

pub const BINS_PER_ARRAY: i32 = 70;
pub const FEE_PRECISION: u128 = 1_000_000_000;
/// 10 %.
pub const MAX_FEE_RATE: u128 = 100_000_000;
const BASIS_POINT_MAX: u64 = 10_000;
pub const MIN_BIN_ID: i32 = -443_636;
pub const MAX_BIN_ID: i32 = 443_636;
/// Bin arrays covered by the pair's own bitmap: indices −512..=511.
const BITMAP_SIZE: i32 = 512;
/// The extension account adds 12 × 512 arrays on each side.
const EXT_BITMAP_WORDS: usize = 12;
const Q64: u128 = 1u128 << 64;
/// Bin arrays a swap may walk per direction: what `pool::bins` loads on each
/// side and what the executor passes. A quote that needs more is refused.
pub const SWAP_ARRAYS: usize = 3;

/// `LbPair` account length (Anchor discriminator + 896).
pub const LB_PAIR_LEN: usize = 904;
/// `BinArray` account length: disc, index i64, version, pad, lb_pair, 70 × 144-byte bins.
pub const BIN_ARRAY_LEN: usize = 10_136;
const BIN_LEN: usize = 144;
const BINS_OFFSET: usize = 56;
/// `BinArrayBitmapExtension`: disc, lb_pair, positive [[u64; 8]; 12], negative [[u64; 8]; 12].
pub const BITMAP_EXT_LEN: usize = 1_576;

// ── LbPair ────────────────────────────────────────────────────────────────

/// The `LbPair` fields a swap reads (program v0.12 layout, offsets include
/// the 8-byte discriminator):
/// `StaticParameters` @8 (base_factor u16, filter_period u16, decay_period u16,
/// reduction_factor u16, variable_fee_control u32 @16, max_volatility_accumulator
/// u32 @20, min/max_bin_id, protocol_share u16 @32, base_fee_power_factor @34,
/// function_type @35, collect_fee_mode @36), `VariableParameters` @40
/// (volatility_accumulator u32, volatility_reference u32 @44, index_reference
/// i32 @48, last_update_timestamp i64 @56), pair_type @75, active_id i32 @76,
/// bin_step u16 @80, status @82, activation_type @86, token_x_mint @88, reward mints @264/@408,
/// bin_array_bitmap [u64; 16] @584, activation_point u64 @816, token program
/// flags @880/@881.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DlmmPair {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub protocol_share: u16,
    pub base_fee_power_factor: u8,
    /// 0 = undetermined, 1 = liquidity mining, 2 = limit orders.
    pub function_type: u8,
    /// 0 = fee on the input, 1 = fee always in Y.
    pub collect_fee_mode: u8,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
    /// 0 permissionless, 1 permission, 2 customizable permissionless, 3 permissionless v2.
    pub pair_type: u8,
    pub active_id: i32,
    /// 0 for a state that was never parsed (old warm file): not quotable.
    pub bin_step: u16,
    /// 0 = enabled.
    pub status: u8,
    /// 0 = slot, 1 = unix timestamp.
    pub activation_type: u8,
    pub activation_point: u64,
    /// Any reward mint set (an undetermined-function pair then takes no limit orders).
    pub has_rewards: bool,
    pub token_x_2022: bool,
    pub token_y_2022: bool,
    /// X (a swap from X is `swap_for_y`).
    pub token_x_mint: Pubkey,
    /// Bin arrays −512..=511 holding liquidity (bit `idx + 512`, limb-major LE).
    pub bitmap: [u64; 16],
}

impl DlmmPair {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < LB_PAIR_LEN {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let mut bitmap = [0u64; 16];
        for (i, w) in bitmap.iter_mut().enumerate() {
            *w = u64_at(584 + 8 * i);
        }
        Some(Self {
            base_factor: u16_at(8),
            filter_period: u16_at(10),
            decay_period: u16_at(12),
            reduction_factor: u16_at(14),
            variable_fee_control: u32_at(16),
            max_volatility_accumulator: u32_at(20),
            protocol_share: u16_at(32),
            base_fee_power_factor: d[34],
            function_type: d[35],
            collect_fee_mode: d[36],
            volatility_accumulator: u32_at(40),
            volatility_reference: u32_at(44),
            index_reference: i32_at(48),
            last_update_timestamp: u64_at(56) as i64,
            pair_type: d[75],
            active_id: i32_at(76),
            bin_step: u16_at(80),
            status: d[82],
            activation_type: d[86],
            activation_point: u64_at(816),
            has_rewards: d[264..296].iter().any(|b| *b != 0) || d[408..440].iter().any(|b| *b != 0),
            token_x_2022: d[880] == 1,
            token_y_2022: d[881] == 1,
            token_x_mint: Pubkey::new_from_array(d[88..120].try_into().unwrap()),
            bitmap,
        })
    }

    pub fn is_parsed(&self) -> bool {
        self.bin_step > 0
    }

    /// `update_references`: at swap start, a pair idle for at least
    /// `filter_period` re-anchors on the active bin and decays its volatility
    /// (to `va·reduction/1e4` inside `decay_period`, to 0 beyond).
    pub fn update_references(&mut self, now: i64) {
        let elapsed = now.saturating_sub(self.last_update_timestamp);
        if elapsed >= self.filter_period as i64 {
            self.index_reference = self.active_id;
            self.volatility_reference = if elapsed < self.decay_period as i64 {
                (self.volatility_accumulator as u64 * self.reduction_factor as u64 / BASIS_POINT_MAX) as u32
            } else {
                0
            };
        }
    }

    /// `update_volatility_accumulator`, run before every bin with liquidity.
    pub fn update_volatility_accumulator(&mut self) {
        let delta = (self.index_reference as i64 - self.active_id as i64).unsigned_abs();
        let va = self.volatility_reference as u64 + delta * BASIS_POINT_MAX;
        self.volatility_accumulator = va.min(self.max_volatility_accumulator as u64) as u32;
    }

    pub fn base_fee_rate(&self) -> Option<u128> {
        (self.base_factor as u128)
            .checked_mul(self.bin_step as u128)?
            .checked_mul(10)?
            .checked_mul(10u128.checked_pow(self.base_fee_power_factor as u32)?)
    }

    pub fn variable_fee_rate(&self) -> Option<u128> {
        if self.variable_fee_control == 0 {
            return Some(0);
        }
        let v = (self.volatility_accumulator as u128).checked_mul(self.bin_step as u128)?;
        let v = (self.variable_fee_control as u128).checked_mul(v.checked_mul(v)?)?;
        Some(v.checked_add(99_999_999_999)? / 100_000_000_000)
    }

    /// Total fee rate per 1e9, capped at 10 %.
    pub fn total_fee_rate(&self) -> Option<u128> {
        Some(self.base_fee_rate()?.checked_add(self.variable_fee_rate()?)?.min(MAX_FEE_RATE))
    }

    /// Limit orders fill after the market-maker liquidity of a bin: always on
    /// a limit-order pair, never on a liquidity-mining one, and on an
    /// undetermined one only while it has no reward mint.
    pub fn supports_limit_orders(&self) -> bool {
        match self.function_type {
            2 => true,
            0 => !self.has_rewards,
            _ => false,
        }
    }

    /// Mode 1 ("only Y") charges an X→Y swap on its output.
    pub fn fee_on_input(&self, swap_for_y: bool) -> bool {
        self.collect_fee_mode != 1 || !swap_for_y
    }

    /// Enabled and past its activation point (checked for the pair types the
    /// program gates: permission and customizable permissionless).
    pub fn is_tradable(&self, now: i64, slot: u64) -> bool {
        if self.status != 0 {
            return false;
        }
        if matches!(self.pair_type, 1 | 2) {
            let current = if self.activation_type == 1 { now.max(0) as u64 } else { slot };
            return current >= self.activation_point;
        }
        true
    }
}

// ── bins ──────────────────────────────────────────────────────────────────

/// One bin: the fields a swap reads (`Bin` @0 amount_x, @8 amount_y, @16
/// price u128, @112 open_order_amount, @128 processed_order_remaining_amount,
/// @140 limit_order_ask_side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    /// Q64.64; 0 = never stored (the program derives it from the id).
    pub price: u128,
    pub open_order_amount: u64,
    pub processed_order_remaining_amount: u64,
    /// Resting orders sell X (ask) or buy X (bid).
    pub limit_order_ask_side: bool,
}

impl Bin {
    /// (open, processed-remaining) order amounts a swap in this direction can
    /// fill: X→Y takes bids, Y→X takes asks.
    fn limit_orders(&self, swap_for_y: bool) -> (u64, u64) {
        if swap_for_y != self.limit_order_ask_side {
            (self.open_order_amount, self.processed_order_remaining_amount)
        } else {
            (0, 0)
        }
    }

    fn max_amount_out(&self, swap_for_y: bool, limit_orders: bool) -> u64 {
        let mm = if swap_for_y { self.amount_y } else { self.amount_x };
        if !limit_orders {
            return mm;
        }
        let (open, processed) = self.limit_orders(swap_for_y);
        mm.saturating_add(open).saturating_add(processed)
    }
}

/// One `BinArray` account.
#[derive(Debug, Clone)]
pub struct BinArray {
    pub index: i64,
    pub key: Pubkey,
    pub bins: Vec<Bin>,
}

impl BinArray {
    pub fn parse(key: Pubkey, d: &[u8]) -> Option<Self> {
        if d.len() < BIN_ARRAY_LEN {
            return None;
        }
        let index = i64::from_le_bytes(d[8..16].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let bins = (0..BINS_PER_ARRAY as usize)
            .map(|k| {
                let o = BINS_OFFSET + k * BIN_LEN;
                Bin {
                    amount_x: u64_at(o),
                    amount_y: u64_at(o + 8),
                    price: u128::from_le_bytes(d[o + 16..o + 32].try_into().unwrap()),
                    open_order_amount: u64_at(o + 112),
                    processed_order_remaining_amount: u64_at(o + 128),
                    limit_order_ask_side: d[o + 140] != 0,
                }
            })
            .collect();
        Some(Self { index, key, bins })
    }

    fn lower_bin_id(&self) -> i32 {
        self.index as i32 * BINS_PER_ARRAY
    }
}

/// Bin array holding `bin_id` (floor division).
pub fn array_index(bin_id: i32) -> i32 {
    bin_id.div_euclid(BINS_PER_ARRAY)
}

pub fn bin_array_pda(pair: &Pubkey, index: i64) -> Pubkey {
    Pubkey::find_program_address(&[b"bin_array", pair.as_ref(), &index.to_le_bytes()], &METEORA_DLMM_PROG_ID).0
}

pub fn bitmap_extension_pda(pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bitmap", pair.as_ref()], &METEORA_DLMM_PROG_ID).0
}

/// The extension account's bitmaps: `positive[k]` covers arrays
/// `512·(k+1) + bit`, `negative[k]` covers `−512·(k+1) − 1 − bit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtBitmap {
    pub positive: [[u64; 8]; EXT_BITMAP_WORDS],
    pub negative: [[u64; 8]; EXT_BITMAP_WORDS],
}

impl ExtBitmap {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < BITMAP_EXT_LEN {
            return None;
        }
        let mut e = ExtBitmap { positive: [[0; 8]; EXT_BITMAP_WORDS], negative: [[0; 8]; EXT_BITMAP_WORDS] };
        for k in 0..EXT_BITMAP_WORDS {
            for w in 0..8 {
                let o = 40 + (k * 8 + w) * 8;
                e.positive[k][w] = u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
                let o = 40 + 768 + (k * 8 + w) * 8;
                e.negative[k][w] = u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
            }
        }
        Some(e)
    }
}

/// Every bin-array index marked as holding liquidity, ascending: the pair's
/// own bitmap, plus the extension's when there is one.
pub fn liquid_arrays(bitmap: &[u64; 16], ext: Option<&ExtBitmap>) -> Vec<i32> {
    let mut out = Vec::new();
    let mut push_bits = |words: &[u64], base: i32, sign: i32| {
        for (w, word) in words.iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let b = bits.trailing_zeros() as i32;
                bits &= bits - 1;
                out.push(base + sign * (w as i32 * 64 + b));
            }
        }
    };
    if let Some(e) = ext {
        for k in 0..EXT_BITMAP_WORDS {
            push_bits(&e.negative[k], -BITMAP_SIZE * (k as i32 + 1) - 1, -1);
        }
    }
    push_bits(bitmap, -BITMAP_SIZE, 1);
    if let Some(e) = ext {
        for k in 0..EXT_BITMAP_WORDS {
            push_bits(&e.positive[k], BITMAP_SIZE * (k as i32 + 1), 1);
        }
    }
    out.sort_unstable();
    out
}

/// The next array with liquidity at or beyond `from` in the swap direction.
fn next_liquid(liquid: &[i32], from: i32, swap_for_y: bool) -> Option<i32> {
    if swap_for_y {
        let i = liquid.partition_point(|x| *x <= from);
        i.checked_sub(1).map(|i| liquid[i])
    } else {
        let i = liquid.partition_point(|x| *x < from);
        liquid.get(i).copied()
    }
}

/// A search that starts outside the pair's own bitmap needs the extension.
fn needs_extension(index: i32) -> bool {
    !(-BITMAP_SIZE..BITMAP_SIZE).contains(&index)
}

/// Bin arrays a swap from `active_id` walks, in walk order, at most `take`.
pub fn arrays_for_swap(liquid: &[i32], active_id: i32, has_extension: bool, swap_for_y: bool, take: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(take);
    let mut from = array_index(active_id);
    while out.len() < take {
        if needs_extension(from) && !has_extension {
            break;
        }
        match next_liquid(liquid, from, swap_for_y) {
            Some(i) => {
                out.push(i);
                from = if swap_for_y { i - 1 } else { i + 1 };
            }
            None => break,
        }
    }
    out
}

// ── price and per-bin amounts ─────────────────────────────────────────────

/// `(1 + bin_step/1e4)^id` in Q64.64 — the program's `pow`: square-and-multiply
/// over the 19 exponent bits on the inverted base, one final inversion.
pub fn price_from_id(id: i32, bin_step: u16) -> Option<u128> {
    let base = Q64.checked_add(((bin_step as u128) << 64) / BASIS_POINT_MAX as u128)?;
    pow_q64(base, id)
}

fn pow_q64(base: u128, exp: i32) -> Option<u128> {
    if exp == 0 {
        return Some(Q64);
    }
    let mut invert = exp < 0;
    let exp = exp.unsigned_abs();
    if exp >= 0x80000 {
        return None;
    }
    let mut squared = base;
    let mut result = Q64;
    if squared >= result {
        squared = u128::MAX.checked_div(squared)?;
        invert = !invert;
    }
    for bit in 0..19 {
        if bit > 0 {
            squared = squared.checked_mul(squared)? >> 64;
        }
        if exp & (1 << bit) != 0 {
            result = result.checked_mul(squared)? >> 64;
        }
    }
    if result == 0 {
        return None;
    }
    if invert {
        result = u128::MAX.checked_div(result)?;
    }
    Some(result)
}

/// Input needed for `out` at `price`, rounded up.
fn amount_in_for(out: u64, price: u128, swap_for_y: bool) -> Option<u64> {
    let v = if swap_for_y { mul_div_ceil(out as u128, Q64, price)? } else { mul_div_ceil(out as u128, price, Q64)? };
    u64::try_from(v).ok()
}

/// Output of `amount` at `price`, rounded down.
fn amount_out_for(amount: u64, price: u128, swap_for_y: bool) -> Option<u64> {
    let v = if swap_for_y { mul_div_floor(price, amount as u128, Q64)? } else { mul_div_floor(amount as u128, Q64, price)? };
    u64::try_from(v).ok()
}

/// Fee included in `amount` (fee on a partially filled bin): ceil(amount·rate/1e9).
fn fee_from_amount(amount: u64, rate: u128) -> Option<u64> {
    u64::try_from((amount as u128 * rate).div_ceil(FEE_PRECISION)).ok()
}

/// Fee on top of `amount` (fee on a drained bin): ceil(amount·rate/(1e9 − rate)).
fn fee_on_top(amount: u64, rate: u128) -> Option<u64> {
    u64::try_from((amount as u128 * rate).div_ceil(FEE_PRECISION - rate)).ok()
}

/// Fill up to `max_out` of one liquidity layer: (in used, out).
fn fill(amount: u64, max_out: u64, price: u128, swap_for_y: bool) -> Option<(u64, u64)> {
    if max_out == 0 || amount == 0 {
        return Some((0, 0));
    }
    let max_in = amount_in_for(max_out, price, swap_for_y)?;
    if amount >= max_in {
        Some((max_in, max_out))
    } else {
        Some((amount, amount_out_for(amount, price, swap_for_y)?))
    }
}

/// One bin: `(input consumed incl. fee, output net of fee, fee)`. Market-maker
/// liquidity fills first, then processed limit orders, then open ones.
fn swap_in_bin(bin: &Bin, price: u128, rate: u128, amount: u64, swap_for_y: bool, limit_orders: bool, fee_on_input: bool) -> Option<(u64, u64, u64)> {
    let mut fee = 0;
    let mut excluded = amount;
    if fee_on_input {
        fee = fee_from_amount(amount, rate)?;
        excluded = amount.checked_sub(fee)?;
    }
    let mm = if swap_for_y { bin.amount_y } else { bin.amount_x };
    let (mut used, mut out) = fill(excluded, mm, price, swap_for_y)?;
    if limit_orders && used < excluded {
        let (open, processed) = bin.limit_orders(swap_for_y);
        let (u, o) = fill(excluded - used, processed, price, swap_for_y)?;
        used += u;
        out += o;
        if used < excluded {
            let (u, o) = fill(excluded - used, open, price, swap_for_y)?;
            used += u;
            out += o;
        }
    }
    let mut consumed = amount;
    if used < excluded {
        // bin drained: the fee is charged on what it absorbed
        if fee_on_input {
            fee = fee_on_top(used, rate)?;
            consumed = used.checked_add(fee)?;
        } else {
            consumed = used;
        }
    }
    if !fee_on_input {
        fee = fee_from_amount(out, rate)?;
        out = out.checked_sub(fee)?;
    }
    Some((consumed, out, fee))
}

// ── bin data ──────────────────────────────────────────────────────────────

/// A pair's LbPair + bin arrays + bitmap extension, read in one call.
#[derive(Debug, Clone)]
pub struct DlmmBins {
    pub pair: DlmmPair,
    /// Loaded arrays, ascending by index.
    pub arrays: Vec<BinArray>,
    /// Every array index with liquidity (pair bitmap + extension).
    pub liquid: Vec<i32>,
    /// The bitmap extension PDA, and whether the account exists on chain.
    pub ext_key: Pubkey,
    pub extension: Option<Pubkey>,
    pub ext_bitmap: Option<ExtBitmap>,
    /// (reserve_in, reserve_out) for X→Y and Y→X — see `implied_reserves`.
    reserves: [(u128, u128); 2],
    /// Slot the snapshot was read at, and that slot's `Clock.unix_timestamp`
    /// (0 = unknown).
    pub slot: u64,
    pub clock_unix: i64,
    pub fetched_at: Instant,
}

impl DlmmBins {
    pub fn new(pair: DlmmPair, mut arrays: Vec<BinArray>, ext_key: Pubkey, ext_bitmap: Option<ExtBitmap>, slot: u64, clock_unix: i64) -> Self {
        arrays.sort_unstable_by_key(|a| a.index);
        arrays.dedup_by_key(|a| a.index);
        let liquid = liquid_arrays(&pair.bitmap, ext_bitmap.as_ref());
        let reserves = [implied_reserves(&pair, &arrays, true), implied_reserves(&pair, &arrays, false)];
        let extension = ext_bitmap.is_some().then_some(ext_key);
        Self { pair, arrays, liquid, ext_key, extension, ext_bitmap, reserves, slot, clock_unix, fetched_at: Instant::now() }
    }

    /// The cluster clock now, as `update_references` will read it: the
    /// snapshot slot's `Clock.unix_timestamp` plus the time since. The cluster
    /// clock runs 1–2 s behind wall time, which matters at the `filter_period`
    /// edge (reading it early is the conservative side: volatility only decays
    /// with time).
    pub fn chain_now(&self) -> i64 {
        if self.clock_unix == 0 {
            return now_unix();
        }
        self.clock_unix + self.fetched_at.elapsed().as_secs() as i64
    }

    fn array(&self, index: i32) -> Option<&BinArray> {
        self.arrays.binary_search_by_key(&(index as i64), |a| a.index).ok().map(|i| &self.arrays[i])
    }

    /// Arrays the swap walks from the snapshot's active bin, in walk order.
    pub fn arrays_for_swap(&self, swap_for_y: bool, take: usize) -> Vec<i32> {
        arrays_for_swap(&self.liquid, self.pair.active_id, self.extension.is_some(), swap_for_y, take)
    }

    /// Usable for a quote: young enough, and not older than the pool state the
    /// caller holds — a state that saw a later swap (account stream) means the
    /// active bin moved since this snapshot.
    pub fn is_current(&self, state_pair: &DlmmPair, max_age: std::time::Duration) -> bool {
        self.fetched_at.elapsed() <= max_age && state_pair.last_update_timestamp <= self.pair.last_update_timestamp
    }

    /// True when every array a swap can walk (`SWAP_ARRAYS` each way) is loaded.
    pub fn covers_both_directions(&self) -> bool {
        [true, false].iter().all(|d| self.arrays_for_swap(*d, SWAP_ARRAYS).iter().all(|i| self.array(*i).is_some()))
    }

    pub fn implied_reserves(&self, swap_for_y: bool) -> (u128, u128) {
        self.reserves[if swap_for_y { 0 } else { 1 }]
    }
}

/// Out-side liquidity of the loaded arrays and its value in input units at
/// the active price — (reserve_in, reserve_out) for price-impact reporting.
fn implied_reserves(pair: &DlmmPair, arrays: &[BinArray], swap_for_y: bool) -> (u128, u128) {
    let out: u128 = arrays
        .iter()
        .flat_map(|a| a.bins.iter())
        .map(|b| if swap_for_y { b.amount_y } else { b.amount_x } as u128)
        .sum();
    let price = price_from_id(pair.active_id, pair.bin_step).unwrap_or(Q64).max(1);
    let inp = if swap_for_y { mul_div_ceil(out, Q64, price) } else { mul_div_ceil(out, price, Q64) };
    (inp.unwrap_or(0), out)
}

/// pair → bins. Filled off the quote path (`pool::bins`).
pub static BINS: std::sync::LazyLock<DashMap<Pubkey, Arc<DlmmBins>>> = std::sync::LazyLock::new(DashMap::new);

// ── the walk ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlmmQuote {
    pub amount_out: u64,
    pub fee: u64,
    /// Active bin before the swap (after an empty-gap shift) and where it stopped.
    pub start_bin: i32,
    pub end_bin: i32,
    /// Bin arrays walked.
    pub arrays: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// Disabled, not activated yet, or unparsed.
    NotTradable,
    /// Liquidity runs out in the swap direction (or past `SWAP_ARRAYS`).
    Exhausted,
    /// The walk needs an array this snapshot does not hold.
    NotLoaded,
    Math,
}

/// Exact-input swap of `amount` (already net of any transfer fee) at unix
/// time `now` / slot `slot`, walking at most `max_arrays` bin arrays.
pub fn swap_exact_in(data: &DlmmBins, swap_for_y: bool, amount: u64, now: i64, slot: u64, max_arrays: usize) -> Result<DlmmQuote, WalkError> {
    let mut pair = data.pair;
    if !pair.is_parsed() || !pair.is_tradable(now, slot) {
        return Err(WalkError::NotTradable);
    }
    pair.update_references(now);
    let limit_orders = pair.supports_limit_orders();
    let fee_on_input = pair.fee_on_input(swap_for_y);
    let has_ext = data.extension.is_some();

    let mut left = amount;
    let mut out_total: u64 = 0;
    let mut fee_total: u64 = 0;
    let mut arrays = 0usize;
    let mut start_bin = None;
    while left > 0 {
        let from = array_index(pair.active_id);
        if needs_extension(from) && !has_ext {
            return Err(WalkError::Exhausted);
        }
        let idx = next_liquid(&data.liquid, from, swap_for_y).ok_or(WalkError::Exhausted)?;
        arrays += 1;
        if arrays > max_arrays {
            return Err(WalkError::Exhausted);
        }
        let arr = data.array(idx).ok_or(WalkError::NotLoaded)?;
        let lower = arr.lower_bin_id();
        let upper = lower + BINS_PER_ARRAY - 1;
        // empty gap: jump to the edge of the next array with liquidity
        if idx != from {
            pair.active_id = if swap_for_y { upper } else { lower };
        }
        start_bin.get_or_insert(pair.active_id);
        while left > 0 && (lower..=upper).contains(&pair.active_id) {
            let bin = &arr.bins[(pair.active_id - lower) as usize];
            if bin.max_amount_out(swap_for_y, limit_orders) > 0 {
                pair.update_volatility_accumulator();
                let rate = pair.total_fee_rate().ok_or(WalkError::Math)?;
                let price = if bin.price != 0 { bin.price } else { price_from_id(pair.active_id, pair.bin_step).ok_or(WalkError::Math)? };
                let (used, out, fee) = swap_in_bin(bin, price, rate, left, swap_for_y, limit_orders, fee_on_input).ok_or(WalkError::Math)?;
                left = left.checked_sub(used).ok_or(WalkError::Math)?;
                out_total = out_total.checked_add(out).ok_or(WalkError::Math)?;
                fee_total = fee_total.saturating_add(fee);
            }
            if left > 0 {
                let next = if swap_for_y { pair.active_id - 1 } else { pair.active_id + 1 };
                if !(MIN_BIN_ID..=MAX_BIN_ID).contains(&next) {
                    return Err(WalkError::Exhausted);
                }
                pair.active_id = next;
            }
        }
    }
    Ok(DlmmQuote { amount_out: out_total, fee: fee_total, start_bin: start_bin.unwrap_or(pair.active_id), end_bin: pair.active_id, arrays })
}

/// Compute units a swap spends: ~90k for the transaction around it plus
/// ~5.5k per bin walked (a full 70-bin array is ≈ 0.5M), with a margin.
pub fn swap_compute_units(bins_walked: u32) -> u32 {
    100_000 + 7_000 * bins_walked
}

/// Compute units for swapping `amount` of `input_mint` on `lb_pair`, from the
/// pair's bins in memory (`None` without them or when the walk fails).
pub fn estimate_compute_units(lb_pair: &Pubkey, input_mint: &Pubkey, amount: u64) -> Option<u32> {
    let bins = BINS.get(lb_pair)?;
    let swap_for_y = *input_mint == bins.pair.token_x_mint;
    let q = swap_exact_in(&bins, swap_for_y, amount, bins.chain_now(), bins.slot, SWAP_ARRAYS).ok()?;
    Some(swap_compute_units(q.start_bin.abs_diff(q.end_bin)))
}

/// Wall-clock unix time (fallback when no cluster clock was read).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Mainnet swaps captured with the pair, its bin arrays, bitmap extension
    /// and Clock read in the SAME slot as a `simulateTransaction` of the swap:
    /// `expect` is the program's own `Swap` event.
    const FIXTURES: &str = include_str!("../../tests/fixtures/dlmm_swaps.json");

    #[derive(serde::Deserialize)]
    struct Expect {
        amount_in: u64,
        amount_out: u64,
        fee: u64,
        start_bin: i32,
        end_bin: i32,
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        note: String,
        slot: u64,
        clock: i64,
        lb_pair: String,
        ext_words: Option<Vec<(String, usize, usize, String)>>,
        swap_for_y: bool,
        amount_in: u64,
        /// [id, amount_x, amount_y, price, open_order_amount, processed_order_remaining_amount, ask]
        bins: Vec<(i32, u64, u64, String, u64, u64, u8)>,
        expect: Expect,
    }

    fn fixtures() -> Vec<Fixture> {
        serde_json::from_str(FIXTURES).unwrap()
    }

    fn lb_pair_bytes(f: &Fixture) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &f.lb_pair).unwrap()
    }

    /// The fixture's bins, laid into the arrays that hold them.
    fn snapshot(f: &Fixture) -> DlmmBins {
        let pair = DlmmPair::parse(&lb_pair_bytes(f)).unwrap();
        let mut arrays: Vec<BinArray> = Vec::new();
        for (id, ax, ay, price, open, processed, ask) in &f.bins {
            let index = array_index(*id) as i64;
            if !arrays.iter().any(|a| a.index == index) {
                arrays.push(BinArray { index, key: Pubkey::new_unique(), bins: vec![Bin::default(); BINS_PER_ARRAY as usize] });
            }
            let arr = arrays.iter_mut().find(|a| a.index == index).unwrap();
            arr.bins[(*id - index as i32 * BINS_PER_ARRAY) as usize] = Bin {
                amount_x: *ax,
                amount_y: *ay,
                price: price.parse().unwrap(),
                open_order_amount: *open,
                processed_order_remaining_amount: *processed,
                limit_order_ask_side: *ask != 0,
            };
        }
        let ext = f.ext_words.as_ref().map(|words| {
            let mut e = ExtBitmap { positive: [[0; 8]; EXT_BITMAP_WORDS], negative: [[0; 8]; EXT_BITMAP_WORDS] };
            for (side, k, w, v) in words {
                let v: u64 = v.parse().unwrap();
                if side == "pos" { e.positive[*k][*w] = v } else { e.negative[*k][*w] = v }
            }
            e
        });
        DlmmBins::new(pair, arrays, Pubkey::new_unique(), ext, f.slot, f.clock)
    }

    #[test]
    fn live_swaps_reproduce_to_the_atom() {
        for f in fixtures() {
            let data = snapshot(&f);
            let q = swap_exact_in(&data, f.swap_for_y, f.amount_in, f.clock, f.slot, SWAP_ARRAYS).unwrap_or_else(|e| panic!("{}: {e:?}", f.note));
            assert_eq!(f.expect.amount_in, f.amount_in, "{}", f.note);
            assert_eq!(q.amount_out, f.expect.amount_out, "{}", f.note);
            assert_eq!(q.fee, f.expect.fee, "{}", f.note);
            assert_eq!((q.start_bin, q.end_bin), (f.expect.start_bin, f.expect.end_bin), "{}", f.note);
        }
    }

    #[test]
    fn walks_cover_the_cases_that_matter() {
        let fx = fixtures();
        let crosses = |f: &Fixture| array_index(f.expect.start_bin) != array_index(f.expect.end_bin);
        assert!(fx.iter().filter(|f| crosses(f)).count() >= 3, "array-boundary crossings");
        assert!(fx.iter().any(|f| f.swap_for_y) && fx.iter().any(|f| !f.swap_for_y));
        assert!(fx.iter().any(|f| f.bins.iter().any(|b| b.4 > 0 && b.6 == 1)), "an ask limit order on the path");
        assert!(fx.iter().any(|f| f.bins.iter().any(|b| b.4 > 0 && b.6 == 0)), "a bid limit order on the path");
        assert!(fx.iter().any(|f| needs_extension(array_index(f.expect.start_bin))), "bitmap-extension range");
    }

    #[test]
    fn price_from_id_is_the_programs_pow() {
        // every bin price the program stored equals our pow, to the bit
        let mut n = 0;
        for f in fixtures() {
            let pair = DlmmPair::parse(&lb_pair_bytes(&f)).unwrap();
            for (id, _, _, price, ..) in &f.bins {
                let stored: u128 = price.parse().unwrap();
                if stored != 0 {
                    assert_eq!(price_from_id(*id, pair.bin_step), Some(stored), "{} bin {id}", f.note);
                    n += 1;
                }
            }
        }
        assert!(n > 150);
        assert_eq!(price_from_id(0, 25), Some(Q64));
    }

    #[test]
    fn parses_live_lb_pair() {
        let f = &fixtures()[0];
        let p = DlmmPair::parse(&lb_pair_bytes(f)).unwrap();
        // SOL/USDC, bin_step 1 (HTvjzs…)
        assert_eq!((p.bin_step, p.base_factor, p.filter_period, p.decay_period, p.reduction_factor), (1, 10_000, 10, 120, 5_000));
        assert_eq!((p.variable_fee_control, p.max_volatility_accumulator, p.protocol_share), (2_000_000, 100_000, 1_000));
        assert_eq!((p.base_fee_power_factor, p.function_type, p.collect_fee_mode, p.pair_type, p.status), (0, 0, 0, 0, 0));
        assert_eq!(p.token_x_mint, Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap());
        assert!(!p.token_x_2022 && !p.token_y_2022 && !p.has_rewards);
        assert_eq!(array_index(p.active_id), -305);
        assert!(liquid_arrays(&p.bitmap, None).contains(&-305));
        assert!(p.supports_limit_orders(), "undetermined function + no reward mint");
        // a token-2022 X, fee-on-Y pair (DPAU7w…)
        let q = DlmmPair::parse(&lb_pair_bytes(&fixtures()[2])).unwrap();
        assert!(q.token_x_2022 && !q.token_y_2022);
        assert_eq!((q.bin_step, q.collect_fee_mode, q.function_type), (80, 1, 2));
        assert!(!q.fee_on_input(true) && q.fee_on_input(false));
        assert!(DlmmPair::parse(&lb_pair_bytes(f)[..900]).is_none());
    }

    #[test]
    fn fee_rate_base_variable_and_cap() {
        let mut p = DlmmPair { bin_step: 1, base_factor: 10_000, variable_fee_control: 2_000_000, max_volatility_accumulator: 100_000, ..Default::default() };
        assert_eq!(p.base_fee_rate(), Some(100_000)); // 1 bp
        p.volatility_accumulator = 18_281;
        // ceil(2e6 · 18281² / 1e11) = ceil(6683.9)
        assert_eq!(p.variable_fee_rate(), Some(6_684));
        assert_eq!(p.total_fee_rate(), Some(106_684));
        p.base_fee_power_factor = 1;
        assert_eq!(p.base_fee_rate(), Some(1_000_000));
        let wild = DlmmPair { bin_step: 400, base_factor: 10_000, variable_fee_control: 1_000_000, volatility_accumulator: 1_000_000, ..Default::default() };
        assert_eq!(wild.total_fee_rate(), Some(MAX_FEE_RATE));
    }

    #[test]
    fn update_references_filter_and_decay_windows() {
        let base = DlmmPair { active_id: 100, index_reference: 90, volatility_accumulator: 30_000, volatility_reference: 7, filter_period: 30, decay_period: 600, reduction_factor: 5_000, max_volatility_accumulator: 350_000, last_update_timestamp: 1_000, bin_step: 10, ..Default::default() };
        let mut p = base;
        p.update_references(1_029); // high-frequency: untouched
        assert_eq!((p.index_reference, p.volatility_reference), (90, 7));
        p.update_volatility_accumulator();
        assert_eq!(p.volatility_accumulator, 7 + 10 * 10_000);
        let mut p = base;
        p.update_references(1_030); // decay window: re-anchor, halve
        assert_eq!((p.index_reference, p.volatility_reference), (100, 15_000));
        let mut p = base;
        p.update_references(1_600); // idle past decay_period
        assert_eq!((p.index_reference, p.volatility_reference), (100, 0));
        let mut p = DlmmPair { index_reference: 0, active_id: -100, max_volatility_accumulator: 350_000, ..base };
        p.update_volatility_accumulator();
        assert_eq!(p.volatility_accumulator, 350_000, "capped");
    }

    #[test]
    fn bitmap_navigation_internal_and_extension() {
        let mut bitmap = [0u64; 16];
        let set = |b: &mut [u64; 16], idx: i32| b[((idx + 512) / 64) as usize] |= 1 << ((idx + 512) % 64);
        for i in [-512, -3, 0, 7, 511] {
            set(&mut bitmap, i);
        }
        let mut ext = ExtBitmap { positive: [[0; 8]; EXT_BITMAP_WORDS], negative: [[0; 8]; EXT_BITMAP_WORDS] };
        ext.positive[0][0] = 1 << 3; // 512 + 3
        ext.positive[1][2] = 1; // 1024 + 128
        ext.negative[0][0] = 1; // −513
        ext.negative[11][7] = 1 << 63; // −512·12 − 1 − 511 = −6656
        let liquid = liquid_arrays(&bitmap, Some(&ext));
        assert_eq!(liquid, vec![-6656, -513, -512, -3, 0, 7, 511, 515, 1152]);
        assert_eq!(liquid_arrays(&bitmap, None), vec![-512, -3, 0, 7, 511]);
        // walk order from bin 10 (array 0)
        assert_eq!(arrays_for_swap(&liquid, 10, true, false, 4), vec![0, 7, 511, 515]);
        assert_eq!(arrays_for_swap(&liquid, 10, true, true, 4), vec![0, -3, -512, -513]);
        // an empty active array is skipped (the program shifts the active bin)
        assert_eq!(arrays_for_swap(&liquid, 70, true, true, 1), vec![0]);
        // leaving the pair's own bitmap needs the extension account
        assert_eq!(arrays_for_swap(&liquid_arrays(&bitmap, None), 10, false, false, 9), vec![0, 7, 511]);
        assert_eq!(arrays_for_swap(&liquid, 515 * 70, false, true, 3), Vec::<i32>::new());
    }

    /// One bin (id 5, price 1.0) holding `amount_y`; bin_step 100 × base_factor 10_000 → a 1 % fee.
    fn one_bin(amount_y: u64) -> DlmmBins {
        let mut bitmap = [0u64; 16];
        bitmap[8] = 1; // array 0
        let pair = DlmmPair { bin_step: 100, base_factor: 10_000, max_volatility_accumulator: 350_000, filter_period: 30, decay_period: 600, active_id: 5, bitmap, ..Default::default() };
        let mut bins = vec![Bin::default(); 70];
        bins[5] = Bin { amount_y, price: Q64, ..Default::default() };
        DlmmBins::new(pair, vec![BinArray { index: 0, key: Pubkey::new_unique(), bins }], Pubkey::new_unique(), None, 0, 0)
    }

    #[test]
    fn partial_and_drained_bin_fees() {
        let rate = 10_000_000u128;
        let data = one_bin(1_000_000);
        assert_eq!(data.pair.total_fee_rate(), Some(rate));
        // partial: fee = ceil(in·r/1e9), out = in − fee
        let q = swap_exact_in(&data, true, 500_000, 0, 0, SWAP_ARRAYS).unwrap();
        assert_eq!((q.amount_out, q.fee), (495_000, 5_000));
        // drained: the bin's 1_000_000 absorb 1_000_000 + ceil(1e6·r/(1e9−r)) of input
        let (used, out, fee) = swap_in_bin(&data.arrays[0].bins[5], Q64, rate, 2_000_000, true, false, true).unwrap();
        assert_eq!((used, out, fee), (1_000_000 + 10_102, 1_000_000, 10_102));
        // … and the walk then runs out of liquidity
        assert_eq!(swap_exact_in(&data, true, 2_000_000, 0, 0, SWAP_ARRAYS), Err(WalkError::Exhausted));
    }

    #[test]
    fn refuses_what_it_cannot_walk() {
        let f = &fixtures()[1]; // crosses from array -43 into -44
        let mut data = snapshot(f);
        assert_eq!(swap_exact_in(&data, f.swap_for_y, f.amount_in, f.clock, f.slot, 1), Err(WalkError::Exhausted), "second array beyond max_arrays");
        data.arrays.retain(|a| a.index != array_index(f.expect.end_bin) as i64);
        assert_eq!(swap_exact_in(&data, f.swap_for_y, f.amount_in, f.clock, f.slot, SWAP_ARRAYS), Err(WalkError::NotLoaded));
        let mut off = snapshot(f);
        off.pair.status = 1;
        assert_eq!(swap_exact_in(&off, f.swap_for_y, 1_000, f.clock, f.slot, SWAP_ARRAYS), Err(WalkError::NotTradable));
    }

    #[test]
    fn snapshot_older_than_the_state_is_not_current() {
        let data = snapshot(&fixtures()[0]);
        let mut state = data.pair;
        assert!(data.is_current(&state, std::time::Duration::from_secs(60)));
        state.last_update_timestamp += 1;
        assert!(!data.is_current(&state, std::time::Duration::from_secs(60)));
        assert!(data.is_current(&DlmmPair::default(), std::time::Duration::from_secs(60)), "unparsed state");
    }
}
