//! Meteora Dynamic Bonding Curve (`dbcij3…`) exact-input math.
//!
//! A DBC pool prices on `sqrt_price` (Q64.64) along its config's curve: up to
//! 20 points `{sqrt_price, liquidity}`, point `i` closing the segment that
//! starts at the previous point (or `sqrt_start_price`) with liquidity
//! `curve[i].liquidity` (stored ×2^64, as in DAMM v2). A swap walks the
//! segments with the DAMM v2 primitives (`quote::damm_v2`), rounding the way
//! the program does: the amount that exhausts a segment rounds up, the output
//! of each piece rounds down.
//!
//! Fees (`/1e9`, rounded up, capped at 99 %): the base fee — a flat cliff, a
//! linear or exponential scheduler decaying from activation, or a rate limiter
//! — plus the dynamic (volatility) fee. `collect_fee_mode` 0 takes the fee in
//! the quote token (off the input on buys, off the output on sells); mode 1
//! takes it off the output either way.
//!
//! Not quoted: migrated or completed curves, pools before activation, buys
//! that would reach the migration threshold (they complete the curve), sells
//! below `sqrt_start_price`, rate-limited buys above the limiter's reference
//! amount, and base-fee modes this module does not know.

use super::clmm::U256;
use super::damm_v2::{delta_a, delta_b, next_sqrt_price_from_a, next_sqrt_price_from_b};

pub const FEE_DENOMINATOR: u64 = 1_000_000_000;
/// DBC accepts fees up to 99 % (launch-sniper schedules start at 50–99 %).
pub const MAX_FEE_NUMERATOR: u64 = 990_000_000;
const BASIS_POINT_MAX: u128 = 10_000;
const CURVE_POINTS: usize = 20;
/// `PoolConfig` account size.
pub const CONFIG_LEN: usize = 1048;
/// `VirtualPool` account size.
pub const POOL_LEN: usize = 424;

/// The immutable `PoolConfig` fields a quote needs.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DbcConfig {
    pub collect_fee_mode: u8,
    /// 0 = slot, 1 = unix timestamp
    pub activation_type: u8,
    pub migration_quote_threshold: u64,
    pub sqrt_start_price: u128,
    /// `(sqrt_price, liquidity)` up to the first empty point.
    pub curve: Vec<(u128, u128)>,
    pub cliff_fee_numerator: u64,
    /// scheduler: number_of_period | rate limiter: fee_increment_bps
    pub first_factor: u16,
    /// scheduler: period_frequency | rate limiter: max_limiter_duration
    pub second_factor: u64,
    /// scheduler: reduction_factor | rate limiter: reference_amount
    pub third_factor: u64,
    /// 0 linear scheduler, 1 exponential scheduler, 2 rate limiter
    pub base_fee_mode: u8,
    pub dynamic_fee: bool,
    pub variable_fee_control: u32,
    pub bin_step: u16,
}

/// Pool state (from `VirtualPool`) + its config.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DbcCurve {
    pub sqrt_price: u128,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub activation_point: u64,
    pub is_migrated: bool,
    pub volatility_accumulator: u128,
    /// Empty until the config account has been read.
    pub config: DbcConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcQuote {
    pub amount_out: u64,
    pub fee: u64,
    pub next_sqrt_price: u128,
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

impl DbcConfig {
    /// `PoolConfig` layout (verified on mainnet, 1048 bytes): base fee @104
    /// (cliff u64, second u64, third u64, first u16 @128, mode u8 @130),
    /// dynamic fee @136 (initialized u8, variable_fee_control u32 @148,
    /// bin_step u16 @152), collect_fee_mode @232, activation_type @234,
    /// migration_quote_threshold u64 @264, sqrt_start_price u128 @392,
    /// curve 20 × (u128, u128) @408.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < CONFIG_LEN {
            return None;
        }
        let curve = (0..CURVE_POINTS)
            .map(|i| (u128_at(d, 408 + 32 * i), u128_at(d, 424 + 32 * i)))
            .take_while(|(p, l)| *p != 0 && *l != 0)
            .collect();
        Some(Self {
            collect_fee_mode: d[232],
            activation_type: d[234],
            migration_quote_threshold: u64_at(d, 264),
            sqrt_start_price: u128_at(d, 392),
            curve,
            cliff_fee_numerator: u64_at(d, 104),
            first_factor: u16::from_le_bytes(d[128..130].try_into().unwrap()),
            second_factor: u64_at(d, 112),
            third_factor: u64_at(d, 120),
            base_fee_mode: d[130],
            dynamic_fee: d[136] != 0,
            variable_fee_control: u32::from_le_bytes(d[148..152].try_into().unwrap()),
            bin_step: u16::from_le_bytes(d[152..154].try_into().unwrap()),
        })
    }
}

impl DbcCurve {
    /// Refresh the pool-side fields from a `VirtualPool` account (verified on
    /// mainnet, 424 bytes): volatility_accumulator u128 @40, base_reserve u64
    /// @232, quote_reserve @240, sqrt_price u128 @280, activation_point u64
    /// @296, is_migrated u8 @305. The config part is kept.
    pub fn apply_pool(&mut self, d: &[u8]) -> bool {
        if d.len() < POOL_LEN {
            return false;
        }
        self.volatility_accumulator = u128_at(d, 40);
        self.base_reserve = u64_at(d, 232);
        self.quote_reserve = u64_at(d, 240);
        self.sqrt_price = u128_at(d, 280);
        self.activation_point = u64_at(d, 296);
        self.is_migrated = d[305] != 0;
        true
    }

    pub fn is_complete(&self) -> bool {
        self.is_migrated || self.quote_reserve >= self.config.migration_quote_threshold
    }

    fn quotable(&self, current_point: u64) -> bool {
        !self.config.curve.is_empty() && self.sqrt_price != 0 && !self.is_complete() && current_point >= self.activation_point
    }

    /// Base fee numerator. `None` for an amount-dependent (rate-limited) fee
    /// above the limiter's reference amount, or an unknown mode.
    fn base_fee_numerator(&self, current_point: u64, quote_to_base: bool, amount_in: u64) -> Option<u64> {
        let c = &self.config;
        let elapsed = current_point.saturating_sub(self.activation_point);
        match c.base_fee_mode {
            0 | 1 => {
                if c.second_factor == 0 {
                    return Some(c.cliff_fee_numerator);
                }
                let period = (elapsed / c.second_factor).min(c.first_factor as u64);
                if c.base_fee_mode == 0 {
                    c.cliff_fee_numerator.checked_sub(period.checked_mul(c.third_factor)?)
                } else {
                    exponential_fee(c.cliff_fee_numerator, c.third_factor, period)
                }
            }
            2 => {
                // the limiter only prices quote→base buys inside its window
                let applied = quote_to_base && c.third_factor > 0 && c.first_factor > 0 && elapsed <= c.second_factor;
                (!applied || amount_in <= c.third_factor).then_some(c.cliff_fee_numerator)
            }
            _ => None,
        }
    }

    fn variable_fee_numerator(&self) -> u64 {
        if !self.config.dynamic_fee {
            return 0;
        }
        let vb = U256::from(self.volatility_accumulator) * U256::from(self.config.bin_step);
        let v = vb * vb * U256::from(self.config.variable_fee_control);
        let scaled = (v + U256::from(99_999_999_999u64)) / U256::from(100_000_000_000u64);
        if scaled.bits() > 64 { u64::MAX } else { scaled.low_u64() }
    }

    pub fn fee_numerator(&self, current_point: u64, quote_to_base: bool, amount_in: u64) -> Option<u64> {
        let base = self.base_fee_numerator(current_point, quote_to_base, amount_in)?;
        Some(base.saturating_add(self.variable_fee_numerator()).min(MAX_FEE_NUMERATOR))
    }

    /// Exact-input swap at `current_point` (slot or unix time per
    /// `config.activation_type`).
    pub fn swap_exact_in(&self, quote_to_base: bool, amount_in: u64, current_point: u64) -> Option<DbcQuote> {
        if amount_in == 0 || !self.quotable(current_point) {
            return None;
        }
        let fee_num = self.fee_numerator(current_point, quote_to_base, amount_in)?;
        let fee_of = |a: u64| u64::try_from((a as u128 * fee_num as u128).div_ceil(FEE_DENOMINATOR as u128)).ok();
        let fee_on_input = quote_to_base && self.config.collect_fee_mode == 0;
        let (actual_in, input_fee) = if fee_on_input {
            let f = fee_of(amount_in)?;
            (amount_in.checked_sub(f)?, f)
        } else {
            (amount_in, 0)
        };
        let (out, next) = if quote_to_base {
            // a buy that reaches the threshold completes the curve: not quoted
            if self.quote_reserve.checked_add(actual_in)? >= self.config.migration_quote_threshold {
                return None;
            }
            self.quote_to_base(actual_in)?
        } else {
            self.base_to_quote(actual_in)?
        };
        let (amount_out, fee) = if fee_on_input {
            (out, input_fee)
        } else {
            let f = fee_of(out)?;
            (out.checked_sub(f)?, f)
        };
        if amount_out == 0 {
            return None;
        }
        Some(DbcQuote { amount_out, fee, next_sqrt_price: next })
    }

    /// Price rises: walk the points above the current price.
    fn quote_to_base(&self, amount: u64) -> Option<(u64, u128)> {
        let (mut out, mut cur, mut left) = (0u64, self.sqrt_price, amount);
        for &(point, liquidity) in &self.config.curve {
            if point <= cur {
                continue;
            }
            let max_in = delta_quote_u256(cur, point, liquidity, true);
            if U256::from(left) < max_in {
                let next = next_sqrt_price_from_b(cur, liquidity, left)?;
                out = out.checked_add(delta_a(cur, next, liquidity, false)?)?;
                return Some((out, next));
            }
            out = out.checked_add(delta_a(cur, point, liquidity, false)?)?;
            cur = point;
            left -= max_in.low_u64();
            if left == 0 {
                return Some((out, cur));
            }
        }
        None // past the last point
    }

    /// Price falls: walk the points below the current price; segment `i`
    /// (below point `i`) has `curve[i].liquidity`, the first one starts at
    /// `sqrt_start_price`.
    fn base_to_quote(&self, amount: u64) -> Option<(u64, u128)> {
        let curve = &self.config.curve;
        let (mut out, mut cur, mut left) = (0u64, self.sqrt_price, amount);
        for i in (0..curve.len().saturating_sub(1)).rev() {
            let (point, _) = curve[i];
            if point >= cur {
                continue;
            }
            let liquidity = curve[i + 1].1;
            let max_in = delta_base_u256(point, cur, liquidity, true);
            if U256::from(left) < max_in {
                let next = next_sqrt_price_from_a(cur, liquidity, left)?;
                out = out.checked_add(delta_b(next, cur, liquidity, false)?)?;
                return Some((out, next));
            }
            out = out.checked_add(delta_b(point, cur, liquidity, false)?)?;
            cur = point;
            left -= max_in.low_u64();
        }
        if left > 0 {
            let liquidity = curve.first()?.1;
            let next = next_sqrt_price_from_a(cur, liquidity, left)?;
            if next < self.config.sqrt_start_price {
                return None; // the program: NotEnoughLiquidity
            }
            out = out.checked_add(delta_b(next, cur, liquidity, false)?)?;
            cur = next;
        }
        Some((out, cur))
    }

    /// `(quote, base)` virtual reserves of the current segment — the marginal
    /// price, for price-impact reporting.
    pub fn implied_reserves(&self) -> (u128, u128) {
        let liquidity = self.config.curve.iter().find(|(p, _)| *p >= self.sqrt_price).map(|(_, l)| *l).unwrap_or(0);
        if self.sqrt_price == 0 {
            return (0, 0);
        }
        let quote = (U256::from(liquidity) * U256::from(self.sqrt_price)) >> 128;
        let base = U256::from(liquidity) / U256::from(self.sqrt_price);
        let clamp = |v: U256| if v.bits() > 128 { u128::MAX } else { v.low_u128() };
        (clamp(quote), clamp(base))
    }
}

/// Base between two sqrt prices, 256-bit (a segment's remaining capacity can
/// exceed u64).
fn delta_base_u256(lower: u128, upper: u128, liquidity: u128, round_up: bool) -> U256 {
    let num = U256::from(liquidity) * U256::from(upper - lower);
    let den = U256::from(lower) * U256::from(upper);
    let (q, r) = num.div_mod(den);
    if round_up && !r.is_zero() { q + U256::one() } else { q }
}

fn delta_quote_u256(lower: u128, upper: u128, liquidity: u128, round_up: bool) -> U256 {
    let prod = U256::from(liquidity) * U256::from(upper - lower);
    let q = prod >> 128;
    if round_up && !(prod - (q << 128)).is_zero() { q + U256::one() } else { q }
}

/// `cliff · (1 − reduction/10⁴)^period`, Q64.64 as the program computes it:
/// base = 1 − ⌊reduction·2⁶⁴/10⁴⌋, square-and-multiply rounding down.
fn exponential_fee(cliff: u64, reduction: u64, period: u64) -> Option<u64> {
    if period == 0 {
        return Some(cliff);
    }
    let one: u128 = 1 << 64;
    let base = one.checked_sub(((reduction as u128) << 64) / BASIS_POINT_MAX)?;
    let (mut acc, mut b, mut e) = (one, base, period);
    while e > 0 {
        if e & 1 == 1 {
            acc = acc.checked_mul(b)? >> 64;
        }
        b = b.checked_mul(b)? >> 64;
        e >>= 1;
    }
    u64::try_from((acc.checked_mul(cliff as u128)?) >> 64).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bags launch config 7bH1hBvb… (2 % flat fee in SOL, two segments) and
    /// its pool CCa7ito… at a live pre-swap price.
    fn bags(sqrt_price: u128) -> DbcCurve {
        DbcCurve {
            sqrt_price,
            base_reserve: 0,
            quote_reserve: 1_438_704_212 - 39_400_714,
            activation_point: 449_545_766,
            is_migrated: false,
            volatility_accumulator: 0,
            config: DbcConfig {
                collect_fee_mode: 0,
                activation_type: 0,
                migration_quote_threshold: 85_000_000_000,
                sqrt_start_price: 3_141_367_320_245_630,
                curve: vec![
                    (6_401_204_812_200_420, 3_929_368_168_768_468_756_200_000_000_000_000),
                    (13_043_817_825_332_782, 2_425_988_008_058_820_449_100_000_000_000_000),
                ],
                cliff_fee_numerator: 20_000_000,
                ..Default::default()
            },
        }
    }

    #[test]
    fn buy_reproduces_live_swap_events() {
        // 658ZWbNq… EvtSwap2: 40_204_811 lamports in, fee 643_278 + 160_819
        // (protocol share), 1_258_276_557_343_754 base out, next price 3_265_958_769_351_710
        let c = bags(3_262_546_676_709_219);
        let q = c.swap_exact_in(true, 40_204_811, 449_600_000).unwrap();
        assert_eq!(q.fee, 643_278 + 160_819);
        assert_eq!(q.amount_out, 1_258_276_557_343_754);
        assert_eq!(q.next_sqrt_price, 3_265_958_769_351_710);
        // 54QzZzXa…, the swap before it
        let q = bags(3_259_647_055_593_071).swap_exact_in(true, 34_166_340, 449_600_000).unwrap();
        assert_eq!((q.amount_out, q.fee, q.next_sqrt_price), (1_071_363_050_293_294, 546_662 + 136_665, 3_262_546_676_709_219));
    }

    #[test]
    fn fee_modes_and_schedulers() {
        let mut c = bags(3_234_587_069_452_993);
        // mode 1: the fee comes off the base output instead
        c.config.collect_fee_mode = 1;
        let q = c.swap_exact_in(true, 40_000_000, 449_600_000).unwrap();
        let (raw, _) = c.quote_to_base(40_000_000).unwrap();
        assert_eq!(q.amount_out + q.fee, raw);
        assert_eq!(q.fee, (raw as u128 * 20_000_000).div_ceil(1_000_000_000) as u64);
        // linear scheduler: 60 % − 1.45 %/period, 40 periods of 1 unit
        c.config = DbcConfig { cliff_fee_numerator: 600_000_000, first_factor: 40, second_factor: 1, third_factor: 14_500_000, base_fee_mode: 0, ..c.config.clone() };
        assert_eq!(c.fee_numerator(c.activation_point, true, 1), Some(600_000_000));
        assert_eq!(c.fee_numerator(c.activation_point + 10, true, 1), Some(455_000_000));
        assert_eq!(c.fee_numerator(c.activation_point + 1_000, true, 1), Some(20_000_000));
        // exponential: 50 % × (1 − 18.86 %)^period, 12 periods of 5 s
        c.config = DbcConfig { cliff_fee_numerator: 500_000_000, first_factor: 12, second_factor: 5, third_factor: 1_886, base_fee_mode: 1, ..c.config.clone() };
        let one = c.fee_numerator(c.activation_point + 5, true, 1).unwrap();
        assert!((one as i64 - 405_700_000).abs() < 10, "{one}");
        assert!(c.fee_numerator(c.activation_point + 60, true, 1).unwrap() < c.fee_numerator(c.activation_point + 55, true, 1).unwrap());
        // rate limiter: flat below the reference amount, not quoted above it inside the window
        c.config = DbcConfig { cliff_fee_numerator: 10_000_000, first_factor: 10, second_factor: 100, third_factor: 1_000_000_000, base_fee_mode: 2, ..c.config.clone() };
        assert_eq!(c.fee_numerator(c.activation_point + 1, true, 500_000_000), Some(10_000_000));
        assert_eq!(c.fee_numerator(c.activation_point + 1, true, 2_000_000_000), None);
        assert_eq!(c.fee_numerator(c.activation_point + 1, false, 2_000_000_000), Some(10_000_000));
        assert_eq!(c.fee_numerator(c.activation_point + 101, true, 2_000_000_000), Some(10_000_000));
    }

    #[test]
    fn dynamic_fee_adds_the_volatility_term() {
        let mut c = bags(3_234_587_069_452_993);
        c.config.dynamic_fee = true;
        c.config.bin_step = 100;
        c.config.variable_fee_control = 20_000;
        c.volatility_accumulator = 50_000;
        // (50_000·100)² · 20_000 / 1e11, rounded up = 5_000_000
        assert_eq!(c.fee_numerator(c.activation_point, false, 1), Some(25_000_000));
    }

    #[test]
    fn walks_across_segments_both_ways() {
        let c = bags(6_300_000_000_000_000);
        let boundary = c.config.curve[0].0;
        let cross_in = delta_quote_u256(c.sqrt_price, boundary, c.config.curve[0].1, true).low_u64();
        // a buy just past the boundary continues in the second segment
        let (out, next) = c.quote_to_base(cross_in + 1_000_000).unwrap();
        assert!(next > boundary);
        let first = delta_a(c.sqrt_price, boundary, c.config.curve[0].1, false).unwrap();
        let second = delta_a(boundary, next, c.config.curve[1].1, false).unwrap();
        assert_eq!(out, first + second);
        // and a sell from above the boundary walks back into the first
        let mut hi = c.clone();
        hi.sqrt_price = next;
        let back_in = delta_base_u256(boundary, next, c.config.curve[1].1, true).low_u64();
        let (_, down) = hi.base_to_quote(back_in + 1_000_000_000).unwrap();
        assert!(down < boundary);
    }

    #[test]
    fn refuses_completed_unactivated_and_threshold_crossing_swaps() {
        let c = bags(3_234_587_069_452_993);
        assert!(c.swap_exact_in(true, 1_000_000, 449_600_000).is_some());
        assert!(c.swap_exact_in(true, 1_000_000, c.activation_point - 1).is_none(), "not active yet");
        assert!(c.swap_exact_in(true, 90_000_000_000, 449_600_000).is_none(), "reaches the migration threshold");
        let mut m = c.clone();
        m.is_migrated = true;
        assert!(m.swap_exact_in(false, 1_000_000, 449_600_000).is_none());
        let mut full = c.clone();
        full.quote_reserve = full.config.migration_quote_threshold;
        assert!(full.swap_exact_in(false, 1_000_000, 449_600_000).is_none());
        // selling more base than the curve holds above its start price
        assert!(c.swap_exact_in(false, u64::MAX / 2, 449_600_000).is_none());
    }

    #[test]
    fn exponential_fee_matches_the_closed_form() {
        assert_eq!(exponential_fee(500_000_000, 1_886, 0), Some(500_000_000));
        let f = exponential_fee(500_000_000, 1_886, 3).unwrap() as f64;
        let exact = 500_000_000.0 * (1.0f64 - 0.1886).powi(3);
        assert!((f - exact).abs() < 2.0, "{f} vs {exact}");
    }
}
