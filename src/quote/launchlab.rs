//! Raydium LaunchLab (`LanMV9…`) bonding-curve math, exact input.
//!
//! The pool prices on VIRTUAL + REAL reserves, not on vault balances:
//! `quote_reserve = virtual_quote + real_quote`, `base_reserve = virtual_base −
//! real_base`, constant product between them. Fees (protocol rate from the
//! GlobalConfig, platform + creator rates from the PlatformConfig, all /1e6)
//! come off the QUOTE side: before the curve on buys, after it on sells. The
//! program rounds the fee ONCE on the summed rate (`ceil(amount · Σrate / 1e6)`)
//! and then splits it; rounding each part up separately over-charges by an
//! atom about a quarter of the time. Matches the program's `TradeEvent` to the
//! atom (see the unit tests).
//!
//! Only `curve_type` 0 (constant product) and `status` 0 (funding) are quoted;
//! a buy that would cross `total_quote_fund_raising` is not (the program fills
//! it partially and migrates the pool).

pub const FEE_RATE_DENOMINATOR: u128 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LaunchLabCurve {
    /// 0 = funding (tradable), 1 = migrating, 2 = migrated
    pub status: u8,
    /// GlobalConfig.curve_type: 0 constant product, 1 fixed price, 2 linear
    pub curve_type: u8,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub real_base: u64,
    pub real_quote: u64,
    pub total_base_sell: u64,
    pub total_quote_fund_raising: u64,
    /// GlobalConfig.trade_fee_rate (/1e6)
    pub protocol_fee_rate: u64,
    /// PlatformConfig.fee_rate (/1e6)
    pub platform_fee_rate: u64,
    /// PlatformConfig.creator_fee_rate (/1e6)
    pub creator_fee_rate: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchLabQuote {
    pub amount_out: u64,
    /// protocol + platform + creator fee, in quote units
    pub fee: u64,
}

#[inline]
fn ceil_fee(amount: u64, rate: u64) -> Option<u64> {
    u64::try_from((amount as u128).checked_mul(rate as u128)?.div_ceil(FEE_RATE_DENOMINATOR)).ok()
}

impl LaunchLabCurve {
    /// Protocol + platform + creator fee on `quote_amount`, rounded up once.
    pub fn total_fee(&self, quote_amount: u64) -> Option<u64> {
        let rate = self.protocol_fee_rate.checked_add(self.platform_fee_rate)?.checked_add(self.creator_fee_rate)?;
        ceil_fee(quote_amount, rate)
    }

    pub fn quotable(&self) -> bool {
        self.status == 0 && self.curve_type == 0 && self.virtual_base > self.real_base
    }

    /// quote in → base out
    pub fn buy_exact_in(&self, quote_in: u64) -> Option<LaunchLabQuote> {
        if !self.quotable() || quote_in == 0 {
            return None;
        }
        let fee = self.total_fee(quote_in)?;
        let less = quote_in.checked_sub(fee)?;
        // a buy that crosses the raise cap is filled partially + migrates: not quoted
        if self.real_quote.checked_add(less)? > self.total_quote_fund_raising {
            return None;
        }
        let in_res = self.virtual_quote as u128 + self.real_quote as u128;
        let out_res = (self.virtual_base - self.real_base) as u128;
        let out = out_res.checked_mul(less as u128)? / (in_res.checked_add(less as u128)?);
        let out = u64::try_from(out).ok()?;
        if out == 0 || self.real_base.checked_add(out)? > self.total_base_sell {
            return None;
        }
        Some(LaunchLabQuote { amount_out: out, fee })
    }

    /// base in → quote out (net of fees)
    pub fn sell_exact_in(&self, base_in: u64) -> Option<LaunchLabQuote> {
        if !self.quotable() || base_in == 0 || base_in > self.real_base {
            return None;
        }
        let in_res = (self.virtual_base - self.real_base) as u128;
        let out_res = self.virtual_quote as u128 + self.real_quote as u128;
        let gross = out_res.checked_mul(base_in as u128)? / (in_res.checked_add(base_in as u128)?);
        let gross = u64::try_from(gross).ok()?;
        if gross == 0 || gross > self.real_quote {
            return None;
        }
        let fee = self.total_fee(gross)?;
        Some(LaunchLabQuote { amount_out: gross.checked_sub(fee)?, fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // mainnet TradeEvent, pool AWfnyEd7R8U8pyaXTgzvWEga4sTMtCkDtbPA4VH6xwGW
    fn live() -> LaunchLabCurve {
        LaunchLabCurve {
            status: 0,
            curve_type: 0,
            virtual_base: 1_073_025_605_880_029,
            virtual_quote: 206_755_467,
            real_base: 655_577_461_695_660,
            real_quote: 324_697_354,
            total_base_sell: 793_100_000_000_000,
            total_quote_fund_raising: 585_790_504,
            protocol_fee_rate: 2_500,
            platform_fee_rate: 10_000,
            creator_fee_rate: 0,
        }
    }

    #[test]
    fn buy_reproduces_the_mainnet_trade_event_exactly() {
        let q = live().buy_exact_in(567_784).unwrap();
        assert_eq!(q.fee, 1_420 + 5_678);
        assert_eq!(q.amount_out, 439_946_217_700);
    }

    #[test]
    fn sell_is_the_inverse_curve_with_fees_on_the_quote_out() {
        let mut c = live();
        // state after the buy above
        c.real_base += 439_946_217_700;
        c.real_quote += 560_686;
        let s = c.sell_exact_in(439_946_217_700).unwrap();
        // selling back what was bought returns (almost) the quote that entered the curve, minus fees on that
        assert!(s.amount_out + s.fee <= 560_686 && s.amount_out + s.fee >= 560_680, "{s:?}");
        assert_eq!(s.fee, ceil_fee(s.amount_out + s.fee, 12_500).unwrap());
    }

    /// Pool BwQVqYDH… (quote USD1 `A7bdiY…`, protocol 0.25 % + platform 1 %),
    /// live `TradeEvent`s; in the first three, rounding the protocol and
    /// platform parts up separately charges one atom more than the program.
    fn bwq(real_base: u64, real_quote: u64) -> LaunchLabCurve {
        LaunchLabCurve {
            status: 0,
            curve_type: 0,
            virtual_base: 1_073_025_606_009_485,
            virtual_quote: 222_467_296,
            real_base,
            real_quote,
            total_base_sell: 793_100_000_000_000,
            total_quote_fund_raising: 630_306_087,
            protocol_fee_rate: 2_500,
            platform_fee_rate: 10_000,
            creator_fee_rate: 0,
        }
    }

    #[test]
    fn fee_is_rounded_once_on_the_summed_rate() {
        // sell, slot 449545795: fee 2_077 + 8_305 = 10_382 (not 2_078 + 8_305)
        let s = bwq(229_595_110_116_818, 60_559_292).sell_exact_in(2_482_337_353_691).unwrap();
        assert_eq!((s.amount_out, s.fee), (820_161, 10_382));
        // sell, slot 449545465: 293 + 1_168
        let s = bwq(127_671_662_292_310, 30_044_745).sell_exact_in(437_501_818_968).unwrap();
        assert_eq!((s.amount_out, s.fee), (115_345, 1_461));
        // buy, slot 449545479: 18_397 + 73_585 off the 7_358_524 quote in
        let b = bwq(127_234_160_473_342, 29_927_939).buy_exact_in(7_358_524).unwrap();
        assert_eq!((b.amount_out, b.fee), (26_467_635_481_941, 91_982));
        // buy, slot 449545459
        let b = bwq(69_924_621_126_487, 15_508_000).buy_exact_in(14_720_755).unwrap();
        assert_eq!((b.amount_out, b.fee), (57_747_041_165_823, 184_010));
    }

    #[test]
    fn refuses_migrated_pools_other_curves_and_cap_crossing_buys() {
        let mut c = live();
        c.status = 2;
        assert!(c.buy_exact_in(1_000).is_none());
        let mut c = live();
        c.curve_type = 1;
        assert!(c.buy_exact_in(1_000).is_none());
        let c = live();
        let room = c.total_quote_fund_raising - c.real_quote;
        assert!(c.buy_exact_in(room * 2).is_none(), "would cross the raise cap");
        assert!(c.buy_exact_in(1_000_000).is_some());
    }
}
