//! Exact concentrated-liquidity quoting by walking real tick arrays.
//!
//! The single-range approximation (`math::compute_clmm_output_multi_tick` with
//! no tick data) prices every swap as if the current liquidity extended
//! forever. It is exact while a swap stays inside the current tick range and
//! over-quotes as soon as it crosses into thinner liquidity (the program then
//! fails the swap with `TooLittleOutputReceived`). This module runs
//! the loop the pool runs: step to the next initialised tick, apply its
//! `liquidity_net`, repeat, with the fee taken from the input of every step.
//!
//! Hot-path rules: no allocation per quote, no RPC. Tick data lives in
//! [`TICKS`], filled off the quote path (pool refresh, block-driven refresher,
//! Geyser account stream). Arithmetic is Q64.64 with 256-bit intermediates
//! (`uint::U256`), with a `u128` fast path when the product fits.
//!
//! Precision: tick → sqrt-price uses `1.0001^(tick/2)` in `f64` (≈1e-16
//! relative) instead of the programs' integer tables; it only positions tick
//! boundaries and moves a quote by well under one atom per crossing.

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

#[inline]
fn u256(a: u128) -> U256 {
    U256::from(a)
}

#[inline]
fn to_u128(v: U256) -> Option<u128> {
    if v.bits() > 128 { None } else { Some(v.low_u128()) }
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

// ── price / amount math (Q64.64 sqrt prices) ──────────────────────────────

/// sqrt(1.0001^tick) · 2^64.
#[inline]
pub fn sqrt_price_x64_at_tick(tick: i32) -> u128 {
    (1.0001f64.powf(tick as f64 / 2.0) * 18_446_744_073_709_551_616.0) as u128
}

/// Token-0 (a) amount between two sqrt prices: L·2^64·(B−A)/(A·B).
#[inline]
fn amount0_delta(mut a: u128, mut b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if a == 0 {
        return None;
    }
    if round_up {
        let t = mul_div_ceil(liquidity, b - a, b)?;
        mul_div_ceil(t, Q64, a)
    } else {
        let t = mul_div_floor(liquidity, b - a, b)?;
        mul_div_floor(t, Q64, a)
    }
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
        let denom = num + u256(amount) * u256(sqrt_p);
        let (q, r) = (num * u256(sqrt_p)).div_mod(denom);
        let q = to_u128(q)?;
        Some(if r.is_zero() { q } else { q.checked_add(1)? })
    } else {
        // P + amount·2^64 / L — the price rises.
        sqrt_p.checked_add(mul_div_floor(amount, Q64, liquidity)?)
    }
}

/// One swap step toward `target` (the next initialised tick's sqrt price).
/// Returns (next_sqrt_price, amount_in_gross_of_fee, amount_out).
#[inline]
fn swap_step(sqrt_p: u128, target: u128, liquidity: u128, remaining: u128, fee_ppm: u128, a_to_b: bool) -> Option<(u128, u128, u128)> {
    let less_fee = mul_div_floor(remaining, FEE_DENOMINATOR_PPM - fee_ppm, FEE_DENOMINATOR_PPM)?;
    let in_to_target = if a_to_b { amount0_delta(target, sqrt_p, liquidity, true)? } else { amount1_delta(sqrt_p, target, liquidity, true)? };
    let (next, reached) = if less_fee >= in_to_target {
        (target, true)
    } else {
        (next_sqrt_price_from_input(sqrt_p, liquidity, less_fee, a_to_b)?, false)
    };
    let amount_out = if a_to_b { amount1_delta(next, sqrt_p, liquidity, false)? } else { amount0_delta(sqrt_p, next, liquidity, false)? };
    let amount_in_gross = if reached {
        // fee on top of what the range absorbed
        in_to_target + mul_div_ceil(in_to_target, fee_ppm, FEE_DENOMINATOR_PPM - fee_ppm)?
    } else {
        remaining
    };
    Some((next, amount_in_gross.min(remaining), amount_out))
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
    pub fetched_at: Instant,
}

impl TickData {
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

/// pool → ticks. Filled off the quote path.
pub static TICKS: std::sync::LazyLock<DashMap<Pubkey, Arc<TickData>>> = std::sync::LazyLock::new(DashMap::new);

/// Which on-chain tick-array layout a pool uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickLayout {
    /// Raydium CLMM and forks (PancakeSwap): 60 ticks/array, seed = start index as i32 BE bytes.
    Raydium,
    /// Orca Whirlpool: 88 ticks/array, seed = start index as decimal string.
    Orca,
}

impl TickLayout {
    pub const fn ticks_per_array(self) -> i32 {
        match self {
            TickLayout::Raydium => 60,
            TickLayout::Orca => 88,
        }
    }

    /// Start index of the array holding `tick`.
    pub fn array_start(self, tick: i32, tick_spacing: i32) -> i32 {
        let span = self.ticks_per_array() * tick_spacing.max(1);
        tick.div_euclid(span) * span
    }

    pub fn array_pda(self, program: &Pubkey, pool: &Pubkey, start: i32) -> Pubkey {
        match self {
            TickLayout::Raydium => Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), &start.to_be_bytes()], program).0,
            TickLayout::Orca => Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), start.to_string().as_bytes()], program).0,
        }
    }

    /// Initialised ticks of one array account: `(start_index, [(tick, liquidity_net)])`.
    pub fn parse_array(self, data: &[u8], tick_spacing: i32) -> Option<(i32, Vec<(i32, i128)>)> {
        let rd_i32 = |o: usize| data.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()));
        let rd_i128 = |o: usize| data.get(o..o + 16).map(|b| i128::from_le_bytes(b.try_into().unwrap()));
        let rd_u128 = |o: usize| data.get(o..o + 16).map(|b| u128::from_le_bytes(b.try_into().unwrap()));
        let mut out = Vec::new();
        match self {
            TickLayout::Raydium => {
                // disc(8) pool(32) start(i32) then 60 × TickState(168): tick i32, liquidity_net i128, liquidity_gross u128, …
                let start = rd_i32(40)?;
                for i in 0..60usize {
                    let o = 44 + i * 168;
                    if rd_u128(o + 20)? != 0 {
                        out.push((rd_i32(o)?, rd_i128(o + 4)?));
                    }
                }
                Some((start, out))
            }
            TickLayout::Orca => {
                // disc(8) start(i32) then 88 × Tick(113): initialized u8, liquidity_net i128, …
                let start = rd_i32(8)?;
                for i in 0..88usize {
                    let o = 12 + i * 113;
                    if *data.get(o)? != 0 {
                        out.push((start + i as i32 * tick_spacing, rd_i128(o + 1)?));
                    }
                }
                Some((start, out))
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

/// Exact-input swap across initialised ticks. `None` when the swap runs past
/// the covered tick range or liquidity runs out — never an optimistic number.
pub fn swap_exact_in(
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    fee_ppm: u32,
    ticks: &TickData,
    a_to_b: bool,
    amount_in: u64,
) -> Option<WalkResult> {
    if amount_in == 0 || sqrt_price_x64 == 0 || fee_ppm as u128 >= FEE_DENOMINATOR_PPM {
        return None;
    }
    let fee = fee_ppm as u128;
    let mut remaining = amount_in as u128;
    let mut sqrt_p = sqrt_price_x64;
    let mut liq = liquidity;
    let mut out: u128 = 0;
    let mut crossed = 0u32;

    // Index of the next tick to cross in the swap direction.
    let mut idx: isize = if a_to_b {
        ticks.ticks.partition_point(|(t, _)| *t <= tick_current) as isize - 1
    } else {
        ticks.ticks.partition_point(|(t, _)| *t <= tick_current) as isize
    };

    for _ in 0..512 {
        if remaining == 0 {
            break;
        }
        let next_tick = if idx >= 0 && (idx as usize) < ticks.ticks.len() { Some(ticks.ticks[idx as usize]) } else { None };
        // Target: the next initialised tick, or the edge of what we know.
        let (target_tick, net) = match next_tick {
            Some((t, n)) => (t, Some(n)),
            None => (if a_to_b { ticks.covered_lo } else { ticks.covered_hi }, None),
        };
        let mut target = sqrt_price_x64_at_tick(target_tick);
        // Never step backwards (the current price may sit a hair past a tick boundary).
        if (a_to_b && target > sqrt_p) || (!a_to_b && target < sqrt_p) {
            target = sqrt_p;
        }
        if liq == 0 {
            // No liquidity in this range: the price jumps to the next tick for free.
            sqrt_p = target;
        } else {
            let (next, used, got) = swap_step(sqrt_p, target, liq, remaining, fee, a_to_b)?;
            remaining = remaining.checked_sub(used)?;
            out = out.checked_add(got)?;
            sqrt_p = next;
        }
        if sqrt_p != target {
            break; // input exhausted inside the range
        }
        match net {
            Some(n) => {
                // Crossing: moving down removes the tick's net liquidity, moving up adds it.
                let delta = if a_to_b { -n } else { n };
                liq = if delta >= 0 { liq.checked_add(delta as u128)? } else { liq.checked_sub(delta.unsigned_abs())? };
                crossed += 1;
                idx += if a_to_b { -1 } else { 1 };
            }
            None => return None, // reached the edge of the covered range with input left
        }
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
    fn tick_price_is_monotone_and_anchored() {
        assert_eq!(sqrt_price_x64_at_tick(0), Q64);
        assert!(sqrt_price_x64_at_tick(1) > Q64 && sqrt_price_x64_at_tick(-1) < Q64);
        // 1.0001^(tick/2): tick 2 → ×1.0001
        let p2 = sqrt_price_x64_at_tick(2) as f64 / Q64 as f64;
        assert!((p2 - 1.0001).abs() < 1e-12, "{p2}");
    }

    fn flat(lo: i32, hi: i32, ticks: Vec<(i32, i128)>) -> TickData {
        TickData { ticks, covered_lo: lo, covered_hi: hi, initialized_arrays: vec![], bitmap_extension: None, fetched_at: Instant::now() }
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
        assert_eq!(TickLayout::Raydium.parse_array(&r, 60), Some((60, vec![(120, -5)])));
        // Orca: slot 2 initialised, spacing 8, start 704
        let mut w = vec![0u8; 9_988];
        w[8..12].copy_from_slice(&704i32.to_le_bytes());
        let o = 12 + 2 * 113;
        w[o] = 1;
        w[o + 1..o + 17].copy_from_slice(&9i128.to_le_bytes());
        assert_eq!(TickLayout::Orca.parse_array(&w, 8), Some((704, vec![(720, 9)])));
        assert_eq!(TickLayout::Orca.array_start(-1, 8), -704);
        assert_eq!(TickLayout::Raydium.array_start(61, 1), 60);
    }
}
