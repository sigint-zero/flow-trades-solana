use std::sync::Arc;
use std::time::Duration;

use std::str::FromStr;

use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use flow_trades::api::{self, AppState};
use flow_trades::config;
use flow_trades::enrichment::PriceOracle;
use flow_trades::execution::AltCache;
use flow_trades::pool::cache::PoolCache;
use flow_trades::pool::registry::PoolRegistry;
use flow_trades::quote::Quoter;
use flow_trades::storage;
use flow_trades::storage::sqlite::PoolDb;
use flow_trades::stream::account_mirror::AccountMirror;
use flow_trades::stream::blockhash::BlockhashCache;
use flow_trades::stream::{StreamConfig, StreamManager, StreamStats, SwapStreamCtx};

#[tokio::main]
async fn main() {
    let mut config = config::Config::parse();

    // Load TOML config file and merge (CLI/env overrides file)
    let config_file = flow_trades::config_file::ConfigFile::load(&config.config);
    flow_trades::config_file::apply_config_file(&mut config, &config_file);

    // Validate rpc_url is set (from CLI, env, or config file)
    if config.rpc_url.is_empty() {
        eprintln!("Error: rpc_url is required. Set via --rpc-url, RPC_URL env, or config.toml");
        std::process::exit(1);
    }

    // Initialize tracing
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();

    // Log startup info — redact RPC URL to prevent API key leakage in logs.
    // RPC URLs often contain API keys (e.g., ?api-key=SECRET).
    let rpc_host = redact_rpc_url(&config.rpc_url);
    info!(
        rpc = %rpc_host,
        listen = %config.listen,
        "flow-trades starting"
    );

    // Initialize RPC client
    let rpc = Arc::new(RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    // Initialize pool cache and registry.
    // Geyser keeps state fresh — TTL disabled to avoid unnecessary eviction.
    let cache = Arc::new(PoolCache::new(u64::MAX));
    let registry = Arc::new(PoolRegistry::new());

    // Open SQLite pool database
    let pool_db = Arc::new(
        PoolDb::open(&config.pool_db_path).expect("failed to open SQLite pool database"),
    );
    info!(path = %config.pool_db_path, "SQLite pool database opened");

    // Bootstrap: SQLite -> warm storage -> snapshot -> pools_file -> empty
    let loaded = try_bootstrap(&config, &registry, &cache, &pool_db).await;
    info!(pools = loaded, cached = cache.len(), total_registry = registry.len(), "bootstrap complete");

    // Spawn stale pool pruning task
    {
        let prune_db = Arc::clone(&pool_db);
        let prune_reg = Arc::clone(&registry);
        let prune_cache = Arc::clone(&cache);
        let prune_interval = config.prune_interval_secs;
        let prune_age = config.prune_max_age_days * 86400;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(prune_interval)).await;
                match prune_db.prune_stale_and_get_addresses(prune_age) {
                    Ok(addresses) if !addresses.is_empty() => {
                        let count = addresses.len();
                        for addr in &addresses {
                            prune_reg.remove(addr);
                            // Also clear from cache (no point keeping state of pruned pools)
                            prune_cache.remove(addr);
                        }
                        tracing::info!(
                            pruned = count,
                            remaining = prune_reg.len(),
                            "pruned stale pools (not seen in {}d)",
                            prune_age / 86400
                        );
                    }
                    Ok(_) => {} // Nothing to prune
                    Err(e) => {
                        tracing::warn!(error = %e, "stale pool pruning failed");
                    }
                }
            }
        });
    }

    // Create account mirror (shared between quoter and stream manager)
    let account_mirror = Arc::new(AccountMirror::new());

    // Pre-warm mirror: register vault pubkeys from cached pool states so the
    // initial Geyser subscription includes them. Eliminates cold-start RPC for
    // vault balances on first quote after restart.
    {
        let mut vault_count = 0usize;
        for (pool_address, state) in cache.all_entries() {
            let vaults = flow_trades::stream::geyser::extract_vault_pubkeys(&state);
            for vault in &vaults {
                account_mirror.register_vault(*vault, pool_address);
                vault_count += 1;
            }
        }
        if vault_count > 0 {
            info!(vaults = vault_count, "pre-warmed vault mirror from cached pool states");
        }
    }

    // Create quoter (with mirror for zero-RPC vault balance lookups)
    let mut quoter = Quoter::with_mirror(
        Arc::clone(&registry),
        Arc::clone(&cache),
        Arc::clone(&rpc),
        Arc::clone(&account_mirror),
    );
    if config.geyser_endpoint.is_none() {
        // No account stream: the block-driven refresher keeps every TRADED pool
        // one block fresh; a quiet pool older than this is re-read in the
        // background while the quote answers from memory (never on the quote path).
        quoter.revalidate_after = Some(Duration::from_secs(10));
        info!("quoter: no Geyser — block-driven refresh + background revalidation of pools older than 10s");
    }
    let quoter = Arc::new(quoter);

    // Create blockhash cache (2s max age, refreshed via Geyser gRPC)
    let blockhash_cache = Arc::new(BlockhashCache::new(2000));

    // Load Address Lookup Tables (for v0 versioned transactions)
    let alt_addresses: Vec<Pubkey> = config
        .alt_addresses
        .as_ref()
        .map(|s| {
            s.split(',')
                .filter_map(|a| {
                    let trimmed = a.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    match Pubkey::from_str(trimmed) {
                        Ok(pk) => Some(pk),
                        Err(e) => {
                            tracing::warn!(address = trimmed, error = %e, "invalid ALT address, skipping");
                            None
                        }
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let alt_cache = Arc::new(AltCache::new());
    if !alt_addresses.is_empty() {
        let loaded = alt_cache.load_alts(&rpc, &alt_addresses).await;
        info!(
            loaded,
            total = alt_addresses.len(),
            addresses = alt_cache.total_addresses(),
            "loaded Address Lookup Tables"
        );

        // Spawn background ALT refresh
        AltCache::spawn_refresh(
            Arc::clone(&alt_cache),
            Arc::clone(&rpc),
            alt_addresses,
            Duration::from_secs(config.alt_refresh_interval_secs),
        );
    }

    // Geyser gRPC streaming — real-time pool state, vault balances, and blockhash.
    let stream_config = StreamConfig {
        geyser_endpoint: config.geyser_endpoint.clone(),
        geyser_token: config.geyser_token.clone(),
    };
    let stream_stats = Arc::new(StreamStats::new());

    // Optional swap stream: parses every confirmed DEX swap off the Geyser
    // block updates and broadcasts to /swap-stream subscribers. When
    // disabled, neither the parser nor the oracle background task runs.
    let (swap_broadcast, price_oracle) = if config.swap_stream_enabled {
        let oracle = Arc::new(PriceOracle::new());

        // Initial refresh at startup, then a 10s tokio loop. Run on a blocking
        // thread so ureq doesn't tie up an async worker.
        let oracle_init = Arc::clone(&oracle);
        match tokio::task::spawn_blocking(move || oracle_init.refresh()).await {
            Ok(Ok(price)) => info!(price = price, "SOL/USD price initialized"),
            Ok(Err(e)) => tracing::warn!(error = %e, "Could not fetch initial SOL price, will retry"),
            Err(e) => tracing::warn!(error = %e, "Initial SOL price fetch task panicked"),
        }

        // Periodic refresh.
        let oracle_refresh = Arc::clone(&oracle);
        let refresh_secs = config.sol_price_refresh_secs;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(refresh_secs)).await;
                let oracle = Arc::clone(&oracle_refresh);
                let result = tokio::task::spawn_blocking(move || oracle.refresh()).await;
                match result {
                    Ok(Err(e)) => tracing::warn!(error = %e, "SOL price refresh failed"),
                    Err(e) => tracing::warn!(error = %e, "SOL price refresh task panicked"),
                    _ => {}
                }
            }
        });

        let (tx, _rx) = tokio::sync::broadcast::channel::<Arc<flow_trades::stream::swap_stream::Swap>>(
            config.swap_stream_buffer_size,
        );
        info!(
            buffer = config.swap_stream_buffer_size,
            "swap stream enabled — clients subscribe at /swap-stream"
        );
        (Some(tx), Some(oracle))
    } else {
        info!("swap stream disabled");
        (None, None)
    };

    let stream_manager_inner = StreamManager::new(
        Arc::clone(&cache),
        Arc::clone(&registry),
        Arc::clone(&rpc),
        stream_config,
        Arc::clone(&stream_stats),
        Some(Arc::clone(&pool_db)),
        Arc::clone(&account_mirror),
    );
    let stream_manager_inner = match (swap_broadcast.as_ref(), price_oracle.as_ref()) {
        (Some(tx), Some(oracle)) => stream_manager_inner.with_swap_stream(SwapStreamCtx {
            tx: tx.clone(),
            oracle: Arc::clone(oracle),
        }),
        _ => stream_manager_inner,
    };
    let stream_manager = Arc::new(stream_manager_inner);

    if config.geyser_endpoint.is_some() {
        let geyser_manager = Arc::clone(&stream_manager);
        tokio::spawn(async move {
            geyser_manager.spawn().await;
        });
        info!("pool state: Geyser gRPC streaming active");
    }

    // RPC blockSubscribe fallback. Geyser is the primary block source, but
    // if no `geyser_endpoint` is configured, the manager's block-handling
    // branch never fires — pool discovery and (when enabled) the swap stream
    // would silently produce nothing. blockSubscribe via the configured RPC
    // gives equivalent block coverage (slightly higher latency) without an
    // extra dependency. Disable explicitly with `--block-scan-enabled false`.
    if config.block_scan_enabled && config.geyser_endpoint.is_none() {
        let rpc_for_scanner = Arc::clone(&rpc);
        let registry_for_scanner = Arc::clone(&registry);
        let cache_for_scanner = Arc::clone(&cache);
        let stats_for_scanner = Arc::clone(&stream_stats);
        let pool_db_for_scanner = Arc::clone(&pool_db);
        let mirror_for_scanner = Arc::clone(&account_mirror);
        let interval_ms = config.block_scan_interval_ms;
        let scanner_swap_ctx = match (swap_broadcast.as_ref(), price_oracle.as_ref()) {
            (Some(tx), Some(oracle)) => Some(flow_trades::stream::block_scanner::SwapStreamCtx {
                tx: tx.clone(),
                oracle: Arc::clone(oracle),
            }),
            _ => None,
        };
        tokio::spawn(async move {
            flow_trades::stream::block_scanner::run_block_scanner(
                rpc_for_scanner,
                registry_for_scanner,
                cache_for_scanner,
                stats_for_scanner,
                pool_db_for_scanner,
                interval_ms,
                scanner_swap_ctx,
                Some(mirror_for_scanner),
            )
            .await;
        });
        info!("block scanner active — RPC blockSubscribe fallback for pool discovery + swap stream");
    }

    // Blockhash refresh: via Geyser gRPC (zero RPC) when configured, otherwise
    // the RPC poller — without it the no-Geyser deployment would run a Geyser
    // reconnect loop against an empty endpoint and every /swap would take the
    // cache-miss RPC fallback.
    match config.geyser_endpoint.clone() {
        Some(endpoint) => {
            Arc::clone(&blockhash_cache).spawn_geyser_refresh(
                endpoint,
                config.geyser_token.clone(),
                Duration::from_millis(400),
            );
            info!("blockhash: refreshing via Geyser gRPC");
        }
        None => {
            Arc::clone(&blockhash_cache).spawn_refresh(Arc::clone(&rpc), Duration::from_millis(400));
            info!("blockhash: refreshing via RPC (no Geyser endpoint)");
        }
    }

    // Spawn dormant pool retry task (every 1 hour, retry pools that have been demoted)
    {
        let dormant_rpc = Arc::clone(&rpc);
        let dormant_cache = Arc::clone(&cache);
        let dormant_registry = Arc::clone(&registry);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                let dormant = dormant_cache.dormant_addresses();
                if dormant.is_empty() {
                    continue;
                }
                tracing::info!(count = dormant.len(), "retrying dormant pools");
                let mut promoted = 0;
                for addr in &dormant {
                    let pool_type = match dormant_registry.get(addr) {
                        Some(entry) => entry.pool_type,
                        None => continue,
                    };
                    match flow_trades::pool::fetcher::fetch_pool_state(
                        &dormant_rpc,
                        pool_type,
                        addr,
                    )
                    .await
                    {
                        Ok(state) => {
                            dormant_cache.insert(*addr, state);
                            dormant_cache.reset_failure(addr);
                            promoted += 1;
                        }
                        Err(_) => {
                            // Still dead, leave as dormant
                        }
                    }
                }
                if promoted > 0 {
                    tracing::info!(
                        promoted,
                        remaining_dormant = dormant_cache.dormant_count(),
                        "promoted dormant pools back to active"
                    );
                }
            }
        });
    }

    // Spawn periodic persistence
    storage::warm::spawn_periodic_save(
        Arc::clone(&registry),
        Arc::clone(&cache),
        config.warm_storage_path.clone(),
        config.warm_save_interval_secs,
    );
    storage::snapshot::spawn_periodic_save(
        Arc::clone(&registry),
        config.snapshot_path.clone(),
        config.snapshot_interval_secs,
    );

    // pump.fun AMM fee tiers (market-cap keyed) — read from the fee program's
    // config so quotes carry the fee each pool actually charges.
    match flow_trades::execution::amms::pumpfun_amm::load_fee_tiers(&rpc).await {
        Ok(n) => info!(tiers = n, "pump.fun AMM fee tiers loaded from chain"),
        Err(e) => tracing::warn!(error = %e, "pump.fun AMM fee tiers: using built-in table"),
    }
    match flow_trades::quote::pump_bonding::load_fee_tiers(&rpc).await {
        Ok(n) => info!(tiers = n, "pump.fun bonding fee tiers loaded from chain"),
        Err(e) => tracing::warn!(error = %e, "pump.fun bonding fee tiers: using built-in table"),
    }

    // Mint facts (token program, Token-2022 transfer fee) for every known pool
    // mint, off the quote path; new pools get theirs at discovery.
    {
        let rpc = Arc::clone(&rpc);
        let mints: Vec<Pubkey> = registry.entries().iter().flat_map(|e| [e.mint_a, e.mint_b]).filter(|m| *m != Pubkey::default()).collect();
        tokio::spawn(async move {
            let n = flow_trades::pool::mints::ensure_mint_info(&rpc, &mints).await;
            info!(mints = n, "mint info loaded (token program + transfer fees)");
        });
    }

    // Build router config — reads config PDA from on-chain program.
    // Fee ATAs are auto-created idempotently on first swap per mint.
    let router_config = {
        use flow_trades::execution::router::RouterConfig;
        let referral_account = config.referral_account.as_ref().map(|a| {
            Pubkey::from_str(a).expect("Invalid REFERRAL_ACCOUNT")
        });
        let router_program_id = match config.router_program_id.as_deref() {
            Some(s) => Pubkey::from_str(s).expect("Invalid ROUTER_PROGRAM_ID"),
            None => flow_trades::constants::FLOW_ROUTER_PROGRAM_ID,
        };
        // Read treasury_wallet from the on-chain config PDA
        let config_pda = flow_trades::execution::router::config_pda(&router_program_id);
        match rpc.get_account(&config_pda).await {
            Ok(acct) if acct.data.len() >= 76 => {
                let fee_bps = u16::from_le_bytes(acct.data[40..42].try_into().unwrap());
                let treasury_wallet = Pubkey::new_from_array(acct.data[42..74].try_into().unwrap());
                flow_trades::quote::router::set_platform_fee_bps(fee_bps);
                let rc = RouterConfig {
                    program_id: router_program_id,
                    treasury_wallet,
                    referral_wallet: referral_account,
                    fee_bps,
                };
                info!(
                    program = %router_program_id,
                    layout = ?rc.layout(),
                    config_pda = %config_pda,
                    fee_bps,
                    referral = ?referral_account,
                    "on-chain router active (all swaps routed through the router)"
                );
                Some(rc)
            }
            _ => {
                tracing::warn!("Router config PDA not found — swaps will fail until router program is deployed");
                None
            }
        }
    };

    // Create Prometheus metrics
    let metrics = Arc::new(api::Metrics::new());

    // Create app state
    let state = Arc::new(AppState {
        quoter,
        rpc,
        cache,
        registry,
        blockhash_cache,
        stream_stats,
        alt_cache,
        router_config,
        pool_db,
        metrics,
        known_fee_atas: dashmap::DashSet::new(),
        account_mirror,
        mint_program_cache: Arc::new(dashmap::DashMap::new()),
        swap_broadcast,
        price_oracle,
    });

    // Build CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Build router
    let app = api::router(state).layer(cors);

    // Start server
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .expect("Failed to bind listener");

    info!(addr = %config.listen, "API server listening");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}

/// Redact a URL to just the host, hiding path/query/fragment (which may contain API keys).
/// Example: "https://mainnet.helius-rpc.com/?api-key=SECRET" -> "https://mainnet.helius-rpc.com/***"
fn redact_rpc_url(url: &str) -> String {
    match url.find("://") {
        Some(scheme_end) => {
            let after_scheme = &url[scheme_end + 3..];
            // Find the first '/' or '?' or end of string
            let host_end = after_scheme
                .find(|c: char| c == '/' || c == '?')
                .unwrap_or(after_scheme.len());
            let host = &after_scheme[..host_end];
            if host_end < after_scheme.len() {
                format!("{}://{}/<redacted>", &url[..scheme_end], host)
            } else {
                format!("{}://{}", &url[..scheme_end], host)
            }
        }
        None => "<redacted>".to_string(),
    }
}

/// Try to bootstrap the pool registry and cache from available sources.
/// Tries in order:
/// 1. SQLite pool database (crash-safe, single-file, fast)
/// 2. L2 warm storage (fast — bincode with cached pool states)
/// 3. L3 JSON snapshot
/// 4. Static pools file
/// 5. Empty (block scanner will discover pools from live blocks)
///
/// When pools are loaded from a non-SQLite source, they are persisted to SQLite.
///
/// Returns the number of pools loaded.
async fn try_bootstrap(
    config: &config::Config,
    registry: &PoolRegistry,
    cache: &PoolCache,
    pool_db: &PoolDb,
) -> usize {
    // 1. Try SQLite first (fastest local — crash-safe)
    match pool_db.load_all() {
        Ok(entries) if !entries.is_empty() => {
            let count = entries.len();
            for e in entries {
                registry.add(e);
            }
            info!(
                pools = count,
                source = "sqlite",
                "bootstrap: loaded pools from SQLite"
            );
            return count;
        }
        Ok(_) => {
            tracing::debug!("SQLite empty, trying warm storage");
        }
        Err(e) => {
            tracing::warn!(error = %e, "SQLite load failed, trying warm storage");
        }
    }

    // 2. Try L2 warm file (bincode with cached pool states)
    match storage::warm::load(&config.warm_storage_path) {
        Ok((entries, states)) => {
            let count = entries.len();
            for e in &entries {
                registry.add(e.clone());
            }
            for (addr, state) in states {
                cache.insert(addr, state);
            }
            info!(
                pools = count,
                cached = cache.len(),
                source = "warm",
                "bootstrap: loaded from warm storage"
            );
            // Persist to SQLite
            if let Err(e) = pool_db.insert_pools(&entries) {
                tracing::warn!(error = %e, "failed to persist warm pools to SQLite");
            }
            return count;
        }
        Err(e) => {
            tracing::debug!(error = %e, "warm storage not available, trying snapshot");
        }
    }

    // 3. Try L3 JSON snapshot (slower — JSON, no cached states)
    match storage::snapshot::load(&config.snapshot_path) {
        Ok(entries) => {
            let count = entries.len();
            for e in &entries {
                registry.add(e.clone());
            }
            info!(
                pools = count,
                source = "snapshot",
                "bootstrap: loaded from JSON snapshot"
            );
            // Persist to SQLite
            if let Err(e) = pool_db.insert_pools(&entries) {
                tracing::warn!(error = %e, "failed to persist snapshot pools to SQLite");
            }
            return count;
        }
        Err(e) => {
            tracing::debug!(error = %e, "snapshot not available, trying pools file");
        }
    }

    // 4. Try static pools file
    if let Some(ref path) = config.pools_file {
        match PoolRegistry::load_from_json(path) {
            Ok(loaded) => {
                let count = loaded.len();
                let entries = loaded.entries();
                for e in &entries {
                    registry.add(e.clone());
                }
                info!(
                    pools = count,
                    source = "pools_file",
                    path,
                    "bootstrap: loaded from pools file"
                );
                // Persist to SQLite
                if let Err(e) = pool_db.insert_pools(&entries) {
                    tracing::warn!(error = %e, "failed to persist pools file to SQLite");
                }
                return count;
            }
            Err(e) => {
                tracing::warn!(error = %e, path, "failed to load pools file");
            }
        }
    }

    info!("bootstrap: starting with empty registry — block scanner will discover pools");
    0
}
