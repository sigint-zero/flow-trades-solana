//! Exact concentrated-liquidity quoting by walking real tick arrays.
//!
//! The single-range approximation (`math::compute_clmm_output_multi_tick` with
//! no tick data) prices every swap as if the current liquidity extended
//! forever. It is exact while a swap stays inside the current tick range and
//! over-quotes as soon as it crosses into thinner liquidity (the program then
//! fails the swap with `TooLittleOutputReceived`). This module runs
//! the loop the pool runs: step to the next initialised tick, apply its
//! `liquidity_net`, repeat.
//!
//! Every rounding is the program's: tick → sqrt price with each program's own
//! integer table (Raydium/PancakeSwap and Orca differ in the low bits), token
//! deltas rounded once (`L·ΔP·2^64 / (Pa·Pb)`, not in two divisions), fees
//! rounded per step. Raydium CLMM pools may also take the fee from the OUTPUT
//! token (`fee_on`), charge a dynamic fee that grows with every tick spacing
//! the swap crosses (the swap then advances one spacing per step), and hold
//! limit orders on ticks, which fill at the tick's price before its
//! liquidity is crossed — all mirrored here.
//!
//! Hot-path rules: no allocation per quote, no RPC. Tick data lives in
//! [`TICKS`], filled off the quote path (pool refresh, block-driven refresher,
//! Geyser account stream). Arithmetic is Q64.64 with 256/512-bit
//! intermediates (`uint`), with a `u128` fast path when the product fits.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;

pub const FEE_DENOMINATOR_PPM: u128 = 1_000_000;
const Q64: u128 = 1u128 << 64;

// ── 256-bit helpers (Knuth division from the `uint` crate — the same
//    primitive Raydium's program uses) ──────────────────────────────────────

uint::construct_uint! {
    pub struct U256(4);
}

uint::construct_uint! {
    struct U512(8);
}

#[inline]
fn u256(a: u128) -> U256 {
    U256::from(a)
}

#[inline]
fn to_u128(v: U256) -> Option<u128> {
    if v.bits() > 128 { None } else { Some(v.low_u128()) }
}

#[inline]
fn u512(v: U256) -> U512 {
    let mut w = [0u64; 8];
    w[..4].copy_from_slice(&v.0);
    U512(w)
}

/// a·b / d on 256-bit operands with a 512-bit product, rounded down or up.
fn mul_div_256(a: U256, b: U256, d: U256, round_up: bool) -> Option<U256> {
    if d.is_zero() {
        return None;
    }
    let (q, r) = (u512(a) * u512(b)).div_mod(u512(d));
    let q = if round_up && !r.is_zero() { q + U512::one() } else { q };
    if q.0[4..].iter().any(|w| *w != 0) {
        return None;
    }
    Some(U256([q.0[0], q.0[1], q.0[2], q.0[3]]))
}

/// floor(a·b / d)
#[inline]
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    if let Some(p) = a.checked_mul(b) {
        return Some(p / d);
    }
    to_u128(u256(a) * u256(b) / u256(d))
}

/// ceil(a·b / d)
#[inline]
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    if let Some(p) = a.checked_mul(b) {
        return Some(p.div_ceil(d));
    }
    let (q, r) = (u256(a) * u256(b)).div_mod(u256(d));
    let q = to_u128(q)?;
    if r.is_zero() { Some(q) } else { q.checked_add(1) }
}

// ── tick → sqrt price (Q64.64), each program's own integer table ─────────

/// Size of an Orca fixed (zero-copy) tick array; other sizes are the Borsh
/// "dynamic" arrays.
const ORCA_FIXED_TICK_ARRAY_LEN: usize = 9_988;

pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;
/// Raydium's swap price limits (a swap with no explicit limit runs to MIN+1 / MAX−1).
const RAYDIUM_MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
const RAYDIUM_MAX_SQRT_PRICE_X64: u128 = 79_226_673_521_066_979_257_578_248_091;
/// Orca's (and Fusion's) upper price limit.
const ORCA_MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Raydium CLMM (and its forks) `tick_math::get_sqrt_price_at_tick`.
pub fn raydium_sqrt_price_at_tick(tick: i32) -> u128 {
    const RATIOS: [u128; 18] = [
        0xfff97272373d4000, 0xfff2e50f5f657000, 0xffe5caca7e10f000, 0xffcb9843d60f7000, 0xff973b41fa98e800,
        0xff2ea16466c9b000, 0xfe5dee046a9a3800, 0xfcbe86c7900bb000, 0xf987a7253ac65800, 0xf3392b0822bb6000,
        0xe7159475a2caf000, 0xd097f3bdfd2f2000, 0xa9f746462d9f8000, 0x70d869a156f31c00, 0x31be135f97ed3200,
        0x9aa508b5b85a500, 0x5d6af8dedc582c, 0x2216e584f5fa,
    ];
    let abs = tick.unsigned_abs();
    let mut ratio: u128 = if abs & 1 != 0 { 0xfffcb933bd6fb800 } else { Q64 };
    for (i, r) in RATIOS.iter().enumerate() {
        if abs & (2 << i) != 0 {
            ratio = (ratio * r) >> 64;
        }
    }
    if tick > 0 { u128::MAX / ratio } else { ratio }
}

/// sqrt(1.0001^tick)·2^64 on Raydium's table.
pub fn sqrt_price_x64_at_tick(tick: i32) -> u128 {
    raydium_sqrt_price_at_tick(tick)
}

/// Orca Whirlpool (and forks) `tick_math::sqrt_price_from_tick_index`.
pub fn orca_sqrt_price_at_tick(tick: i32) -> u128 {
    if tick >= 0 {
        const RATIOS: [u128; 18] = [
            79236085330515764027303304731, 79244008939048815603706035061, 79259858533276714757314932305,
            79291567232598584799939703904, 79355022692464371645785046466, 79482085999252804386437311141,
            79736823300114093921829183326, 80248749790819932309965073892, 81282483887344747381513967011,
            83390072131320151908154831281, 87770609709833776024991924138, 97234110755111693312479820773,
            119332217159966728226237229890, 179736315981702064433883588727, 407748233172238350107850275304,
            2098478828474011932436660412517, 55581415166113811149459800483533, 38992368544603139932233054999993551,
        ];
        let mut ratio: u128 = if tick & 1 != 0 { 79232123823359799118286999567 } else { 79228162514264337593543950336 };
        for (i, r) in RATIOS.iter().enumerate() {
            if tick & (2 << i) != 0 {
                ratio = ((u256(ratio) * u256(*r)) >> 96).low_u128();
            }
        }
        ratio >> 32
    } else {
        const RATIOS: [u128; 18] = [
            18444899583751176498, 18443055278223354162, 18439367220385604838, 18431993317065449817,
            18417254355718160513, 18387811781193591352, 18329067761203520168, 18212142134806087854,
            17980523815641551639, 17526086738831147013, 16651378430235024244, 15030750278693429944,
            12247334978882834399, 8131365268884726200, 3584323654723342297, 696457651847595233,
            26294789957452057, 37481735321082,
        ];
        let abs = tick.unsigned_abs();
        let mut ratio: u128 = if abs & 1 != 0 { 18445821805675392311 } else { Q64 };
        for (i, r) in RATIOS.iter().enumerate() {
            if abs & (2 << i) != 0 {
                ratio = (ratio * r) >> 64;
            }
        }
        ratio
    }
}

// ── price / amount math (Q64.64 sqrt prices) ──────────────────────────────

/// Token-0 (a) amount between two sqrt prices: L·(B−A)·2^64 / (A·B), rounded once.
#[inline]
fn amount0_delta(mut a: u128, mut b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if a == 0 {
        return None;
    }
    let num = u256(liquidity) * u256(b - a);
    to_u128(mul_div_256(num, U256::one() << 64, u256(a) * u256(b), round_up)?)
}

/// Token-1 (b) amount between two sqrt prices: L·(B−A)/2^64.
#[inline]
fn amount1_delta(mut a: u128, mut b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if round_up { mul_div_ceil(liquidity, b - a, Q64) } else { mul_div_floor(liquidity, b - a, Q64) }
}

/// New sqrt price after adding `amount` of the INPUT token.
#[inline]
fn next_sqrt_price_from_input(sqrt_p: u128, liquidity: u128, amount: u128, a_to_b: bool) -> Option<u128> {
    if liquidity == 0 || sqrt_p == 0 {
        return None;
    }
    if amount == 0 {
        return Some(sqrt_p);
    }
    if a_to_b {
        // L·2^64·P / (L·2^64 + amount·P), rounded up — the price falls.
        let num = u256(liquidity) << 64;
        let denom = num.checked_add(u256(amount) * u256(sqrt_p))?;
        to_u128(mul_div_256(num, u256(sqrt_p), denom, true)?)
    } else {
        // P + amount·2^64 / L — the price rises.
        sqrt_p.checked_add(mul_div_floor(amount, Q64, liquidity)?)
    }
}

/// Input needed to move the price to `target` (rounded up) and the output it
/// yields (rounded down); `None` when either exceeds u64 (the program then
/// treats the target as unreachable).
#[inline]
fn deltas_to(sqrt_p: u128, target: u128, liquidity: u128, a_to_b: bool) -> Option<(u128, u128)> {
    let (a_in, a_out) = if a_to_b {
        (amount0_delta(target, sqrt_p, liquidity, true)?, amount1_delta(target, sqrt_p, liquidity, false)?)
    } else {
        (amount1_delta(sqrt_p, target, liquidity, true)?, amount0_delta(sqrt_p, target, liquidity, false)?)
    };
    (a_in <= u64::MAX as u128 && a_out <= u64::MAX as u128).then_some((a_in, a_out))
}

/// One swap step toward `target` (`swap_math::compute_swap`, exact input).
/// Returns (next_sqrt_price, input consumed incl. fee, output net of fee).
#[inline]
fn swap_step(sqrt_p: u128, target: u128, liquidity: u128, remaining: u128, fee_ppm: u128, a_to_b: bool, fee_on_input: bool) -> Option<(u128, u128, u128)> {
    let for_price = if fee_on_input { mul_div_floor(remaining, FEE_DENOMINATOR_PPM - fee_ppm, FEE_DENOMINATOR_PPM)? } else { remaining };
    let (next, amount_in, amount_out) = match deltas_to(sqrt_p, target, liquidity, a_to_b) {
        Some((a_in, a_out)) if for_price >= a_in => (target, a_in, a_out),
        _ => {
            let next = next_sqrt_price_from_input(sqrt_p, liquidity, for_price, a_to_b)?;
            let (a_in, a_out) = deltas_to(sqrt_p, next, liquidity, a_to_b)?;
            (next, a_in, a_out)
        }
    };
    if (a_to_b && next < target) || (!a_to_b && next > target) {
        return None;
    }
    let reached = next == target;
    if fee_on_input {
        let fee = if reached { mul_div_ceil(amount_in, fee_ppm, FEE_DENOMINATOR_PPM - fee_ppm)? } else { remaining.checked_sub(amount_in)? };
        Some((next, amount_in.checked_add(fee)?, amount_out))
    } else {
        let fee = mul_div_ceil(amount_out, fee_ppm, FEE_DENOMINATOR_PPM)?;
        Some((next, if reached { amount_in } else { remaining }, amount_out - fee))
    }
}

/// DefiTuna Fusion `fill_limit_orders` (exact input): the swap buys the
/// orders' tokens at the tick's price, fee on the input.
/// Returns (input consumed incl. fee, output).
fn fill_fusion_orders(unfilled: u128, remaining: u128, tick_sqrt_price: u128, fee_ppm: u128, a_to_b: bool) -> Option<(u128, u128)> {
    if unfilled == 0 {
        return Some((0, 0));
    }
    let sq = u256(tick_sqrt_price);
    // input that buys every order token, rounded up
    let full_in = if a_to_b {
        // orders pay token B for A: A = B / P
        let (q, r) = (u256(unfilled) << 128).div_mod(sq * sq);
        to_u128(if r.is_zero() { q } else { q + U256::one() })?
    } else {
        let price = (sq * sq) >> 64;
        let v = u256(unfilled) * price;
        let q = to_u128(v >> 64)?;
        if v.low_u64() != 0 { q + 1 } else { q }
    };
    let fee = mul_div_ceil(full_in, fee_ppm, FEE_DENOMINATOR_PPM - fee_ppm)?;
    if remaining >= full_in + fee {
        return Some((full_in + fee, unfilled));
    }
    let fee = mul_div_ceil(remaining, fee_ppm, FEE_DENOMINATOR_PPM)?;
    let amount_in = remaining - fee;
    Some((remaining, mul_div_floor(unfilled, amount_in, full_in)?))
}

// ── Raydium CLMM fee side and dynamic fee ─────────────────────────────────

/// Raydium CLMM `DynamicFeeInfo` (pool offset 1096, 80 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RaydiumDynamicFee {
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub dynamic_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub tick_spacing_index_reference: i32,
    pub volatility_reference: u32,
    pub volatility_accumulator: u32,
    pub last_update_timestamp: u64,
}

/// Raydium CLMM pool fee features beyond the classic step (`fee_on` at 390,
/// dynamic fee at 1096). Default = fee from the input, no dynamic fee — also
/// what PancakeSwap and the Orca forks do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RaydiumFeeExt {
    /// 0: fee on the input token; 1: always token 0; 2: always token 1.
    pub fee_on: u8,
    pub dynamic_fee: Option<RaydiumDynamicFee>,
}

impl RaydiumFeeExt {
    /// Parse from a Raydium CLMM pool account (1544 B).
    pub fn parse(d: &[u8]) -> Self {
        if d.len() < 1176 {
            return Self::default();
        }
        let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let dynamic = &d[1096..1176];
        let dynamic_fee = dynamic.iter().any(|b| *b != 0).then(|| RaydiumDynamicFee {
            filter_period: u16_at(1096),
            decay_period: u16_at(1098),
            reduction_factor: u16_at(1100),
            dynamic_fee_control: u32_at(1102),
            max_volatility_accumulator: u32_at(1106),
            tick_spacing_index_reference: u32_at(1110) as i32,
            volatility_reference: u32_at(1114),
            volatility_accumulator: u32_at(1118),
            last_update_timestamp: u64::from_le_bytes(d[1122..1130].try_into().unwrap()),
        });
        Self { fee_on: d[390], dynamic_fee }
    }

    pub fn fee_on_input(&self, zero_for_one: bool) -> bool {
        match self.fee_on {
            1 => zero_for_one,
            2 => !zero_for_one,
            _ => true,
        }
    }
}

const VOLATILITY_ACCUMULATOR_SCALE: u64 = 10_000;
const MAX_DYNAMIC_FEE_RATE: u32 = 100_000;

impl RaydiumDynamicFee {
    fn update_reference(&mut self, tick_spacing_index: i32, now: u64) {
        let elapsed = now.saturating_sub(self.last_update_timestamp);
        if elapsed < self.filter_period as u64 {
            return;
        }
        self.tick_spacing_index_reference = tick_spacing_index;
        self.volatility_reference = if elapsed < self.decay_period as u64 {
            (self.volatility_accumulator as u64 * self.reduction_factor as u64 / 10_000) as u32
        } else {
            0
        };
        self.last_update_timestamp = now;
    }

    fn update_volatility_accumulator(&mut self, tick_spacing_index: i32) {
        let delta = (self.tick_spacing_index_reference as i64 - tick_spacing_index as i64).unsigned_abs();
        let v = self.volatility_reference as u64 + delta * VOLATILITY_ACCUMULATOR_SCALE;
        self.volatility_accumulator = v.min(self.max_volatility_accumulator as u64) as u32;
    }

    /// `base + ceil(control · (accumulator·spacing)² / 1e13)`, capped at 10 %.
    fn total_fee_rate(&self, base: u32, tick_spacing: i32) -> u32 {
        let crossed = self.volatility_accumulator as u128 * tick_spacing as u128;
        let dynamic = (self.dynamic_fee_control as u128 * crossed * crossed).div_ceil(100_000u128 * 10_000 * 10_000);
        (base as u128 + dynamic).min(MAX_DYNAMIC_FEE_RATE as u128) as u32
    }
}

// ── Orca adaptive fee (`Oracle` account) ──────────────────────────────────

/// Orca `Oracle` account (PDA `["oracle", whirlpool]`, 254 B): adaptive fee
/// constants and variables. A whirlpool whose oracle exists charges its
/// static fee plus a volatility fee re-read every `tick_group_size` ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OrcaAdaptiveFee {
    pub trade_enable_timestamp: u64,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub adaptive_fee_control_factor: u32,
    pub max_volatility_accumulator: u32,
    pub tick_group_size: u16,
    pub last_reference_update_timestamp: u64,
    pub last_major_swap_timestamp: u64,
    pub volatility_reference: u32,
    pub tick_group_index_reference: i32,
    pub volatility_accumulator: u32,
}

/// `sha256("account:Oracle")[..8]`
pub const ORCA_ORACLE_DISCRIMINATOR: [u8; 8] = [139, 194, 131, 179, 140, 179, 229, 244];
/// An adaptive-fee reference older than this is reset (`MAX_REFERENCE_AGE`).
const ORCA_MAX_REFERENCE_AGE: u64 = 3_600;

impl OrcaAdaptiveFee {
    /// Parse an `Oracle` account belonging to `whirlpool`.
    pub fn parse(d: &[u8], whirlpool: &Pubkey) -> Option<Self> {
        if d.len() < 126 || d[..8] != ORCA_ORACLE_DISCRIMINATOR || d[8..40] != whirlpool.to_bytes() {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let group = u16_at(62);
        if group == 0 {
            return None;
        }
        Some(Self {
            trade_enable_timestamp: u64_at(40),
            filter_period: u16_at(48),
            decay_period: u16_at(50),
            reduction_factor: u16_at(52),
            adaptive_fee_control_factor: u32_at(54),
            max_volatility_accumulator: u32_at(58),
            tick_group_size: group,
            last_reference_update_timestamp: u64_at(82),
            last_major_swap_timestamp: u64_at(90),
            volatility_reference: u32_at(98),
            tick_group_index_reference: u32_at(102) as i32,
            volatility_accumulator: u32_at(106),
        })
    }

    /// `AdaptiveFeeVariables::update_reference` (a clock behind the stored
    /// timestamps is treated as equal to them).
    fn update_reference(&mut self, tick_group_index: i32, now: u64) {
        let max_timestamp = self.last_reference_update_timestamp.max(self.last_major_swap_timestamp);
        let now = now.max(max_timestamp);
        if now - self.last_reference_update_timestamp > ORCA_MAX_REFERENCE_AGE {
            self.tick_group_index_reference = tick_group_index;
            self.volatility_reference = 0;
            self.last_reference_update_timestamp = now;
            return;
        }
        let elapsed = now - max_timestamp;
        if elapsed < self.filter_period as u64 {
            return;
        }
        self.tick_group_index_reference = tick_group_index;
        self.volatility_reference = if elapsed < self.decay_period as u64 {
            (self.volatility_accumulator as u64 * self.reduction_factor as u64 / 10_000) as u32
        } else {
            0
        };
        self.last_reference_update_timestamp = now;
    }

    fn update_volatility_accumulator(&mut self, tick_group_index: i32) {
        let delta = (self.tick_group_index_reference as i64 - tick_group_index as i64).unsigned_abs();
        let v = self.volatility_reference as u64 + delta * VOLATILITY_ACCUMULATOR_SCALE;
        self.volatility_accumulator = v.min(self.max_volatility_accumulator as u64) as u32;
    }

    fn total_fee_rate(&self, static_fee: u32) -> u32 {
        let crossed = self.volatility_accumulator as u128 * self.tick_group_size as u128;
        let adaptive = (self.adaptive_fee_control_factor as u128 * crossed * crossed).div_ceil(100_000u128 * 10_000 * 10_000);
        (static_fee as u128 + adaptive.min(MAX_DYNAMIC_FEE_RATE as u128)).min(MAX_DYNAMIC_FEE_RATE as u128) as u32
    }
}

/// Orca `tick_index_from_sqrt_price`.
pub fn orca_tick_at_sqrt_price(sqrt_price: u128) -> i32 {
    let msb = 127 - sqrt_price.leading_zeros();
    let log2p_integer_x32 = (msb as i128 - 64) << 32;
    let mut bit: i128 = 0x8000_0000_0000_0000;
    let mut log2p_fraction_x64: i128 = 0;
    let mut r = if msb >= 64 { sqrt_price >> (msb - 63) } else { sqrt_price << (63 - msb) };
    for _ in 0..14 {
        r *= r;
        let more_than_two = (r >> 127) as u32;
        r >>= 63 + more_than_two;
        log2p_fraction_x64 += bit * more_than_two as i128;
        bit >>= 1;
    }
    let log2p_x32 = log2p_integer_x32 + (log2p_fraction_x64 >> 32);
    let logbp_x64 = log2p_x32 * 59_543_866_431_248i128;
    let tick_low = ((logbp_x64 - 184_467_440_737_095_516i128) >> 64) as i32;
    let tick_high = ((logbp_x64 + 15_793_534_762_490_258_745i128) >> 64) as i32;
    if tick_low == tick_high || orca_sqrt_price_at_tick(tick_high) > sqrt_price { tick_low } else { tick_high }
}

/// The swap's per-step fee: static, Raydium dynamic (re-read every tick
/// spacing) or Orca adaptive (re-read every tick group).
enum StepFee {
    Static,
    Raydium { fee: RaydiumDynamicFee, ts_index: i32 },
    Orca { fee: OrcaAdaptiveFee, group_index: i32, core_lower: Option<(i32, u128)>, core_upper: Option<(i32, u128)> },
}

impl StepFee {
    fn new(pool: &ClmmPool) -> Option<Self> {
        if let Some(mut fee) = pool.fee_ext.dynamic_fee {
            let ts_index = tick_spacing_index(pool.tick_current, pool.tick_spacing);
            fee.update_reference(ts_index, pool.now);
            return Some(StepFee::Raydium { fee, ts_index });
        }
        let Some(mut fee) = pool.adaptive_fee else { return Some(StepFee::Static) };
        if pool.now < fee.trade_enable_timestamp {
            return None; // trading not enabled yet
        }
        let group = fee.tick_group_size as i32;
        let group_index = pool.tick_current.div_euclid(group);
        fee.update_reference(group_index, pool.now);
        // tick groups beyond the "core" range already saturate the accumulator
        let delta = (fee.max_volatility_accumulator.saturating_sub(fee.volatility_reference) as u64).div_ceil(VOLATILITY_ACCUMULATOR_SCALE) as i32;
        let (lo, hi) = (fee.tick_group_index_reference - delta, fee.tick_group_index_reference + delta);
        let (lo_tick, hi_tick) = (lo * group, hi * group + group);
        let core_lower = (lo_tick > MIN_TICK).then(|| (lo, orca_sqrt_price_at_tick(lo_tick)));
        let core_upper = (hi_tick < MAX_TICK).then(|| (hi, orca_sqrt_price_at_tick(hi_tick)));
        Some(StepFee::Orca { fee, group_index, core_lower, core_upper })
    }

    /// (fee rate, bounded target, adaptive update skipped) for the next step.
    fn step(&mut self, base: u32, target: u128, liquidity: u128, spacing: i32, layout: TickLayout, a_to_b: bool) -> (u32, u128, bool) {
        match self {
            StepFee::Static => (base, target, true),
            StepFee::Raydium { fee, ts_index } => {
                fee.update_volatility_accumulator(*ts_index);
                let rate = fee.total_fee_rate(base, spacing);
                if liquidity == 0 || fee.volatility_accumulator == fee.max_volatility_accumulator {
                    return (rate, target, true);
                }
                let b_tick = if a_to_b { ts_index.saturating_mul(spacing) } else { ts_index.saturating_add(1).saturating_mul(spacing) }.clamp(MIN_TICK, MAX_TICK);
                let b_price = layout.sqrt_price_at_tick(b_tick);
                let bounded = if (a_to_b && target <= b_price) || (!a_to_b && target >= b_price) { b_price } else { target };
                (rate, bounded, false)
            }
            StepFee::Orca { fee, group_index, core_lower, core_upper } => {
                fee.update_volatility_accumulator(*group_index);
                let rate = fee.total_fee_rate(base);
                if fee.adaptive_fee_control_factor == 0 || liquidity == 0 {
                    return (rate, target, true);
                }
                if let Some((lo, lo_price)) = core_lower {
                    if *group_index < *lo {
                        return (rate, if a_to_b { target } else { target.min(*lo_price) }, true);
                    }
                }
                if let Some((hi, hi_price)) = core_upper {
                    if *group_index > *hi {
                        return (rate, if a_to_b { target.max(*hi_price) } else { target }, true);
                    }
                }
                let group = fee.tick_group_size as i32;
                let b_tick = if a_to_b { *group_index * group } else { *group_index * group + group }.clamp(MIN_TICK, MAX_TICK);
                let b_price = orca_sqrt_price_at_tick(b_tick);
                (rate, if a_to_b { target.max(b_price) } else { target.min(b_price) }, false)
            }
        }
    }

    /// After a step: move to the next tick spacing / group, or re-derive it
    /// from where the price landed when bounding was skipped.
    fn advance(&mut self, skipped: bool, sqrt_p: u128, next_tick_price: u128, next_tick: i32, tick: i32, spacing: i32, a_to_b: bool) {
        let dir = if a_to_b { -1 } else { 1 };
        match self {
            StepFee::Static => {}
            StepFee::Raydium { fee, ts_index } => {
                if skipped {
                    let t = if sqrt_p == next_tick_price { next_tick } else { tick };
                    *ts_index = tick_spacing_index(t, spacing);
                    if !a_to_b && t % spacing == 0 {
                        *ts_index -= 1;
                    }
                    if fee.volatility_accumulator != fee.max_volatility_accumulator {
                        fee.update_volatility_accumulator(*ts_index);
                    }
                }
                *ts_index += dir;
            }
            StepFee::Orca { fee, group_index, .. } => {
                if skipped {
                    let group = fee.tick_group_size as i32;
                    let (t, on_boundary) = if sqrt_p == next_tick_price {
                        (next_tick, next_tick % group == 0)
                    } else {
                        let t = orca_tick_at_sqrt_price(sqrt_p);
                        (t, t % group == 0 && sqrt_p == orca_sqrt_price_at_tick(t))
                    };
                    let last = if on_boundary && !a_to_b { t / group - 1 } else { t.div_euclid(group) };
                    if (a_to_b && last < *group_index) || (!a_to_b && last > *group_index) {
                        *group_index = last;
                        fee.update_volatility_accumulator(*group_index);
                    }
                }
                *group_index += dir;
            }
        }
    }
}

/// `tick_spacing_index_from_tick`: floor(tick / spacing).
#[inline]
fn tick_spacing_index(tick: i32, spacing: i32) -> i32 {
    tick.div_euclid(spacing)
}

/// Limit orders resting on a Raydium tick fill at the tick's price.
/// Returns (input consumed incl. fee, output net of fee, order amount filled).
fn match_limit_orders(unfilled: u128, remaining: u128, tick_sqrt_price: u128, fee_ppm: u128, a_to_b: bool, fee_on_input: bool) -> Option<(u128, u128, u128)> {
    if unfilled == 0 || remaining == 0 {
        return Some((0, 0, 0));
    }
    // token-1 per token-0 price; rounded up when token 1 is paid in
    let sq = u256(tick_sqrt_price);
    let prod = sq * sq + if a_to_b { U256::zero() } else { u256(Q64 - 1) };
    let price = to_u128(prod >> 64)?;
    let (mut amount_in, mut fee) = if fee_on_input {
        let fee = mul_div_ceil(remaining, fee_ppm, FEE_DENOMINATOR_PPM)?;
        (remaining - fee, fee)
    } else {
        (remaining, 0)
    };
    let matched = if a_to_b { mul_div_floor(amount_in, price, Q64)? } else { mul_div_floor(amount_in, Q64, price)? };
    let amount_out = if matched > unfilled {
        amount_in = if a_to_b { mul_div_ceil(unfilled, Q64, price)? } else { mul_div_ceil(unfilled, price, Q64)? };
        if fee_on_input {
            fee = mul_div_ceil(amount_in, fee_ppm, FEE_DENOMINATOR_PPM - fee_ppm)?;
        }
        unfilled
    } else {
        matched
    };
    if fee_on_input {
        Some((amount_in.checked_add(fee)?, amount_out, amount_out))
    } else {
        let out_fee = mul_div_ceil(amount_out, fee_ppm, FEE_DENOMINATOR_PPM)?;
        Some((amount_in, amount_out - out_fee, amount_out))
    }
}

// ── tick data ─────────────────────────────────────────────────────────────

/// Initialised ticks around a pool's current price.
#[derive(Debug, Clone)]
pub struct TickData {
    /// (tick index, liquidity_net), ascending by tick, initialised ticks only.
    pub ticks: Vec<(i32, i128)>,
    /// Every tick in `[covered_lo, covered_hi]` is known: present in `ticks` or uninitialised.
    pub covered_lo: i32,
    pub covered_hi: i32,
    /// Start indices (ascending) of the tick arrays that hold at least one
    /// initialised tick — the only arrays a Raydium-style swap accepts.
    pub initialized_arrays: Vec<i32>,
    /// The pool's tick-array bitmap extension account, when it exists on chain.
    pub bitmap_extension: Option<Pubkey>,
    /// (tick index, unfilled limit-order amount), ascending by tick — Raydium
    /// CLMM ticks holding limit orders (they count as initialised).
    pub limit_orders: Vec<(i32, u64)>,
    /// Orca adaptive-fee state (the whirlpool's `Oracle`), read with the ticks.
    pub adaptive_fee: Option<OrcaAdaptiveFee>,
    pub fetched_at: Instant,
}

impl TickData {
    /// Unfilled limit-order amount resting on  (0 if none).
    pub fn limit_orders_at(&self, tick: i32) -> u64 {
        match self.limit_orders.binary_search_by_key(&tick, |(t, _)| *t) {
            Ok(i) => self.limit_orders[i].1,
            Err(_) => 0,
        }
    }

    /// Tick arrays the swap program must receive, in walk order: the first
    /// initialised array at or beyond the current tick in the swap direction,
    /// then the next ones. Empty when nothing initialised lies that way.
    pub fn arrays_for_swap(&self, layout: TickLayout, tick_current: i32, tick_spacing: i32, a_to_b: bool, max: usize) -> Vec<i32> {
        let cur = layout.array_start(tick_current, tick_spacing);
        let mut v: Vec<i32> = if a_to_b {
            self.initialized_arrays.iter().rev().copied().filter(|s| *s <= cur).collect()
        } else {
            self.initialized_arrays.iter().copied().filter(|s| *s >= cur).collect()
        };
        v.truncate(max);
        v
    }
}

/// Discriminator of Byreal's sparse ("dynamic") tick array account, which a
/// pool may hold next to classic 10240-byte `TickArrayState` accounts.
pub const DYNAMIC_TICK_ARRAY_DISC: [u8; 8] = [0x6a, 0x8b, 0x98, 0x24, 0x75, 0x99, 0xb8, 0x38];

/// pool → ticks. Filled off the quote path.
pub static TICKS: std::sync::LazyLock<DashMap<Pubkey, Arc<TickData>>> = std::sync::LazyLock::new(DashMap::new);

/// Which on-chain tick-array layout (and tick → price table) a pool uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickLayout {
    /// Raydium CLMM and forks (PancakeSwap): 60 ticks/array, seed = start index as i32 BE bytes.
    Raydium,
    /// Orca Whirlpool: 88 ticks/array, seed = start index as decimal string.
    Orca,
    /// DefiTuna Fusion: Orca seeds and price table, Borsh tick arrays whose
    /// ticks also hold limit orders.
    Fusion,
}

impl TickLayout {
    pub const fn ticks_per_array(self) -> i32 {
        match self {
            TickLayout::Raydium => 60,
            TickLayout::Orca | TickLayout::Fusion => 88,
        }
    }

    /// Start index of the array holding `tick`.
    pub fn array_start(self, tick: i32, tick_spacing: i32) -> i32 {
        let span = self.ticks_per_array() * tick_spacing.max(1);
        tick.div_euclid(span) * span
    }

    /// The program's own tick → Q64.64 sqrt price table.
    pub fn sqrt_price_at_tick(self, tick: i32) -> u128 {
        match self {
            TickLayout::Raydium => raydium_sqrt_price_at_tick(tick),
            TickLayout::Orca | TickLayout::Fusion => orca_sqrt_price_at_tick(tick),
        }
    }

    pub fn array_pda(self, program: &Pubkey, pool: &Pubkey, start: i32) -> Pubkey {
        match self {
            TickLayout::Raydium => Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), &start.to_be_bytes()], program).0,
            TickLayout::Orca | TickLayout::Fusion => Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), start.to_string().as_bytes()], program).0,
        }
    }

    /// Initialised ticks of one array account: `(start_index, [(tick,
    /// liquidity_net)], [(tick, unfilled limit orders)])`.
    pub fn parse_array(self, data: &[u8], tick_spacing: i32) -> Option<(i32, Vec<(i32, i128)>, Vec<(i32, u64)>)> {
        let rd_i32 = |o: usize| data.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()));
        let rd_i128 = |o: usize| data.get(o..o + 16).map(|b| i128::from_le_bytes(b.try_into().unwrap()));
        let rd_u128 = |o: usize| data.get(o..o + 16).map(|b| u128::from_le_bytes(b.try_into().unwrap()));
        let rd_u64 = |o: usize| data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        let mut out = Vec::new();
        let mut orders = Vec::new();
        match self {
            TickLayout::Raydium if data.get(..8) == Some(&DYNAMIC_TICK_ARRAY_DISC[..]) => {
                // Byreal's sparse array (10296 B): disc(8) pool(32) start(i32),
                // 4 bytes, then a [u8; 60] slot map @48 (tick i → 1-based slot
                // of its TickState, 0 = not allocated), header up to 216, then
                // the allocated TickState(168)s — same record as the classic array.
                let start = rd_i32(40)?;
                for i in 0..60usize {
                    let slot = *data.get(48 + i)? as usize;
                    if slot == 0 {
                        continue;
                    }
                    let o = 216 + (slot - 1) * 168;
                    let unfilled = rd_u64(o + 124)?.saturating_add(rd_u64(o + 132)?);
                    if rd_u128(o + 20)? != 0 || unfilled != 0 {
                        let tick = rd_i32(o)?;
                        if tick != start + i as i32 * tick_spacing {
                            return None;
                        }
                        out.push((tick, rd_i128(o + 4)?));
                        if unfilled != 0 {
                            orders.push((tick, unfilled));
                        }
                    }
                }
                out.sort_unstable_by_key(|(t, _)| *t);
                orders.sort_unstable_by_key(|(t, _)| *t);
                Some((start, out, orders))
            }
            TickLayout::Raydium => {
                // disc(8) pool(32) start(i32) then 60 × TickState(168): tick i32,
                // liquidity_net i128, liquidity_gross u128, fee/reward growths,
                // order_phase u64 @116, orders_amount u64 @124,
                // part_filled_orders_remaining u64 @132 (zero padding in older forks)
                let start = rd_i32(40)?;
                for i in 0..60usize {
                    let o = 44 + i * 168;
                    let unfilled = rd_u64(o + 124)?.saturating_add(rd_u64(o + 132)?);
                    if rd_u128(o + 20)? != 0 || unfilled != 0 {
                        let tick = rd_i32(o)?;
                        out.push((tick, rd_i128(o + 4)?));
                        if unfilled != 0 {
                            orders.push((tick, unfilled));
                        }
                    }
                }
                Some((start, out, orders))
            }
            TickLayout::Orca if data.len() == ORCA_FIXED_TICK_ARRAY_LEN => {
                // disc(8) start(i32) then 88 × Tick(113): initialized u8, liquidity_net i128, …
                let start = rd_i32(8)?;
                for i in 0..88usize {
                    let o = 12 + i * 113;
                    if *data.get(o)? != 0 {
                        out.push((start + i as i32 * tick_spacing, rd_i128(o + 1)?));
                    }
                }
                Some((start, out, orders))
            }
            TickLayout::Orca | TickLayout::Fusion => {
                // Borsh arrays: disc(8) start(i32) pool(32) [Orca: tick_bitmap u128]
                // then 88 × {0 | 1 + TickData(112)}: liquidity_net i128 first;
                // Fusion's TickData holds open_orders_input u64 @72 and
                // part_filled_orders_remaining_input u64 @88.
                let start = rd_i32(8)?;
                let mut o = if self == TickLayout::Orca { 60 } else { 44 };
                for i in 0..88usize {
                    match *data.get(o)? {
                        0 => o += 1,
                        1 => {
                            let tick = start + i as i32 * tick_spacing;
                            out.push((tick, rd_i128(o + 1)?));
                            if self == TickLayout::Fusion {
                                let unfilled = rd_u64(o + 1 + 72)?.saturating_add(rd_u64(o + 1 + 88)?);
                                if unfilled != 0 {
                                    orders.push((tick, unfilled));
                                }
                            }
                            o += 113;
                        }
                        _ => return None,
                    }
                }
                Some((start, out, orders))
            }
        }
    }
}

/// Result of an exact walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkResult {
    pub amount_out: u64,
    pub ticks_crossed: u32,
    pub end_sqrt_price_x64: u128,
}

/// Pool-side inputs of an exact-input swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClmmPool {
    /// Picks the program's tick → price table.
    pub layout: TickLayout,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick_current: i32,
    pub tick_spacing: i32,
    /// Base trade fee, 1e-6 units.
    pub fee_ppm: u32,
    /// Raydium CLMM fee side / dynamic fee (default for every other venue).
    pub fee_ext: RaydiumFeeExt,
    /// Orca adaptive fee (the whirlpool's `Oracle`), when it has one.
    pub adaptive_fee: Option<OrcaAdaptiveFee>,
    /// Unix time the dynamic fee's volatility reference decays against.
    pub now: u64,
}

/// Classic exact-input walk (fee from the input, static fee) on the Orca
/// tick table — kept for callers that have no pool context.
pub fn swap_exact_in(
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    fee_ppm: u32,
    ticks: &TickData,
    a_to_b: bool,
    amount_in: u64,
) -> Option<WalkResult> {
    let pool = ClmmPool { layout: TickLayout::Orca, sqrt_price_x64, liquidity, tick_current, tick_spacing: 1, fee_ppm, fee_ext: RaydiumFeeExt::default(), adaptive_fee: None, now: 0 };
    swap_exact_in_pool(&pool, ticks, a_to_b, amount_in)
}

/// Exact-input swap across initialised ticks (`swap_internal` of the Raydium
/// CLMM program; Orca's loop is the same with the fee always on the input
/// and no dynamic fee or limit orders). `None` when the swap runs past the
/// covered tick range, liquidity runs out, or the program would error —
/// never an optimistic number.
pub fn swap_exact_in_pool(pool: &ClmmPool, ticks: &TickData, a_to_b: bool, amount_in: u64) -> Option<WalkResult> {
    if amount_in == 0 || pool.sqrt_price_x64 == 0 || pool.fee_ppm as u128 >= FEE_DENOMINATOR_PPM || pool.tick_spacing <= 0 {
        return None;
    }
    let layout = pool.layout;
    let spacing = pool.tick_spacing;
    let fee_on_input = pool.fee_ext.fee_on_input(a_to_b);
    let mut step_fee = StepFee::new(pool)?;
    let limit = match (layout, a_to_b) {
        (TickLayout::Raydium, true) => RAYDIUM_MIN_SQRT_PRICE_X64 + 1,
        (TickLayout::Raydium, false) => RAYDIUM_MAX_SQRT_PRICE_X64 - 1,
        (_, true) => RAYDIUM_MIN_SQRT_PRICE_X64,
        (_, false) => ORCA_MAX_SQRT_PRICE_X64,
    };

    let mut remaining = amount_in as u128;
    let mut sqrt_p = pool.sqrt_price_x64;
    let mut tick = pool.tick_current;
    let mut liq = pool.liquidity;
    let mut out: u128 = 0;
    let mut crossed = 0u32;

    // Index of the next tick to cross in the swap direction.
    let mut idx: isize = if a_to_b {
        ticks.ticks.partition_point(|(t, _)| *t <= tick) as isize - 1
    } else {
        ticks.ticks.partition_point(|(t, _)| *t <= tick) as isize
    };

    for _ in 0..512 {
        if remaining == 0 || sqrt_p == limit {
            break;
        }
        let next_tick = (idx >= 0 && (idx as usize) < ticks.ticks.len()).then(|| ticks.ticks[idx as usize]);
        // The next initialised tick, or the edge of what we know (a pseudo
        // tick the walk must not pass with input left).
        let (target_tick, net) = match next_tick {
            Some((t, n)) => (t, Some(n)),
            None => (if a_to_b { ticks.covered_lo } else { ticks.covered_hi.saturating_add(1) }, None),
        };
        let target_tick = target_tick.clamp(MIN_TICK, MAX_TICK);
        let tick_price = layout.sqrt_price_at_tick(target_tick);
        let target = if (a_to_b && tick_price < limit) || (!a_to_b && tick_price > limit) { limit } else { tick_price };
        if (a_to_b && (tick < target_tick || sqrt_p < tick_price)) || (!a_to_b && (target_tick <= tick || tick_price < sqrt_p)) {
            return None; // the program's require! on the step bounds
        }
        let mut liq_next = liq;
        loop {
            // Dynamic / adaptive fee: re-price every tick spacing (group);
            // without one, a single step to the tick.
            let (fee, bounded, skipped) = step_fee.step(pool.fee_ppm, target, liq, spacing, layout, a_to_b);
            let next = if sqrt_p != bounded {
                if liq == 0 {
                    bounded // no liquidity in this range: the price jumps for free
                } else {
                    let (next, used, got) = swap_step(sqrt_p, bounded, liq, remaining, fee as u128, a_to_b, fee_on_input)?;
                    remaining = remaining.checked_sub(used)?;
                    out = out.checked_add(got)?;
                    next
                }
            } else {
                bounded
            };
            if next == tick_price {
                match net {
                    Some(n) => {
                        // Limit orders on the tick fill first. Raydium crosses the
                        // tick's liquidity once none are left; Fusion always does.
                        let unfilled = ticks.limit_orders_at(target_tick) as u128;
                        let (used, got, filled) = match layout {
                            TickLayout::Fusion => {
                                let (used, got) = fill_fusion_orders(unfilled, remaining, tick_price, fee as u128, a_to_b)?;
                                (used, got, unfilled)
                            }
                            _ => match_limit_orders(unfilled, remaining, tick_price, fee as u128, a_to_b, fee_on_input)?,
                        };
                        remaining = remaining.checked_sub(used)?;
                        out = out.checked_add(got)?;
                        let orders_left = unfilled - filled;
                        if orders_left == 0 {
                            // Crossing: moving down removes the tick's net liquidity, moving up adds it.
                            let delta = if a_to_b { -n } else { n };
                            liq_next = if delta >= 0 { liq.checked_add(delta as u128)? } else { liq.checked_sub(delta.unsigned_abs())? };
                            crossed += 1;
                        } else if remaining != 0 {
                            return None;
                        }
                        tick = if (a_to_b && orders_left == 0) || (!a_to_b && orders_left != 0) { target_tick - 1 } else { target_tick };
                    }
                    // reached the edge of the covered range with input left
                    None if remaining != 0 => return None,
                    None => {}
                }
            }
            sqrt_p = next;
            step_fee.advance(skipped, sqrt_p, tick_price, target_tick, tick, spacing, a_to_b);
            if remaining == 0 || sqrt_p == target {
                break;
            }
        }
        liq = liq_next;
        if net.is_none() {
            break;
        }
        idx += if a_to_b { -1 } else { 1 };
    }
    if remaining != 0 {
        return None;
    }
    Some(WalkResult { amount_out: u64::try_from(out).ok()?, ticks_crossed: crossed, end_sqrt_price_x64: sqrt_p })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_matches_wide_reference() {
        // fits-in-u128 path and the 256-bit path must agree with hand values
        assert_eq!(mul_div_floor(10, 20, 3), Some(66));
        assert_eq!(mul_div_ceil(10, 20, 3), Some(67));
        let a = u128::MAX / 3;
        assert_eq!(mul_div_floor(a, 9, 3), Some(a * 3));
        // (2^127)·(2^100)/(2^120) = 2^107 through the wide path
        assert_eq!(mul_div_floor(1 << 127, 1 << 100, 1 << 120), Some(1 << 107));
        assert_eq!(mul_div_ceil((1 << 127) + 1, 1 << 100, 1 << 120), Some((1 << 107) + 1));
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, 1), None, "quotient overflow is refused");
        assert_eq!(mul_div_ceil(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(mul_div_floor(1, 1, 0), None);
    }

    #[test]
    fn tick_tables_match_the_programs() {
        // the programs' own bounds
        assert_eq!(raydium_sqrt_price_at_tick(MIN_TICK), RAYDIUM_MIN_SQRT_PRICE_X64);
        assert_eq!(raydium_sqrt_price_at_tick(MAX_TICK), RAYDIUM_MAX_SQRT_PRICE_X64);
        assert_eq!(orca_sqrt_price_at_tick(MIN_TICK), 4_295_048_016);
        assert_eq!(orca_sqrt_price_at_tick(MAX_TICK), 79_226_673_515_401_279_992_447_579_055);
        assert_eq!(orca_sqrt_price_at_tick(-1), 18_445_821_805_675_392_311);
        assert_eq!(raydium_sqrt_price_at_tick(-1), 18_445_821_805_675_395_072);
        assert_eq!(raydium_sqrt_price_at_tick(0), Q64);
        // the two tables differ by ~1e-12 at larger ticks — far more than an
        // atom on a big pool, so each program's own table is used
        assert_eq!(raydium_sqrt_price_at_tick(66_240), 506_093_934_307_275_701_861);
        assert_eq!(orca_sqrt_price_at_tick(66_240), 506_093_934_307_811_261_938);
        assert_eq!(TickLayout::Fusion.sqrt_price_at_tick(67_648), orca_sqrt_price_at_tick(67_648));
    }

    #[test]
    fn token0_out_is_rounded_once() {
        // PancakeSwap 22HUWiJa… (USDT→SOL, 1 bp, slot 449_557_4xx): 3_008_800
        // USDT in, simulated router output 25_401_298 lamports (rounding
        // L·ΔP/B before the ·2^64/A would give 25_401_296).
        let pool = ClmmPool {
            layout: TickLayout::Raydium,
            sqrt_price_x64: 6_348_430_820_826_501_793,
            liquidity: 6_163_788_825_811,
            tick_current: -21_335,
            tick_spacing: 10,
            fee_ppm: 100,
            fee_ext: RaydiumFeeExt::default(),
            adaptive_fee: None,
            now: 0,
        };
        let td = flat(-21_600, -21_001, vec![(-21_380, 1_649_403_211), (-21_310, 114_466_501), (-21_300, 36_703_672_101), (-21_250, -857_056_650_693)]);
        let r = swap_exact_in_pool(&pool, &td, false, 3_008_800).unwrap();
        assert_eq!(r.amount_out, 25_401_298);
        assert_eq!(r.ticks_crossed, 0);
    }

    #[test]
    fn raydium_fee_on_token0_is_taken_from_the_output() {
        // Raydium CLMM 88v4aoey… (SOL/C1MHyo, 1 %, fee_on = 1: always token 0),
        // C1MHyo→SOL 50_449 in: fee comes off the SOL out (67 → 66); a
        // fee-on-input model gives 67, one atom more than the program pays.
        let fee_ext = RaydiumFeeExt {
            fee_on: 1,
            dynamic_fee: Some(RaydiumDynamicFee {
                filter_period: 30, decay_period: 900, reduction_factor: 5_000, dynamic_fee_control: 1_736, max_volatility_accumulator: 200_000,
                tick_spacing_index_reference: 551, volatility_reference: 0, volatility_accumulator: 0, last_update_timestamp: 1_790_127_199,
            }),
        };
        let pool = ClmmPool {
            layout: TickLayout::Raydium,
            sqrt_price_x64: 503_491_989_071_015_610_776,
            liquidity: 3_351_663_468_437_963,
            tick_current: 66_136,
            tick_spacing: 120,
            fee_ppm: 10_000,
            fee_ext,
            adaptive_fee: None,
            now: 1_790_127_277,
        };
        let td = flat(64_800, 72_000 - 1, vec![(65_880, 1_416_888_411_491_391), (66_000, 145_497_606_779_696), (66_120, 1_707_608_173_276_316), (66_240, -1_853_105_780_056_012), (66_360, -15_661_387_187_829)]);
        assert_eq!(swap_exact_in_pool(&pool, &td, false, 50_449).unwrap().amount_out, 66);
        let on_input = ClmmPool { fee_ext: RaydiumFeeExt { fee_on: 0, ..fee_ext }, ..pool };
        assert_eq!(swap_exact_in_pool(&on_input, &td, false, 50_449).unwrap().amount_out, 67);
    }

    #[test]
    fn raydium_dynamic_fee_reprices_every_tick_spacing() {
        // Raydium CLMM G4G5Szkb… (6GmAFS/USDC, spacing 10, base 0.18 %,
        // dynamic fee): 2_034_833_656 USDC in, simulated output
        // 6_350_362_320_262. The swap crosses the spacing boundary at tick
        // −80_480: 0.1834 % before it (accumulator 14_970), 0.1804 % after (4_970).
        let dynamic_fee = RaydiumDynamicFee {
            filter_period: 60, decay_period: 600, reduction_factor: 5_000, dynamic_fee_control: 15_000, max_volatility_accumulator: 600_000,
            tick_spacing_index_reference: -8_048, volatility_reference: 4_970, volatility_accumulator: 14_970, last_update_timestamp: 1_790_127_468,
        };
        assert_eq!(dynamic_fee.total_fee_rate(1_800, 10), 1_834);
        let pool = ClmmPool {
            layout: TickLayout::Raydium,
            sqrt_price_x64: 329_843_977_620_519_865,
            liquidity: 307_053_989_438_731,
            tick_current: -80_485,
            tick_spacing: 10,
            fee_ppm: 1_800,
            fee_ext: RaydiumFeeExt { fee_on: 0, dynamic_fee: Some(dynamic_fee) },
            adaptive_fee: None,
            now: 1_790_127_519,
        };
        let td = flat(-81_085, -79_885, vec![
            (-81_010, 52_744_677_984), (-80_850, -2_929_579_159), (-80_800, -231_004_166_856), (-80_620, 8_064_822_955_037),
            (-80_520, -8_955_689_508_843), (-80_430, -68_063_838_364), (-80_410, 113_870_591_173), (-80_380, 1_255_072_850_756),
            (-80_350, 8_551_482_158), (-80_300, -32_374_655_957_855), (-80_250, 81_462_450_253), (-80_230, 67_461_008_760),
            (-79_960, -3_226_179_168_934), (-79_950, -52_744_677_984), (-79_910, 325_917_529_210),
        ]);
        assert_eq!(swap_exact_in_pool(&pool, &td, false, 2_034_833_656).unwrap().amount_out, 6_350_362_320_262);
        // one step at the first fee over-quotes
        let static_fee = ClmmPool { fee_ext: RaydiumFeeExt::default(), fee_ppm: 1_834, ..pool };
        assert!(swap_exact_in_pool(&static_fee, &td, false, 2_034_833_656).unwrap().amount_out < 6_350_362_320_262);
    }

    #[test]
    fn raydium_limit_orders_fill_at_the_tick_price_before_crossing() {
        // A tick holding only limit orders stops the walk: the orders fill at the
        // tick's price (fee on the input), then the swap continues past it.
        let (l, p) = (1_000_000_000_000u128, Q64);
        let mut td = flat(-6_000, 6_000, vec![(60, 0)]);
        let base = ClmmPool { layout: TickLayout::Raydium, sqrt_price_x64: p, liquidity: l, tick_current: 0, tick_spacing: 60, fee_ppm: 3_000, fee_ext: RaydiumFeeExt::default(), adaptive_fee: None, now: 0 };
        let without = swap_exact_in_pool(&base, &td, false, 10_000_000_000).unwrap();
        td.limit_orders = vec![(60, 1_000_000_000)];
        let with = swap_exact_in_pool(&base, &td, false, 10_000_000_000).unwrap();
        assert!(with.amount_out > without.amount_out, "orders at tick 60 are cheaper than the curve beyond it");
        // the orders alone: 1e9 of token 0 bought at 1.0001^60
        let price = (u256(raydium_sqrt_price_at_tick(60)) * u256(raydium_sqrt_price_at_tick(60)) + u256(Q64 - 1)) >> 64;
        let (used, got, filled) = match_limit_orders(1_000_000_000, u64::MAX as u128, raydium_sqrt_price_at_tick(60), 3_000, false, true).unwrap();
        assert_eq!((got, filled), (1_000_000_000, 1_000_000_000));
        let paid = mul_div_ceil(1_000_000_000, price.low_u128(), Q64).unwrap();
        assert_eq!(used, paid + mul_div_ceil(paid, 3_000, 997_000).unwrap());
    }

    #[test]
    fn fusion_limit_orders_reproduce_a_simulated_swap() {
        // DefiTuna Fusion 3hL12JDX… (cbBTC/USDC, spacing 4, 0.04 %): nearly all
        // depth is limit orders on ticks. 50_000_000 USDC in → 57_633 sats
        // (simulated), filling the orders on 67_648 … 67_704 one after another.
        let pool = ClmmPool {
            layout: TickLayout::Fusion,
            sqrt_price_x64: 542_462_426_557_481_051_304,
            liquidity: 142_334_817,
            tick_current: 67_628,
            tick_spacing: 4,
            fee_ppm: 400,
            fee_ext: RaydiumFeeExt::default(),
            adaptive_fee: None,
            now: 0,
        };
        let mut td = flat(66_176, 69_343, vec![
            (67_624, 0), (67_628, 0), (67_648, 0), (67_656, 0), (67_664, 0), (67_668, 0), (67_672, 0), (67_696, 0), (67_704, 0), (67_724, 0), (68_880, -138_191_919),
        ]);
        td.limit_orders = vec![
            (67_624, 49_999_996), (67_628, 676_237), (67_648, 7_335), (67_656, 28_851), (67_664, 4_978), (67_668, 28_816),
            (67_672, 28_852), (67_696, 56_695), (67_704, 28_713), (67_724, 261),
        ];
        assert_eq!(swap_exact_in_pool(&pool, &td, false, 50_000_000).unwrap().amount_out, 57_633);
        assert_eq!(swap_exact_in_pool(&pool, &td, false, 5_404_214).unwrap().amount_out, 6_238);
        assert_eq!(swap_exact_in_pool(&pool, &td, false, 540_421).unwrap().amount_out, 624);
    }

    #[test]
    fn orca_adaptive_fee_reprices_every_tick_group() {
        // Orca adaptive-fee whirlpool BCvzjDbA… (spacing 4, static 0.04 %,
        // tick group 4): 2_291_341 in (A→B), simulated router output
        // 3_858_341. The volatility fee grows by one accumulator step per
        // tick group the price crosses.
        let fee = OrcaAdaptiveFee {
            trade_enable_timestamp: 0,
            filter_period: 30,
            decay_period: 600,
            reduction_factor: 5_000,
            adaptive_fee_control_factor: 60_000,
            max_volatility_accumulator: 880_000,
            tick_group_size: 4,
            last_reference_update_timestamp: 1_790_128_367,
            last_major_swap_timestamp: 1_790_128_564,
            volatility_reference: 29_808,
            tick_group_index_reference: 1_305,
            volatility_accumulator: 29_808,
        };
        let pool = ClmmPool {
            layout: TickLayout::Orca,
            sqrt_price_x64: 23_949_917_230_897_821_664,
            liquidity: 6_218_180_143,
            tick_current: 5_221,
            tick_spacing: 4,
            fee_ppm: 400,
            fee_ext: RaydiumFeeExt::default(),
            adaptive_fee: Some(fee),
            now: 1_790_128_566,
        };
        let td = flat(3_872, 6_335, vec![
            (3_984, 129_476_030), (4_168, -449_442_999), (4_260, -798_314_398), (4_308, 13_090_896), (4_764, 2_234_077_118),
            (5_208, -861_704_873), (5_484, 1_426_466_181), (5_548, -1_426_466_181), (5_860, 7_146_839_978), (5_932, 933_124_847),
        ]);
        let r = swap_exact_in_pool(&pool, &td, true, 2_291_341).unwrap();
        assert_eq!(r.amount_out, 3_858_341);
        // it ends several tick groups below the one it started in (5_220..5_224)
        assert!(orca_tick_at_sqrt_price(r.end_sqrt_price_x64) < 5_216);
        // the static fee alone over-quotes
        let static_fee = ClmmPool { adaptive_fee: None, ..pool };
        assert!(swap_exact_in_pool(&static_fee, &td, true, 2_291_341).unwrap().amount_out > 3_858_341);
        // trading not yet enabled: no quote
        let gated = ClmmPool { adaptive_fee: Some(OrcaAdaptiveFee { trade_enable_timestamp: 1_790_128_567, ..fee }), ..pool };
        assert!(swap_exact_in_pool(&gated, &td, true, 2_291_341).is_none());
    }

    #[test]
    fn orca_tick_index_from_sqrt_price_inverts_the_table() {
        for t in [-443_636, -60_000, -5_345, -1, 0, 1, 5_221, 66_240, 443_635] {
            let p = orca_sqrt_price_at_tick(t);
            assert_eq!(orca_tick_at_sqrt_price(p), t);
            assert_eq!(orca_tick_at_sqrt_price(p + 1), t);
            assert_eq!(orca_tick_at_sqrt_price(p - 1), t - 1);
        }
    }

    #[test]
    fn parses_the_orca_oracle() {
        let pool = Pubkey::new_unique();
        let mut d = vec![0u8; 254];
        d[..8].copy_from_slice(&ORCA_ORACLE_DISCRIMINATOR);
        d[8..40].copy_from_slice(pool.as_ref());
        d[48..50].copy_from_slice(&30u16.to_le_bytes());
        d[54..58].copy_from_slice(&60_000u32.to_le_bytes());
        d[62..64].copy_from_slice(&4u16.to_le_bytes());
        d[102..106].copy_from_slice(&(-7i32).to_le_bytes());
        d[106..110].copy_from_slice(&29_808u32.to_le_bytes());
        let f = OrcaAdaptiveFee::parse(&d, &pool).unwrap();
        assert_eq!((f.filter_period, f.adaptive_fee_control_factor, f.tick_group_size, f.tick_group_index_reference, f.volatility_accumulator), (30, 60_000, 4, -7, 29_808));
        assert_eq!(f.total_fee_rate(400), 486);
        assert!(OrcaAdaptiveFee::parse(&d, &Pubkey::new_unique()).is_none(), "another pool's oracle");
        let mut sha = d.clone();
        sha[0] ^= 1;
        assert!(OrcaAdaptiveFee::parse(&sha, &pool).is_none(), "discriminator");
    }

    #[test]
    fn parses_borsh_tick_arrays() {
        // Fusion: disc(8) start(4) pool(32) then 88 × {0 | 1 + TickData(112)}
        let mut f = vec![0u8; 44];
        f[8..12].copy_from_slice(&67_584i32.to_le_bytes());
        for i in 0..88 {
            if i == 16 {
                let mut t = vec![1u8];
                let mut d = [0u8; 112];
                d[0..16].copy_from_slice(&(-7i128).to_le_bytes());
                d[72..80].copy_from_slice(&7_335u64.to_le_bytes());
                d[88..96].copy_from_slice(&5u64.to_le_bytes());
                t.extend_from_slice(&d);
                f.extend(t);
            } else {
                f.push(0);
            }
        }
        assert_eq!(TickLayout::Fusion.parse_array(&f, 4), Some((67_584, vec![(67_648, -7)], vec![(67_648, 7_340)])));
        // Orca dynamic array: same, with a 16-byte tick bitmap before the ticks
        let mut o = f[..44].to_vec();
        o.extend_from_slice(&[0u8; 16]);
        o.extend_from_slice(&f[44..]);
        assert_eq!(TickLayout::Orca.parse_array(&o, 4), Some((67_584, vec![(67_648, -7)], vec![])));
        assert_eq!(TickLayout::Orca.parse_array(&o[..100], 4), None, "truncated");
    }

    #[test]
    fn tick_price_is_monotone_and_anchored() {
        assert_eq!(sqrt_price_x64_at_tick(0), Q64);
        assert!(sqrt_price_x64_at_tick(1) > Q64 && sqrt_price_x64_at_tick(-1) < Q64);
        // 1.0001^(tick/2): tick 2 → ×1.0001
        let p2 = sqrt_price_x64_at_tick(2) as f64 / Q64 as f64;
        assert!((p2 - 1.0001).abs() < 1e-12, "{p2}");
    }

    fn flat(lo: i32, hi: i32, ticks: Vec<(i32, i128)>) -> TickData {
        TickData { ticks, covered_lo: lo, covered_hi: hi, initialized_arrays: vec![], bitmap_extension: None, limit_orders: vec![], adaptive_fee: None, fetched_at: Instant::now() }
    }

    #[test]
    fn inside_one_range_equals_the_closed_form() {
        // No initialised ticks nearby: the walk must equal x·y=k on virtual reserves.
        let (l, p) = (1_000_000_000_000u128, Q64); // price 1
        let td = flat(-10_000, 10_000, vec![]);
        let r = swap_exact_in(p, l, 0, 3_000, &td, true, 1_000_000).unwrap();
        // virtual reserves x = y = L; in_eff = 997_000; out = y·in/(x+in)
        let in_eff = 997_000u128;
        let expect = l * in_eff / (l + in_eff);
        assert!((r.amount_out as i128 - expect as i128).abs() <= 1, "{} vs {expect}", r.amount_out);
        assert_eq!(r.ticks_crossed, 0);
        assert!(r.end_sqrt_price_x64 < p);
        // symmetric the other way
        let r2 = swap_exact_in(p, l, 0, 3_000, &td, false, 1_000_000).unwrap();
        assert!((r2.amount_out as i128 - expect as i128).abs() <= 1);
        assert!(r2.end_sqrt_price_x64 > p);
    }

    #[test]
    fn crossing_into_thinner_liquidity_pays_less_than_the_single_range_guess() {
        let (l, p) = (1_000_000_000_000u128, Q64);
        // Below tick -10 only 10% of the liquidity remains (net +0.9L when crossed upward).
        let thin = flat(-20_000, 20_000, vec![(-10, 900_000_000_000)]);
        let none = flat(-20_000, 20_000, vec![]);
        let amt = 5_000_000_000u64; // big enough to push through tick -10
        let exact = swap_exact_in(p, l, 0, 3_000, &thin, true, amt).unwrap();
        let naive = swap_exact_in(p, l, 0, 3_000, &none, true, amt).unwrap();
        assert_eq!(exact.ticks_crossed, 1);
        assert!(exact.amount_out < naive.amount_out, "{} !< {}", exact.amount_out, naive.amount_out);
        // and a small swap that never reaches the tick is unaffected
        let small = swap_exact_in(p, l, 0, 3_000, &thin, true, 1_000).unwrap();
        assert_eq!(small, swap_exact_in(p, l, 0, 3_000, &none, true, 1_000).unwrap());
    }

    #[test]
    fn refuses_to_quote_past_known_liquidity() {
        let (l, p) = (1_000_000u128, Q64);
        let td = flat(-100, 100, vec![]);
        assert!(swap_exact_in(p, l, 0, 3_000, &td, true, u64::MAX / 2).is_none(), "ran past the covered range");
        // liquidity drops to zero at a tick and nothing is left beyond it
        let dry = flat(-100, 100, vec![(-10, 1_000_000)]);
        assert!(swap_exact_in(p, l, 0, 3_000, &dry, true, 1_000_000_000).is_none());
    }

    #[test]
    fn swap_arrays_start_at_the_first_initialised_array_in_direction() {
        let mut td = flat(-10_000, 10_000, vec![]);
        td.initialized_arrays = vec![-1200, -600, 600, 1800];
        // current tick 30 lives in array [0, 600) which is NOT initialised
        assert_eq!(td.arrays_for_swap(TickLayout::Raydium, 30, 10, true, 3), vec![-600, -1200]);
        assert_eq!(td.arrays_for_swap(TickLayout::Raydium, 30, 10, false, 3), vec![600, 1800]);
        // current array initialised: it comes first either way
        assert_eq!(td.arrays_for_swap(TickLayout::Raydium, 650, 10, true, 2), vec![600, -600]);
        assert_eq!(td.arrays_for_swap(TickLayout::Raydium, 650, 10, false, 5), vec![600, 1800]);
    }

    #[test]
    fn parses_both_tick_array_layouts() {
        // Raydium: tick 120 initialised with net -5
        let mut r = vec![0u8; 10_240];
        r[40..44].copy_from_slice(&60i32.to_le_bytes());
        let o = 44 + 1 * 168;
        r[o..o + 4].copy_from_slice(&120i32.to_le_bytes());
        r[o + 4..o + 20].copy_from_slice(&(-5i128).to_le_bytes());
        r[o + 20..o + 36].copy_from_slice(&7u128.to_le_bytes());
        assert_eq!(TickLayout::Raydium.parse_array(&r, 60), Some((60, vec![(120, -5)], vec![])));
        // Orca: slot 2 initialised, spacing 8, start 704
        let mut w = vec![0u8; 9_988];
        w[8..12].copy_from_slice(&704i32.to_le_bytes());
        let o = 12 + 2 * 113;
        w[o] = 1;
        w[o + 1..o + 17].copy_from_slice(&9i128.to_le_bytes());
        assert_eq!(TickLayout::Orca.parse_array(&w, 8), Some((704, vec![(720, 9)], vec![])));
        assert_eq!(TickLayout::Orca.array_start(-1, 8), -704);
        assert_eq!(TickLayout::Raydium.array_start(61, 1), 60);
    }

    #[test]
    fn parses_byreal_sparse_tick_arrays() {
        // Mainnet Byreal array 2j4WinR9… (pool 27x6aSxc…, spacing 10, start 20400):
        // (index in array, slot, tick, liquidity_net, liquidity_gross) of its first entries.
        let live: [(usize, u8, i32, i128, u128); 6] = [
            (0, 10, 20400, 234_304_535_831, 234_304_535_831),
            (1, 42, 20410, 106_423_065_093, 109_363_733_791),
            (2, 56, 20420, 414_249_467, 414_249_467),
            (3, 29, 20430, -2_498_258_506, 2_498_258_506),
            (4, 46, 20440, -14_956_590_231, 14_956_590_231),
            (6, 52, 20460, -497_007, 497_007),
        ];
        let mut d = vec![0u8; 10_296];
        d[..8].copy_from_slice(&DYNAMIC_TICK_ARRAY_DISC);
        d[40..44].copy_from_slice(&20400i32.to_le_bytes());
        for (i, slot, tick, net, gross) in live {
            d[48 + i] = slot;
            let o = 216 + (slot as usize - 1) * 168;
            d[o..o + 4].copy_from_slice(&tick.to_le_bytes());
            d[o + 4..o + 20].copy_from_slice(&net.to_le_bytes());
            d[o + 20..o + 36].copy_from_slice(&gross.to_le_bytes());
        }
        // an allocated slot whose tick is no longer initialised (gross 0) is skipped
        d[48 + 5] = 7;
        let o = 216 + 6 * 168;
        d[o..o + 4].copy_from_slice(&20450i32.to_le_bytes());
        let (start, ticks, orders) = TickLayout::Raydium.parse_array(&d, 10).unwrap();
        assert!(orders.is_empty());
        assert_eq!(start, 20400);
        assert_eq!(ticks, live.iter().map(|(_, _, t, n, _)| (*t, *n)).collect::<Vec<_>>());
        // a slot holding a tick that does not belong at its index is corrupt
        d[48 + 5] = 10;
        assert!(TickLayout::Raydium.parse_array(&d, 10).is_none());
    }
}
