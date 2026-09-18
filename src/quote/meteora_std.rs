//! Meteora Standard / Dynamic AMM (`Eo7WjKq…`) reserve accounting.
//!
//! The pool does not hold its tokens: it holds LP shares of two Meteora
//! *dynamic vaults*, which lend part of their balance out. Its reserves are
//! `pool_lp_balance · vault_unlocked_amount / vault_lp_supply` per side, where
//! the unlocked amount is `total_amount` minus profit still vesting
//! (`locked_profit_tracker`, degrading linearly to zero over
//! `1e12 / locked_profit_degradation` seconds). Quoting on the vault TOKEN
//! balance instead over-quotes by the lent-out share plus other pools' LP.
//!
//! Layout: pool fees at 330 (`trade_fee_numerator`,
//! `trade_fee_denominator` = 100 000, protocol numerator/denominator), vault
//! `total_amount` u64@11, `lp_mint`@115, tracker at 1203/1211/1219. Only the
//! constant-product curve is quoted (tag byte 874 == 0).

pub const LOCKED_PROFIT_DEGRADATION_DENOMINATOR: u128 = 1_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MeteoraStdReserves {
    /// Pool's share of each vault, in token units, as of `computed_at` (unix s).
    pub token_a_amount: u64,
    pub token_b_amount: u64,
    pub computed_at: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    /// false when the pool is a stable-swap curve (not quoted)
    pub constant_product: bool,
}

/// Vault fields needed for the unlocked amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VaultView {
    pub total_amount: u64,
    pub last_updated_locked_profit: u64,
    pub last_report: u64,
    pub locked_profit_degradation: u64,
}

impl VaultView {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 1227 {
            return None;
        }
        let rd = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        Some(Self { total_amount: rd(11), last_updated_locked_profit: rd(1203), last_report: rd(1211), locked_profit_degradation: rd(1219) })
    }

    pub fn locked_profit(&self, now: u64) -> u64 {
        let duration = now.saturating_sub(self.last_report) as u128;
        let ratio = duration.saturating_mul(self.locked_profit_degradation as u128);
        if ratio >= LOCKED_PROFIT_DEGRADATION_DENOMINATOR {
            return 0;
        }
        ((self.last_updated_locked_profit as u128) * (LOCKED_PROFIT_DEGRADATION_DENOMINATOR - ratio) / LOCKED_PROFIT_DEGRADATION_DENOMINATOR) as u64
    }

    pub fn unlocked_amount(&self, now: u64) -> u64 {
        self.total_amount.saturating_sub(self.locked_profit(now))
    }

    /// Token amount owned through `share` LP tokens out of `lp_supply`.
    pub fn amount_by_share(&self, share: u64, lp_supply: u64, now: u64) -> u64 {
        if lp_supply == 0 {
            return 0;
        }
        ((share as u128) * (self.unlocked_amount(now) as u128) / (lp_supply as u128)) as u64
    }
}

/// Pool `fees` at 330 and curve tag at 874.
pub fn parse_pool_fees(d: &[u8]) -> Option<(u64, u64, bool)> {
    if d.len() < 346 {
        return None;
    }
    let num = u64::from_le_bytes(d[330..338].try_into().unwrap());
    let den = u64::from_le_bytes(d[338..346].try_into().unwrap());
    if den == 0 || num >= den {
        return None;
    }
    let cp = d.len() <= 874 || d[874] == 0;
    Some((num, den, cp))
}

/// Constant-product swap with the trade fee off the input (as the program does).
pub fn swap_exact_in(reserves: &MeteoraStdReserves, a_to_b: bool, amount_in: u64) -> Option<(u64, u64)> {
    if !reserves.constant_product || amount_in == 0 || reserves.trade_fee_denominator == 0 {
        return None;
    }
    let fee = u64::try_from((amount_in as u128 * reserves.trade_fee_numerator as u128).div_ceil(reserves.trade_fee_denominator as u128)).ok()?;
    let in_less = amount_in.checked_sub(fee)? as u128;
    let (rin, rout) = if a_to_b { (reserves.token_a_amount, reserves.token_b_amount) } else { (reserves.token_b_amount, reserves.token_a_amount) };
    if rin == 0 || rout == 0 {
        return None;
    }
    let out = (rout as u128).checked_mul(in_less)? / (rin as u128 + in_less);
    let out = u64::try_from(out).ok()?;
    if out == 0 || out >= rout {
        return None;
    }
    Some((out, fee))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_profit_degrades_linearly_to_zero() {
        // mainnet vault 3ESUFCnR…: degradation 46_296_296 ⇒ fully unlocked after 21_600 s
        let v = VaultView { total_amount: 9_458_151_669_147, last_updated_locked_profit: 36_063_543, last_report: 1_789_687_760, locked_profit_degradation: 46_296_296 };
        assert_eq!(v.locked_profit(1_789_687_760), 36_063_543);
        let half = v.locked_profit(1_789_687_760 + 10_800);
        assert!((half as i64 - 18_031_771).abs() <= 2, "{half}");
        assert_eq!(v.locked_profit(1_789_687_760 + 21_600), 0);
        assert_eq!(v.unlocked_amount(1_789_687_760 + 30_000), 9_458_151_669_147);
    }

    #[test]
    fn pool_share_of_a_vault_matches_the_live_snapshot() {
        // pool 5NQTw1Wq…: lp balance 47_364_547_202 of supply 8_122_892_172_785 ⇒ ≈ 55_150_242_674 USDC atoms
        let v = VaultView { total_amount: 9_458_151_669_147, last_updated_locked_profit: 36_063_543, last_report: 1_789_687_760, locked_profit_degradation: 46_296_296 };
        let amt = v.amount_by_share(47_364_547_202, 8_122_892_172_785, 1_789_687_760 + 1_500);
        assert!((amt as i64 - 55_150_242_674).abs() < 2_000, "{amt}");
        // and the naive vault TOKEN balance (8_035_235_798_176) would have been 145× too large
    }

    #[test]
    fn swap_takes_the_fee_off_the_input() {
        let r = MeteoraStdReserves { token_a_amount: 1_000_000_000, token_b_amount: 4_000_000_000, computed_at: 0, trade_fee_numerator: 250, trade_fee_denominator: 100_000, constant_product: true };
        let (out, fee) = swap_exact_in(&r, true, 10_000_000).unwrap();
        assert_eq!(fee, 25_000);
        assert_eq!(out, 4_000_000_000u128.checked_mul(9_975_000).unwrap().checked_div(1_009_975_000).unwrap() as u64);
        let stable = MeteoraStdReserves { constant_product: false, ..r };
        assert!(swap_exact_in(&stable, true, 10_000_000).is_none());
    }

    #[test]
    fn parses_fees_and_curve_tag() {
        let mut d = vec![0u8; 944];
        d[330..338].copy_from_slice(&250u64.to_le_bytes());
        d[338..346].copy_from_slice(&100_000u64.to_le_bytes());
        assert_eq!(parse_pool_fees(&d), Some((250, 100_000, true)));
        d[874] = 1;
        assert_eq!(parse_pool_fees(&d), Some((250, 100_000, false)));
    }
}
