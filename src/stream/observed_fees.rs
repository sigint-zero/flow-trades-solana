//! Per-pool EFFECTIVE swap fee, measured from the swaps we stream.
//!
//! Why measure instead of read: several venues charge what no config field
//! states. pump.fun AMM's young pools were measured at 3–36% effective on buys
//! (the 2026 buyback mechanic moves the price against the buyer, on top of the
//! 1.25% tier), stable per pool but not a function of the tier table; Meteora
//! DAMM v2 runs fee schedulers; Raydium CPMM fees live in a config account.
//! Every confirmed swap carries the pool's vault balances before and after in
//! its meta, so for a constant-product pool the fee the trader actually paid
//! is `1 − implied_in / paid_in` with `implied_in = out × R_in / (R_out − out)`.
//! The quote engine prefers a fresh observation over any table.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;

static OBSERVED: std::sync::LazyLock<DashMap<Pubkey, (u32, Instant)>> = std::sync::LazyLock::new(DashMap::new);
const CAP: usize = 200_000;

/// Record an observation (ppm of the user's input). Bounded: when full, the
/// stalest entries are dropped.
pub fn record(pool: Pubkey, fee_ppm: u32) {
    if OBSERVED.len() >= CAP {
        let cutoff = Instant::now() - Duration::from_secs(600);
        OBSERVED.retain(|_, (_, t)| *t > cutoff);
    }
    OBSERVED.insert(pool, (fee_ppm, Instant::now()));
}

/// The last observed fee for `pool` (ppm) if it is younger than `max_age`.
pub fn get_fresh_ppm(pool: &Pubkey, max_age: Duration) -> Option<u32> {
    OBSERVED.get(pool).filter(|e| e.1.elapsed() <= max_age).map(|e| e.0)
}

/// [`get_fresh_ppm`] in whole bps, rounded up.
pub fn get_fresh(pool: &Pubkey, max_age: Duration) -> Option<u16> {
    get_fresh_ppm(pool, max_age).map(|ppm| ppm.div_ceil(100) as u16)
}

/// Number of pools with an observation (metrics/health).
pub fn len() -> usize {
    OBSERVED.len()
}

/// Effective fee in ppm implied by one constant-product swap: the trader paid
/// `paid_in`, the pool priced `implied_in = out × r_in / (r_out − out)`.
/// Rounded UP at ppm resolution: a fee read even a fraction low over-quotes.
/// `None` when the numbers
/// cannot be a constant-product swap (out ≥ r_out, implied > paid) or the
/// implied fee exceeds 50% (not a CP venue after all).
pub fn implied_fee_ppm(paid_in: u128, out: u128, r_in: u128, r_out: u128) -> Option<u32> {
    if paid_in == 0 || out == 0 || r_in == 0 || out >= r_out {
        return None;
    }
    let implied = out.checked_mul(r_in)? / (r_out - out);
    if implied > paid_in {
        return None;
    }
    let ppm = (paid_in - implied).checked_mul(1_000_000)?.div_ceil(paid_in);
    (ppm <= 500_000).then_some(ppm as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implied_fee_recovers_a_known_fee() {
        // pool 100 SOL / 200M tokens, 1 SOL in at 120 bps → out via CP on 0.988 SOL
        let (r_in, r_out, paid) = (100_000_000_000u128, 200_000_000_000_000u128, 1_000_000_000u128);
        let eff_in = paid * 9_880 / 10_000;
        let out = r_out * eff_in / (r_in + eff_in);
        let ppm = implied_fee_ppm(paid, out, r_in, r_out).unwrap();
        assert!((11_990..=12_010).contains(&ppm), "{ppm}");
        assert_eq!(implied_fee_ppm(paid, r_out, r_in, r_out), None, "drains the pool");
        assert_eq!(implied_fee_ppm(0, 1, r_in, r_out), None);
    }

    #[test]
    fn record_and_expire() {
        let p = Pubkey::new_unique();
        record(p, 32_101);
        assert_eq!(get_fresh_ppm(&p, Duration::from_secs(60)), Some(32_101));
        assert_eq!(get_fresh(&p, Duration::from_secs(60)), Some(322), "bps rounded up");
        assert_eq!(get_fresh(&p, Duration::from_secs(0)), None);
        assert_eq!(get_fresh(&Pubkey::new_unique(), Duration::from_secs(60)), None);
    }
}
