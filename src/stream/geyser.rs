//! Yellowstone gRPC (Geyser) streaming backend.
//!
//! Subscribes to all 19 DEX programs via a single gRPC stream. Account updates
//! arrive as raw bytes — no base64 decoding overhead. For 14/19 pool types, pool
//! state is parsed inline (zero RPC). For 5 async types, companion data is cached
//! in the AccountMirror for sync parsing on subsequent updates.
//!
//! **Pool discovery**: Unknown accounts are automatically parsed and registered.
//! When Geyser is active, no block scanner is needed.
//!
//! **Vault balance streaming**: Vault token accounts are subscribed to dynamically.
//! When a pool is discovered, its vault pubkeys are registered and added to the
//! Geyser subscription. Vault balance updates are parsed inline (u64 at offset 64)
//! and stored in the AccountMirror for zero-RPC quoting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterBlocks,
};

use crate::constants::*;
use crate::pool::registry::PoolEntry;
use crate::pool::types::PoolType;
use crate::stream::account_mirror::parse_token_balance;
use crate::stream::block_scanner::extract_mints_from_state;

use super::StreamManager;

/// How often to flush pending subscriptions (vaults + pool candidates).
const SUBSCRIPTION_FLUSH_SECS: u64 = 2;

/// All 19 DEX program IDs to subscribe to.
fn all_dex_programs() -> Vec<Pubkey> {
    vec![
        RAYDIUM_V4_PROG_ID,
        RAYDIUM_CPMM_PROG_ID,
        RAYDIUM_CL_PROG_ID,
        RAYDIUM_LP_PROG_ID,
        PUMP_FUN_PROG_ID,
        PUMP_FUN_AMM_PROG_ID,
        ORCA_PROG_ID,
        METEORA_PROG_ID,
        METEORA_DLMM_PROG_ID,
        METEORA_DAMM_PROG_ID,
        METEORA_DBC_PROG_ID,
        FLUXBEAM_PROG_ID,
        FLASH_TRADE_PROG_ID,
        BYREAL_PROG_ID,
        DEFITUNA_FUSION_PROG_ID,
        DEFITUNA_POOLS_PROG_ID,
        SAROS_PROG_ID,
        PANCAKESWAP_PROG_ID,
        DOOAR_PROG_ID,
        PUMPUP_PROG_ID,
        // OnChain Labs DEX V2 — discovery-only. We don't quote or execute
        // against this aggregator, but including its program ID here means
        // (a) Filter 1 picks up its account-state pushes, and
        // (b) Filter 3 (block scanner) sees txs containing OnChain Labs and
        //     the pool-discovery pipeline walks inner instructions to register
        //     the underlying private DEX pools that OnChain Labs routes through.
        ONCHAIN_LABS_DEX_V2_PROG_ID,
    ]
}

/// Build the SubscribeRequest with DEX program owners + explicit account pubkeys.
/// `extra_accounts` includes vault token accounts + pool candidates from tx scanning.
fn build_subscribe_request(extra_accounts: &[Pubkey]) -> SubscribeRequest {
    let mut accounts = HashMap::new();

    // Filter 1: all 19 DEX programs by owner
    let owners: Vec<String> = all_dex_programs().iter().map(|p| p.to_string()).collect();
    accounts.insert(
        "all_dex".to_string(),
        SubscribeRequestFilterAccounts {
            account: vec![],
            owner: owners,
            filters: vec![],
            nonempty_txn_signature: None,
        },
    );

    // Filter 2: explicit account pubkeys (vaults + pool candidates from tx discovery)
    if !extra_accounts.is_empty() {
        let acct_strings: Vec<String> = extra_accounts.iter().map(|p| p.to_string()).collect();
        accounts.insert(
            "tracked_accounts".to_string(),
            SubscribeRequestFilterAccounts {
                account: acct_strings,
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );
    }

    // Filter 3: blocks with DEX transactions — full block data, equivalent to
    // RPC `blockSubscribe`. Gets ALL transactions that touch DEX programs, plus
    // post-state account data. Catches everything account-level misses.
    let mut blocks = HashMap::new();
    let dex_strings: Vec<String> = all_dex_programs().iter().map(|p| p.to_string()).collect();
    blocks.insert(
        "dex_blocks".to_string(),
        SubscribeRequestFilterBlocks {
            account_include: dex_strings,
            include_transactions: Some(true),
            include_accounts: Some(false), // we get accounts from Filter 1
            include_entries: Some(false),
        },
    );

    SubscribeRequest {
        accounts,
        slots: HashMap::new(),
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        blocks,
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(CommitmentLevel::Confirmed as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    }
}

/// Map a program ID to its PoolType.
fn program_to_pool_type(program_id: &Pubkey) -> Option<PoolType> {
    crate::stream::block_scanner::dex_program_to_type(program_id)
}

/// Extract vault pubkeys from a PoolState (if it has vaults for reserve fetching).
pub fn extract_vault_pubkeys(state: &crate::pool::types::PoolState) -> Vec<Pubkey> {
    use crate::pool::types::PoolState;
    match state {
        PoolState::RaydiumCpmm { token_0_vault, token_1_vault, .. } => vec![*token_0_vault, *token_1_vault],
        PoolState::RaydiumLp { base_vault, quote_vault, .. } => vec![*base_vault, *quote_vault],
        PoolState::RaydiumV4 { coin_vault, pc_vault, .. } => vec![*coin_vault, *pc_vault],
        PoolState::PumpFunAmm { pool_base_vault, pool_quote_vault, .. } => vec![*pool_base_vault, *pool_quote_vault],
        PoolState::Meteora { a_token_vault, b_token_vault, .. } => vec![*a_token_vault, *b_token_vault],
        PoolState::MeteoraDamm { token_a_vault, token_b_vault, .. } => vec![*token_a_vault, *token_b_vault],
        PoolState::MeteoraDbc { base_vault, quote_vault, .. } => vec![*base_vault, *quote_vault],
        PoolState::FluxBeam { token_a_vault, token_b_vault, .. } => vec![*token_a_vault, *token_b_vault],
        PoolState::Saros { token_a_vault, token_b_vault, .. } => vec![*token_a_vault, *token_b_vault],
        PoolState::Dooar { token_a_vault, token_b_vault, .. } => vec![*token_a_vault, *token_b_vault],
        // Pumpup stores reserves inline (token_a_reserve / token_b_reserve in Pool state),
        // but expose vaults too in case anything outside the inline-reserve path needs them.
        PoolState::Pumpup { token_a_vault, token_b_vault, .. } => vec![*token_a_vault, *token_b_vault],
        // CLMM and bonding curve pools don't use vault balance reads for quoting
        _ => vec![],
    }
}

/// Register a pool's vaults in the mirror and buffer for subscription.
fn register_pool_vaults(
    manager: &StreamManager,
    pool_address: &Pubkey,
    state: &crate::pool::types::PoolState,
    pending_vaults: &Mutex<Vec<Pubkey>>,
) {
    let vaults = extract_vault_pubkeys(state);
    for vault in &vaults {
        if !manager.mirror.is_vault(vault) {
            manager.mirror.register_vault(*vault, *pool_address);
            if let Ok(mut pending) = pending_vaults.try_lock() {
                pending.push(*vault);
            }
        }
    }
}

/// Run the Geyser gRPC streaming backend.
///
/// Connects to the Yellowstone gRPC endpoint and subscribes to all 19 DEX
/// programs + vault token accounts. Handles pool state updates, pool discovery,
/// and vault balance streaming. Reconnects automatically on error.
pub async fn run(manager: Arc<StreamManager>) {
    let endpoint = match &manager.config.geyser_endpoint {
        Some(ep) => ep.clone(),
        None => {
            error!("Geyser mode selected but no geyser_endpoint configured");
            return;
        }
    };

    let x_token = manager.config.geyser_token.clone();

    loop {
        info!(endpoint = %endpoint, "Geyser: connecting");

        match run_session(&manager, &endpoint, x_token.as_deref()).await {
            Ok(()) => info!("Geyser session ended cleanly, reconnecting"),
            Err(e) => {
                manager.stats.record_error();
                warn!(error = %e, "Geyser error, reconnecting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Run a single Geyser gRPC session with bidirectional subscribe.
async fn run_session(
    manager: &Arc<StreamManager>,
    endpoint: &str,
    x_token: Option<&str>,
) -> Result<(), crate::error::TradeError> {
    // Build client
    let mut client = GeyserGrpcClient::build_from_shared(endpoint.to_string())
        .map_err(|e| crate::error::TradeError::Rpc(format!("Geyser build: {e}")))?
        .x_token(x_token)
        .map_err(|e| crate::error::TradeError::Rpc(format!("Geyser x_token: {e}")))?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(10))
        .max_decoding_message_size(64 * 1024 * 1024)
        .connect()
        .await
        .map_err(|e| crate::error::TradeError::Rpc(format!("Geyser connect: {e}")))?;

    // Health check
    match client.health_check().await {
        Ok(resp) => info!(status = ?resp.status, "Geyser health check OK"),
        Err(e) => warn!(error = %e, "Geyser health check failed (continuing anyway)"),
    }

    // Bidirectional subscribe — we get a sink to send updated requests
    let (mut sink, mut stream) = client
        .subscribe()
        .await
        .map_err(|e| crate::error::TradeError::Rpc(format!("Geyser subscribe: {e}")))?;

    // Send initial subscription: DEX programs + any pre-registered vaults from bootstrap
    let existing_vaults = manager.mirror.all_vault_pubkeys();
    let initial_request = build_subscribe_request(&existing_vaults);
    sink.send(initial_request).await.map_err(|e|
        crate::error::TradeError::Rpc(format!("Geyser initial send: {e}")))?;

    info!(
        programs = 19,
        vaults = existing_vaults.len(),
        "Geyser: subscribed (DEX programs + vaults + transactions)"
    );

    let debounce = Duration::from_secs(2);
    let discovered = AtomicU64::new(0);
    let updated = AtomicU64::new(0);
    let vault_updates = AtomicU64::new(0);
    let tx_discovered = AtomicU64::new(0);
    let raw_account_updates = AtomicU64::new(0);
    let raw_tx_updates = AtomicU64::new(0);
    let parse_failures = AtomicU64::new(0);
    let pending_vaults: Arc<Mutex<Vec<Pubkey>>> = Arc::new(Mutex::new(Vec::new()));
    // Pool candidates from tx scanning — added to Geyser subscription for account data
    let pending_pool_subs: Arc<Mutex<Vec<Pubkey>>> = Arc::new(Mutex::new(Vec::new()));
    // Companion accounts (Serum markets, PumpFun global, Meteora vault configs) —
    // subscribed so future updates arrive via Geyser instead of RPC fallback.
    let pending_companions: Arc<Mutex<Vec<Pubkey>>> = Arc::new(Mutex::new(Vec::new()));

    // Spawn background task to flush pending subscriptions (vaults + pools + companions)
    let flush_sink = Arc::new(Mutex::new(sink));
    let flush_pending_vaults = Arc::clone(&pending_vaults);
    let flush_pending_pools = Arc::clone(&pending_pool_subs);
    let flush_pending_companions = Arc::clone(&pending_companions);
    let flush_mirror = Arc::clone(&manager.mirror);
    let flush_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(SUBSCRIPTION_FLUSH_SECS));
        loop {
            interval.tick().await;

            let new_vaults: Vec<Pubkey> = {
                let mut pending = flush_pending_vaults.lock().await;
                pending.drain(..).collect()
            };
            let new_pools: Vec<Pubkey> = {
                let mut pending = flush_pending_pools.lock().await;
                pending.drain(..).collect()
            };
            let new_companions: Vec<Pubkey> = {
                let mut pending = flush_pending_companions.lock().await;
                pending.drain(..).collect()
            };

            if new_vaults.is_empty() && new_pools.is_empty() && new_companions.is_empty() {
                continue;
            }

            let all_vaults = flush_mirror.all_vault_pubkeys();
            let all_companions = flush_mirror.all_companion_pubkeys();
            // Combine vaults + pool candidates + companions into one subscription update
            let mut all_account_subs: Vec<Pubkey> = all_vaults;
            all_account_subs.extend_from_slice(&new_pools);
            all_account_subs.extend(all_companions);
            let request = build_subscribe_request(&all_account_subs);

            let mut sink_guard = flush_sink.lock().await;
            if let Err(e) = sink_guard.send(request).await {
                warn!(error = %e, "Geyser: failed to update subscription");
                break;
            }
            info!(
                new_vaults = new_vaults.len(),
                new_pool_subs = new_pools.len(),
                new_companions = new_companions.len(),
                total_accounts = all_account_subs.len(),
                "Geyser: subscription updated"
            );
        }
    });

    while let Some(msg) = stream.next().await {
        let update = match msg {
            Ok(u) => u,
            Err(e) => {
                flush_handle.abort();
                let d = discovered.load(Ordering::Relaxed);
                let u = updated.load(Ordering::Relaxed);
                let v = vault_updates.load(Ordering::Relaxed);
                let t = tx_discovered.load(Ordering::Relaxed);
                info!(discovered = d, updated = u, vault_updates = v, tx_discovered = t, "Geyser session stats at disconnect");
                manager.stats.record_error();
                return Err(crate::error::TradeError::Rpc(format!(
                    "Geyser stream error: {e}"
                )));
            }
        };

        // Dispatch: account updates, transaction updates, or skip
        let account_info = match update.update_oneof {
            Some(UpdateOneof::Block(block_update)) => {
                crate::stream::note_slot(block_update.slot);
                raw_tx_updates.fetch_add(block_update.transactions.len() as u64, Ordering::Relaxed);

                // Swap stream: parse + broadcast every confirmed DEX swap.
                // Lossy by design — slow consumers lag, never block.
                if let Some(ctx) = manager.swap_stream.as_ref() {
                    let emitted = crate::stream::swap_stream::parse_swaps_from_yellowstone_block(
                        &block_update,
                        &ctx.oracle,
                        &ctx.tx,
                    );
                    if emitted > 0 {
                        manager.stats.record_swap_emitted(emitted as u64);
                    }
                }
                // Block-level discovery: process ALL transactions in the block.
                // Equivalent to RPC `blockSubscribe` but via Geyser gRPC.
                for tx_info in &block_update.transactions {
                    if let Some(ref tx) = tx_info.transaction {
                        if let Some(ref msg) = tx.message {
                            let mut all_keys: Vec<Pubkey> = msg.account_keys.iter()
                                .filter_map(|k| <[u8; 32]>::try_from(k.as_slice()).ok())
                                .map(Pubkey::new_from_array)
                                .collect();

                            if let Some(ref meta) = tx_info.meta {
                                for addr in &meta.loaded_writable_addresses {
                                    if let Ok(bytes) = <[u8; 32]>::try_from(addr.as_slice()) {
                                        all_keys.push(Pubkey::new_from_array(bytes));
                                    }
                                }
                                for addr in &meta.loaded_readonly_addresses {
                                    if let Ok(bytes) = <[u8; 32]>::try_from(addr.as_slice()) {
                                        all_keys.push(Pubkey::new_from_array(bytes));
                                    }
                                }
                            }

                            // Helper: process a single instruction (top-level or inner).
                            // Registers the pool IMMEDIATELY from block data — no waiting
                            // for Geyser account delivery. Account stream fills in state later.
                            let process_ix = |prog_id_index: usize, accounts: &[u8], data: &[u8]| {
                                if prog_id_index >= all_keys.len() { return; }
                                let prog_id = all_keys[prog_id_index];

                                // Program id + discriminator (Pumpup AMM vs bonding;
                                // pump.fun AMM event self-CPI is not a swap).
                                if program_to_pool_type(&prog_id).is_none() { return; }
                                let (pool_type, pool_idx) =
                                    match crate::stream::block_scanner::swap_pool_index(&prog_id, data) {
                                        Some(x) => x,
                                        None => return,
                                    };
                                if pool_idx >= accounts.len() { return; }
                                let acct_idx = accounts[pool_idx] as usize;
                                if acct_idx >= all_keys.len() { return; }
                                let candidate = all_keys[acct_idx];
                                if manager.registry.contains(&candidate) { return; }

                                // PumpFun bonding: extract mint from tx accounts (index 2)
                                if pool_type == PoolType::PumpFun && accounts.len() > 2 {
                                    let mint_idx = accounts[2] as usize;
                                    if mint_idx < all_keys.len() {
                                        let mint = all_keys[mint_idx];
                                        let mut mint_data = vec![0u8; 64];
                                        mint_data[..32].copy_from_slice(mint.as_ref());
                                        if accounts.len() > 8 {
                                            let tp_idx = accounts[8] as usize;
                                            if tp_idx < all_keys.len() {
                                                mint_data[32..64].copy_from_slice(all_keys[tp_idx].as_ref());
                                            }
                                        }
                                        manager.mirror.insert_companion(candidate, mint_data);
                                    }
                                }

                                // Register immediately with placeholder mints.
                                // The account stream will update with real state + mints.
                                // registry.contains() returns true from now on — prevents re-processing.
                                let entry = PoolEntry {
                                    address: candidate,
                                    pool_type,
                                    mint_a: Pubkey::default(),
                                    mint_b: Pubkey::default(),
                                };
                                if let Some(ref db) = manager.pool_db {
                                    let _ = db.insert_pool(&entry);
                                }
                                manager.registry.add(entry);
                                discovered.fetch_add(1, Ordering::Relaxed);
                                tx_discovered.fetch_add(1, Ordering::Relaxed);

                                // Also queue for Geyser account subscription so we get
                                // real state data for parsing + vault registration.
                                if let Ok(mut pending) = pending_pool_subs.try_lock() {
                                    if !pending.contains(&candidate) {
                                        pending.push(candidate);
                                    }
                                }
                            };

                            // Process top-level instructions
                            for ix in &msg.instructions {
                                let accounts: Vec<u8> = ix.accounts.iter().copied().collect();
                                process_ix(ix.program_id_index as usize, &accounts, &ix.data);
                            }

                            // Process inner instructions (CPI calls — Jupiter routes, etc.)
                            if let Some(ref meta) = tx_info.meta {
                                for inner_set in &meta.inner_instructions {
                                    for inner_ix in &inner_set.instructions {
                                        process_ix(
                                            inner_ix.program_id_index as usize,
                                            &inner_ix.accounts,
                                            &inner_ix.data,
                                        );
                                    }
                                }

                                // Extract post-swap vault balances from transaction meta.
                                // post_token_balances has the exact balance of every token
                                // account touched by the tx. We update ANY token account
                                // that's either a registered vault OR whose owner is a
                                // known pool — this catches vaults before they're formally
                                // registered, giving same-slot freshness.
                                for ptb in &meta.post_token_balances {
                                    let idx = ptb.account_index as usize;
                                    if idx >= all_keys.len() { continue; }
                                    let account = all_keys[idx];

                                    // Match: registered vault OR owner is a known pool
                                    let dominated = manager.mirror.is_vault(&account)
                                        || manager.registry.contains(&account)
                                        || ptb.owner.parse::<Pubkey>()
                                            .map(|o| manager.registry.contains(&o))
                                            .unwrap_or(false);

                                    if !dominated { continue; }

                                    if let Some(ref ui) = ptb.ui_token_amount {
                                        if let Ok(balance) = ui.amount.parse::<u64>() {
                                            manager.mirror.update_vault_balance(account, balance);
                                            // Auto-register as vault if not already
                                            if !manager.mirror.is_vault(&account) {
                                                if let Ok(owner) = ptb.owner.parse::<Pubkey>() {
                                                    manager.mirror.register_vault(account, owner);
                                                }
                                            }
                                            vault_updates.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                continue;
            }
            Some(UpdateOneof::Account(acct)) => {
                crate::stream::note_slot(acct.slot);
                match acct.account {
                    Some(info) => info,
                    None => continue,
                }
            }
            _ => continue,
        };

        let info = account_info;

        // Parse pubkey and owner from raw bytes
        let address = match <[u8; 32]>::try_from(info.pubkey.as_slice()) {
            Ok(bytes) => Pubkey::new_from_array(bytes),
            Err(_) => continue,
        };
        let owner = match <[u8; 32]>::try_from(info.owner.as_slice()) {
            Ok(bytes) => Pubkey::new_from_array(bytes),
            Err(_) => continue,
        };

        let raw_count = raw_account_updates.fetch_add(1, Ordering::Relaxed);
        // Log throughput periodically
        if raw_count > 0 && raw_count % 5000 == 0 {
            info!(
                raw_accounts = raw_count,
                raw_txs = raw_tx_updates.load(Ordering::Relaxed),
                pools_discovered = discovered.load(Ordering::Relaxed),
                pools_updated = updated.load(Ordering::Relaxed),
                vault_updates = vault_updates.load(Ordering::Relaxed),
                parse_failures = parse_failures.load(Ordering::Relaxed),
                tx_candidates = tx_discovered.load(Ordering::Relaxed),
                "Geyser throughput"
            );
        }

        // ── Check 1: Is this a vault token account update? ──
        if manager.mirror.is_vault(&address) {
            if let Some(balance) = parse_token_balance(&info.data) {
                manager.mirror.update_vault_balance(address, balance);
                vault_updates.fetch_add(1, Ordering::Relaxed);
                manager.stats.record_update();
            }
            continue;
        }

        // ── Check 2: Is this a companion account update? (Serum markets, Meteora vaults, etc.) ──
        if manager.mirror.has_companion(&address) {
            manager.mirror.insert_companion(address, info.data.clone());
            continue;
        }

        // ── Check 3: Is this a DEX program account (pool)? ──
        let pool_type = match program_to_pool_type(&owner) {
            Some(pt) => pt,
            None => continue,
        };

        let pool_address = address;
        let is_known = manager.registry.contains(&pool_address);

        // Try mirror-aware parsing first (handles both sync and async types)
        match crate::pool::fetcher::parse_with_mirror(
            pool_type,
            &pool_address,
            &info.data,
            &owner,
            &manager.mirror,
        ) {
            Ok(state) => {
                if !is_known {
                    if let Some((mint_a, mint_b)) = extract_mints_from_state(&state) {
                        let entry = PoolEntry {
                            address: pool_address,
                            pool_type,
                            mint_a,
                            mint_b,
                        };
                        if let Some(ref db) = manager.pool_db {
                            let _ = db.insert_pool(&entry);
                        }
                        manager.registry.add(entry);
                        discovered.fetch_add(1, Ordering::Relaxed);
                        let d = discovered.load(Ordering::Relaxed);
                        if d <= 10 || d % 100 == 0 {
                            debug!(
                                pool = %pool_address,
                                pool_type = ?pool_type,
                                total_discovered = d,
                                "Geyser: new pool discovered"
                            );
                        }
                    }
                }

                // Register vaults for this pool (buffers for subscription update)
                register_pool_vaults(manager, &pool_address, &state, &pending_vaults);

                // A raw-bytes re-parse cannot see the pump.fun AMM buyback accounts
                // resolved earlier from a swap — keep them across the refresh.
                let mut state = state;
                if state.needs_pamm_fee_accounts() {
                    if let Some(prev) = manager.cache.get(&pool_address) {
                        state.carry_over_pamm_fee_accounts(&prev);
                    }
                }
                manager.cache.insert(pool_address, state);
                manager.stats.record_update();
                updated.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(_) => {
                parse_failures.fetch_add(1, Ordering::Relaxed);
            }
        }

        // RPC fallback for async types whose companions aren't cached yet
        if !crate::pool::fetcher::is_sync_parseable(pool_type) {
            if is_known {
                if let Some(age) = manager.cache.get_with_age(&pool_address) {
                    if age < debounce {
                        continue;
                    }
                }
            }

            match crate::pool::fetcher::fetch_pool_state(
                &manager.rpc, pool_type, &pool_address,
            ).await {
                Ok(state) => {
                    // Cache companion bytes + subscribe via Geyser for future updates
                    let companion_keys =
                        crate::pool::fetcher::extract_companion_keys(pool_type, &info.data);
                    for key in &companion_keys {
                        if !manager.mirror.has_companion(key) {
                            if let Ok(acct) = crate::pool::fetcher::fetch_account(&manager.rpc, key).await {
                                manager.mirror.insert_companion(*key, acct.data);
                            }
                        }
                        // Subscribe to companion so future changes arrive via Geyser
                        if let Ok(mut pending) = pending_companions.try_lock() {
                            pending.push(*key);
                        }
                    }

                    // Register vaults
                    register_pool_vaults(manager, &pool_address, &state, &pending_vaults);

                    if !is_known {
                        if let Some((mint_a, mint_b)) = extract_mints_from_state(&state) {
                            let entry = PoolEntry {
                                address: pool_address,
                                pool_type,
                                mint_a,
                                mint_b,
                            };
                            if let Some(ref db) = manager.pool_db {
                                let _ = db.insert_pool(&entry);
                            }
                            manager.registry.add(entry);
                            discovered.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let mut state = state;
                    if state.needs_pamm_fee_accounts() {
                        if let Some(prev) = manager.cache.get(&pool_address) {
                            state.carry_over_pamm_fee_accounts(&prev);
                        }
                    }
                    manager.cache.insert(pool_address, state);
                    manager.stats.record_update();
                    updated.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    if is_known {
                        debug!(pool = %pool_address, error = %e, "async refresh failed");
                        manager.stats.record_error();
                    }
                }
            }
        }
    }

    flush_handle.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_dex_programs_count() {
        assert_eq!(all_dex_programs().len(), 21);
    }

    #[test]
    fn test_build_subscribe_request_no_extras() {
        let req = build_subscribe_request(&[]);
        assert_eq!(req.accounts.len(), 1);
        assert!(req.accounts.contains_key("all_dex"));
        assert!(req.blocks.contains_key("dex_blocks"));
    }

    #[test]
    fn test_build_subscribe_request_with_accounts() {
        let accounts = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let req = build_subscribe_request(&accounts);
        assert_eq!(req.accounts.len(), 2);
        assert!(req.accounts.contains_key("all_dex"));
        assert!(req.accounts.contains_key("tracked_accounts"));
        let tracked = req.accounts.get("tracked_accounts").unwrap();
        assert_eq!(tracked.account.len(), 2);
        assert!(tracked.owner.is_empty());
    }

    #[test]
    fn test_build_subscribe_request_dex_filter_unchanged() {
        // 19 base DEXes + Pumpup + OnChain Labs DEX V2 (discovery only) = 21.
        let req = build_subscribe_request(&[Pubkey::new_unique()]);
        let dex = req.accounts.get("all_dex").unwrap();
        assert_eq!(dex.owner.len(), 21);
        assert!(dex.account.is_empty());
    }

    #[test]
    fn test_build_subscribe_request_has_block_filter() {
        let req = build_subscribe_request(&[]);
        let block_filter = req.blocks.get("dex_blocks").unwrap();
        assert_eq!(block_filter.account_include.len(), 21);
        assert_eq!(block_filter.include_transactions, Some(true));
        assert!(req.transactions.is_empty()); // using blocks, not individual txs
    }

    #[test]
    fn test_extract_vault_pubkeys_cpmm() {
        let v0 = Pubkey::new_unique();
        let v1 = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::RaydiumCpmm {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            token_0_vault: v0,
            token_1_vault: v1,
            token_0_mint: Pubkey::new_unique(),
            token_1_mint: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        };
        let vaults = extract_vault_pubkeys(&state);
        assert_eq!(vaults, vec![v0, v1]);
    }

    #[test]
    fn test_extract_vault_pubkeys_clmm_empty() {
        let state = crate::pool::types::PoolState::Orca {
            whirlpool: Pubkey::new_unique(),
            token_mint_a: Pubkey::new_unique(),
            token_mint_b: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            tick_current: 0,
            tick_spacing: 1,
            fee_rate: 300,
            oracle: Pubkey::new_unique(),
            sqrt_price_x64: 0,
            liquidity: 0,
        };
        // CLMM pools don't need vault balance reads
        assert!(extract_vault_pubkeys(&state).is_empty());
    }

    #[test]
    fn test_all_programs_have_pool_type() {
        // Every DEX program in the Geyser filter should map to a PoolType,
        // EXCEPT OnChain Labs DEX V2 which is discovery-only (aggregator, not a
        // directly-quotable DEX). Discovery-only programs surface other DEXes'
        // pools via the block-scanner's inner-instruction walk.
        for pk in all_dex_programs() {
            if pk == ONCHAIN_LABS_DEX_V2_PROG_ID {
                assert!(
                    program_to_pool_type(&pk).is_none(),
                    "OnChain Labs DEX V2 must remain discovery-only (no PoolType)"
                );
                continue;
            }
            assert!(
                program_to_pool_type(&pk).is_some(),
                "no pool type for program {pk}"
            );
        }
    }

    #[test]
    fn test_program_to_pool_type_unknown() {
        let unknown = Pubkey::new_unique();
        assert!(program_to_pool_type(&unknown).is_none());
    }
}
