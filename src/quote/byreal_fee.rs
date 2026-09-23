//! Byreal CLMM fee rate (program `REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2`,
//! source: github.com/byreal-git/byreal-clmm). Byreal is a Raydium CLMM fork
//! whose swap loop is Raydium's; only the fee RATE fed into it differs:
//!
//! - `pool.trade_fee_rate` (u32 @393), when non-zero, overrides the
//!   `AmmConfig` rate;
//! - a launch "decay fee" (flag bit 0, sell-side bits 1/2) raises the rate to
//!   `init% · (1 − decrease%)^⌊(now − open_time)/interval⌋` while that is
//!   higher;
//! - the swap-dynamic fee (flag bit 4, `swap_v3_dyn` only) adds, per swap, an
//!   arbitrage term (pool price vs the Pyth index price), a trade-size term and
//!   an inventory-imbalance term, all computed ONCE from the state before the
//!   swap; the tick walk then runs with `base + dynamic`.
//!
//! The deployed program charges the arbitrage term as above (e.g. 608 ppm on
//! SOL/USDC). It does not charge the published source's trade-size term:
//! swaps of 20 001–55 000 USDC on the USDC/USDT pool (where the source charges
//! 0.5 %–60 %) and 2 001–2 100 USDC on SOL/USDC deliver exactly the
//! base+arbitrage output, so that term is left out. A swap the imbalance term
//! would apply to is not quoted.
//!
//! The dynamic term needs both vault balances and both Pyth prices. They are
//! read in the same `getMultipleAccounts` as the pool's tick arrays
//! (`pool::ticks`), so they are exactly as fresh as the ticks the quote walks.

use std::sync::LazyLock;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use super::clmm::{mul_div_ceil, mul_div_floor};

/// Denominator of every Byreal fee rate (hundredths of a basis point).
const FEE_RATE_DENOMINATOR: u64 = 1_000_000;
const Q64: u128 = 1 << 64;
/// Pyth prices older than this make `swap_v3_dyn` fail (`PythPriceStale`).
const MAX_PYTH_AGE_SECONDS: i64 = 3_600;

/// Pyth receiver program (owner of `PriceUpdateV2` accounts).
pub const PYTH_RECEIVER_PROG_ID: Pubkey = solana_sdk::pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
/// Pyth push-oracle program: sponsored price feed account = PDA([shard u16 LE, feed_id]).
pub const PYTH_PUSH_ORACLE_PROG_ID: Pubkey = solana_sdk::pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");

/// Pool-level fee fields of a Byreal `PoolState` (1544-byte account).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByrealFee {
    /// `trade_fee_rate` u32 @393: overrides the AmmConfig rate when non-zero.
    pub pool_trade_fee_rate: u32,
    /// `decay_fee_flag` u8 @1096: bit0 decay on, bit1/bit2 decay on selling
    /// token 0 / token 1, bit3 token 1 is the quote token, bit4 dynamic fee on.
    pub flags: u8,
    /// `decay_fee_init_fee_rate` (percent), `decay_fee_decrease_rate`
    /// (percent per interval), `decay_fee_decrease_interval` (seconds) @1097..1100.
    pub decay_init_rate: u8,
    pub decay_decrease_rate: u8,
    pub decay_interval: u8,
    /// `open_time` u64 @1080: swaps need `now > open_time`.
    pub open_time: u64,
    /// `status` u8 @389: bit4 set = swaps disabled.
    pub status: u8,
    /// Byte @390: padding in Byreal's source, `fee_on` in Raydium CLMM's
    /// August 2026 layout (0 = fee from the input). Zero on every Byreal pool;
    /// anything else is a layout this module does not price.
    pub fee_on: u8,
    /// Dynamic-fee parameters @1100..1106: arbitrage buffer (ppm), trade-size
    /// fee base (1/1000) and threshold (×100 quote units), imbalance fee base
    /// (1/10) and threshold x (percent).
    pub arbitrage_fee_buffer_ppm: u16,
    pub slippage_fee_base: u8,
    pub slippage_fee_threshold: u8,
    pub imbalance_fee_base: u8,
    pub imbalance_fee_x: u8,
    /// `mint_decimals_0/1` @233/@234.
    pub decimals_0: u8,
    pub decimals_1: u8,
    /// Pyth feed ids @1112/@1144 and their sponsored price accounts (shard 0).
    pub feed_id_0: [u8; 32],
    pub feed_id_1: [u8; 32],
    pub oracle_0: Pubkey,
    pub oracle_1: Pubkey,
}

/// One Pyth price as `swap_v3_dyn` reads it (`PriceUpdateV2.price_message`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythPrice {
    pub price: i64,
    pub exponent: i32,
    pub publish_time: i64,
}

/// What the dynamic fee reads besides the pool: both vault balances and both prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynInputs {
    pub vault_0: u64,
    pub vault_1: u64,
    pub price_0: PythPrice,
    pub price_1: PythPrice,
}

/// pool → dynamic-fee inputs, published with the pool's tick arrays.
pub static DYN_INPUTS: LazyLock<DashMap<Pubkey, DynInputs>> = LazyLock::new(DashMap::new);

/// Pyth price account → the dynamic-fee pools that read it. A block that
/// writes the account (a Pyth price push) re-reads those pools
/// (`stream::block_refresh`): the arbitrage term moves with the oracle even
/// when the pool itself does not trade.
static ORACLE_POOLS: LazyLock<DashMap<Pubkey, Vec<Pubkey>>> = LazyLock::new(DashMap::new);

/// Dynamic-fee pools whose fee reads `account` as an oracle.
pub fn pools_reading_oracle(account: &Pubkey) -> Option<Vec<Pubkey>> {
    ORACLE_POOLS.get(account).map(|v| v.clone())
}

impl ByrealFee {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 1176 {
            return None;
        }
        let mut f = Self {
            pool_trade_fee_rate: u32::from_le_bytes(d[393..397].try_into().ok()?),
            flags: d[1096],
            decay_init_rate: d[1097],
            decay_decrease_rate: d[1098],
            decay_interval: d[1099],
            open_time: u64::from_le_bytes(d[1080..1088].try_into().ok()?),
            status: d[389],
            fee_on: d[390],
            arbitrage_fee_buffer_ppm: u16::from_le_bytes(d[1100..1102].try_into().ok()?),
            slippage_fee_base: d[1102],
            slippage_fee_threshold: d[1103],
            imbalance_fee_base: d[1104],
            imbalance_fee_x: d[1105],
            decimals_0: d[233],
            decimals_1: d[234],
            feed_id_0: d[1112..1144].try_into().ok()?,
            feed_id_1: d[1144..1176].try_into().ok()?,
            oracle_0: Pubkey::default(),
            oracle_1: Pubkey::default(),
        };
        if f.is_dynamic() {
            f.oracle_0 = sponsored_price_account(&f.feed_id_0);
            f.oracle_1 = sponsored_price_account(&f.feed_id_1);
        }
        Some(f)
    }

    /// The pool charges the oracle-driven dynamic fee (`swap_v3_dyn` only).
    pub fn is_dynamic(&self) -> bool {
        self.flags & 0b1_0000 != 0
    }

    fn token1_is_quote(&self) -> bool {
        self.flags & 0b1000 != 0
    }

    fn decay_enabled(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Whether a swap can execute at `now` (unix seconds) and be priced here.
    pub fn can_swap(&self, now: u64) -> bool {
        self.status & (1 << 4) == 0 && now > self.open_time && self.fee_on == 0
    }

    /// `PoolState::calculate_base_trade_fee_rate`: the effective trade fee
    /// (pool override, else AmmConfig) raised to the decay fee while that is
    /// higher. For a dynamic-fee pool this is the `fee_base` the dynamic
    /// terms are added to.
    pub fn base_fee_rate(&self, config_trade_fee_rate: u32, zero_for_one: bool, now: u64) -> Option<u32> {
        let base = if self.pool_trade_fee_rate != 0 { self.pool_trade_fee_rate } else { config_trade_fee_rate };
        let decay_side = (zero_for_one && self.flags & 0b10 != 0) || (!zero_for_one && self.flags & 0b100 != 0);
        let decay = if self.decay_enabled() && decay_side { self.decay_fee_rate(now)? } else { 0 };
        Some(base.max(decay))
    }

    /// `PoolState::calculate_decay_fee_rate_all_side`:
    /// `init% · (1 − decrease%)^n`, n = elapsed intervals, ceil at every step.
    fn decay_fee_rate(&self, now: u64) -> Option<u32> {
        if now < self.open_time {
            return Some(0);
        }
        if self.decay_interval == 0 {
            return None; // the program divides by it
        }
        let intervals = (now - self.open_time) / self.decay_interval as u64;
        let mul_div_ceil = |a: u64, b: u64, d: u64| -> u64 { ((a as u128 * b as u128).div_ceil(d as u128)) as u64 };
        let mut rate = FEE_RATE_DENOMINATOR;
        let mut base = FEE_RATE_DENOMINATOR.checked_sub(self.decay_decrease_rate as u64 * 10_000)?;
        let mut exp = intervals;
        while exp > 0 {
            if exp % 2 == 1 {
                rate = mul_div_ceil(rate, base, FEE_RATE_DENOMINATOR);
            }
            base = mul_div_ceil(base, base, FEE_RATE_DENOMINATOR);
            exp /= 2;
        }
        u32::try_from(mul_div_ceil(rate, self.decay_init_rate as u64, 100)).ok()
    }

    /// `swap_v3_dyn`'s total fee rate for an exact-in swap of `amount`:
    /// `fee_base` + the arbitrage term (`PoolState::calculate_dynamic_fee_rate`,
    /// `libraries::dynamic_fee_math`; see the module doc for the trade-size and
    /// imbalance terms). `None` where the program fails (stale or non-positive
    /// price, overflow, total above 100 %) or the unverified imbalance term applies.
    pub fn dynamic_fee_rate(&self, fee_base: u32, sqrt_price_x64: u128, inp: &DynInputs, zero_for_one: bool, amount: u64, now: i64) -> Option<u32> {
        for p in [&inp.price_0, &inp.price_1] {
            if p.price <= 0 || p.publish_time < now.checked_sub(MAX_PYTH_AGE_SECONDS)? {
                return None;
            }
        }
        let p_index = price_index(&inp.price_0, &inp.price_1, self.decimals_0, self.decimals_1)?;
        let p_0 = to_u128_opt(super::clmm::U256::from(sqrt_price_x64) * super::clmm::U256::from(sqrt_price_x64) >> 64)?;
        let token1_quote = self.token1_is_quote();
        let input_is_quote = if token1_quote { !zero_for_one } else { zero_for_one };
        // the program derives the trade size from the amount even though the
        // deployed build charges no size term; its conversion can still fail
        let quote_amount = if input_is_quote { amount as u128 } else { quote_from_base(amount as u128, p_0, token1_quote)? };
        let quote_decimals = if token1_quote { self.decimals_1 } else { self.decimals_0 };
        u64::try_from(quote_amount / 10u128.checked_pow(quote_decimals as u32)?).ok()?;
        let (v0, v1) = (inp.vault_0 as u128, inp.vault_1 as u128);
        let (quote_value_of_base, quote_balance) = if token1_quote { (quote_from_base(v0, p_0, true)?, v1) } else { (quote_from_base(v1, p_0, false)?, v0) };

        let arbitrage = arbitrage_fee(p_0, p_index, self.arbitrage_fee_buffer_ppm, fee_base)?;
        if imbalance_fee(quote_value_of_base, quote_balance, self.imbalance_fee_base, self.imbalance_fee_x, input_is_quote)? != 0 {
            return None;
        }
        let total = arbitrage as u64 + fee_base as u64;
        if total > FEE_RATE_DENOMINATOR {
            return None;
        }
        Some(total as u32)
    }
}

fn to_u128_opt(v: super::clmm::U256) -> Option<u128> {
    if v.bits() > 128 { None } else { Some(v.low_u128()) }
}

/// `pyth::calculate_price_index`: token-0 price in token-1 atoms, Q64.64.
fn price_index(p0: &PythPrice, p1: &PythPrice, decimals_0: u8, decimals_1: u8) -> Option<u128> {
    let net_exp = p0.exponent.checked_sub(p1.exponent)?.checked_add(decimals_1 as i32 - decimals_0 as i32)?;
    let (a, b) = (p0.price as u128, p1.price as u128);
    let (num, den) = if net_exp >= 0 {
        (a.checked_mul(10u128.checked_pow(net_exp as u32)?)?, b)
    } else {
        (a, b.checked_mul(10u128.checked_pow(net_exp.unsigned_abs())?)?)
    };
    mul_div_floor(num, Q64, den)
}

/// `quote_amount_from_base`: floor(base · price) or floor(base / price), Q64.64 price.
fn quote_from_base(base: u128, price_x64: u128, quote_is_token1: bool) -> Option<u128> {
    if price_x64 == 0 {
        return None;
    }
    if quote_is_token1 { mul_div_floor(base, price_x64, Q64) } else { mul_div_floor(base, Q64, price_x64) }
}

fn arbitrage_fee(p_0: u128, p_index: u128, buffer_ppm: u16, fee_base: u32) -> Option<u32> {
    if p_index == 0 {
        return None;
    }
    let diff = p_0.abs_diff(p_index);
    let diff_ppm = mul_div_ceil(diff, FEE_RATE_DENOMINATOR as u128, p_index)?;
    let free = buffer_ppm as u128 + fee_base as u128;
    if diff_ppm <= free {
        return Some(0);
    }
    u32::try_from(diff_ppm - free).ok()
}

fn imbalance_fee(quote_value_of_base: u128, quote_balance: u128, base: u8, x: u8, is_buying_base: bool) -> Option<u32> {
    let total = quote_value_of_base.checked_add(quote_balance)?;
    if total == 0 {
        return Some(0);
    }
    // only a swap that deepens the imbalance pays
    match quote_value_of_base.cmp(&quote_balance) {
        std::cmp::Ordering::Greater if is_buying_base => return Some(0),
        std::cmp::Ordering::Less if !is_buying_base => return Some(0),
        std::cmp::Ordering::Equal => return Some(0),
        _ => {}
    }
    let diff = quote_value_of_base.abs_diff(quote_balance);
    let imbalance_ppm = mul_div_ceil(diff, FEE_RATE_DENOMINATOR as u128, total)?;
    let x_ppm = x as u128 * 10_000;
    if imbalance_ppm <= x_ppm {
        return Some(0);
    }
    u32::try_from(mul_div_ceil(imbalance_ppm - x_ppm, base as u128, 10)?).ok()
}

/// Sponsored Pyth price-feed account (push oracle, shard 0) for `feed_id`.
pub fn sponsored_price_account(feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&0u16.to_le_bytes(), feed_id], &PYTH_PUSH_ORACLE_PROG_ID).0
}

/// `PriceUpdateV2` → price, if the account is a receiver-owned update for `feed_id`.
/// Layout: disc(8) write_authority(32) verification_level(enum: Partial{u8} = 2
/// bytes, Full = 1 byte) then feed_id(32) price i64, conf u64, exponent i32,
/// publish_time i64, …
pub fn parse_price_update(acct: &Account, feed_id: &[u8; 32]) -> Option<PythPrice> {
    if acct.owner != PYTH_RECEIVER_PROG_ID {
        return None;
    }
    let d = &acct.data;
    let o = match *d.get(40)? {
        0 => 42,
        1 => 41,
        _ => return None,
    };
    if d.get(o..o + 32)? != feed_id {
        return None;
    }
    let rd = |at: usize, n: usize| d.get(o + at..o + at + n);
    Some(PythPrice {
        price: i64::from_le_bytes(rd(32, 8)?.try_into().ok()?),
        exponent: i32::from_le_bytes(rd(48, 4)?.try_into().ok()?),
        publish_time: i64::from_le_bytes(rd(52, 8)?.try_into().ok()?),
    })
}

/// Token account amount (SPL Token and Token-2022 share the base layout).
fn token_amount(acct: &Account) -> Option<u64> {
    Some(u64::from_le_bytes(acct.data.get(64..72)?.try_into().ok()?))
}

/// Accounts the dynamic fee reads, for a dynamic-fee Byreal pool:
/// `[token_vault_0, token_vault_1, oracle_0, oracle_1]`.
pub fn dyn_input_keys(state: &crate::pool::types::PoolState) -> Option<[Pubkey; 4]> {
    match state {
        crate::pool::types::PoolState::Byreal { token_vault_a, token_vault_b, fee, .. } if fee.is_dynamic() => {
            Some([*token_vault_a, *token_vault_b, fee.oracle_0, fee.oracle_1])
        }
        _ => None,
    }
}

/// Parse the accounts fetched for `dyn_input_keys` (same order) and publish them.
pub fn publish_dyn_inputs(state: &crate::pool::types::PoolState, accounts: &[Option<Account>]) -> bool {
    let crate::pool::types::PoolState::Byreal { pool, fee, .. } = state else { return false };
    let parsed = (|| {
        Some(DynInputs {
            vault_0: token_amount(accounts.first()?.as_ref()?)?,
            vault_1: token_amount(accounts.get(1)?.as_ref()?)?,
            price_0: parse_price_update(accounts.get(2)?.as_ref()?, &fee.feed_id_0)?,
            price_1: parse_price_update(accounts.get(3)?.as_ref()?, &fee.feed_id_1)?,
        })
    })();
    for oracle in [fee.oracle_0, fee.oracle_1] {
        let mut pools = ORACLE_POOLS.entry(oracle).or_default();
        if !pools.contains(pool) {
            pools.push(*pool);
        }
    }
    match parsed {
        Some(inp) => {
            DYN_INPUTS.insert(*pool, inp);
            true
        }
        None => {
            DYN_INPUTS.remove(pool);
            false
        }
    }
}

/// The fee rate a swap on this pool pays, beyond what `extract_clmm_params` knows.
pub enum SwapFee {
    /// Not a dynamic-fee pool: the pool's base rate applies.
    Base,
    /// `swap_v3_dyn` total rate for this swap.
    Rate(u32),
    /// Dynamic-fee pool whose vault / oracle inputs are missing or unusable.
    Unavailable,
}

/// Hot path: the rate for an exact-in swap of `amount`, given the base rate
/// already derived from the pool (`fee_base`).
pub fn swap_fee_ppm(state: &crate::pool::types::PoolState, pool: &Pubkey, zero_for_one: bool, amount: u64, fee_base: u32) -> SwapFee {
    let crate::pool::types::PoolState::Byreal { fee, sqrt_price_x64, .. } = state else { return SwapFee::Base };
    if !fee.is_dynamic() {
        return SwapFee::Base;
    }
    let Some(inp) = DYN_INPUTS.get(pool).map(|r| *r) else { return SwapFee::Unavailable };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    match fee.dynamic_fee_rate(fee_base, *sqrt_price_x64, &inp, zero_for_one, amount, now) {
        Some(r) => SwapFee::Rate(r),
        None => SwapFee::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fee() -> ByrealFee {
        ByrealFee::default()
    }

    #[test]
    fn pool_rate_overrides_the_config_rate() {
        assert_eq!(fee().base_fee_rate(400, true, 10), Some(400));
        // mainnet pool 27x6aSxc…: trade_fee_rate 80, only the quote flag (8) set
        let f = ByrealFee { pool_trade_fee_rate: 80, flags: 8, ..fee() };
        assert_eq!(f.base_fee_rate(2_000, false, 10), Some(80));
        assert!(!f.is_dynamic());
    }

    #[test]
    fn decay_fee_follows_the_program_power_and_turns_off_below_base() {
        // init 2 %, −1 % every 255 s, on selling token 0 only (flags 0b011)
        let f = ByrealFee { flags: 0b011, decay_init_rate: 2, decay_decrease_rate: 1, decay_interval: 255, open_time: 1_000, ..fee() };
        assert_eq!(f.base_fee_rate(2_500, true, 1_000), Some(20_000));
        // one interval: 1e6·0.99 = 990_000 → ·2/100 = 19_800
        assert_eq!(f.base_fee_rate(2_500, true, 1_255), Some(19_800));
        // two intervals: base² = 980_100, rate·base² /1e6 = 980_100 → 19_602
        assert_eq!(f.base_fee_rate(2_500, true, 1_510), Some(19_602));
        // the other side pays the base rate
        assert_eq!(f.base_fee_rate(2_500, false, 1_255), Some(2_500));
        // long after launch the decay is below the base rate
        assert_eq!(f.base_fee_rate(2_500, true, 1_000 + 255 * 1_000), Some(2_500));
        // flags 6 without bit 0: decay already switched off on chain (mainnet pool A5vkCw1V…)
        let off = ByrealFee { flags: 6, decay_init_rate: 2, decay_decrease_rate: 1, decay_interval: 255, ..fee() };
        assert_eq!(off.base_fee_rate(2_500, true, 5), Some(2_500));
    }

    #[test]
    fn swaps_need_open_time_passed_and_the_swap_bit_clear() {
        let f = ByrealFee { open_time: 100, ..fee() };
        assert!(!f.can_swap(100));
        assert!(f.can_swap(101));
        let disabled = ByrealFee { status: 1 << 4, ..fee() };
        assert!(!disabled.can_swap(u64::MAX));
        let fee_on_output = ByrealFee { fee_on: 1, ..fee() };
        assert!(!fee_on_output.can_swap(u64::MAX), "fee taken from the output is not priced");
    }

    // The program's own unit tests (libraries/dynamic_fee_math.rs, util/pyth.rs).
    #[test]
    fn dynamic_fee_terms_match_the_program_tests() {
        let q = Q64;
        assert_eq!(arbitrage_fee(q * 101 / 100, q, 0, 1_000), Some(9_000));
        assert_eq!(arbitrage_fee(q * 101 / 100, q, 9_000, 1_000), Some(0));
        assert_eq!(arbitrage_fee(q * 99 / 100, q, 0, 1_000), Some(9_001));
        assert_eq!(arbitrage_fee(q * 99 / 100, q, 9_000, 1_000), Some(1));
        assert_eq!(arbitrage_fee(q * 105 / 100, q, 5_000, 1_000), Some(44_000));
        assert_eq!(imbalance_fee(150, 50, 5, 10, false), Some(200_000));
        assert_eq!(imbalance_fee(150, 50, 5, 10, true), Some(0));
        assert_eq!(imbalance_fee(100, 100, 5, 10, false), Some(0));
        assert_eq!(imbalance_fee(50, 150, 5, 10, true), Some(200_000));
        let p = |price, exponent| PythPrice { price, exponent, publish_time: 0 };
        assert_eq!(price_index(&p(10_000_000_000, -8), &p(100_000_000, -8), 0, 0), Some(100 * q));
        assert_eq!(price_index(&p(10_000, -2), &p(100_000_000, -8), 0, 0), Some(100 * q));
        assert_eq!(price_index(&p(100_000_000, -8), &p(10_000, -2), 0, 0), Some(q / 100));
        assert_eq!(price_index(&p(10_000_000_000, -8), &p(100_000_000, -8), 9, 6), Some(q / 10));
        assert_eq!(price_index(&p(100_000_000, -8), &p(100_000_000, -8), 6, 9), Some(1000 * q));
    }

    #[test]
    fn dynamic_rate_adds_the_terms_to_the_base_rate() {
        // SOL/USDC-like pool: token 1 (USDC) is the quote, pool price == index
        let f = ByrealFee { flags: 0b1_1000, arbitrage_fee_buffer_ppm: 100, slippage_fee_base: 10, slippage_fee_threshold: 20, imbalance_fee_base: 40, imbalance_fee_x: 50, decimals_0: 9, decimals_1: 6, ..fee() };
        let px = |price| PythPrice { price, exponent: -8, publish_time: 1_000 };
        // 150 USD/SOL → 0.15 USDC atom per lamport; sqrt(0.15)·2^64
        let sqrt = (0.15f64.sqrt() * 18_446_744_073_709_551_616.0) as u128;
        let p_0 = ((sqrt * sqrt) >> 64) as f64 / Q64 as f64;
        let inp = DynInputs { vault_0: 1_000_000_000_000, vault_1: 150_000_000_000, price_0: px((p_0 * 1e3 * 1e8) as i64), price_1: px(100_000_000) };
        // small buy of SOL with USDC: only the base rate (1 ppm)
        assert_eq!(f.dynamic_fee_rate(1, sqrt, &inp, false, 1_000_000, 1_000), Some(1));
        // 2 001 USDC, over the published trade-size threshold of 2 000: the
        // deployed program charges no size term (mainnet swaps of 2 001 and 2 100 USDC)
        assert_eq!(f.dynamic_fee_rate(1, sqrt, &inp, false, 2_001_000_000, 1_000), Some(1));
        // a pool 90 % in SOL value: buying SOL is fine, selling more SOL would
        // pay the (unverified) imbalance term → not quoted
        let heavy = DynInputs { vault_0: 9_000_000_000_000, vault_1: 150_000_000_000, ..inp };
        assert_eq!(f.dynamic_fee_rate(1, sqrt, &heavy, false, 1_000_000, 1_000), Some(1));
        assert_eq!(f.dynamic_fee_rate(1, sqrt, &heavy, true, 1_000_000, 1_000), None);
        // index 1 % above the pool price: ⌈0.01/1.01·1e6⌉ − 100 − 1 = 9_800, + base 1
        let high = DynInputs { price_0: px((p_0 * 1.01 * 1e3 * 1e8) as i64), ..inp };
        let r = f.dynamic_fee_rate(1, sqrt, &high, false, 1_000_000, 1_000).unwrap();
        assert!((9_799..=9_802).contains(&r), "{r}");
        // a price older than an hour fails the swap
        assert_eq!(f.dynamic_fee_rate(1, sqrt, &inp, false, 1_000_000, 1_000 + 3_601), None);
    }

    #[test]
    fn sponsored_feed_accounts_are_the_push_oracle_pdas() {
        // USDC/USD and USD1/USD feeds as used by mainnet swap_v3_dyn transactions
        let usdc = hex32("eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a");
        assert_eq!(sponsored_price_account(&usdc), solana_sdk::pubkey!("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX"));
    }

    fn hex32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }
}
