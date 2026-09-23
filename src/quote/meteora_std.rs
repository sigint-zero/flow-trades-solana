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
//! A swap also moves tokens through both vaults (deposit the input, withdraw
//! the output) and every conversion between tokens and vault LP rounds down,
//! so the exact output is usually one or two atoms below the plain
//! constant-product figure on the pool's share: [`swap_exact_in`] replays it.
//!
//! Layout: pool fees at 330 (`trade_fee_numerator`,
//! `trade_fee_denominator` = 100 000, protocol numerator/denominator at
//! 346/354), vault `total_amount` u64@11, `lp_mint`@115, tracker at
//! 1203/1211/1219. Only the constant-product curve is quoted (tag byte 874 == 0).

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
    pub protocol_fee_numerator: u64,
    pub protocol_fee_denominator: u64,
    /// The vaults behind each side, for the swap's exact share rounding.
    pub vault_a: VaultShare,
    pub vault_b: VaultShare,
}

/// Vault fields needed for the unlocked amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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
        if ratio > LOCKED_PROFIT_DEGRADATION_DENOMINATOR {
            return 0;
        }
        ((self.last_updated_locked_profit as u128) * (LOCKED_PROFIT_DEGRADATION_DENOMINATOR - ratio) / LOCKED_PROFIT_DEGRADATION_DENOMINATOR) as u64
    }

    pub fn unlocked_amount(&self, now: u64) -> u64 {
        self.total_amount.saturating_sub(self.locked_profit(now))
    }

    /// Token amount owned through `share` LP tokens out of `lp_supply` (rounded down).
    pub fn amount_by_share(&self, share: u64, lp_supply: u64, now: u64) -> u64 {
        if lp_supply == 0 {
            return 0;
        }
        ((share as u128) * (self.unlocked_amount(now) as u128) / (lp_supply as u128)) as u64
    }

    /// LP tokens worth `amount` (`get_unmint_amount`, rounded down).
    pub fn unmint_amount(&self, amount: u64, lp_supply: u64, now: u64) -> Option<u64> {
        let unlocked = self.unlocked_amount(now);
        if unlocked == 0 {
            return None;
        }
        u64::try_from((amount as u128) * (lp_supply as u128) / (unlocked as u128)).ok()
    }
}

/// One side of the pool: its vault, the vault's LP supply and the pool's LP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct VaultShare {
    pub vault: VaultView,
    pub lp_supply: u64,
    pub pool_lp: u64,
}

impl VaultShare {
    fn pool_amount(&self, now: u64) -> u64 {
        self.vault.amount_by_share(self.pool_lp, self.lp_supply, now)
    }
}

impl MeteoraStdReserves {
    /// Refresh both vault shares from `vault_a` / `vault_b` account data and
    /// `[lp_mint_a, lp_mint_b, pool_lp_a, pool_lp_b]` account data.
    pub fn update(&mut self, vault_a: &[u8], vault_b: &[u8], lp: [Option<&[u8]>; 4], now: u64) -> bool {
        let supply = |d: Option<&[u8]>| d.and_then(|d| d.get(36..44)).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        let balance = |d: Option<&[u8]>| d.and_then(|d| d.get(64..72)).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        let (Some(va), Some(vb)) = (VaultView::parse(vault_a), VaultView::parse(vault_b)) else { return false };
        let (Some(sa), Some(sb), Some(la), Some(lb)) = (supply(lp[0]), supply(lp[1]), balance(lp[2]), balance(lp[3])) else { return false };
        self.vault_a = VaultShare { vault: va, lp_supply: sa, pool_lp: la };
        self.vault_b = VaultShare { vault: vb, lp_supply: sb, pool_lp: lb };
        self.token_a_amount = self.vault_a.pool_amount(now);
        self.token_b_amount = self.vault_b.pool_amount(now);
        self.computed_at = now;
        true
    }
}

/// Pool `fees` at 330 (trade numerator/denominator, protocol numerator/denominator) and curve tag at 874.
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

/// Protocol share of the trade fee (numerator, denominator) at 346.
pub fn parse_protocol_fee(d: &[u8]) -> Option<(u64, u64)> {
    let rd = |o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    Some((rd(346)?, rd(354)?))
}

/// `calculate_fee`: floor, but at least one token when the rate is non-zero.
fn fee_of(amount: u64, numerator: u64, denominator: u64) -> Option<u64> {
    if numerator == 0 || amount == 0 {
        return Some(0);
    }
    let fee = u64::try_from((amount as u128) * (numerator as u128) / (denominator as u128)).ok()?;
    Some(fee.max(1))
}

/// Exact-input swap as the program runs it (`dynamic-amm` swap / the
/// official `compute_quote`): the protocol part of the trade fee is taken
/// off the input, the rest is deposited into the input vault (LP minted and
/// valued back at the vault's share price), the LP part of the fee is
/// deducted, x·y=k on the pool's vault shares, and the output is withdrawn
/// by burning the LP it is worth — every vault conversion rounds down.
/// Returns (amount_out, trade_fee).
pub fn swap_exact_in(reserves: &MeteoraStdReserves, a_to_b: bool, amount_in: u64, now: u64) -> Option<(u64, u64)> {
    if !reserves.constant_product || amount_in == 0 || reserves.trade_fee_denominator == 0 {
        return None;
    }
    let (vin, vout) = if a_to_b { (reserves.vault_a, reserves.vault_b) } else { (reserves.vault_b, reserves.vault_a) };
    if vin.lp_supply == 0 || vout.lp_supply == 0 {
        return None;
    }
    let (res_in, res_out) = (vin.pool_amount(now), vout.pool_amount(now));
    if res_in == 0 || res_out == 0 {
        return None;
    }
    let trade_fee = fee_of(amount_in, reserves.trade_fee_numerator, reserves.trade_fee_denominator)?;
    let protocol_fee = if reserves.protocol_fee_denominator == 0 { 0 } else { fee_of(trade_fee, reserves.protocol_fee_numerator, reserves.protocol_fee_denominator)? };
    let lp_fee = trade_fee.checked_sub(protocol_fee)?;
    let deposit = amount_in.checked_sub(protocol_fee)?;
    let minted = vin.vault.unmint_amount(deposit, vin.lp_supply, now)?;
    let after = VaultView { total_amount: vin.vault.total_amount.checked_add(deposit)?, ..vin.vault };
    let res_in_after = after.amount_by_share(vin.pool_lp.checked_add(minted)?, vin.lp_supply.checked_add(minted)?, now);
    let actual_in = res_in_after.checked_sub(res_in)?.checked_sub(lp_fee)?;
    // spl constant_product::swap: dst − ceil(src·dst / (src + x)); a zero result is refused
    let invariant = res_in as u128 * res_out as u128;
    let new_src = res_in as u128 + actual_in as u128;
    let new_dst = invariant / new_src;
    if new_dst == 0 {
        return None;
    }
    let new_dst = if invariant % new_src != 0 { new_dst + 1 } else { new_dst };
    let dest = u64::try_from((res_out as u128).checked_sub(new_dst)?).ok()?;
    if dest == 0 {
        return None;
    }
    let burned = vout.vault.unmint_amount(dest, vout.lp_supply, now)?;
    let out = vout.vault.amount_by_share(burned, vout.lp_supply, now);
    if out == 0 || out >= res_out {
        return None;
    }
    Some((out, trade_fee))
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

    /// Pool 5yuefgbJ… (USDC/SOL, 0.25 % with 20 % of it to the protocol):
    /// state read at slot 449_554_945, swap simulated at slot 449_554_950
    /// (clock 1_790_127_171) through the router.
    fn live_5yue() -> MeteoraStdReserves {
        let usdc = VaultView { total_amount: 10_016_198_225_281, last_updated_locked_profit: 44_924_243, last_report: 1_790_126_968, locked_profit_degradation: 46_296_296 };
        let sol = VaultView { total_amount: 247_089_387_669_855, last_updated_locked_profit: 973_218_579, last_report: 1_790_124_389, locked_profit_degradation: 46_296_296 };
        MeteoraStdReserves {
            trade_fee_numerator: 25,
            trade_fee_denominator: 10_000,
            constant_product: true,
            protocol_fee_numerator: 20_000,
            protocol_fee_denominator: 100_000,
            vault_a: VaultShare { vault: usdc, lp_supply: 8_601_347_807_124, pool_lp: 34_478_120_981 },
            vault_b: VaultShare { vault: sol, lp_supply: 219_305_752_445_913, pool_lp: 299_450_407_403 },
            ..Default::default()
        }
    }

    #[test]
    fn swap_matches_a_simulated_swap_to_the_atom() {
        let r = live_5yue();
        let now = 1_790_127_171;
        // 0.1 SOL → USDC: the simulated router output
        let (out, fee) = swap_exact_in(&r, false, 100_000_000, now).unwrap();
        assert_eq!((out, fee), (11_866_839, 250_000));
        // the constant-product figure on the pool's share, fee off the input,
        // is one atom higher (the vault withdrawal rounds down)
        let (ra, rb) = (r.vault_a.pool_amount(now) as u128, r.vault_b.pool_amount(now) as u128);
        let x = 100_000_000u128 - 250_000;
        assert_eq!((ra * x / (rb + x)) as u64, 11_866_840);
    }

    #[test]
    fn fees_are_floored_with_a_one_token_minimum() {
        assert_eq!(fee_of(25_598, 25, 10_000), Some(63), "floor, not ceil (63.995)");
        assert_eq!(fee_of(3, 25, 10_000), Some(1), "minimum one token");
        assert_eq!(fee_of(3, 0, 10_000), Some(0));
        let r = live_5yue();
        assert!(swap_exact_in(&MeteoraStdReserves { constant_product: false, ..r }, true, 10_000_000, 1_790_127_171).is_none());
        assert!(swap_exact_in(&MeteoraStdReserves { vault_a: VaultShare::default(), ..r }, true, 10_000_000, 1_790_127_171).is_none());
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
