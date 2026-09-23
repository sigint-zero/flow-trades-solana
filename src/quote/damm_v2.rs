//! Meteora DAMM v2 (`cpamdpZ…`) exact-input math.
//!
//! A DAMM v2 pool prices on one of two curves, picked by `collect_fee_mode`:
//!
//! * modes 0 (fee on the output token) and 1 (fee only in token B): a
//!   single-range concentrated pool — price walks on `sqrt_price` (Q64.64)
//!   with a constant `liquidity` between `sqrt_min_price` and
//!   `sqrt_max_price`. The vaults also hold accrued protocol/partner fees the
//!   curve never sees, so x·y=k on vault balances over-quotes.
//! * mode 2 ("compounding"): constant product on the reserves the pool
//!   tracks itself (`token_a_amount` / `token_b_amount`, u64 at 680 / 688);
//!   `liquidity` is then an LP share count and `sqrt_price` is derived.
//!
//! Layout (1112-byte account): `liquidity` at 360 is stored ×2^64 (so
//! `a = L·(√Pb−√Pa)/(√Pa·√Pb)`, `b = L·(√Pb−√Pa)/2^128`), the base fee at 8
//! (32 bytes whose meaning depends on the mode byte at 16), the dynamic fee
//! at 56, `init_sqrt_price` at 152, `collect_fee_mode` at 484 and
//! `fee_version` at 486 (0 caps the total fee at 50 %, 1 at 99 %). Fee
//! denominator is 1e9.

use super::clmm::U256;

pub const FEE_DENOMINATOR: u64 = 1_000_000_000;
/// Total-fee cap of `fee_version` 0 pools.
pub const MAX_FEE_NUMERATOR: u64 = 500_000_000;
/// Total-fee cap of `fee_version` 1 pools.
pub const MAX_FEE_NUMERATOR_V1: u64 = 990_000_000;
const BASIS_POINT_MAX: u128 = 10_000;
const ONE_Q64: u128 = 1 << 64;

/// Base-fee modes (`BaseFeeMode`, byte 16).
pub const BASE_FEE_TIME_LINEAR: u8 = 0;
pub const BASE_FEE_TIME_EXPONENTIAL: u8 = 1;
pub const BASE_FEE_RATE_LIMITER: u8 = 2;
pub const BASE_FEE_MARKET_CAP_LINEAR: u8 = 3;
pub const BASE_FEE_MARKET_CAP_EXPONENTIAL: u8 = 4;

/// `collect_fee_mode` 2: constant product on the tracked reserves.
pub const COLLECT_FEE_MODE_COMPOUNDING: u8 = 2;

/// `PoolFeesStruct` fields that matter for a quote. The base fee's 32 bytes
/// are kept raw-ish: their meaning depends on `fee_scheduler_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DammFees {
    pub cliff_fee_numerator: u64,
    /// `BaseFeeMode`: 0/1 time scheduler (linear/exponential), 2 rate limiter
    /// (fee grows with the amount), 3/4 market-cap scheduler (linear/exponential).
    pub fee_scheduler_mode: u8,
    /// u16 at 22: `number_of_period` (schedulers) or `fee_increment_bps` (rate limiter).
    pub number_of_period: u16,
    /// 8 bytes at 24: `period_frequency` (time scheduler); `max_limiter_duration`
    /// u32 + `max_fee_bps` u32 (rate limiter); `sqrt_price_step_bps` u32 +
    /// `scheduler_expiration_duration` u32 (market-cap scheduler).
    pub period_frequency: u64,
    /// u64 at 32: `reduction_factor` (schedulers) or `reference_amount` (rate limiter).
    pub reduction_factor: u64,
    pub dynamic_fee_initialized: bool,
    pub variable_fee_control: u32,
    pub bin_step: u16,
    pub volatility_accumulator: u128,
    /// Price the market-cap scheduler measures its periods from.
    pub init_sqrt_price: u128,
    /// Pool `fee_version`: 0 caps the total fee at 50 %, 1 at 99 %.
    pub fee_version: u8,
}

impl DammFees {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 168 {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        Some(Self {
            cliff_fee_numerator: u64_at(8),
            fee_scheduler_mode: d[16],
            number_of_period: u16::from_le_bytes(d[22..24].try_into().unwrap()),
            period_frequency: u64_at(24),
            reduction_factor: u64_at(32),
            dynamic_fee_initialized: d[56] != 0,
            variable_fee_control: u32::from_le_bytes(d[68..72].try_into().unwrap()),
            bin_step: u16::from_le_bytes(d[72..74].try_into().unwrap()),
            volatility_accumulator: u128::from_le_bytes(d[120..136].try_into().unwrap()),
            init_sqrt_price: u128::from_le_bytes(d[152..168].try_into().unwrap()),
            fee_version: d.get(486).copied().unwrap_or(0),
        })
    }

    /// Low / high u32 of the 8 bytes at 24 (rate limiter, market-cap scheduler).
    fn factor_lo(&self) -> u64 {
        self.period_frequency & 0xffff_ffff
    }

    fn factor_hi(&self) -> u64 {
        self.period_frequency >> 32
    }

    pub fn max_fee_numerator(&self) -> u64 {
        if self.fee_version == 0 { MAX_FEE_NUMERATOR } else { MAX_FEE_NUMERATOR_V1 }
    }

    /// Scheduler fee after `period` periods (linear or exponential decay).
    fn scheduled_fee(&self, period: u64, exponential: bool) -> Option<u64> {
        let period = period.min(self.number_of_period as u64);
        if exponential {
            fee_in_period(self.cliff_fee_numerator, self.reduction_factor, period as u16)
        } else {
            self.cliff_fee_numerator.checked_sub(self.reduction_factor.checked_mul(period)?)
        }
    }

    /// Base fee numerator at `current_point` (slot or unix time per
    /// `activation_type`), for `amount_in` (after any input transfer fee)
    /// going `a_to_b` from `sqrt_price`. Mirrors the program's
    /// `get_base_fee_numerator_from_included_fee_amount`.
    pub fn base_fee_numerator(&self, current_point: u64, activation_point: u64, a_to_b: bool, amount_in: u64, sqrt_price: u128) -> Option<u64> {
        match self.fee_scheduler_mode {
            BASE_FEE_TIME_LINEAR | BASE_FEE_TIME_EXPONENTIAL => {
                if self.period_frequency == 0 {
                    return Some(self.cliff_fee_numerator);
                }
                let period = if current_point < activation_point {
                    self.number_of_period as u64
                } else {
                    (current_point - activation_point) / self.period_frequency
                };
                self.scheduled_fee(period, self.fee_scheduler_mode == BASE_FEE_TIME_EXPONENTIAL)
            }
            BASE_FEE_RATE_LIMITER => {
                if self.rate_limiter_applies(current_point, activation_point, a_to_b) {
                    self.rate_limited_fee_numerator(amount_in)
                } else {
                    Some(self.cliff_fee_numerator)
                }
            }
            BASE_FEE_MARKET_CAP_LINEAR | BASE_FEE_MARKET_CAP_EXPONENTIAL => {
                let (step_bps, expiration) = (self.factor_lo(), self.factor_hi());
                let period = if current_point > activation_point.checked_add(expiration)? || current_point < activation_point {
                    self.number_of_period as u64
                } else if sqrt_price <= self.init_sqrt_price {
                    0
                } else {
                    if self.init_sqrt_price == 0 || step_bps == 0 {
                        return None;
                    }
                    let passed = U256::from(sqrt_price - self.init_sqrt_price) * U256::from(BASIS_POINT_MAX)
                        / U256::from(self.init_sqrt_price)
                        / U256::from(step_bps);
                    passed.min(U256::from(self.number_of_period)).low_u64()
                };
                self.scheduled_fee(period, self.fee_scheduler_mode == BASE_FEE_MARKET_CAP_EXPONENTIAL)
            }
            _ => None,
        }
    }

    /// The rate limiter only charges more on B→A (quote → base) swaps inside
    /// `[activation, activation + max_limiter_duration]`.
    pub fn rate_limiter_applies(&self, current_point: u64, activation_point: u64, a_to_b: bool) -> bool {
        let zero = self.reduction_factor == 0 && self.factor_lo() == 0 && self.factor_hi() == 0 && self.number_of_period == 0;
        !zero
            && self.fee_scheduler_mode == BASE_FEE_RATE_LIMITER
            && !a_to_b
            && current_point >= activation_point
            && current_point as u128 <= activation_point as u128 + self.factor_lo() as u128
    }

    /// `FeeRateLimiter::get_fee_numerator_from_included_fee_amount`: the fee
    /// rate climbs by `fee_increment_bps` per `reference_amount` of input, up
    /// to `max_fee_bps`.
    fn rate_limited_fee_numerator(&self, amount_in: u64) -> Option<u64> {
        let (reference, c) = (self.reduction_factor, self.cliff_fee_numerator);
        if amount_in <= reference {
            return Some(c);
        }
        let to_numerator = |bps: u64| bps * FEE_DENOMINATOR / BASIS_POINT_MAX as u64;
        let max_fee = to_numerator(self.factor_hi());
        let inc = to_numerator(self.number_of_period as u64);
        let max_index = max_fee.checked_sub(c)?.checked_div(inc)?;
        let (a, b) = ((amount_in - reference) / reference, (amount_in - reference) % reference);
        let (c, a256, b256, i, x0, mi) = (U256::from(c), U256::from(a), U256::from(b), U256::from(inc), U256::from(reference), U256::from(max_index));
        let one = U256::one();
        let fee_numerator = if a < max_index {
            let n1 = c + c * a256 + i * a256 * (a256 + one) / U256::from(2u8);
            let n2 = c + i * (a256 + one);
            x0 * n1 + b256 * n2
        } else {
            let n1 = c + c * mi + i * mi * (mi + one) / U256::from(2u8);
            x0 * n1 + ((a256 - mi) * x0 + b256) * U256::from(max_fee)
        };
        let d = U256::from(FEE_DENOMINATOR);
        let fee = (fee_numerator + d - one) / d;
        if fee.bits() > 64 {
            return None;
        }
        // back to a rate, rounded up
        to_u64(mul_div(fee, d, U256::from(amount_in), true)?)
    }

    pub fn variable_fee_numerator(&self) -> u64 {
        if !self.dynamic_fee_initialized {
            return 0;
        }
        let vfa_bin = self.volatility_accumulator.saturating_mul(self.bin_step as u128);
        let square = U256::from(vfa_bin) * U256::from(vfa_bin);
        let v = square * U256::from(self.variable_fee_control);
        let scaled = (v + U256::from(99_999_999_999u64)) / U256::from(100_000_000_000u64);
        if scaled.bits() > 64 { u64::MAX } else { scaled.low_u64() }
    }

    /// Total trade fee numerator (base + dynamic, capped per `fee_version`).
    /// `None` where the program would error or the mode is unknown.
    pub fn total_fee_numerator(&self, current_point: u64, activation_point: u64, a_to_b: bool, amount_in: u64, sqrt_price: u128) -> Option<u64> {
        let base = self.base_fee_numerator(current_point, activation_point, a_to_b, amount_in, sqrt_price)?;
        Some(base.saturating_add(self.variable_fee_numerator()).min(self.max_fee_numerator()))
    }
}

/// `fee_math::get_fee_in_period`: `cliff · (1 − reduction/10⁴)^period` in
/// Q64.64, with the program's rounding (base = 1 − ⌊reduction·2^64/10⁴⌋,
/// squarings truncated, result truncated).
fn fee_in_period(cliff: u64, reduction_factor: u64, period: u16) -> Option<u64> {
    if reduction_factor == 0 {
        return Some(cliff);
    }
    let bps = ((reduction_factor as u128) << 64) / BASIS_POINT_MAX;
    let base = ONE_Q64.checked_sub(bps)?;
    let result = pow_q64(base, period as u32)?;
    u64::try_from(result.checked_mul(cliff as u128)? >> 64).ok()
}

/// `fee_math::pow` for a base below 1.0 (Q64.64): binary exponentiation that
/// squares the base once per exponent bit. The program errors on a zero result.
fn pow_q64(base: u128, exp: u32) -> Option<u128> {
    if exp == 0 {
        return Some(ONE_Q64);
    }
    if base >= ONE_Q64 || exp >= 0x80000 {
        return None;
    }
    let (mut result, mut squared) = (ONE_Q64, base);
    for bit in 0..19 {
        if exp & (1 << bit) != 0 {
            result = result.checked_mul(squared)? >> 64;
        }
        squared = squared.checked_mul(squared)? >> 64;
    }
    if result == 0 { None } else { Some(result) }
}

/// Where the fee is taken for a given direction (`FeeMode::get_fee_mode`):
/// 0 both tokens = fee on the OUTPUT; 1 only-B and 2 compounding = fee on B
/// whichever side it is.
pub fn fee_on_input(collect_fee_mode: u8, a_to_b: bool) -> bool {
    match collect_fee_mode {
        1 | COLLECT_FEE_MODE_COMPOUNDING => !a_to_b, // b→a: fee on input (B)
        _ => false,
    }
}

#[inline]
fn mul_div(a: U256, b: U256, d: U256, round_up: bool) -> Option<U256> {
    if d.is_zero() {
        return None;
    }
    let (q, r) = (a * b).div_mod(d);
    Some(if round_up && !r.is_zero() { q + U256::one() } else { q })
}

fn to_u64(v: U256) -> Option<u64> {
    if v.bits() > 64 { None } else { Some(v.low_u64()) }
}

/// Token-A between two sqrt prices: L·(upper−lower)/(lower·upper).
pub fn delta_a(lower: u128, upper: u128, liquidity: u128, round_up: bool) -> Option<u64> {
    if lower == 0 || upper < lower {
        return None;
    }
    let num = U256::from(liquidity) * U256::from(upper - lower);
    let den = U256::from(lower) * U256::from(upper);
    let (q, r) = num.div_mod(den);
    to_u64(if round_up && !r.is_zero() { q + U256::one() } else { q })
}

/// Token-B between two sqrt prices: L·(upper−lower) / 2^128.
pub fn delta_b(lower: u128, upper: u128, liquidity: u128, round_up: bool) -> Option<u64> {
    if upper < lower {
        return None;
    }
    let prod = U256::from(liquidity) * U256::from(upper - lower);
    let q = prod >> 128;
    let r = prod - (q << 128);
    to_u64(if round_up && !r.is_zero() { q + U256::one() } else { q })
}

/// Next sqrt price after `amount` of A goes in (price falls): L·√P / (L + amount·√P), rounded up.
pub fn next_sqrt_price_from_a(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    let l = U256::from(liquidity);
    let denom = l + U256::from(amount) * U256::from(sqrt_price);
    let q = mul_div(l, U256::from(sqrt_price), denom, true)?;
    if q.bits() > 128 { None } else { Some(q.low_u128()) }
}

/// Next sqrt price after `amount` of B goes in (price rises): √P + amount·2^128 / L, rounded down.
pub fn next_sqrt_price_from_b(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    if liquidity == 0 {
        return None;
    }
    let q = (U256::from(amount) << 128) / U256::from(liquidity);
    if q.bits() > 128 {
        return None;
    }
    sqrt_price.checked_add(q.low_u128())
}

/// The curve a pool prices on (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DammCurve {
    Concentrated { liquidity: u128, sqrt_price: u128, sqrt_min: u128, sqrt_max: u128 },
    /// `collect_fee_mode` 2: x·y=k on the tracked reserves.
    Compounding { reserve_a: u64, reserve_b: u64 },
}

impl DammCurve {
    /// Reserves implied by the curve (for price-impact reporting).
    pub fn reserves(&self) -> (u64, u64) {
        match *self {
            DammCurve::Concentrated { liquidity, sqrt_price, sqrt_min, sqrt_max } => (
                delta_a(sqrt_price, sqrt_max, liquidity, false).unwrap_or(0),
                delta_b(sqrt_min, sqrt_price, liquidity, false).unwrap_or(0),
            ),
            DammCurve::Compounding { reserve_a, reserve_b } => (reserve_a, reserve_b),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DammQuote {
    pub amount_out: u64,
    pub fee: u64,
    /// Post-swap sqrt price (0 for a compounding pool: the program derives it
    /// from the reserves after the swap).
    pub next_sqrt_price: u128,
}

/// Exact-input swap (`get_swap_result_from_exact_input`). `None` when the
/// price would leave `[sqrt_min, sqrt_max]` (the program errors there) or on
/// overflow.
pub fn swap_exact_in(curve: &DammCurve, fee_numerator: u64, collect_fee_mode: u8, a_to_b: bool, amount_in: u64) -> Option<DammQuote> {
    if amount_in == 0 || fee_numerator >= FEE_DENOMINATOR {
        return None;
    }
    let fee_of = |amt: u64| -> Option<u64> { to_u64(mul_div(U256::from(amt), U256::from(fee_numerator), U256::from(FEE_DENOMINATOR), true)?) };
    let on_input = fee_on_input(collect_fee_mode, a_to_b);
    let (actual_in, mut fee) = if on_input {
        let f = fee_of(amount_in)?;
        (amount_in.checked_sub(f)?, f)
    } else {
        (amount_in, 0)
    };
    let (next, out_raw) = match *curve {
        DammCurve::Concentrated { liquidity, sqrt_price, sqrt_min, sqrt_max } => {
            if liquidity == 0 || sqrt_price == 0 {
                return None;
            }
            if a_to_b {
                let next = next_sqrt_price_from_a(sqrt_price, liquidity, actual_in)?;
                if next < sqrt_min {
                    return None;
                }
                (next, delta_b(next, sqrt_price, liquidity, false)?)
            } else {
                let next = next_sqrt_price_from_b(sqrt_price, liquidity, actual_in)?;
                if next > sqrt_max {
                    return None;
                }
                (next, delta_a(sqrt_price, next, liquidity, false)?)
            }
        }
        DammCurve::Compounding { reserve_a, reserve_b } => {
            let (res_in, res_out) = if a_to_b { (reserve_a, reserve_b) } else { (reserve_b, reserve_a) };
            let den = (res_in as u128).checked_add(actual_in as u128)?;
            if den == 0 {
                return None;
            }
            (0, u64::try_from(res_out as u128 * actual_in as u128 / den).ok()?)
        }
    };
    let amount_out = if on_input {
        out_raw
    } else {
        fee = fee_of(out_raw)?;
        out_raw.checked_sub(fee)?
    };
    Some(DammQuote { amount_out, fee, next_sqrt_price: next })
}

#[cfg(test)]
mod tests {
    use super::*;

    // mainnet pool snapshot (CZViVZ…)
    const L: u128 = 67_866_898_358_284_586_169_766_704_927_877;
    const P: u128 = 244_778_937_746_739_512;
    const MIN: u128 = 4_295_048_016;
    const MAX: u128 = 79_226_673_521_066_979_257_578_248_091;

    fn conc(sqrt_max: u128) -> DammCurve {
        DammCurve::Concentrated { liquidity: L, sqrt_price: P, sqrt_min: MIN, sqrt_max }
    }

    #[test]
    fn reserves_implied_by_the_curve_match_the_vaults_within_fees() {
        // vault a 277_257_916_809_294 ; vault b 61_054_705_929. Fees are
        // collected in B only (mode 1): the B vault also holds the protocol fee
        // (3_411_233_122) and unclaimed LP fees, none of which the curve sees.
        let (a, b) = conc(MAX).reserves();
        assert!((a as i128 - 277_257_916_809_294).abs() < 2_000_000_000, "{a}");
        assert!(b < 61_054_705_929 - 3_411_233_122, "{b}");
        assert!(b > 40_000_000_000, "{b}");
    }

    #[test]
    fn small_swap_matches_constant_product_on_implied_reserves() {
        let (a, b) = conc(MAX).reserves();
        let (a, b) = (a as u128, b as u128);
        let amt = 1_000_000_000u64; // 1 SOL of b → a  (b is the quote here)
        let q = swap_exact_in(&conc(MAX), 10_000_000, 1, false, amt).unwrap();
        // fee on input (only-B mode, b→a): 1 %
        let in_eff = amt as u128 * 99 / 100;
        let cp = a * in_eff / (b + in_eff);
        let rel = (q.amount_out as f64 - cp as f64).abs() / cp as f64;
        assert!(rel < 1e-6, "{} vs {cp} ({rel})", q.amount_out);
        assert_eq!(q.fee, 10_000_000);
        assert!(q.next_sqrt_price > P);
    }

    #[test]
    fn fee_modes() {
        assert!(!fee_on_input(0, true) && !fee_on_input(0, false));
        assert!(!fee_on_input(1, true) && fee_on_input(1, false));
        assert!(!fee_on_input(2, true) && fee_on_input(2, false), "compounding: fee in B like only-B");
        // a→b in mode 1: fee comes off the output (B)
        let q = swap_exact_in(&conc(MAX), 10_000_000, 1, true, 1_000_000_000_000).unwrap();
        let raw = delta_b(q.next_sqrt_price, P, L, false).unwrap();
        assert_eq!(q.amount_out + q.fee, raw);
    }

    #[test]
    fn leaving_the_price_range_is_refused() {
        let narrow_max = next_sqrt_price_from_b(P, L, 10).unwrap();
        assert!(swap_exact_in(&conc(narrow_max), 0, 0, false, 1_000_000).is_none());
        assert!(swap_exact_in(&conc(narrow_max), 0, 0, false, 5).is_some());
    }

    #[test]
    fn fee_scheduler_linear_exponential_and_limiter() {
        let lin = DammFees { cliff_fee_numerator: 500_000_000, fee_scheduler_mode: 0, number_of_period: 10, period_frequency: 60, reduction_factor: 40_000_000, ..Default::default() };
        assert_eq!(lin.base_fee_numerator(1_000, 1_000, false, 1, 0), Some(500_000_000));
        assert_eq!(lin.base_fee_numerator(1_000 + 180, 1_000, false, 1, 0), Some(380_000_000));
        assert_eq!(lin.base_fee_numerator(1_000 + 100_000, 1_000, false, 1, 0), Some(100_000_000), "clamped at number_of_period");
        let exp = DammFees { cliff_fee_numerator: 500_000_000, fee_scheduler_mode: 1, number_of_period: 10, period_frequency: 60, reduction_factor: 5_000, ..Default::default() };
        assert_eq!(exp.base_fee_numerator(1_000, 1_000, false, 1, 0), Some(500_000_000));
        let one = exp.base_fee_numerator(1_060, 1_000, false, 1, 0).unwrap();
        assert!((one as i64 - 250_000_000).abs() <= 1, "{one}");
        let two = exp.base_fee_numerator(1_120, 1_000, false, 1, 0).unwrap();
        assert!((two as i64 - 125_000_000).abs() <= 1, "{two}");
        let flat = DammFees { cliff_fee_numerator: 10_000_000, ..Default::default() };
        assert_eq!(flat.total_fee_numerator(0, 0, true, 1, 0), Some(10_000_000));
    }

    #[test]
    fn exponential_decay_rounds_like_the_program() {
        // base = 1 − ⌊154·2^64/10⁴⌋ (not ⌊9846·2^64/10⁴⌋, one ulp lower);
        // mainnet pool G9vriU9Z…: cliff 99 %, 180 periods of 154 bps.
        assert_eq!(ONE_Q64 - (154u128 << 64) / 10_000, 18_162_664_214_974_424_522);
        assert_eq!((9_846u128 << 64) / 10_000, 18_162_664_214_974_424_521);
        let f = DammFees { cliff_fee_numerator: 990_000_000, fee_scheduler_mode: 1, number_of_period: 180, period_frequency: 1, reduction_factor: 154, fee_version: 1, ..Default::default() };
        let floor = f.base_fee_numerator(10_000, 0, false, 1, 0).unwrap();
        assert_eq!(floor, f.base_fee_numerator(180, 0, false, 1, 0).unwrap(), "clamped at number_of_period");
        assert_eq!(floor, 60_590_544);
        let approx = 990_000_000f64 * (1.0 - 0.0154f64).powi(180);
        assert!((floor as f64 - approx).abs() < 2.0, "{floor} vs {approx}");
        assert_eq!(f.base_fee_numerator(0, 0, false, 1, 0), Some(990_000_000), "period 0 = cliff");
    }

    #[test]
    fn fee_version_sets_the_cap() {
        let mut f = DammFees { cliff_fee_numerator: 900_000_000, ..Default::default() };
        assert_eq!(f.total_fee_numerator(0, 0, false, 1, 0), Some(MAX_FEE_NUMERATOR));
        f.fee_version = 1;
        assert_eq!(f.total_fee_numerator(0, 0, false, 1, 0), Some(900_000_000));
    }

    #[test]
    fn rate_limiter_matches_the_program_formula() {
        // cliff 1 %, +1 % per 1 SOL above the first, max 50 %, 60 s window
        // (test_rate_limiter_behavior in the program).
        let f = DammFees {
            cliff_fee_numerator: 10_000_000,
            fee_scheduler_mode: BASE_FEE_RATE_LIMITER,
            number_of_period: 100,
            period_frequency: 60 | (5_000u64 << 32),
            reduction_factor: 1_000_000_000,
            ..Default::default()
        };
        // only B→A inside the window pays more
        assert_eq!(f.base_fee_numerator(1_030, 1_000, true, 3_000_000_000, 0), Some(10_000_000));
        assert_eq!(f.base_fee_numerator(1_061, 1_000, false, 3_000_000_000, 0), Some(10_000_000));
        assert_eq!(f.base_fee_numerator(1_030, 1_000, false, 1_000_000_000, 0), Some(10_000_000));
        // 2 SOL: 1 SOL at 1 % + 1 SOL at 2 % = 0.03 SOL → 1.5 %
        assert_eq!(f.base_fee_numerator(1_030, 1_000, false, 2_000_000_000, 0), Some(15_000_000));
        // 3 SOL: 1 % + 2 % + 3 % → 2 %
        assert_eq!(f.base_fee_numerator(1_030, 1_000, false, 3_000_000_000, 0), Some(20_000_000));
        // 2.5 SOL: (0.01 + 0.02·1 + 0.03·0.5) / 2.5 = 1.8 %
        assert_eq!(f.base_fee_numerator(1_030, 1_000, false, 2_500_000_000, 0), Some(18_000_000));
        // past the cap (49 increments): the rest of the input pays 50 %
        assert_eq!(f.base_fee_numerator(1_030, 1_000, false, 1_000_000_000_000, 0), Some(487_750_000));
    }

    #[test]
    fn market_cap_scheduler_counts_price_steps_from_the_initial_price() {
        // mainnet pool DKkjHFdr…: mode 4, cliff 6 %, 180 periods of 165 bps,
        // one period per 17.01 % of sqrt-price growth, 180-day expiry.
        let f = DammFees {
            cliff_fee_numerator: 60_000_000,
            fee_scheduler_mode: BASE_FEE_MARKET_CAP_EXPONENTIAL,
            number_of_period: 180,
            period_frequency: 66_795_331_387_393_701,
            reduction_factor: 165,
            init_sqrt_price: 1_000_000_000_000,
            fee_version: 1,
            ..Default::default()
        };
        let (step, expiry) = (f.factor_lo(), f.factor_hi());
        assert_eq!((step, expiry), (1_701, 15_552_000));
        let act = 1_790_100_876;
        assert_eq!(f.base_fee_numerator(act + 10, act, true, 1, f.init_sqrt_price), Some(60_000_000), "no growth: cliff");
        assert_eq!(f.base_fee_numerator(act + 10, act, true, 1, f.init_sqrt_price / 2), Some(60_000_000), "below init: cliff");
        // +170.1 % sqrt price = 10 steps of 1701 bps (and one atom less = 9)
        let p10 = f.init_sqrt_price + f.init_sqrt_price * 17_010 / 10_000;
        assert_eq!(f.base_fee_numerator(act + 10, act, true, 1, p10 - 1), fee_in_period(60_000_000, 165, 9));
        assert_eq!(f.base_fee_numerator(act + 10, act, true, 1, p10), fee_in_period(60_000_000, 165, 10));
        // expired or before activation: floor fee
        assert_eq!(f.base_fee_numerator(act + expiry + 1, act, true, 1, f.init_sqrt_price), fee_in_period(60_000_000, 165, 180));
        assert_eq!(f.base_fee_numerator(act - 1, act, true, 1, f.init_sqrt_price), fee_in_period(60_000_000, 165, 180));
        let lin = DammFees { fee_scheduler_mode: BASE_FEE_MARKET_CAP_LINEAR, reduction_factor: 100_000, ..f };
        assert_eq!(lin.base_fee_numerator(act + 10, act, true, 1, p10), Some(60_000_000 - 10 * 100_000));
    }

    #[test]
    fn parses_live_fee_struct() {
        let mut d = vec![0u8; 1112];
        d[8..16].copy_from_slice(&10_000_000u64.to_le_bytes());
        d[48] = 20;
        let f = DammFees::parse(&d).unwrap();
        assert_eq!(f.cliff_fee_numerator, 10_000_000);
        assert!(!f.dynamic_fee_initialized);
        assert_eq!(f.variable_fee_numerator(), 0);
        d[56] = 1;
        d[68..72].copy_from_slice(&5_000u32.to_le_bytes());
        d[72..74].copy_from_slice(&80u16.to_le_bytes());
        d[120..136].copy_from_slice(&1_000u128.to_le_bytes());
        d[152..168].copy_from_slice(&412_481_737_123_559_485u128.to_le_bytes());
        d[486] = 1;
        let f = DammFees::parse(&d).unwrap();
        // (1000·80)^2 · 5000 / 1e11 = 320
        assert_eq!(f.variable_fee_numerator(), 320);
        assert_eq!(f.init_sqrt_price, 412_481_737_123_559_485);
        assert_eq!(f.max_fee_numerator(), MAX_FEE_NUMERATOR_V1);
    }

    /// Compounding pool 7euSAKj1… (collect_fee_mode 2, flat 0.3 %, half of
    /// the LP fee compounded): two consecutive live `EvtSwap2`s. The reserves
    /// before each swap are the event's post reserves with the swap undone —
    /// and equal the previous event's post reserves.
    const POOL_7EU_FEE: u64 = 3_000_000;

    #[test]
    fn compounding_sell_reproduces_a_mainnet_swap_event() {
        // slot 449523631, A→B: 7_176_360_349 in → 645_350 out, fee 777 + 388
        // + 777 (claiming, protocol, compounding) off the output. Reserves
        // before = post of slot 449493719.
        let curve = DammCurve::Compounding { reserve_a: 1_529_957_184_074, reserve_b: 138_646_135 };
        let q = swap_exact_in(&curve, POOL_7EU_FEE, COLLECT_FEE_MODE_COMPOUNDING, true, 7_176_360_349).unwrap();
        assert_eq!((q.amount_out, q.fee), (645_350, 1_942));
        // post: a += in; b −= out + fee, then += the compounding share
        assert_eq!(1_529_957_184_074 + 7_176_360_349, 1_537_133_544_423u64);
        assert_eq!(138_646_135 - 645_350 - 1_942 + 777, 137_999_620u64);
    }

    #[test]
    fn compounding_buy_reproduces_a_mainnet_swap_event() {
        // slot 449543933, B→A: 335_560 in, fee 1_007 off the input (403 + 201
        // + 403), 334_553 into the curve → 3_717_466_389 out. Reserves before
        // = post of the swap above.
        let curve = DammCurve::Compounding { reserve_a: 1_537_133_544_423, reserve_b: 137_999_620 };
        let q = swap_exact_in(&curve, POOL_7EU_FEE, COLLECT_FEE_MODE_COMPOUNDING, false, 335_560).unwrap();
        assert_eq!((q.amount_out, q.fee), (3_717_466_389, 1_007));
        // the concentrated formula on the same pool is not even close
        assert_eq!(curve.reserves(), (1_537_133_544_423, 137_999_620));
    }
}
