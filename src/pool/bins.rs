//! Bin-array loading for Meteora DLMM pairs — the COLD side of `quote::dlmm`.
//! Everything here does RPC and runs off the quote path (cold-path first
//! quote, background revalidation, block-driven refresh).
//!
//! One `getMultipleAccounts` reads the LbPair, its bitmap extension and the
//! bin arrays a swap can walk from the active bin in either direction
//! (`SWAP_ARRAYS` each way, found through the pair's liquidity bitmap), so the
//! published snapshot is from a single slot: the active bin's composition
//! changes on every swap, and a pair from one slot walked over bins from
//! another is not a quote.

use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::constants::METEORA_DLMM_PROG_ID;
use crate::error::{TradeError, TradeResult};
use crate::quote::dlmm::{arrays_for_swap, bin_array_pda, bitmap_extension_pda, liquid_arrays, BinArray, DlmmBins, DlmmPair, ExtBitmap, BINS, SWAP_ARRAYS};
use super::types::PoolState;

/// Accounts to read for one pair, in order: LbPair, bitmap extension PDA,
/// the bin arrays at `indices`, then the Clock sysvar (the cluster time the
/// swap's `update_references` reads).
pub struct BinFetchPlan {
    pub pair: Pubkey,
    pub indices: Vec<i64>,
    pub keys: Vec<Pubkey>,
}

/// Plan from the pair's last known state: its active bin and bitmap, plus the
/// extension bitmap of the previous snapshot (without one, arrays beyond the
/// pair's own bitmap are found on the next load, once the extension is read).
pub fn plan_for(lb_pair: &Pubkey, pair: &DlmmPair) -> BinFetchPlan {
    let prev = BINS.get(lb_pair).map(|b| Arc::clone(&b));
    let ext = prev.as_ref().and_then(|p| p.ext_bitmap.as_ref());
    let liquid = liquid_arrays(&pair.bitmap, ext);
    let mut indices: Vec<i64> = arrays_for_swap(&liquid, pair.active_id, true, true, SWAP_ARRAYS)
        .into_iter()
        .chain(arrays_for_swap(&liquid, pair.active_id, true, false, SWAP_ARRAYS))
        .map(|i| i as i64)
        .collect();
    indices.sort_unstable();
    indices.dedup();
    // PDAs are ~20 µs each: reuse the previous snapshot's
    let known = |i: i64| prev.as_ref().and_then(|p| p.arrays.iter().find(|a| a.index == i).map(|a| a.key));
    let ext_key = prev.as_ref().map(|p| p.ext_key).unwrap_or_else(|| bitmap_extension_pda(lb_pair));
    let mut keys = vec![*lb_pair, ext_key];
    keys.extend(indices.iter().map(|i| known(*i).unwrap_or_else(|| bin_array_pda(lb_pair, *i))));
    keys.push(solana_sdk::sysvar::clock::ID);
    BinFetchPlan { pair: *lb_pair, indices, keys }
}

pub fn bin_fetch_plan(state: &PoolState) -> Option<BinFetchPlan> {
    match state {
        PoolState::MeteoraDlmm { lb_pair, pair, .. } if pair.is_parsed() => Some(plan_for(lb_pair, pair)),
        _ => None,
    }
}

/// Build the snapshot from the accounts fetched for `plan.keys` (same order)
/// and publish it to `quote::dlmm::BINS`.
pub fn publish_bins(plan: &BinFetchPlan, accounts: &[Option<Account>], slot: u64) -> TradeResult<Arc<DlmmBins>> {
    if accounts.len() != plan.keys.len() {
        return Err(TradeError::Execution("bin array batch size mismatch".into()));
    }
    let pair = accounts[0]
        .as_ref()
        .filter(|a| a.owner == METEORA_DLMM_PROG_ID)
        .and_then(|a| DlmmPair::parse(&a.data))
        .ok_or_else(|| TradeError::Execution(format!("dlmm lb_pair {} unreadable", plan.pair)))?;
    let ext = accounts[1].as_ref().filter(|a| a.owner == METEORA_DLMM_PROG_ID).and_then(|a| ExtBitmap::parse(&a.data));
    let mut arrays = Vec::with_capacity(plan.indices.len());
    for (i, index) in plan.indices.iter().enumerate() {
        // an array the bitmap marks but that is gone is simply not loaded; a
        // walk that needs it reports `NotLoaded`
        let Some(acct) = &accounts[i + 2] else { continue };
        let arr = BinArray::parse(plan.keys[i + 2], &acct.data)
            .ok_or_else(|| TradeError::Execution(format!("bin array {} unreadable", plan.keys[i + 2])))?;
        if arr.index != *index {
            return Err(TradeError::Execution(format!("bin array {} index {} != derived {index}", plan.keys[i + 2], arr.index)));
        }
        arrays.push(arr);
    }
    // Clock: slot, epoch_start_timestamp, epoch, leader_schedule_epoch, unix_timestamp @32
    let clock_unix = accounts[accounts.len() - 1]
        .as_ref()
        .and_then(|a| a.data.get(32..40))
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        .unwrap_or(0);
    let data = Arc::new(DlmmBins::new(pair, arrays, plan.keys[1], ext, slot, clock_unix));
    debug!(pair = %plan.pair, active = data.pair.active_id, arrays = data.arrays.len(), ext = data.extension.is_some(), slot, "loaded dlmm bins");
    BINS.insert(plan.pair, Arc::clone(&data));
    Ok(data)
}

async fn fetch_plan(rpc: &RpcClient, plan: &BinFetchPlan) -> TradeResult<Arc<DlmmBins>> {
    let res = rpc
        .get_multiple_accounts_with_commitment(&plan.keys, rpc.commitment())
        .await
        .map_err(|e| TradeError::Rpc(format!("dlmm bin arrays: {e}")))?;
    publish_bins(plan, &res.value, res.context.slot)
}

/// Read a pair's LbPair + bin arrays + extension (one `getMultipleAccounts`)
/// and publish them. When the pair moved since `state` was read (or the
/// extension was unknown) far enough that the arrays read do not cover the
/// walk, plan again from the fresh snapshot and read once more.
pub async fn load_dlmm_bins(rpc: &RpcClient, state: &PoolState) -> TradeResult<Arc<DlmmBins>> {
    let PoolState::MeteoraDlmm { lb_pair, pair, .. } = state else {
        return Err(TradeError::Execution("not a dlmm pair".into()));
    };
    // a state cached before its pair was parsed (older warm file): read the pair first
    let pair = if pair.is_parsed() {
        *pair
    } else {
        let acct = super::fetcher::fetch_account(rpc, lb_pair).await?;
        DlmmPair::parse(&acct.data).ok_or_else(|| TradeError::Execution(format!("dlmm lb_pair {lb_pair} unreadable")))?
    };
    let plan = plan_for(lb_pair, &pair);
    let data = fetch_plan(rpc, &plan).await?;
    if data.covers_both_directions() {
        return Ok(data);
    }
    fetch_plan(rpc, &plan_for(&plan.pair, &data.pair)).await
}

/// Bins for many pairs in as few RPC calls as possible. A pair's accounts
/// never straddle two calls (one snapshot = one slot), so plans are packed
/// whole into calls of at most 100 keys. Returns the number of pairs published.
pub async fn load_dlmm_bins_many(rpc: &RpcClient, states: &[PoolState]) -> usize {
    let plans: Vec<BinFetchPlan> = states.iter().filter_map(bin_fetch_plan).collect();
    let mut n = 0;
    let mut start = 0;
    while start < plans.len() {
        let mut end = start;
        let mut keys: Vec<Pubkey> = Vec::new();
        while end < plans.len() && (keys.is_empty() || keys.len() + plans[end].keys.len() <= 100) {
            keys.extend(plans[end].keys.iter().copied());
            end += 1;
        }
        let res = match rpc.get_multiple_accounts_with_commitment(&keys, rpc.commitment()).await {
            Ok(r) => r,
            Err(e) => {
                debug!(error = %e, "dlmm bin array batch failed");
                return n;
            }
        };
        let mut off = 0;
        for plan in &plans[start..end] {
            let slice = &res.value[off..off + plan.keys.len()];
            off += plan.keys.len();
            if publish_bins(plan, slice, res.context.slot).is_ok() {
                n += 1;
            }
        }
        start = end;
    }
    n
}
