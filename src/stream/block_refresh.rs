//! Block-driven state refresh for the RPC `blockSubscribe` fallback (no Geyser).
//!
//! Geyser hands the process every changed account, so the quoter's mirror and
//! pool cache are always as fresh as the last slot. Without it, this module
//! does the Geyser job from the block stream instead, in two parts that both
//! run OFF the quote path:
//!
//! 1. **Sync, per block:** every `postTokenBalances` entry of a known pool's
//!    vault is written to the mirror (constant-product venues and pump.fun AMM
//!    are priced from vault balances, so this alone makes them block-fresh).
//! 2. **Async, per block:** pools whose price lives in the pool account (CLMM
//!    venues, DAMM v2, DLMM) and that were touched by the block are re-read in
//!    ONE `getMultipleAccounts` (100 per call), re-parsed, and their tick
//!    arrays / DLMM bin arrays reloaded in a second batched call.
//!
//! What this cannot do: pools touched by a swap this block but not yet in the
//! registry (discovery handles those), and vault changes that leave no token
//! balance in the block (none for SPL swaps).

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use dashmap::DashSet;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status_client_types::option_serializer::OptionSerializer;
use solana_transaction_status_client_types::{EncodedTransaction, UiConfirmedBlock, UiMessage};
use tracing::debug;

use crate::pool::fetcher::{is_state_priced, reparse_pool_state};
use crate::pool::{PoolCache, PoolRegistry, PoolType};
use crate::stream::account_mirror::AccountMirror;

/// Upper bound on state-priced pools re-read per block (one RPC call per 100).
pub const MAX_STATE_REFRESH_PER_BLOCK: usize = 300;

/// Cumulative counters for the periodic scanner summary (and `/health`).
pub struct RefreshStats {
    pub vault_updates: std::sync::atomic::AtomicU64,
    pub touched: std::sync::atomic::AtomicU64,
    pub refreshed: std::sync::atomic::AtomicU64,
    pub ticks: std::sync::atomic::AtomicU64,
}
pub static REFRESH_STATS: RefreshStats = RefreshStats {
    vault_updates: std::sync::atomic::AtomicU64::new(0),
    touched: std::sync::atomic::AtomicU64::new(0),
    refreshed: std::sync::atomic::AtomicU64::new(0),
    ticks: std::sync::atomic::AtomicU64::new(0),
};

pub struct BlockRefreshCtx {
    pub rpc: Arc<RpcClient>,
    pub registry: Arc<PoolRegistry>,
    pub cache: Arc<PoolCache>,
    pub mirror: Arc<AccountMirror>,
    /// Pools whose re-read is in flight; a later block does not queue them twice.
    pub in_flight: Arc<DashSet<Pubkey>>,
}

/// Outcome of the synchronous pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MirrorPass {
    pub vault_updates: usize,
    /// Known state-priced pools this block touched, in first-seen order.
    pub touched: Vec<(Pubkey, PoolType)>,
}

/// Sync, no RPC: mirror vault balances and collect touched state-priced pools.
pub fn mirror_block(block: &UiConfirmedBlock, registry: &PoolRegistry, mirror: &AccountMirror) -> MirrorPass {
    let mut pass = MirrorPass::default();
    let Some(txs) = &block.transactions else { return pass };
    let mut seen: HashSet<Pubkey> = HashSet::new();

    for tx in txs {
        let Some(meta) = &tx.meta else { continue };
        if meta.err.is_some() {
            continue;
        }
        let static_keys = match &tx.transaction {
            EncodedTransaction::Json(ui) => match &ui.message {
                UiMessage::Raw(raw) => &raw.account_keys,
                UiMessage::Parsed(_) => continue,
            },
            _ => continue,
        };
        let mut keys: Vec<Pubkey> = static_keys.iter().filter_map(|k| Pubkey::from_str(k).ok()).collect();
        if keys.len() != static_keys.len() {
            continue;
        }
        if let OptionSerializer::Some(la) = &meta.loaded_addresses {
            for k in la.writable.iter().chain(la.readonly.iter()) {
                match Pubkey::from_str(k) {
                    Ok(p) => keys.push(p),
                    Err(_) => break,
                }
            }
        }

        // 1. vault balances → mirror
        if let OptionSerializer::Some(post) = &meta.post_token_balances {
            for ptb in post {
                let Some(account) = keys.get(ptb.account_index as usize) else { continue };
                let owner = match &ptb.owner {
                    OptionSerializer::Some(o) => Pubkey::from_str(o).ok(),
                    _ => None,
                };
                let known_vault = mirror.is_vault(account);
                let owner_pool = owner.filter(|o| registry.contains(o));
                if !known_vault && owner_pool.is_none() {
                    continue;
                }
                let Ok(balance) = ptb.ui_token_amount.amount.parse::<u64>() else { continue };
                mirror.update_vault_balance(*account, balance);
                REFRESH_STATS.vault_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !known_vault {
                    if let Some(pool) = owner_pool {
                        mirror.register_vault(*account, pool);
                    }
                }
                pass.vault_updates += 1;
            }
        }

        // 2. touched state-priced pools: any writable known pool in the key set
        // (plus Raydium V4, whose curve reserves net out the PnL kept in the pool)
        for k in &keys {
            if seen.contains(k) {
                continue;
            }
            if let Some(entry) = registry.get(k) {
                if (is_state_priced(entry.pool_type) || matches!(entry.pool_type, PoolType::Meteora | PoolType::RaydiumV4)) && entry.mint_a != Pubkey::default() {
                    seen.insert(*k);
                    pass.touched.push((*k, entry.pool_type));
                }
            } else if let Some(pools) = crate::quote::byreal_fee::pools_reading_oracle(k) {
                // a Pyth price push: re-read the dynamic-fee pools that price on it
                seen.insert(*k);
                for p in pools {
                    if seen.insert(p) {
                        pass.touched.push((p, PoolType::Byreal));
                    }
                }
            }
        }
    }
    pass
}

/// Async: batch re-read the touched pools, re-parse, refresh ticks, publish.
/// Returns (states refreshed, tick sets refreshed).
pub async fn refresh_touched(ctx: &BlockRefreshCtx, mut touched: Vec<(Pubkey, PoolType)>) -> (usize, usize) {
    // Only pools whose state was fully fetched once (fees from their config
    // accounts, pAMM buyback accounts…) are refreshed here: a bare re-parse of
    // a never-fetched pool would enter the cache with default fees. The rest
    // stay cold until a quote or discovery fetches them properly.
    touched.retain(|(p, _)| ctx.cache.get(p).is_some());
    // Truncate BEFORE claiming in-flight slots, or the pools cut off by the
    // cap would stay "in flight" forever and never refresh.
    touched.retain(|(p, _)| !ctx.in_flight.contains(p));
    touched.truncate(MAX_STATE_REFRESH_PER_BLOCK);
    for (p, _) in &touched {
        ctx.in_flight.insert(*p);
    }
    if touched.is_empty() {
        return (0, 0);
    }
    REFRESH_STATS.touched.fetch_add(touched.len() as u64, std::sync::atomic::Ordering::Relaxed);
    // Meteora Standard: reserves live in 6 other accounts, refreshed separately.
    let (meteora, touched): (Vec<_>, Vec<_>) = touched.into_iter().partition(|(_, t)| *t == PoolType::Meteora);
    let meteora_n = refresh_meteora_std(ctx, &meteora).await;
    if touched.is_empty() {
        return (meteora_n, 0);
    }
    let keys: Vec<Pubkey> = touched.iter().map(|(p, _)| *p).collect();
    let mut accounts = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        match ctx.rpc.get_multiple_accounts(chunk).await {
            Ok(a) => accounts.extend(a),
            Err(e) => {
                debug!(error = %e, "block refresh batch failed");
                for (p, _) in &touched {
                    ctx.in_flight.remove(p);
                }
                return (0, 0);
            }
        }
    }
    let mut fresh_states = Vec::with_capacity(touched.len());
    for ((pool, pool_type), acct) in touched.iter().zip(accounts.iter()) {
        if let Some(acct) = acct {
            let prev = ctx.cache.get(pool);
            match reparse_pool_state(*pool_type, pool, acct, prev.as_ref()) {
                Ok(st) => fresh_states.push((*pool, st)),
                Err(e) => debug!(%pool, ?pool_type, error = %e, "block refresh reparse failed"),
            }
        }
    }
    let states_only: Vec<_> = fresh_states.iter().map(|(_, s)| s.clone()).collect();
    let ticks = crate::pool::ticks::load_clmm_ticks_many(&ctx.rpc, &states_only).await
        + crate::pool::bins::load_dlmm_bins_many(&ctx.rpc, &states_only).await;
    let n = fresh_states.len() + meteora_n;
    REFRESH_STATS.refreshed.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
    REFRESH_STATS.ticks.fetch_add(ticks as u64, std::sync::atomic::Ordering::Relaxed);
    for (pool, st) in fresh_states {
        ctx.cache.insert(pool, st);
    }
    for (p, _) in &touched {
        ctx.in_flight.remove(p);
    }
    (n, ticks)
}

/// Batched Meteora Standard reserve refresh: 6 accounts per pool, 96 per call.
async fn refresh_meteora_std(ctx: &BlockRefreshCtx, pools: &[(Pubkey, PoolType)]) -> usize {
    use crate::pool::fetcher::{meteora_std_refresh_keys, refresh_meteora_std_from};
    let mut plans: Vec<(Pubkey, crate::pool::PoolState, [Pubkey; 6])> = Vec::new();
    for (p, _) in pools {
        if let Some(st) = ctx.cache.get(p) {
            if let Some(keys) = meteora_std_refresh_keys(&st) {
                plans.push((*p, st, keys));
            }
        }
    }
    let mut done = 0;
    for chunk in plans.chunks_mut(16) {
        let keys: Vec<Pubkey> = chunk.iter().flat_map(|(_, _, k)| k.iter().copied()).collect();
        let accounts = match ctx.rpc.get_multiple_accounts(&keys).await {
            Ok(a) => a,
            Err(e) => {
                debug!(error = %e, "meteora std refresh batch failed");
                break;
            }
        };
        for (i, (pool, st, _)) in chunk.iter_mut().enumerate() {
            if refresh_meteora_std_from(st, &accounts[i * 6..i * 6 + 6]) {
                ctx.cache.insert(*pool, st.clone());
                done += 1;
            }
        }
    }
    for (p, _) in pools {
        ctx.in_flight.remove(p);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolEntry;
    use solana_transaction_status_client_types::{EncodedTransactionWithStatusMeta, UiRawMessage, UiTransaction, UiTransactionStatusMeta, UiTransactionTokenBalance};
    use solana_account_decoder::parse_token::UiTokenAmount;

    fn block_with(keys: Vec<Pubkey>, post: Vec<UiTransactionTokenBalance>) -> UiConfirmedBlock {
        let tx = UiTransaction {
            signatures: vec!["sig".into()],
            message: UiMessage::Raw(UiRawMessage {
                header: solana_sdk::message::MessageHeader { num_required_signatures: 1, num_readonly_signed_accounts: 0, num_readonly_unsigned_accounts: 0 },
                account_keys: keys.iter().map(|k| k.to_string()).collect(),
                recent_blockhash: String::new(),
                instructions: vec![],
                address_table_lookups: None,
            }),
        };
        let meta = UiTransactionStatusMeta {
            err: None,
            status: Ok(()),
            fee: 0,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: OptionSerializer::None,
            log_messages: OptionSerializer::None,
            pre_token_balances: OptionSerializer::None,
            post_token_balances: OptionSerializer::Some(post),
            rewards: OptionSerializer::None,
            loaded_addresses: OptionSerializer::None,
            return_data: OptionSerializer::None,
            compute_units_consumed: OptionSerializer::None,
            cost_units: OptionSerializer::None,
        };
        UiConfirmedBlock {
            previous_blockhash: String::new(),
            blockhash: String::new(),
            parent_slot: 0,
            transactions: Some(vec![EncodedTransactionWithStatusMeta { transaction: EncodedTransaction::Json(tx), meta: Some(meta), version: None }]),
            signatures: None,
            rewards: None,
            num_reward_partitions: None,
            block_time: None,
            block_height: None,
        }
    }

    fn ptb(idx: u8, owner: &Pubkey, amount: u64) -> UiTransactionTokenBalance {
        UiTransactionTokenBalance {
            account_index: idx,
            mint: String::new(),
            ui_token_amount: UiTokenAmount { ui_amount: None, decimals: 6, amount: amount.to_string(), ui_amount_string: String::new() },
            owner: OptionSerializer::Some(owner.to_string()),
            program_id: OptionSerializer::None,
        }
    }

    #[test]
    fn mirrors_known_pool_vaults_and_collects_touched_clmm_pools() {
        let registry = PoolRegistry::new();
        let mirror = AccountMirror::new();
        let cp_pool = Pubkey::new_unique();
        let clmm_pool = Pubkey::new_unique();
        let (ma, mb) = (Pubkey::new_unique(), Pubkey::new_unique());
        registry.add(PoolEntry { address: cp_pool, pool_type: PoolType::PumpFunAmm, mint_a: ma, mint_b: mb });
        registry.add(PoolEntry { address: clmm_pool, pool_type: PoolType::RaydiumCl, mint_a: ma, mint_b: mb });
        let vault = Pubkey::new_unique();
        let stranger_vault = Pubkey::new_unique();
        let stranger = Pubkey::new_unique();
        let block = block_with(
            vec![Pubkey::new_unique(), cp_pool, clmm_pool, vault, stranger_vault],
            vec![ptb(3, &cp_pool, 4_242), ptb(4, &stranger, 9)],
        );
        let pass = mirror_block(&block, &registry, &mirror);
        assert_eq!(pass.vault_updates, 1);
        assert_eq!(mirror.get_vault_balance(&vault), Some(4_242));
        assert!(mirror.is_vault(&vault), "auto-registered from owner = known pool");
        assert_eq!(mirror.get_vault_balance(&stranger_vault), None);
        // only the state-priced pool is queued for a re-read
        assert_eq!(pass.touched, vec![(clmm_pool, PoolType::RaydiumCl)]);
    }

    #[test]
    fn raydium_v4_pools_are_re_read_for_their_pnl() {
        let registry = PoolRegistry::new();
        let mirror = AccountMirror::new();
        let v4 = Pubkey::new_unique();
        registry.add(PoolEntry { address: v4, pool_type: PoolType::RaydiumV4, mint_a: Pubkey::new_unique(), mint_b: Pubkey::new_unique() });
        let block = block_with(vec![Pubkey::new_unique(), v4], vec![]);
        assert_eq!(mirror_block(&block, &registry, &mirror).touched, vec![(v4, PoolType::RaydiumV4)]);
    }

    #[test]
    fn a_pyth_price_push_re_reads_the_dynamic_fee_pools_on_that_oracle() {
        let registry = PoolRegistry::new();
        let mirror = AccountMirror::new();
        let (pool, oracle_0, oracle_1) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let fee = crate::quote::byreal_fee::ByrealFee { flags: 0b1_0000, oracle_0, oracle_1, ..Default::default() };
        let state = crate::pool::PoolState::Byreal {
            pool, amm_config: Pubkey::new_unique(), token_vault_a: Pubkey::new_unique(), token_vault_b: Pubkey::new_unique(),
            observation: Pubkey::new_unique(), token_mint_a: Pubkey::new_unique(), token_mint_b: Pubkey::new_unique(),
            tick_current: 0, tick_spacing: 1, sqrt_price_x64: 1 << 64, liquidity: 1, fee_rate: 100, fee,
        };
        crate::quote::byreal_fee::publish_dyn_inputs(&state, &[]);
        let block = block_with(vec![Pubkey::new_unique(), oracle_1], vec![]);
        assert_eq!(mirror_block(&block, &registry, &mirror).touched, vec![(pool, PoolType::Byreal)]);
    }

    #[test]
    fn failed_transactions_are_ignored() {
        let registry = PoolRegistry::new();
        let mirror = AccountMirror::new();
        let pool = Pubkey::new_unique();
        registry.add(PoolEntry { address: pool, pool_type: PoolType::Orca, mint_a: Pubkey::new_unique(), mint_b: Pubkey::new_unique() });
        let mut block = block_with(vec![pool], vec![]);
        if let Some(txs) = &mut block.transactions {
            txs[0].meta.as_mut().unwrap().err = Some(solana_sdk::transaction::TransactionError::AccountNotFound);
        }
        assert_eq!(mirror_block(&block, &registry, &mirror), MirrorPass::default());
    }
}
