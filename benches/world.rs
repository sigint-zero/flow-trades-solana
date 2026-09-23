//! Synthetic quote world shared by the latency test and the criterion bench:
//! NO network, registry + cache + tick data shaped like production.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use flow_trades::constants::{SOL_NATIVE_MINT, USDC_MINT, USDT_MINT};
use flow_trades::pool::cache::PoolCache;
use flow_trades::pool::registry::{PoolEntry, PoolRegistry};
use flow_trades::pool::types::{PoolState, PoolType};
use flow_trades::quote::clmm::{TickData, TICKS};
use flow_trades::quote::{QuoteRequest, Quoter};
use flow_trades::stream::account_mirror::AccountMirror;

pub fn pamm(base: Pubkey, quote: Pubkey, br: u64, qr: u64) -> PoolState {
    PoolState::PumpFunAmm {
        pool: Pubkey::new_unique(), base_mint: base, quote_mint: quote,
        pool_base_vault: Pubkey::new_unique(), pool_quote_vault: Pubkey::new_unique(),
        coin_creator: Pubkey::new_unique(), base_reserve: br, quote_reserve: qr,
        protocol_fee_recipient: Pubkey::new_unique(),
        buyback_accounts: vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), false), (Pubkey::new_unique(), true)],
        base_supply: 1_000_000_000_000_000,
        virtual_quote_reserve: 0,
    }
}

pub const ORCA_TICK: i32 = -17_000;
pub const ORCA_SPACING: i32 = 4;

/// sqrt price consistent with `ORCA_TICK` (a hair above its lower bound, as on chain).
pub fn orca_sqrt_price() -> u128 {
    flow_trades::quote::clmm::sqrt_price_x64_at_tick(ORCA_TICK) + 1_000_000
}

pub fn orca(a: Pubkey, b: Pubkey) -> PoolState {
    PoolState::Orca {
        whirlpool: Pubkey::new_unique(), token_vault_a: Pubkey::new_unique(), token_vault_b: Pubkey::new_unique(),
        oracle: Pubkey::new_unique(), token_mint_a: a, token_mint_b: b, tick_current: ORCA_TICK, tick_spacing: ORCA_SPACING,
        sqrt_price_x64: orca_sqrt_price(), liquidity: 900_000_000_000_000, fee_rate: 400,
    }
}

/// Tick data like a busy whirlpool: `n_ticks` initialised ticks on each side
/// of the current price, alternating ± liquidity so walks cross real changes.
pub fn ticks_around(n_ticks: usize) -> TickData {
    let mut ticks = Vec::with_capacity(2 * n_ticks);
    for i in 1..=n_ticks as i32 {
        let net = if i % 2 == 0 { 50_000_000_000_000i128 } else { -40_000_000_000_000i128 };
        ticks.push((ORCA_TICK - i * ORCA_SPACING, net));
        ticks.push((ORCA_TICK + i * ORCA_SPACING, -net));
    }
    ticks.sort_unstable_by_key(|(t, _)| *t);
    let span = 88 * ORCA_SPACING;
    let cur = ORCA_TICK.div_euclid(span) * span;
    TickData { ticks, covered_lo: cur - 3 * span, covered_hi: cur + 4 * span - 1, initialized_arrays: vec![cur - span, cur, cur + span], bitmap_extension: None, limit_orders: vec![], adaptive_fee: None, fetched_at: Instant::now() }
}

pub struct World {
    pub quoter: Quoter,
    pub x: Pubkey,
}

/// `n_sol_usdc` SOL/USDC pools (alternating Orca / pAMM), 5 X/SOL pAMM pools,
/// 40+40 USDT bridge pools. Every CLMM pool gets `ticks_per_side` ticks.
pub fn build(n_sol_usdc: usize, ticks_per_side: usize) -> World {
    let registry = Arc::new(PoolRegistry::new());
    let cache = Arc::new(PoolCache::new(u64::MAX));
    let mirror = Arc::new(AccountMirror::new());
    let x = Pubkey::new_unique();
    let mut add = |pt: PoolType, a: Pubkey, b: Pubkey, st: PoolState| {
        let addr = Pubkey::new_unique();
        registry.add(PoolEntry { address: addr, pool_type: pt, mint_a: a, mint_b: b });
        if matches!(st, PoolState::Orca { .. }) {
            TICKS.insert(addr, Arc::new(ticks_around(ticks_per_side)));
        }
        cache.insert(addr, st);
    };
    for i in 0..5u64 {
        add(PoolType::PumpFunAmm, x, SOL_NATIVE_MINT, pamm(x, SOL_NATIVE_MINT, 200_000_000_000_000 + i, 100_000_000_000 + i));
    }
    for i in 0..n_sol_usdc {
        if i % 2 == 0 {
            add(PoolType::Orca, SOL_NATIVE_MINT, USDC_MINT, orca(SOL_NATIVE_MINT, USDC_MINT));
        } else {
            add(PoolType::PumpFunAmm, SOL_NATIVE_MINT, USDC_MINT, pamm(SOL_NATIVE_MINT, USDC_MINT, 50_000_000_000_000, 5_000_000_000_000));
        }
    }
    for _ in 0..40 {
        add(PoolType::Orca, SOL_NATIVE_MINT, USDT_MINT, orca(SOL_NATIVE_MINT, USDT_MINT));
        add(PoolType::Orca, USDT_MINT, USDC_MINT, orca(USDT_MINT, USDC_MINT));
    }
    let rpc = Arc::new(RpcClient::new("http://127.0.0.1:1".to_string())); // never reached on a warm cache
    World { quoter: Quoter::with_mirror(registry, cache, rpc, mirror), x }
}

pub fn req(input: Pubkey, output: Pubkey, amount: u64, direct: bool) -> QuoteRequest {
    QuoteRequest { input_mint: input, output_mint: output, amount, slippage_bps: 50, only_direct_routes: direct, exclude_dexes: vec![], dexes: vec![], max_accounts: 64 }
}

/// Sorted-sample percentile helper.
pub fn pct(samples: &[f64], f: f64) -> f64 {
    samples[((samples.len() as f64 * f) as usize).min(samples.len() - 1)]
}
