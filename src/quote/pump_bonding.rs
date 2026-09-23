//! pump.fun bonding curve (`6EF8rr…`) exact-input math.
//!
//! The curve is constant product on the VIRTUAL reserves stored in the
//! `BondingCurve` account (`virtual_sol_reserves`, `virtual_token_reserves`).
//! Fees come off the SOL side, each rounded up: the protocol fee and the
//! creator fee of the fee program's `fee_config` (market-cap tiers; a single
//! flat tier of 95 + 30 bps on mainnet), the creator fee replaced by the
//! curve's own `creator_fee_bps` when that is set. Pinned to the atom on live
//! `TradeEvent`s (see the unit tests):
//!
//! - sell `t` tokens: `gross = ⌊t·vs / (vt + t)⌋`; the user receives
//!   `gross − fees(gross)`.
//! - `buy_exact_sol_in(s)`: `x` is the largest amount with `x + fees(x) ≤ s`.
//!   The curve is credited `x` but prices `x − 1`:
//!   `tokens = ⌊(x − 1)·vt / (vs + x − 1)⌋`.
//!
//! Not quoted: completed curves (migrated to the AMM), curves quoted in a
//! token other than SOL (`quote_mint` set, traded with the `_v2`
//! instructions), cashback coins (their fee split is not pinned), and buys
//! that would take the curve's last real tokens (the program fills those
//! partially and completes the curve).

use std::sync::RwLock;

use crate::execution::amms::pumpfun_amm::FeeTier;

const BPS: u128 = 10_000;

/// The fee program's `fee_config` for the bonding curve (seed program = pump).
pub const FEE_CONFIG: solana_sdk::pubkey::Pubkey = solana_sdk::pubkey!("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");

/// The mainnet table: one tier, protocol 95 + creator 30 bps.
const DEFAULT_TIER: FeeTier = FeeTier { market_cap_lamports: 0, lp_bps: 0, protocol_bps: 95, creator_bps: 30 };

static FEE_TIERS: RwLock<Option<Vec<FeeTier>>> = RwLock::new(None);

/// Read the bonding curve's fee tiers from chain and install them. On failure
/// the built-in table stays in force.
pub async fn load_fee_tiers(rpc: &solana_client::nonblocking::rpc_client::RpcClient) -> crate::error::TradeResult<usize> {
    let acct = rpc
        .get_account(&FEE_CONFIG)
        .await
        .map_err(|e| crate::error::TradeError::Rpc(format!("pump bonding fee_config fetch: {e}")))?;
    let tiers = crate::execution::amms::pumpfun_amm::parse_fee_config(&acct.data)?;
    let n = tiers.len();
    set_fee_tiers(tiers);
    Ok(n)
}

pub fn set_fee_tiers(tiers: Vec<FeeTier>) {
    *FEE_TIERS.write().unwrap_or_else(|p| p.into_inner()) = Some(tiers);
}

/// `(protocol_bps, creator_bps)` of the tier for `market_cap` (lamports).
fn tier_fees(market_cap: u128) -> (u64, u64) {
    let guard = FEE_TIERS.read().unwrap_or_else(|p| p.into_inner());
    let t = match guard.as_deref() {
        Some(tiers) if !tiers.is_empty() => *tiers.iter().rev().find(|t| market_cap >= t.market_cap_lamports).unwrap_or(&tiers[0]),
        _ => DEFAULT_TIER,
    };
    (t.protocol_bps as u64, t.creator_bps as u64)
}

/// The `BondingCurve` account fields a quote needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct PumpCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// `quote_mint` is set: the curve trades against a token, not SOL.
    pub non_sol_quote: bool,
    /// `creator` is set (no creator = no creator fee).
    pub has_creator: bool,
    /// The curve's own creator fee (bps); 0 = the fee config's.
    pub creator_fee_bps: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpQuote {
    pub amount_out: u64,
    /// protocol + creator fee, lamports
    pub fee: u64,
}

#[inline]
fn ceil_bps(amount: u64, bps: u64) -> u64 {
    ((amount as u128 * bps as u128).div_ceil(BPS)) as u64
}

impl PumpCurve {
    /// Layout (verified on mainnet, 125/151-byte accounts): disc(8),
    /// virtual_token u64@8, virtual_sol@16, real_token@24, real_sol@32,
    /// token_total_supply@40, complete u8@48, creator@49, is_mayhem_mode@81,
    /// is_cashback_coin@82, quote_mint@83, creator_fee_bps u64@115. Older,
    /// shorter accounts simply lack the later fields.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 49 {
            return None;
        }
        let u64_at = |o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
        let flag = |o: usize| d.get(o).is_some_and(|b| *b != 0);
        let nonzero = |o: usize| d.get(o..o + 32).is_some_and(|b| b.iter().any(|x| *x != 0));
        Some(Self {
            virtual_token_reserves: u64_at(8),
            virtual_sol_reserves: u64_at(16),
            real_token_reserves: u64_at(24),
            real_sol_reserves: u64_at(32),
            token_total_supply: u64_at(40),
            complete: flag(48),
            is_mayhem_mode: flag(81),
            is_cashback_coin: flag(82),
            non_sol_quote: nonzero(83),
            has_creator: nonzero(49),
            creator_fee_bps: u64_at(115),
        })
    }

    pub fn quotable(&self) -> bool {
        !self.complete && !self.non_sol_quote && !self.is_cashback_coin && self.virtual_token_reserves > 0 && self.virtual_sol_reserves > 0
    }

    /// Market cap (lamports) the fee program keys its tiers on.
    pub fn market_cap(&self) -> u128 {
        if self.virtual_token_reserves == 0 {
            return 0;
        }
        self.virtual_sol_reserves as u128 * self.token_total_supply as u128 / self.virtual_token_reserves as u128
    }

    /// `(protocol_bps, creator_bps)` charged on a trade right now.
    pub fn fee_bps(&self) -> (u64, u64) {
        let (protocol, tier_creator) = tier_fees(self.market_cap());
        let creator = if !self.has_creator { 0 } else if self.creator_fee_bps > 0 { self.creator_fee_bps } else { tier_creator };
        (protocol, creator)
    }

    fn fees(&self, sol: u64) -> u64 {
        let (p, c) = self.fee_bps();
        ceil_bps(sol, p) + ceil_bps(sol, c)
    }

    /// SOL in → tokens out (`buy_exact_sol_in`).
    pub fn buy_exact_in(&self, sol_in: u64) -> Option<PumpQuote> {
        if !self.quotable() || sol_in == 0 {
            return None;
        }
        let (p, c) = self.fee_bps();
        let mut x = u64::try_from(sol_in as u128 * BPS / (BPS + p as u128 + c as u128)).ok()?;
        // each rounded-up fee adds at most one lamport over the exact share
        while x > 0 && x.checked_add(ceil_bps(x, p) + ceil_bps(x, c))? > sol_in {
            x -= 1;
        }
        if x <= 1 {
            return None;
        }
        let priced = (x - 1) as u128;
        let tokens = priced * self.virtual_token_reserves as u128 / (self.virtual_sol_reserves as u128 + priced);
        let tokens = u64::try_from(tokens).ok()?;
        if tokens == 0 || tokens >= self.real_token_reserves {
            return None;
        }
        Some(PumpQuote { amount_out: tokens, fee: ceil_bps(x, p) + ceil_bps(x, c) })
    }

    /// Tokens in → SOL out, net of fees (`sell`).
    pub fn sell_exact_in(&self, tokens_in: u64) -> Option<PumpQuote> {
        if !self.quotable() || tokens_in == 0 {
            return None;
        }
        let gross = tokens_in as u128 * self.virtual_sol_reserves as u128 / (self.virtual_token_reserves as u128 + tokens_in as u128);
        let gross = u64::try_from(gross).ok()?;
        if gross == 0 || gross > self.real_sol_reserves {
            return None;
        }
        let fee = self.fees(gross);
        Some(PumpQuote { amount_out: gross.checked_sub(fee).filter(|o| *o > 0)?, fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Curve as it stood before a live trade: pre-trade reserves are the
    /// event's post-trade reserves with the trade taken back out.
    fn curve(vs: u64, vt: u64, rs: u64, rt: u64) -> PumpCurve {
        PumpCurve {
            virtual_sol_reserves: vs,
            virtual_token_reserves: vt,
            real_sol_reserves: rs,
            real_token_reserves: rt,
            token_total_supply: 1_000_000_000_000_000,
            has_creator: true,
            ..Default::default()
        }
    }

    #[test]
    fn buy_exact_sol_in_reproduces_live_trade_events() {
        // FSP4kDr3…: buy_exact_sol_in(100_000_000) on oitehREQ…pump
        let c = curve(41_556_085_197, 774_615_796_118_287, 11_556_085_197, 494_715_796_118_287);
        let q = c.buy_exact_in(100_000_000).unwrap();
        assert_eq!(q.amount_out, 1_836_647_138_012);
        assert_eq!(q.fee, 938_272 + 296_297);
        // KSqe4QAp…: 2 SOL on the same curve, later
        let c = curve(39_580_776_556, 813_273_584_264_209, 9_580_776_556, 533_373_584_264_209);
        let q = c.buy_exact_in(2_000_000_000).unwrap();
        assert_eq!(q.amount_out, 38_657_788_145_922);
        assert_eq!(q.fee, 18_765_433 + 5_925_926);
        // 26NoZ8Y4…: 0.00037 SOL on 8y6csx1d…pump
        let c = curve(31_060_677_684, 1_036_358_772_952_760, 1_060_677_684, 756_458_772_952_760);
        let q = c.buy_exact_in(372_493).unwrap();
        assert_eq!(q.amount_out, 12_274_832_703);
        assert_eq!(q.fee, 3_495 + 1_104);
    }

    #[test]
    fn the_exact_in_fee_base_is_the_largest_that_fits() {
        // 4qbaKxCN… (buy_exact_quote_in_v2 on a SOL curve, same math): x lands
        // on ⌊s·1e4/10125⌋; 4QdzAsCm…: one lamport below it, because the two
        // rounded-up fees would overshoot the budget by one.
        let c = curve(81_705_247_539, 393_977_147_532_679, 51_705_247_539, 114_077_147_532_679);
        let q = c.buy_exact_in(49_500_000).unwrap();
        assert_eq!((q.amount_out, q.fee), (235_597_916_978, 464_445 + 146_667));
        let c = curve(78_510_261_744, 410_010_107_278_663, 48_510_261_744, 130_110_107_278_663);
        let q = c.buy_exact_in(840_468_914).unwrap();
        assert_eq!((q.amount_out, q.fee), (4_289_701_255_568, 7_885_882 + 2_490_279));
    }

    #[test]
    fn sell_reproduces_live_trade_events() {
        // 4h2FG4zq…: sell 1_115_803_456_525 of EWo32eU3…; the user's balance
        // moved by gross − fees
        let c = curve(41_741_878_254, 771_167_984_359_362, 11_741_878_254, 491_267_984_359_362);
        let q = c.sell_exact_in(1_115_803_456_525).unwrap();
        assert_eq!(q.fee, 572_937 + 180_928);
        assert_eq!(q.amount_out, 60_309_089 - 572_937 - 180_928);
        // 2EnBxARd…: the seller's lamports rose by exactly 88_422
        let c = curve(41_556_085_198, 774_615_796_118_287, 11_556_085_198, 494_715_796_118_287);
        assert_eq!(c.sell_exact_in(1_669_106_311).unwrap().amount_out, 88_422);
    }

    #[test]
    fn parses_a_live_bonding_curve_account() {
        // FzX16pn7… (oitehREQ…pump), 151 bytes
        let b64 = "F7f4N2DYrGBQbTxC488DAGq2I/wGAAAAUNUp9lHRAgBqCgAAAAAAAACAxqR+jQMAAFXdT/Jk5k59S89129z+u71orMylozVNR0k24alChUo7AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let d = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let c = PumpCurve::parse(&d).unwrap();
        assert_eq!(c.virtual_token_reserves, 1_072_999_905_914_192);
        assert_eq!(c.virtual_sol_reserves, 30_000_002_666);
        assert_eq!(c.real_token_reserves, 793_099_905_914_192);
        assert_eq!(c.real_sol_reserves, 2_666);
        assert_eq!(c.token_total_supply, 1_000_000_000_000_000);
        assert!(!c.complete && c.has_creator && !c.is_mayhem_mode && !c.is_cashback_coin && !c.non_sol_quote);
        assert_eq!(c.creator_fee_bps, 0);
        assert!(c.quotable());
        assert_eq!(c.fee_bps(), (95, 30));
    }

    #[test]
    fn parses_the_live_fee_config() {
        let b64 = "jzSSu9t7TJv907uMqzQc4FKEV/LDgX0yeEQZY9zVX+1YuiTJmd2sAqoAAAAAAAAAAF8AAAAAAAAAHgAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAXwAAAAAAAAAeAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABfAAAAAAAAAB4AAAAAAAAAAAAAAAAAAABfAAAAAAAAAB4AAAAAAAAA";
        let d = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let tiers = crate::execution::amms::pumpfun_amm::parse_fee_config(&d).unwrap();
        assert_eq!(tiers, vec![DEFAULT_TIER]);
    }

    #[test]
    fn refuses_completed_foreign_quote_and_cashback_curves_and_curve_draining_buys() {
        let live = curve(41_556_085_197, 774_615_796_118_287, 11_556_085_197, 494_715_796_118_287);
        assert!(live.buy_exact_in(1_000_000).is_some());
        for f in [
            |c: &mut PumpCurve| c.complete = true,
            |c: &mut PumpCurve| c.non_sol_quote = true,
            |c: &mut PumpCurve| c.is_cashback_coin = true,
        ] {
            let mut c = live;
            f(&mut c);
            assert!(c.buy_exact_in(1_000_000).is_none() && c.sell_exact_in(1_000_000_000).is_none());
        }
        // the whole remaining real supply cannot be bought exact-in
        assert!(live.buy_exact_in(200_000_000_000).is_none());
        // selling more than the real SOL can pay out
        let mut thin = live;
        thin.real_sol_reserves = 1_000;
        assert!(thin.sell_exact_in(1_000_000_000_000).is_none());
    }

    #[test]
    fn creator_fee_follows_the_curve_override_and_the_creator() {
        let mut c = curve(41_556_085_197, 774_615_796_118_287, 11_556_085_197, 494_715_796_118_287);
        c.creator_fee_bps = 200;
        assert_eq!(c.fee_bps(), (95, 200));
        c.has_creator = false;
        assert_eq!(c.fee_bps(), (95, 0));
    }
}
