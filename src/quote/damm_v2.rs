//! Meteora DAMM v2 (`cpamdpZ…`) exact-input math.
//!
//! DAMM v2 is NOT constant-product on vault balances: it is a single-range
//! concentrated pool — price walks on `sqrt_price` (Q64.64) with a constant
//! `liquidity` between `sqrt_min_price` and `sqrt_max_price`. The vaults also
//! hold accrued protocol/partner fees the curve never sees, so x·y=k on vault
//! balances over-quotes.
//!
//! Layout (1112-byte account): `liquidity` at 360
//! is stored ×2^64 (so `a = L·(√Pb−√Pa)/(√Pa·√Pb)`, `b = L·(√Pb−√Pa)/2^128`),
//! fee struct at 8 (base scheduler) / 56 (dynamic fee), `collect_fee_mode` at
//! 484. Fee denominator is 1e9, capped at 50 %.

use super::clmm::U256;

pub const FEE_DENOMINATOR: u64 = 1_000_000_000;
pub const MAX_FEE_NUMERATOR: u64 = 500_000_000;
const BASIS_POINT_MAX: u128 = 10_000;

/// `PoolFeesStruct` fields that matter for a quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DammFees {
    pub cliff_fee_numerator: u64,
    /// 0 = linear scheduler, 1 = exponential, 2 = rate limiter (fee grows with amount)
    pub fee_scheduler_mode: u8,
    pub number_of_period: u16,
    pub period_frequency: u64,
    pub reduction_factor: u64,
    pub dynamic_fee_initialized: bool,
    pub variable_fee_control: u32,
    pub bin_step: u16,
    pub volatility_accumulator: u128,
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
        })
    }

    /// Base fee numerator at `current_point` (slot or unix time per
    /// `activation_type`). `None` for a rate-limiter schedule still inside its
    /// window — that fee depends on the amount and is not modelled.
    pub fn base_fee_numerator(&self, current_point: u64, activation_point: u64) -> Option<u64> {
        if self.period_frequency == 0 {
            return Some(self.cliff_fee_numerator);
        }
        let elapsed = current_point.saturating_sub(activation_point);
        match self.fee_scheduler_mode {
            0 => {
                let period = (elapsed / self.period_frequency).min(self.number_of_period as u64);
                Some(self.cliff_fee_numerator.saturating_sub(period.checked_mul(self.reduction_factor)?))
            }
            1 => {
                let period = (elapsed / self.period_frequency).min(self.number_of_period as u64);
                // cliff · (1 − reduction/10000)^period, in Q64.64
                let base = ((BASIS_POINT_MAX - (self.reduction_factor as u128).min(BASIS_POINT_MAX)) << 64) / BASIS_POINT_MAX;
                let mut acc: u128 = 1 << 64;
                let mut b = base;
                let mut e = period;
                while e > 0 {
                    if e & 1 == 1 {
                        acc = ((U256::from(acc) * U256::from(b)) >> 64).low_u128();
                    }
                    b = ((U256::from(b) * U256::from(b)) >> 64).low_u128();
                    e >>= 1;
                }
                Some((((U256::from(self.cliff_fee_numerator) * U256::from(acc)) >> 64).low_u128()) as u64)
            }
            2 => {
                // rate limiter: (max_limiter_duration = period_frequency) after which the cliff applies
                if elapsed > self.period_frequency { Some(self.cliff_fee_numerator) } else { None }
            }
            _ => None,
        }
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

    pub fn total_fee_numerator(&self, current_point: u64, activation_point: u64) -> Option<u64> {
        let base = self.base_fee_numerator(current_point, activation_point)?;
        Some(base.saturating_add(self.variable_fee_numerator()).min(MAX_FEE_NUMERATOR))
    }
}

/// Where the fee is taken for a given direction (`collect_fee_mode`: 0 both
/// tokens = fee on the OUTPUT; 1 only-B = fee on B whichever side it is).
pub fn fee_on_input(collect_fee_mode: u8, a_to_b: bool) -> bool {
    match collect_fee_mode {
        1 => !a_to_b, // b→a: fee on input (B)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DammQuote {
    pub amount_out: u64,
    pub fee: u64,
    pub next_sqrt_price: u128,
}

/// Exact-input swap. `None` when the price would leave `[sqrt_min, sqrt_max]`
/// (the program errors there) or on overflow.
pub fn swap_exact_in(
    sqrt_price: u128,
    liquidity: u128,
    sqrt_min: u128,
    sqrt_max: u128,
    fee_numerator: u64,
    collect_fee_mode: u8,
    a_to_b: bool,
    amount_in: u64,
) -> Option<DammQuote> {
    if amount_in == 0 || liquidity == 0 || sqrt_price == 0 || fee_numerator >= FEE_DENOMINATOR {
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
    let (next, out_raw) = if a_to_b {
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

    #[test]
    fn reserves_implied_by_the_curve_match_the_vaults_within_fees() {
        // vault a 277_257_916_809_294 ; vault b 61_054_705_929. Fees are
        // collected in B only (mode 1): the B vault also holds the protocol fee
        // (3_411_233_122) and unclaimed LP fees, none of which the curve sees.
        let a = delta_a(P, MAX, L, false).unwrap();
        let b = delta_b(MIN, P, L, false).unwrap();
        assert!((a as i128 - 277_257_916_809_294).abs() < 2_000_000_000, "{a}");
        assert!(b < 61_054_705_929 - 3_411_233_122, "{b}");
        assert!(b > 40_000_000_000, "{b}");
    }

    #[test]
    fn small_swap_matches_constant_product_on_implied_reserves() {
        let a = delta_a(P, MAX, L, false).unwrap() as u128;
        let b = delta_b(MIN, P, L, false).unwrap() as u128;
        let amt = 1_000_000_000u64; // 1 SOL of b → a  (b is the quote here)
        let q = swap_exact_in(P, L, MIN, MAX, 10_000_000, 1, false, amt).unwrap();
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
        // a→b in mode 1: fee comes off the output (B)
        let q = swap_exact_in(P, L, MIN, MAX, 10_000_000, 1, true, 1_000_000_000_000).unwrap();
        let raw = delta_b(q.next_sqrt_price, P, L, false).unwrap();
        assert_eq!(q.amount_out + q.fee, raw);
    }

    #[test]
    fn leaving_the_price_range_is_refused() {
        let narrow_max = next_sqrt_price_from_b(P, L, 10).unwrap();
        assert!(swap_exact_in(P, L, MIN, narrow_max, 0, 0, false, 1_000_000).is_none());
        assert!(swap_exact_in(P, L, MIN, narrow_max, 0, 0, false, 5).is_some());
    }

    #[test]
    fn fee_scheduler_linear_exponential_and_limiter() {
        let lin = DammFees { cliff_fee_numerator: 500_000_000, fee_scheduler_mode: 0, number_of_period: 10, period_frequency: 60, reduction_factor: 40_000_000, ..Default::default() };
        assert_eq!(lin.base_fee_numerator(1_000, 1_000), Some(500_000_000));
        assert_eq!(lin.base_fee_numerator(1_000 + 180, 1_000), Some(380_000_000));
        assert_eq!(lin.base_fee_numerator(1_000 + 100_000, 1_000), Some(100_000_000), "clamped at number_of_period");
        let exp = DammFees { cliff_fee_numerator: 500_000_000, fee_scheduler_mode: 1, number_of_period: 10, period_frequency: 60, reduction_factor: 5_000, ..Default::default() };
        assert_eq!(exp.base_fee_numerator(1_000, 1_000), Some(500_000_000));
        let one = exp.base_fee_numerator(1_060, 1_000).unwrap();
        assert!((one as i64 - 250_000_000).abs() <= 1, "{one}");
        let two = exp.base_fee_numerator(1_120, 1_000).unwrap();
        assert!((two as i64 - 125_000_000).abs() <= 1, "{two}");
        let flat = DammFees { cliff_fee_numerator: 10_000_000, ..Default::default() };
        assert_eq!(flat.total_fee_numerator(0, 0), Some(10_000_000));
        let lim = DammFees { cliff_fee_numerator: 10_000_000, fee_scheduler_mode: 2, period_frequency: 600, ..Default::default() };
        assert_eq!(lim.base_fee_numerator(1_100, 1_000), None, "inside the limiter window: amount-dependent");
        assert_eq!(lim.base_fee_numerator(2_000, 1_000), Some(10_000_000));
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
        let f = DammFees::parse(&d).unwrap();
        // (1000·80)^2 · 5000 / 1e11 = 320
        assert_eq!(f.variable_fee_numerator(), 320);
    }
}
