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

/// Raydium CLMM's `tick_math::get_sqrt_price_at_tick` (shared by its forks
/// PancakeSwap and Byreal): the integer Q64.64 value the program steps to at a
/// tick boundary, bit for bit. `None` outside ±443636.
pub fn raydium_sqrt_price_at_tick(tick: i32) -> Option<u128> {
    const FACTORS: [u128; 18] = [
        0xfff97272373d4000, 0xfff2e50f5f657000, 0xffe5caca7e10f000, 0xffcb9843d60f7000,
        0xff973b41fa98e800, 0xff2ea16466c9b000, 0xfe5dee046a9a3800, 0xfcbe86c7900bb000,
        0xf987a7253ac65800, 0xf3392b0822bb6000, 0xe7159475a2caf000, 0xd097f3bdfd2f2000,
        0xa9f746462d9f8000, 0x70d869a156f31c00, 0x31be135f97ed3200, 0x9aa508b5b85a500,
        0x5d6af8dedc582c, 0x2216e584f5fa,
    ];
    let abs = tick.unsigned_abs();
    if abs > 443_636 {
        return None;
    }
    let mut ratio: u128 = if abs & 1 != 0 { 0xfffcb933bd6fb800 } else { 1 << 64 };
    for (i, f) in FACTORS.iter().enumerate() {
        if abs & (2 << i) != 0 {
            ratio = (ratio * f) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Some(ratio)
}

/// Boundary sqrt price of `tick` as `layout`'s program computes it.
#[inline]
fn boundary_sqrt_price(layout: TickLayout, tick: i32) -> u128 {
    match layout {
        TickLayout::Raydium => raydium_sqrt_price_at_tick(tick).unwrap_or_else(|| sqrt_price_x64_at_tick(tick)),
        TickLayout::Orca => sqrt_price_x64_at_tick(tick),
    }
}

/// Token-0 (a) amount between two sqrt prices: L·2^64·(B−A)/(A·B), rounded
/// once. Raydium divides by B then by A (⌊⌊x/B⌋/A⌋ = ⌊x/(A·B)⌋, likewise
/// for ceilings), Orca by A·B: the same integer either way. Rounding the
/// intermediate L·(B−A)/B before the ·2^64 under-counts by up to 2^64/A atoms.
#[inline]
fn amount0_delta(mut a: u128, mut b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if a == 0 {
        return None;
    }
    let prod = u256(liquidity) * u256(b - a);
    if prod.bits() <= 192 {
        // 256-bit fast path: L·(B−A)·2^64 and A·B both fit
        let (q, r) = (prod << 64).div_mod(u256(a) * u256(b));
        let q = if round_up && !r.is_zero() { q + U256::one() } else { q };
        return to_u128(q);
    }
    let num = (U512::from(liquidity) * U512::from(b - a)) << 64;
    let den = U512::from(a) * U512::from(b);
    let (q, r) = num.div_mod(den);
    let q = if round_up && !r.is_zero() { q + U512::one() } else { q };
    if q.bits() > 128 { None } else { Some(q.low_u128()) }
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
    /// Whose tick math prices the boundaries (Raydium-layout pools: exact).
    pub layout: TickLayout,
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

/// Discriminator of Byreal's sparse ("dynamic") tick array account, which a
/// pool may hold next to classic 10240-byte `TickArrayState` accounts.
pub const DYNAMIC_TICK_ARRAY_DISC: [u8; 8] = [0x6a, 0x8b, 0x98, 0x24, 0x75, 0x99, 0xb8, 0x38];

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
            TickLayout::Raydium if data.get(..8) == Some(&DYNAMIC_TICK_ARRAY_DISC[..]) => {
                // Byreal's sparse array (10296 B): disc(8) pool(32) start(i32),
                // 4 bytes, then a [u8; 60] slot map @48 (tick i → 1-based slot
                // of its TickState, 0 = not allocated), header up to 216, then
                // the allocated TickState(168)s.
                let start = rd_i32(40)?;
                for i in 0..60usize {
                    let slot = *data.get(48 + i)? as usize;
                    if slot == 0 {
                        continue;
                    }
                    let o = 216 + (slot - 1) * 168;
                    if rd_u128(o + 20)? != 0 {
                        let tick = rd_i32(o)?;
                        if tick != start + i as i32 * tick_spacing {
                            return None;
                        }
                        out.push((tick, rd_i128(o + 4)?));
                    }
                }
                out.sort_unstable_by_key(|(t, _)| *t);
                Some((start, out))
            }
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
        let mut target = boundary_sqrt_price(ticks.layout, target_tick);
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
    fn token0_delta_rounds_once_like_the_programs() {
        // mainnet-like 4SoQ8/USDC Byreal range (token 0 far below 1: 2^64/A ≈ 38)
        let (a, b, l) = (480_000_000_000_000_000u128, 480_100_000_000_000_000u128, 1_000_000_000_000_000u128);
        let exact_floor = ((l * (b - a)) as f64 * 18_446_744_073_709_551_616.0 / (a as f64 * b as f64)).floor();
        let got = amount0_delta(a, b, l, false).unwrap();
        assert!((got as f64 - exact_floor).abs() <= 1.0, "{got} vs {exact_floor}");
        // the former two-step rounding lost up to 2^64/A atoms
        let two_step = mul_div_floor(mul_div_floor(l, b - a, b).unwrap(), Q64, a).unwrap();
        assert!(got > two_step && got - two_step <= Q64 / a + 1);
        assert_eq!(amount0_delta(a, b, l, true).unwrap(), got + 1);
        assert_eq!(amount0_delta(b, a, l, false), Some(got), "order-insensitive");
        // the 512-bit path (L·(B−A) ≥ 2^192) agrees with the 256-bit one
        let (a2, b2, l2) = (1u128 << 64, (1u128 << 64) + (1u128 << 70), u128::MAX >> 1);
        let wide = amount0_delta(a2, b2, l2, false);
        let big = (U512::from(l2) * U512::from(b2 - a2) << 64) / (U512::from(a2) * U512::from(b2));
        assert_eq!(wide, if big.bits() > 128 { None } else { Some(big.low_u128()) });
    }

    #[test]
    fn raydium_tick_math_matches_the_program() {
        // reference values of Raydium's get_sqrt_price_at_tick (MIN/MAX_SQRT_PRICE_X64 and tick 0)
        assert_eq!(raydium_sqrt_price_at_tick(0), Some(1u128 << 64));
        assert_eq!(raydium_sqrt_price_at_tick(-443_636), Some(4_295_048_016));
        assert_eq!(raydium_sqrt_price_at_tick(443_636), Some(79_226_673_521_066_979_257_578_248_091));
        assert_eq!(raydium_sqrt_price_at_tick(443_637), None);
        // its constants are ~50-bit: it differs from the float formula by ~1e-13 relative
        for (t, tol) in [(-300_000, 1e-9), (-21_288, 1e-12), (-1, 1e-14), (1, 1e-14), (20_400, 1e-12), (300_000, 1e-9)] {
            let (x, f) = (raydium_sqrt_price_at_tick(t).unwrap() as f64, sqrt_price_x64_at_tick(t) as f64);
            assert!(((x - f) / f).abs() < tol, "tick {t}: {}", (x - f) / f);
            assert!(raydium_sqrt_price_at_tick(t + 1).unwrap() > raydium_sqrt_price_at_tick(t).unwrap());
        }
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
        TickData { ticks, covered_lo: lo, covered_hi: hi, initialized_arrays: vec![], bitmap_extension: None, fetched_at: Instant::now(), layout: TickLayout::Orca }
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
        let (start, ticks) = TickLayout::Raydium.parse_array(&d, 10).unwrap();
        assert_eq!(start, 20400);
        assert_eq!(ticks, live.iter().map(|(_, _, t, n, _)| (*t, *n)).collect::<Vec<_>>());
        // a slot holding a tick that does not belong at its index is corrupt
        d[48 + 5] = 10;
        assert!(TickLayout::Raydium.parse_array(&d, 10).is_none());
    }
}
