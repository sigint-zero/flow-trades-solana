//! Tick-array loading for concentrated-liquidity pools — the COLD side of
//! `quote::clmm`. Everything here does RPC and runs off the quote path
//! (cold-path first quote, background revalidation, block-driven refresh).

use std::sync::Arc;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::constants::*;
use crate::error::{TradeError, TradeResult};
use crate::quote::clmm::{TickData, TickLayout, TICKS};
use super::types::PoolState;

/// Arrays fetched on each side of the current one.
pub const ARRAYS_EACH_SIDE: i32 = 3;

/// Where a pool's ticks live: (layout, program, pool, tick_current, tick_spacing).
pub fn tick_source(state: &PoolState) -> Option<(TickLayout, Pubkey, Pubkey, i32, i32)> {
    match state {
        PoolState::RaydiumClmm { pool, tick_current, tick_spacing, .. } => Some((TickLayout::Raydium, RAYDIUM_CL_PROG_ID, *pool, *tick_current, *tick_spacing)),
        PoolState::PancakeSwap { pool, tick_current, tick_spacing, .. } => Some((TickLayout::Raydium, PANCAKESWAP_PROG_ID, *pool, *tick_current, *tick_spacing)),
        PoolState::Orca { whirlpool, tick_current, tick_spacing, .. } => Some((TickLayout::Orca, ORCA_PROG_ID, *whirlpool, *tick_current, *tick_spacing)),
        PoolState::Byreal { pool, tick_current, tick_spacing, .. } => Some((TickLayout::Raydium, BYREAL_PROG_ID, *pool, *tick_current, *tick_spacing)),
        PoolState::DefiTunaFusion { pool, tick_current_index, tick_spacing, .. } => Some((TickLayout::Fusion, DEFITUNA_FUSION_PROG_ID, *pool, *tick_current_index, *tick_spacing as i32)),
        _ => None,
    }
}

pub fn bitmap_extension_pda(program: &Pubkey, pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool_tick_array_bitmap_extension", pool.as_ref()], program).0
}

/// Everything needed to turn a batch of fetched accounts back into `TickData`.
pub struct TickFetchPlan {
    pub layout: TickLayout,
    pub pool: Pubkey,
    pub span: i32,
    pub spacing: i32,
    pub starts: Vec<i32>,
    /// Account keys in order: the arrays for `starts`, then (Raydium layouts)
    /// the bitmap extension PDA, then (Orca whirlpools) the `Oracle` PDA or
    /// (Byreal dynamic-fee pools) the vaults and oracle accounts its fee reads
    /// (`quote::byreal_fee::dyn_input_keys`).
    pub keys: Vec<Pubkey>,
    pub extension: Option<Pubkey>,
    pub oracle: Option<Pubkey>,
    /// The pool state, when its fee needs the Byreal accounts above.
    pub dyn_fee_state: Option<PoolState>,
}

pub fn tick_fetch_plan(state: &PoolState) -> Option<TickFetchPlan> {
    let (layout, program, pool, tick_current, tick_spacing) = tick_source(state)?;
    let spacing = tick_spacing.max(1);
    let span = layout.ticks_per_array() * spacing;
    let cur = layout.array_start(tick_current, spacing);
    let starts: Vec<i32> = (-ARRAYS_EACH_SIDE..=ARRAYS_EACH_SIDE).map(|k| cur + k * span).collect();
    let mut keys: Vec<Pubkey> = starts.iter().map(|s| layout.array_pda(&program, &pool, *s)).collect();
    let extension = matches!(layout, TickLayout::Raydium).then(|| bitmap_extension_pda(&program, &pool));
    if let Some(e) = extension {
        keys.push(e);
    }
    // adaptive-fee whirlpools keep their volatility state in an oracle account
    let oracle = (program == ORCA_PROG_ID).then(|| Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &program).0);
    if let Some(o) = oracle {
        keys.push(o);
    }
    let dyn_keys = crate::quote::byreal_fee::dyn_input_keys(state);
    if let Some(k) = dyn_keys {
        keys.extend(k);
    }
    Some(TickFetchPlan { layout, pool, span, spacing, starts, keys, extension, oracle, dyn_fee_state: dyn_keys.map(|_| state.clone()) })
}

/// Build `TickData` from the accounts fetched for `plan.keys` (same order) and
/// publish it to `quote::clmm::TICKS`.
pub fn publish_ticks(plan: &TickFetchPlan, accounts: &[Option<solana_sdk::account::Account>]) -> TradeResult<Arc<TickData>> {
    if accounts.len() != plan.keys.len() {
        return Err(TradeError::Execution("tick array batch size mismatch".into()));
    }
    let mut ticks: Vec<(i32, i128)> = Vec::new();
    let mut limit_orders: Vec<(i32, u64)> = Vec::new();
    let mut initialized_arrays = Vec::new();
    for (i, start) in plan.starts.iter().enumerate() {
        if let Some(acct) = &accounts[i] {
            if let Some((parsed_start, arr, orders)) = plan.layout.parse_array(&acct.data, plan.spacing) {
                if parsed_start != *start {
                    return Err(TradeError::Execution(format!("tick array {} start {parsed_start} != derived {start}", plan.keys[i])));
                }
                if !arr.is_empty() {
                    initialized_arrays.push(*start);
                }
                ticks.extend(arr);
                limit_orders.extend(orders);
            }
        }
    }
    ticks.sort_unstable_by_key(|(t, _)| *t);
    limit_orders.sort_unstable_by_key(|(t, _)| *t);
    let n = plan.starts.len();
    let bitmap_extension = match plan.extension {
        Some(e) if accounts.get(n).map(|a| a.is_some()).unwrap_or(false) => Some(e),
        _ => None,
    };
    let oracle_index = n + plan.extension.is_some() as usize;
    let adaptive_fee = match (plan.oracle, accounts.get(oracle_index)) {
        (Some(_), Some(Some(acct))) => crate::quote::clmm::OrcaAdaptiveFee::parse(&acct.data, &plan.pool),
        _ => None,
    };
    if let Some(st) = &plan.dyn_fee_state {
        let from = oracle_index + plan.oracle.is_some() as usize;
        crate::quote::byreal_fee::publish_dyn_inputs(st, &accounts[from..]);
    }
    let data = Arc::new(TickData {
        ticks,
        covered_lo: plan.starts[0],
        covered_hi: plan.starts[plan.starts.len() - 1] + plan.span - 1,
        initialized_arrays,
        bitmap_extension,
        limit_orders,
        adaptive_fee,
        fetched_at: Instant::now(),
    });
    debug!(pool = %plan.pool, ticks = data.ticks.len(), arrays = data.initialized_arrays.len(), ext = data.bitmap_extension.is_some(), "loaded clmm ticks");
    TICKS.insert(plan.pool, Arc::clone(&data));
    Ok(data)
}

/// Fetch the tick arrays around the current price (one `getMultipleAccounts`)
/// and publish them to `quote::clmm::TICKS`.
pub async fn load_clmm_ticks(rpc: &RpcClient, state: &PoolState) -> TradeResult<Arc<TickData>> {
    let plan = tick_fetch_plan(state).ok_or_else(|| TradeError::Execution("not a tick-array pool".into()))?;
    let accounts = rpc.get_multiple_accounts(&plan.keys).await.map_err(|e| TradeError::Rpc(format!("tick arrays: {e}")))?;
    publish_ticks(&plan, &accounts)
}

/// Tick arrays for many pools in as few RPC calls as possible (100 keys per
/// `getMultipleAccounts`). Returns the number of pools published.
pub async fn load_clmm_ticks_many(rpc: &RpcClient, states: &[PoolState]) -> usize {
    let plans: Vec<TickFetchPlan> = states.iter().filter_map(tick_fetch_plan).collect();
    if plans.is_empty() {
        return 0;
    }
    let keys: Vec<Pubkey> = plans.iter().flat_map(|p| p.keys.iter().copied()).collect();
    let mut accounts = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        match rpc.get_multiple_accounts(chunk).await {
            Ok(a) => accounts.extend(a),
            Err(e) => {
                debug!(error = %e, "tick array batch failed");
                return 0;
            }
        }
    }
    let mut n = 0;
    let mut off = 0;
    for plan in &plans {
        let slice = &accounts[off..off + plan.keys.len()];
        off += plan.keys.len();
        if publish_ticks(plan, slice).is_ok() {
            n += 1;
        }
    }
    n
}

/// Raydium-style `AmmConfig.trade_fee_rate` (u32 at 47, 1e6 denominator),
/// cached per config. Shared by Raydium CLMM, PancakeSwap and Byreal (same layout:
/// bump u8, index u16, owner, protocol_fee_rate u32, trade_fee_rate u32,
/// tick_spacing u16, fund_fee_rate u32).
static CLMM_CONFIG_FEES: std::sync::LazyLock<dashmap::DashMap<Pubkey, u32>> = std::sync::LazyLock::new(dashmap::DashMap::new);

pub async fn clmm_config_fee_ppm(rpc: &RpcClient, config: &Pubkey, expect_tick_spacing: i32) -> TradeResult<u32> {
    if let Some(f) = CLMM_CONFIG_FEES.get(config) {
        return Ok(*f);
    }
    let acct = super::fetcher::fetch_account(rpc, config).await?;
    let d = &acct.data;
    if d.len() < 53 {
        return Err(TradeError::Execution("clmm amm_config too small".into()));
    }
    let rate = u32::from_le_bytes(d[47..51].try_into().unwrap());
    let spacing = u16::from_le_bytes(d[51..53].try_into().unwrap()) as i32;
    if spacing != expect_tick_spacing {
        return Err(TradeError::Execution(format!("clmm amm_config layout mismatch: tick_spacing {spacing} != pool {expect_tick_spacing}")));
    }
    if rate == 0 || rate > 100_000 {
        return Err(TradeError::Execution(format!("clmm trade_fee_rate implausible: {rate}")));
    }
    CLMM_CONFIG_FEES.insert(*config, rate);
    Ok(rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raydium_style_pdas_match_the_known_derivation() {
        let pool = Pubkey::new_unique();
        let a = TickLayout::Raydium.array_pda(&RAYDIUM_CL_PROG_ID, &pool, -600);
        let b = Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), &(-600i32).to_be_bytes()], &RAYDIUM_CL_PROG_ID).0;
        assert_eq!(a, b);
        let o = TickLayout::Orca.array_pda(&ORCA_PROG_ID, &pool, -704);
        let o2 = Pubkey::find_program_address(&[b"tick_array", pool.as_ref(), b"-704"], &ORCA_PROG_ID).0;
        assert_eq!(o, o2);
    }
}
