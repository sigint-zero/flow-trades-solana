use std::str::FromStr;
use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

use crate::constants::{SOL_NATIVE_MINT, TOKEN_PROGRAM_ID};
use crate::error::{TradeError, TradeResult};
use crate::execution::amms::AmmExecutorType;
use crate::execution::router::wrap_swap;
use crate::execution::simulator::{simulate_versioned, cu_with_headroom};
use crate::execution::tx_builder::{build_unsigned_versioned_tx, TxBuildConfig};
use crate::pool::fetcher::{ensure_pamm_fee_accounts, fetch_pool_state, refresh_pamm_reserves};
use crate::pool::types::{PoolType, SwapInstructions, SwapOrder};
use crate::quote::QuoteResponse;

use super::AppState;

/// POST /swap request body.
#[derive(Debug, Deserialize)]
pub struct SwapRequest {
    pub wallet: String,
    pub quote: QuoteResponse,
    pub auto_wrap_sol: Option<bool>,
    pub compute_price: Option<u64>,
    pub priority_fee: Option<u64>,
    /// Explicit compute unit limit. Takes priority over simulation and defaults.
    pub compute_limit: Option<u32>,
    /// Simulate the transaction via RPC before returning it.
    /// When true, the response includes a `simulation` field with CU consumed, logs, etc.
    /// Also sets compute_limit to the simulated value (+ headroom) unless `compute_limit` is set.
    pub simulate: Option<bool>,
    /// Deprecated alias for `simulate`. Use `simulate` instead.
    pub dynamic_cu: Option<bool>,
    /// Optional tip: appended as the last instruction (SOL transfer to the given address).
    /// Used for Jito bundles, MEV tips, or any destination. Must be last instruction.
    pub tip: Option<TipRequest>,
    /// Transaction format: `0` (default) = v0 with lookup tables, legacy fallback,
    /// 1,232-byte limit; `1` = SIMD-0385 v1 — 4,096 bytes, every account inline
    /// (no lookup tables), compute budget in the header. Use 1 for multi-hop
    /// routes that do not fit v0. The signer must support v1 (first byte 0x81).
    pub tx_version: Option<u8>,
}

/// Tip configuration — appended as a SOL transfer as the very last instruction.
#[derive(Debug, Deserialize)]
pub struct TipRequest {
    /// Destination address (base58).
    pub address: String,
    /// Amount in lamports.
    pub lamports: u64,
}

/// POST /swap response body.
#[derive(Debug, Serialize)]
pub struct SwapResponse {
    pub transaction: String,
    /// 0 (legacy/v0) or 1 — what `transaction` decodes as.
    pub tx_version: u8,
    pub block_height: u64,
    pub compute_limit: u32,
    pub priority_fee: u64,
    /// Simulation result, present only when `simulate: true` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simulation: Option<SimulationResponse>,
}

/// Simulation results returned to the caller.
#[derive(Debug, Clone, Serialize)]
pub struct SimulationResponse {
    pub success: bool,
    pub units_consumed: u64,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

/// POST /swap — build an unsigned transaction from a quote.
pub async fn handle_swap(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SwapRequest>,
) -> Result<Json<SwapResponse>, TradeError> {
    let (swap_ixs, mut tx_config, user_pubkey) = build_swap_from_quote(&state, &req).await?;

    // Get recent blockhash — try cache first (refreshed by Geyser or RPC background task).
    // Validate the hash is non-default (Geyser may return empty on some nodes).
    let (blockhash, last_valid_block_height) = match state.blockhash_cache.get().await {
        Some((hash, height)) if hash != solana_sdk::hash::Hash::default() => (hash, height),
        _ => {
            tracing::warn!("blockhash cache miss or invalid — falling back to RPC");
            state
                .rpc
                .get_latest_blockhash_with_commitment(
                    solana_sdk::commitment_config::CommitmentConfig::confirmed(),
                )
                .await
                .map_err(|e| TradeError::Rpc(format!("get_latest_blockhash: {e}")))?
        }
    };

    let should_simulate = req.simulate.unwrap_or(false) || req.dynamic_cu.unwrap_or(false);

    match req.tx_version.unwrap_or(0) {
        0 => {}
        1 => return handle_swap_v1(&state, &req, swap_ixs, tx_config, &user_pubkey, blockhash, last_valid_block_height, should_simulate).await,
        v => return Err(TradeError::Validation(format!("unsupported tx_version {v} (0 or 1)"))),
    }

    // Build versioned transaction (v0 with ALTs if available, else legacy)
    let alt_tables = state.alt_cache.all_tables();
    let vtx = build_unsigned_versioned_tx(
        &swap_ixs,
        &user_pubkey,
        &tx_config,
        blockhash,
        &alt_tables,
    )?;

    // Optionally simulate the transaction
    let simulation = if should_simulate {
        match simulate_versioned(&state.rpc, &vtx).await {
            Ok(sim) => {
                // If user didn't set an explicit compute_limit, use simulated CU + headroom
                if req.compute_limit.is_none() && sim.success && sim.units_consumed > 0 {
                    let tight_cu = cu_with_headroom(sim.units_consumed);
                    tx_config.compute_unit_limit = tight_cu;
                }
                Some(SimulationResponse {
                    success: sim.success,
                    units_consumed: sim.units_consumed,
                    error: sim.error,
                    logs: sim.logs,
                })
            }
            Err(e) => {
                // Simulation RPC failure — return the error but still provide the TX
                tracing::warn!(error = %e, "simulation RPC call failed");
                Some(SimulationResponse {
                    success: false,
                    units_consumed: 0,
                    error: Some("simulation RPC call failed".to_string()),
                    logs: vec![],
                })
            }
        }
    } else {
        None
    };

    // If simulation changed the CU limit, rebuild the TX with the updated compute budget
    let final_vtx = if should_simulate && req.compute_limit.is_none() {
        build_unsigned_versioned_tx(
            &swap_ixs,
            &user_pubkey,
            &tx_config,
            blockhash,
            &alt_tables,
        )?
    } else {
        vtx
    };

    // Serialize VersionedTransaction to base64
    let tx_bytes = bincode::serialize(&final_vtx)
        .map_err(|e| TradeError::Internal(format!("serialize tx: {e}")))?;
    if tx_bytes.len() > MAX_TX_BYTES {
        return Err(TradeError::Validation(format!(
            "transaction is {} bytes, over the {MAX_TX_BYTES}-byte limit — the route's accounts are not \
             covered by the configured lookup tables; quote with direct_only=true, or request \
             tx_version=1 (4096-byte v1 transactions, no lookup tables)",
            tx_bytes.len()
        )));
    }
    let tx_base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes);

    Ok(Json(SwapResponse {
        transaction: tx_base64,
        tx_version: 0,
        block_height: last_valid_block_height,
        compute_limit: tx_config.compute_unit_limit,
        priority_fee: tx_config.priority_fee_lamports,
        simulation,
    }))
}

/// `/swap` with `tx_version: 1`: the same instructions encoded as a v1
/// transaction (see `execution::tx_v1`). Simulation goes through the raw-bytes
/// RPC path since `solana_sdk` has no v1 type.
#[allow(clippy::too_many_arguments)]
async fn handle_swap_v1(
    state: &AppState,
    req: &SwapRequest,
    swap_ixs: SwapInstructions,
    mut tx_config: TxBuildConfig,
    user_pubkey: &Pubkey,
    blockhash: solana_sdk::hash::Hash,
    last_valid_block_height: u64,
    should_simulate: bool,
) -> Result<Json<SwapResponse>, TradeError> {
    use crate::execution::simulator::simulate_raw;
    use crate::execution::tx_builder::assemble_instructions;
    use crate::execution::tx_v1::{encode_unsigned_v1, V1Budget};

    let budget = |c: &TxBuildConfig| V1Budget {
        compute_unit_limit: c.compute_unit_limit,
        priority_fee_lamports: c.priority_fee_lamports,
    };
    let instructions = assemble_instructions(&swap_ixs, &tx_config)?;
    let mut tx_bytes = encode_unsigned_v1(&instructions, user_pubkey, blockhash, budget(&tx_config))?;

    let simulation = if should_simulate {
        match simulate_raw(&state.rpc, &tx_bytes).await {
            Ok(sim) => {
                if req.compute_limit.is_none() && sim.success && sim.units_consumed > 0 {
                    tx_config.compute_unit_limit = cu_with_headroom(sim.units_consumed);
                    tx_bytes = encode_unsigned_v1(&instructions, user_pubkey, blockhash, budget(&tx_config))?;
                }
                Some(SimulationResponse {
                    success: sim.success,
                    units_consumed: sim.units_consumed,
                    error: sim.error,
                    logs: sim.logs,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "v1 simulation RPC call failed");
                Some(SimulationResponse {
                    success: false,
                    units_consumed: 0,
                    error: Some(format!("simulation RPC call failed: {e}")),
                    logs: vec![],
                })
            }
        }
    } else {
        None
    };

    Ok(Json(SwapResponse {
        transaction: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_bytes),
        tx_version: 1,
        block_height: last_valid_block_height,
        compute_limit: tx_config.compute_unit_limit,
        priority_fee: tx_config.priority_fee_lamports,
        simulation,
    }))
}

/// Shared logic: parse a SwapRequest into SwapInstructions + TxBuildConfig + user Pubkey.
/// Handles both single-hop and multi-hop (2-hop) route plans.
pub(crate) async fn build_swap_from_quote(
    state: &AppState,
    req: &SwapRequest,
) -> TradeResult<(SwapInstructions, TxBuildConfig, Pubkey)> {
    let user_pubkey = Pubkey::from_str(&req.wallet)
        .map_err(|_| TradeError::Validation(format!("invalid wallet: {}", req.wallet)))?;

    let quote = &req.quote;

    if quote.routes.is_empty() {
        return Err(TradeError::Validation("quote.routes is empty".into()));
    }

    let swap_ixs = if quote.routes.len() == 1 {
        // Single-hop route
        build_single_hop_ixs(state, quote, &user_pubkey).await?
    } else if quote.routes.len() == 2 {
        // Two-hop route
        build_two_hop_ixs(state, quote, &user_pubkey).await?
    } else if quote.routes.len() == 3 {
        // Three-hop route
        build_three_hop_ixs(state, quote, &user_pubkey).await?
    } else {
        return Err(TradeError::Validation(format!(
            "unsupported routes length: {} (max 3)",
            quote.routes.len()
        )));
    };

    // Wrap ALL swaps in flow-router CPI for on-chain fee enforcement.
    let swap_ixs = wrap_in_router(state, swap_ixs, quote, &user_pubkey).await?;

    // Build transaction config
    // Priority: user-defined compute_limit > simulation (handled later) > hop-based default
    let priority_fee = req.priority_fee.unwrap_or(5_000);
    let compute_limit = if let Some(user_cu) = req.compute_limit {
        user_cu
    } else if quote.routes.len() == 3 {
        800_000
    } else if quote.routes.len() == 2 {
        600_000
    } else {
        400_000
    };
    // A Meteora DLMM hop's cost grows with the bins it walks (a full array is
    // ≈ 0.5M CU): budget the walk its quote made.
    let compute_limit = if req.compute_limit.is_none() { compute_limit.max(dlmm_route_compute_units(quote)) } else { compute_limit };

    let tx_config = TxBuildConfig {
        compute_unit_limit: compute_limit,
        priority_fee_lamports: priority_fee,
    };

    // Append tip as the very last instruction (after cleanup)
    let swap_ixs = if let Some(ref tip) = req.tip {
        let tip_dest = Pubkey::from_str(&tip.address)
            .map_err(|_| TradeError::Validation(format!("invalid tip address: {}", tip.address)))?;
        if tip.lamports == 0 {
            return Err(TradeError::Validation("tip lamports must be > 0".into()));
        }
        let tip_ix = solana_sdk::system_instruction::transfer(&user_pubkey, &tip_dest, tip.lamports);
        let mut ixs = swap_ixs;
        ixs.cleanup.push(tip_ix);
        ixs
    } else {
        swap_ixs
    };

    Ok((swap_ixs, tx_config, user_pubkey))
}

/// Compute units for a route with Meteora DLMM hops: each DLMM hop's own
/// estimate (`quote::dlmm::estimate_compute_units`) plus 200k per other hop,
/// capped at the 1.4M transaction maximum. 0 when no hop is DLMM.
fn dlmm_route_compute_units(quote: &QuoteResponse) -> u32 {
    let mut total: u32 = 0;
    let mut any = false;
    for r in &quote.routes {
        let est = (r.pool.dex == "Meteora DLMM")
            .then(|| {
                let pool = Pubkey::from_str(&r.pool.pool_address).ok()?;
                let input = Pubkey::from_str(&r.pool.input_token).ok()?;
                crate::quote::dlmm::estimate_compute_units(&pool, &input, r.pool.amount_in.parse().ok()?)
            })
            .flatten();
        any |= est.is_some();
        total = total.saturating_add(est.unwrap_or(200_000));
    }
    if any { total.min(1_400_000) } else { 0 }
}

/// Wrap ALL swap instructions in the flow-router CPI for on-chain fee enforcement.
/// Every swap must go through the router — no unwrapped swaps allowed.
///
/// Fee is ALWAYS taken from the OUTPUT token, same as Jupiter.
/// Treasury + referral ATAs are auto-created on the first swap for each mint
/// (user pays ~0.002 SOL rent once). Cached after first creation.
///
/// Handles single-hop (1 route), 2-hop (2 routes), and 3-hop (3 routes).
async fn wrap_in_router(
    state: &AppState,
    ixs: SwapInstructions,
    quote: &QuoteResponse,
    user: &Pubkey,
) -> TradeResult<SwapInstructions> {
    let router = state.router_config.as_ref().ok_or_else(|| {
        TradeError::Execution("router not configured — all swaps must route through the fee wrapper".into())
    })?;

    let input_mint = Pubkey::from_str(&quote.input_token)
        .map_err(|_| TradeError::Validation("invalid input_token".into()))?;
    let output_mint = Pubkey::from_str(&quote.output_token)
        .map_err(|_| TradeError::Validation("invalid output_token".into()))?;

    let amount_in: u64 = quote.amount_in.parse().unwrap_or(0);
    let min_amount_out: u64 = quote.minimum_out.parse().unwrap_or(0);

    let fee_mint = &output_mint;

    let input_tp = get_token_program(state, &input_mint).await?;
    let output_tp = get_token_program(state, &output_mint).await?;
    let fee_tp = output_tp;

    let user_input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        user, &input_mint, &input_tp,
    );
    let user_output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        user, &output_mint, &output_tp,
    );

    // Zero-fee router: it never reads the fee-account slots, so pass the
    // treasury wallet itself and create nothing (saves the rent and the
    // existence check per mint).
    let zero_fee = router.fee_bps == 0;
    let protocol_fee_acct = if zero_fee { router.treasury_wallet } else { router.fee_account_for_mint(fee_mint, &fee_tp) };
    let referral_ata = if zero_fee { None } else { router.referral_account_for_mint(fee_mint, &fee_tp) };

    // Auto-create treasury + referral ATAs unless they are KNOWN to exist
    // on-chain. `known_fee_atas` is a positive cache filled only after an RPC
    // read confirmed the account: this endpoint returns UNSIGNED transactions
    // that the caller may never send, so "we emitted a create instruction
    // once" proves nothing — marking the ATA known at build time left every
    // later swap on that mint without the create, and the router then failed
    // with `protocol fee account data too short` (InvalidAccount, 6).
    let mut fee_setup = Vec::new();
    if !zero_fee && !fee_ata_exists(state, &protocol_fee_acct).await {
        fee_setup.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                user, &router.treasury_wallet, fee_mint, &fee_tp,
            )
        );
    }
    if let (Some(ref wallet), Some(ref r_ata)) = (&router.referral_wallet, &referral_ata) {
        if !fee_ata_exists(state, r_ata).await {
            fee_setup.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    user, wallet, fee_mint, &fee_tp,
                )
            );
        }
    }

    let num_hops = quote.routes.len();
    // A hop may take more than one CPI (a native-SOL venue wraps its output
    // for the router), never fewer.
    if ixs.swap.len() < num_hops {
        return Err(TradeError::Execution(format!(
            "route has {} hops but {} swap instructions", num_hops, ixs.swap.len()
        )));
    }

    // Build token account list: [input, intermediate_1..N-1, output]
    let mut token_accounts = vec![user_input_ata];
    for i in 0..num_hops.saturating_sub(1) {
        let bridge_mint = Pubkey::from_str(&quote.routes[i].pool.output_token)
            .map_err(|_| TradeError::Validation(format!("invalid bridge mint at hop {i}")))?;
        let bridge_tp = get_token_program(state, &bridge_mint).await?;
        token_accounts.push(
            spl_associated_token_account::get_associated_token_address_with_program_id(
                user, &bridge_mint, &bridge_tp,
            )
        );
    }
    // The router wants one account per CPI boundary but reads only the last
    // (output) one; extra CPIs get the output account as a placeholder.
    token_accounts.extend(std::iter::repeat_n(user_output_ata, ixs.swap.len() - num_hops));
    token_accounts.push(user_output_ata);

    let router_ix = wrap_swap(
        router,
        user,
        &token_accounts,
        &protocol_fee_acct,
        referral_ata.as_ref(),
        &ixs.swap,
        amount_in,
        min_amount_out,
        &fee_tp,
        fee_mint,
    )?;

    let mut setup = fee_setup;
    setup.extend(ixs.setup);

    Ok(SwapInstructions {
        setup,
        swap: vec![router_ix],
        cleanup: ixs.cleanup,
    })
}

/// True when the fee ATA exists on-chain. Positive results are cached for the
/// process lifetime (an ATA is never closed by us); negatives cost one
/// `getAccountInfo` per swap until the first executed swap creates it.
async fn fee_ata_exists(state: &AppState, ata: &Pubkey) -> bool {
    if state.known_fee_atas.contains(ata) {
        return true;
    }
    match state.rpc.get_account(ata).await {
        Ok(acct) if acct.data.len() >= 64 => {
            state.known_fee_atas.insert(*ata);
            true
        }
        _ => false,
    }
}

/// Solana's transaction size limit (one IPv6 MTU packet).
pub const MAX_TX_BYTES: usize = 1232;

/// Pool state for swap building: cache hit, else RPC fetch + cache. For a
/// pump.fun AMM pool whose buyback remaining-accounts are still unresolved
/// (inline Geyser parse, or a pool with no swap at fetch time) this resolves
/// them now and writes the enriched state back, so the resolve happens once
/// per pool rather than once per `/swap`.
async fn load_pool_state(
    state: &AppState,
    pool_type: PoolType,
    pool_address: &Pubkey,
) -> TradeResult<crate::pool::types::PoolState> {
    let mut pool_state = match state.cache.get(pool_address) {
        Some(s) => s,
        None => {
            let s = fetch_pool_state(&state.rpc, pool_type, pool_address).await?;
            state.cache.insert(*pool_address, s.clone());
            s
        }
    };
    let mut dirty = false;
    if pool_state.needs_pamm_fee_accounts() && !crate::execution::amms::pumpfun_amm::recipients_loaded() {
        // else: leave it — the executor refuses with a clear error, and the
        // next /swap retries the resolve (the pool may have a swap by then).
        dirty |= ensure_pamm_fee_accounts(&state.rpc, &mut pool_state).await;
    }
    // A pAMM buy is exact-output against the reserves we hold. With Geyser an
    // active pool's entry is re-inserted on every change, so it is fresh; a
    // quiet pool, or any pool without Geyser (blockSubscribe fallback), ages —
    // and stale reserves on a moving pool revert with ExceededSlippage (6004).
    // Stale state (no Geyser, or a quiet pool) is re-read before building:
    // reserves for pAMM, the whole state elsewhere (CLMM tick_current drives the
    // tick-array accounts — a stale one is "Invalid first tick array").
    if state.cache.get_with_age(pool_address).is_some_and(|age| age > PAMM_RESERVE_MAX_AGE) {
        let refreshed = if pool_type == PoolType::PumpFunAmm {
            refresh_pamm_reserves(&state.rpc, &mut pool_state).await
        } else {
            fetch_pool_state(&state.rpc, pool_type, pool_address).await.map(|mut fresh| {
                fresh.carry_over_pamm_fee_accounts(&pool_state);
                pool_state = fresh;
            })
        };
        match refreshed {
            Ok(()) => dirty = true,
            Err(e) => tracing::warn!(pool = %pool_address, error = %e, "pool refresh failed — using cached state"),
        }
    }
    if dirty {
        state.cache.insert(*pool_address, pool_state.clone());
    }
    // DLMM: the executor picks bin arrays + bitmap extension from the pair's
    // bins in memory (normally loaded by the quote that preceded this swap)
    if pool_type == PoolType::MeteoraDlmm && !crate::quote::dlmm::BINS.contains_key(pool_address) {
        if let Err(e) = crate::pool::bins::load_dlmm_bins(&state.rpc, &pool_state).await {
            tracing::warn!(pool = %pool_address, error = %e, "dlmm bin arrays unreadable — swap built from the pair's own bitmap");
        }
    }
    Ok(pool_state)
}

/// Cached pAMM reserves older than this are re-read before a swap is built.
const PAMM_RESERVE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(3);

/// Build swap instructions for a single-hop route.
async fn build_single_hop_ixs(
    state: &AppState,
    quote: &QuoteResponse,
    user_pubkey: &Pubkey,
) -> TradeResult<SwapInstructions> {
    let route = &quote.routes[0];
    let pool_info = &route.pool;

    let pool_address = Pubkey::from_str(&pool_info.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid pool_address: {}", pool_info.pool_address)))?;
    let input_mint = Pubkey::from_str(&pool_info.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid input_token: {}", pool_info.input_token)))?;
    let output_mint = Pubkey::from_str(&pool_info.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid output_token: {}", pool_info.output_token)))?;
    let amount_in: u64 = pool_info.amount_in.parse()
        .map_err(|_| TradeError::Validation(format!("invalid amount_in: {}", pool_info.amount_in)))?;
    let min_amount_out: u64 = quote.minimum_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid minimum_out: {}", quote.minimum_out)))?;

    let pool_type = pool_type_from_label(&pool_info.dex)?;

    let pool_state = load_pool_state(state, pool_type, &pool_address).await?;

    let input_token_program = get_token_program(state, &input_mint).await?;
    let output_token_program = get_token_program(state, &output_mint).await?;

    let order = SwapOrder {
        pool_address,
        pool_type,
        input_mint,
        output_mint,
        amount_in,
        min_amount_out,
        user: *user_pubkey,
        input_token_program,
        output_token_program,
    };

    let executor = AmmExecutorType::from_pool_type(pool_type)?;
    let mut ixs = executor.build_swap_ix(&order, &pool_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs, pool_type, &order.pool_address).await?;
    Ok(ixs)
}


/// Multi-hop assembly hygiene. Each hop's executor is built as if it stood
/// alone, so when SOL is the bridge: hop N (output SOL) and hop N+1 (input
/// SOL) both queue a `close_account` on the user's WSOL ATA (the second one
/// fails with InvalidAccountData on an already-closed account), and hop N+1
/// funds the WSOL ATA from the payer's lamports although hop N just delivered
/// the WSOL — the payer would need the whole bridge amount spare. Drop the
/// funding of later hops and any exact-duplicate instruction.
fn tidy_multihop(setup: Vec<Instruction>, later_hop_setups: Vec<Vec<Instruction>>, cleanup: Vec<Instruction>, user: &Pubkey) -> (Vec<Instruction>, Vec<Instruction>) {
    let wsol_ata = spl_associated_token_account::get_associated_token_address(user, &SOL_NATIVE_MINT);
    let system_program = solana_sdk::system_program::id();
    let is_wsol_funding = |ix: &Instruction| {
        (ix.program_id == system_program && ix.accounts.get(1).is_some_and(|a| a.pubkey == wsol_ata))
            || (ix.program_id == TOKEN_PROGRAM_ID && ix.data == [17u8] && ix.accounts.first().is_some_and(|a| a.pubkey == wsol_ata))
    };
    let mut all_setup = setup;
    for later in later_hop_setups {
        all_setup.extend(later.into_iter().filter(|ix| !is_wsol_funding(ix)));
    }
    (dedupe_instructions(all_setup), dedupe_instructions(cleanup))
}

/// The amount a hop can rely on receiving from the previous one.
use crate::quote::router::guaranteed;

/// pump.fun bonding pays NATIVE SOL; as the first hop of a route its executor
/// wraps `min_amount_out` into WSOL for the next hop, which spends exactly the
/// guaranteed amount — so that is the floor it gets. 0 (no per-hop floor) for
/// every other hop.
fn native_sol_hop_floor(pool_type: PoolType, output_mint: &Pubkey, quoted_out: u64, slippage_bps: u16) -> u64 {
    if pool_type == PoolType::PumpFun && *output_mint == SOL_NATIVE_MINT {
        guaranteed(quoted_out, slippage_bps)
    } else {
        0
    }
}

/// pump.fun bonding spends NATIVE SOL; as a later hop the SOL arrives as WSOL
/// from the previous hop, so the WSOL account is closed (unwrapped into the
/// payer) inside the router, right before the buy. Returns true when it did.
fn unwrap_for_native_sol_hop(pool_type: PoolType, input_mint: &Pubkey, ixs: &mut SwapInstructions, user: &Pubkey) -> bool {
    if pool_type != PoolType::PumpFun || *input_mint != SOL_NATIVE_MINT {
        return false;
    }
    let wsol_ata = spl_associated_token_account::get_associated_token_address(user, &SOL_NATIVE_MINT);
    match spl_token::instruction::close_account(&TOKEN_PROGRAM_ID, &wsol_ata, user, user, &[]) {
        Ok(close) => {
            ixs.swap.insert(0, close);
            true
        }
        Err(_) => false,
    }
}

/// The WSOL account was closed mid-route: a second close would fail.
fn drop_wsol_close(cleanup: &mut Vec<Instruction>, user: &Pubkey) {
    let wsol_ata = spl_associated_token_account::get_associated_token_address(user, &SOL_NATIVE_MINT);
    cleanup.retain(|ix| !(ix.program_id == TOKEN_PROGRAM_ID && ix.data == [9u8] && ix.accounts.first().is_some_and(|a| a.pubkey == wsol_ata)));
}

/// Raydium CLMM / PancakeSwap / Byreal swaps carry the current tick array plus the next
/// two in the swap direction as remaining accounts. A neighbouring array that
/// no LP has ever touched does not exist on chain, and passing it fails the
/// whole swap with `AccountOwnedByWrongProgram` (owner = System). One
/// `getMultipleAccounts` per swap prunes the missing neighbours; the first
/// array (holding the current tick) must exist.
async fn prune_missing_tick_arrays(rpc: &solana_client::nonblocking::rpc_client::RpcClient, ixs: &mut SwapInstructions, pool_type: PoolType, pool: &Pubkey) -> TradeResult<()> {
    if !matches!(pool_type, PoolType::RaydiumCl | PoolType::PancakeSwap | PoolType::Byreal) {
        return Ok(());
    }
    // With the pool's ticks in memory the executor already passed exactly the
    // initialised arrays the program checks — nothing to prune.
    if flow_trades_ticks_loaded(pool) {
        return Ok(());
    }
    let Some(ix) = ixs.swap.first_mut() else { return Ok(()) };
    let n = ix.accounts.len();
    if n < 3 {
        return Ok(());
    }
    let tail: Vec<Pubkey> = ix.accounts[n - 3..].iter().map(|a| a.pubkey).collect();
    let infos = rpc
        .get_multiple_accounts(&tail)
        .await
        .map_err(|e| TradeError::Rpc(format!("tick array lookup: {e}")))?;
    if infos[0].is_none() {
        return Err(TradeError::Execution("current tick array account does not exist — pool state is stale".into()));
    }
    let keep: Vec<bool> = infos.iter().map(|a| a.is_some()).collect();
    let mut idx = 0;
    ix.accounts = ix
        .accounts
        .iter()
        .cloned()
        .filter(|_| {
            let i = idx;
            idx += 1;
            i < n - 3 || keep[i - (n - 3)]
        })
        .collect();
    Ok(())
}

fn flow_trades_ticks_loaded(pool: &Pubkey) -> bool {
    crate::quote::clmm::TICKS.get(pool).map(|t| !t.initialized_arrays.is_empty()).unwrap_or(false)
}

fn dedupe_instructions(ixs: Vec<Instruction>) -> Vec<Instruction> {
    let mut out: Vec<Instruction> = Vec::with_capacity(ixs.len());
    for ix in ixs {
        if !out.iter().any(|o| o.program_id == ix.program_id && o.data == ix.data && o.accounts.iter().map(|a| a.pubkey).eq(ix.accounts.iter().map(|a| a.pubkey))) {
            out.push(ix);
        }
    }
    out
}

/// Build swap instructions for a two-hop route.
/// Combines setup + swap1 + swap2 + cleanup into a single SwapInstructions.
async fn build_two_hop_ixs(
    state: &AppState,
    quote: &QuoteResponse,
    user_pubkey: &Pubkey,
) -> TradeResult<SwapInstructions> {
    let hop1 = &quote.routes[0].pool;
    let hop2 = &quote.routes[1].pool;

    // Parse all mints and addresses
    let pool1_address = Pubkey::from_str(&hop1.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 pool_address: {}", hop1.pool_address)))?;
    let pool2_address = Pubkey::from_str(&hop2.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 pool_address: {}", hop2.pool_address)))?;

    let input_mint = Pubkey::from_str(&hop1.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 input_token: {}", hop1.input_token)))?;
    let bridge_mint = Pubkey::from_str(&hop1.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 output_token: {}", hop1.output_token)))?;
    let output_mint = Pubkey::from_str(&hop2.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 output_token: {}", hop2.output_token)))?;

    // Validate bridge mint matches between hops
    let hop2_input = Pubkey::from_str(&hop2.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 input_token: {}", hop2.input_token)))?;
    if bridge_mint != hop2_input {
        return Err(TradeError::Validation(
            "hop1 output_token must match hop2 input_token".into(),
        ));
    }

    let amount_in: u64 = hop1.amount_in.parse()
        .map_err(|_| TradeError::Validation(format!("invalid hop1 amount_in: {}", hop1.amount_in)))?;
    let hop1_out: u64 = hop1.amount_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid hop1 amount_out: {}", hop1.amount_out)))?;
    // Validated here; enforced by the router on the route's total output.
    let _route_floor: u64 = quote.minimum_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid minimum_out: {}", quote.minimum_out)))?;

    let pool1_type = pool_type_from_label(&hop1.dex)?;
    let pool2_type = pool_type_from_label(&hop2.dex)?;

    // Fetch pool states
    let pool1_state = load_pool_state(state, pool1_type, &pool1_address).await?;
    let pool2_state = load_pool_state(state, pool2_type, &pool2_address).await?;

    // Get token programs
    let input_token_program = get_token_program(state, &input_mint).await?;
    let bridge_token_program = get_token_program(state, &bridge_mint).await?;
    let output_token_program = get_token_program(state, &output_mint).await?;

    // Build swap 1: input -> bridge
    // For hop1, min_amount_out is 0 (we only enforce the final threshold)
    let order1 = SwapOrder {
        pool_address: pool1_address,
        pool_type: pool1_type,
        input_mint,
        output_mint: bridge_mint,
        amount_in,
        // intermediate — no slippage enforcement, except a native-SOL hop's wrap
        min_amount_out: native_sol_hop_floor(pool1_type, &bridge_mint, hop1_out, quote.slippage_bps),
        user: *user_pubkey,
        input_token_program,
        output_token_program: bridge_token_program,
    };

    let executor1 = AmmExecutorType::from_pool_type(pool1_type)?;
    let mut ixs1 = executor1.build_swap_ix(&order1, &pool1_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs1, pool1_type, &order1.pool_address).await?;

    // Build swap 2: bridge -> output
    let order2 = SwapOrder {
        pool_address: pool2_address,
        pool_type: pool2_type,
        input_mint: bridge_mint,
        output_mint,
        // Hop 2 can only spend what hop 1 is GUARANTEED to deliver: the quoted
        // output less the route's slippage. Spending the exact quote fails with
        // "insufficient funds" whenever hop 1 lands a hair short; the surplus
        // WSOL/bridge tokens stay with the user.
        amount_in: guaranteed(hop1_out, quote.slippage_bps),
        // The DEX-level floor is off for the last hop too: its input was scaled
        // down to the guaranteed amount, so the quoted minimum no longer applies
        // per hop. The router checks `minimum_out` on the route's total output.
        min_amount_out: 0,
        user: *user_pubkey,
        input_token_program: bridge_token_program,
        output_token_program,
    };

    let executor2 = AmmExecutorType::from_pool_type(pool2_type)?;
    let mut ixs2 = executor2.build_swap_ix(&order2, &pool2_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs2, pool2_type, &order2.pool_address).await?;
    let unwrapped = unwrap_for_native_sol_hop(pool2_type, &bridge_mint, &mut ixs2, user_pubkey);

    // Combine: setup1 + setup2, swap1 + swap2, cleanup1 + cleanup2 — see tidy_multihop
    let mut combined_swap = ixs1.swap;
    combined_swap.extend(ixs2.swap);
    let mut combined_cleanup = ixs1.cleanup;
    combined_cleanup.extend(ixs2.cleanup);
    if unwrapped {
        drop_wsol_close(&mut combined_cleanup, user_pubkey);
    }
    let (setup, cleanup) = tidy_multihop(ixs1.setup, vec![ixs2.setup], combined_cleanup, user_pubkey);

    Ok(SwapInstructions { setup, swap: combined_swap, cleanup })
}

/// Build swap instructions for a three-hop route.
/// Combines setup + swap1 + swap2 + swap3 + cleanup into a single SwapInstructions.
async fn build_three_hop_ixs(
    state: &AppState,
    quote: &QuoteResponse,
    user_pubkey: &Pubkey,
) -> TradeResult<SwapInstructions> {
    let hop1 = &quote.routes[0].pool;
    let hop2 = &quote.routes[1].pool;
    let hop3 = &quote.routes[2].pool;

    // Parse all pool addresses
    let pool1_address = Pubkey::from_str(&hop1.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 pool_address: {}", hop1.pool_address)))?;
    let pool2_address = Pubkey::from_str(&hop2.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 pool_address: {}", hop2.pool_address)))?;
    let pool3_address = Pubkey::from_str(&hop3.pool_address)
        .map_err(|_| TradeError::Validation(format!("invalid hop3 pool_address: {}", hop3.pool_address)))?;

    // Parse all mints
    let input_mint = Pubkey::from_str(&hop1.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 input_token: {}", hop1.input_token)))?;
    let bridge1_mint = Pubkey::from_str(&hop1.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop1 output_token: {}", hop1.output_token)))?;
    let bridge2_mint = Pubkey::from_str(&hop2.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 output_token: {}", hop2.output_token)))?;
    let output_mint = Pubkey::from_str(&hop3.output_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop3 output_token: {}", hop3.output_token)))?;

    // Validate bridge mint continuity: hop1.output == hop2.input
    let hop2_input = Pubkey::from_str(&hop2.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop2 input_token: {}", hop2.input_token)))?;
    if bridge1_mint != hop2_input {
        return Err(TradeError::Validation(
            "hop1 output_token must match hop2 input_token".into(),
        ));
    }

    // Validate bridge mint continuity: hop2.output == hop3.input
    let hop3_input = Pubkey::from_str(&hop3.input_token)
        .map_err(|_| TradeError::Validation(format!("invalid hop3 input_token: {}", hop3.input_token)))?;
    if bridge2_mint != hop3_input {
        return Err(TradeError::Validation(
            "hop2 output_token must match hop3 input_token".into(),
        ));
    }

    // Parse amounts
    let amount_in: u64 = hop1.amount_in.parse()
        .map_err(|_| TradeError::Validation(format!("invalid hop1 amount_in: {}", hop1.amount_in)))?;
    let hop1_out: u64 = hop1.amount_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid hop1 amount_out: {}", hop1.amount_out)))?;
    let hop2_out: u64 = hop2.amount_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid hop2 amount_out: {}", hop2.amount_out)))?;
    // Validated here; enforced by the router on the route's total output.
    let _route_floor: u64 = quote.minimum_out.parse()
        .map_err(|_| TradeError::Validation(format!("invalid minimum_out: {}", quote.minimum_out)))?;

    let pool1_type = pool_type_from_label(&hop1.dex)?;
    let pool2_type = pool_type_from_label(&hop2.dex)?;
    let pool3_type = pool_type_from_label(&hop3.dex)?;

    // Fetch pool states
    let pool1_state = load_pool_state(state, pool1_type, &pool1_address).await?;
    let pool2_state = load_pool_state(state, pool2_type, &pool2_address).await?;
    let pool3_state = load_pool_state(state, pool3_type, &pool3_address).await?;

    // Get token programs
    let input_token_program = get_token_program(state, &input_mint).await?;
    let bridge1_token_program = get_token_program(state, &bridge1_mint).await?;
    let bridge2_token_program = get_token_program(state, &bridge2_mint).await?;
    let output_token_program = get_token_program(state, &output_mint).await?;

    // Build swap 1: input -> bridge1
    let order1 = SwapOrder {
        pool_address: pool1_address,
        pool_type: pool1_type,
        input_mint,
        output_mint: bridge1_mint,
        amount_in,
        // intermediate — no slippage enforcement, except a native-SOL hop's wrap
        min_amount_out: native_sol_hop_floor(pool1_type, &bridge1_mint, hop1_out, quote.slippage_bps),
        user: *user_pubkey,
        input_token_program,
        output_token_program: bridge1_token_program,
    };

    let executor1 = AmmExecutorType::from_pool_type(pool1_type)?;
    let mut ixs1 = executor1.build_swap_ix(&order1, &pool1_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs1, pool1_type, &order1.pool_address).await?;

    // Build swap 2: bridge1 -> bridge2
    let order2 = SwapOrder {
        pool_address: pool2_address,
        pool_type: pool2_type,
        input_mint: bridge1_mint,
        output_mint: bridge2_mint,
        amount_in: guaranteed(hop1_out, quote.slippage_bps),
        min_amount_out: 0, // intermediate — no slippage enforcement
        user: *user_pubkey,
        input_token_program: bridge1_token_program,
        output_token_program: bridge2_token_program,
    };

    let executor2 = AmmExecutorType::from_pool_type(pool2_type)?;
    let mut ixs2 = executor2.build_swap_ix(&order2, &pool2_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs2, pool2_type, &order2.pool_address).await?;

    // Build swap 3: bridge2 -> output
    let order3 = SwapOrder {
        pool_address: pool3_address,
        pool_type: pool3_type,
        input_mint: bridge2_mint,
        output_mint,
        amount_in: guaranteed(hop2_out, quote.slippage_bps),
        min_amount_out: 0, // router enforces the route floor
        user: *user_pubkey,
        input_token_program: bridge2_token_program,
        output_token_program,
    };

    let executor3 = AmmExecutorType::from_pool_type(pool3_type)?;
    let mut ixs3 = executor3.build_swap_ix(&order3, &pool3_state)?;
    prune_missing_tick_arrays(&state.rpc, &mut ixs3, pool3_type, &order3.pool_address).await?;
    let unwrapped = unwrap_for_native_sol_hop(pool3_type, &bridge2_mint, &mut ixs3, user_pubkey);

    // Combine: setups, swap1 + swap2 + swap3, cleanups — see tidy_multihop
    let mut combined_swap = ixs1.swap;
    combined_swap.extend(ixs2.swap);
    combined_swap.extend(ixs3.swap);
    let mut combined_cleanup = ixs1.cleanup;
    combined_cleanup.extend(ixs2.cleanup);
    combined_cleanup.extend(ixs3.cleanup);
    if unwrapped {
        drop_wsol_close(&mut combined_cleanup, user_pubkey);
    }
    let (setup, cleanup) = tidy_multihop(ixs1.setup, vec![ixs2.setup, ixs3.setup], combined_cleanup, user_pubkey);

    Ok(SwapInstructions { setup, swap: combined_swap, cleanup })
}

/// Get token program for a mint. Checks immutable cache first, falls back to RPC.
async fn get_token_program(state: &AppState, mint: &Pubkey) -> TradeResult<Pubkey> {
    if *mint == SOL_NATIVE_MINT {
        return Ok(TOKEN_PROGRAM_ID);
    }
    // Check cache — mint owner is immutable, so this never goes stale
    if let Some(tp) = state.mint_program_cache.get(mint) {
        return Ok(*tp);
    }
    // RPC fallback + cache the result
    let tp = crate::pool::fetcher::get_mint_token_program(&state.rpc, mint).await?;
    state.mint_program_cache.insert(*mint, tp);
    Ok(tp)
}

/// Map a human-readable label back to a PoolType.
fn pool_type_from_label(label: &str) -> TradeResult<PoolType> {
    match label {
        "Raydium V4" => Ok(PoolType::RaydiumV4),
        "Raydium CPMM" => Ok(PoolType::RaydiumCpmm),
        "Raydium CLMM" => Ok(PoolType::RaydiumCl),
        "Raydium LP" => Ok(PoolType::RaydiumLp),
        "PumpFun" => Ok(PoolType::PumpFun),
        "PumpFun AMM" => Ok(PoolType::PumpFunAmm),
        "Meteora" => Ok(PoolType::Meteora),
        "Meteora DLMM" => Ok(PoolType::MeteoraDlmm),
        "Meteora DAMM" => Ok(PoolType::MeteoraDamm),
        "Meteora DBC" => Ok(PoolType::MeteoraDbc),
        "Orca" => Ok(PoolType::Orca),
        "FluxBeam" => Ok(PoolType::FluxBeam),
        "FlashTrade" => Ok(PoolType::FlashTrade),
        "Byreal" => Ok(PoolType::Byreal),
        "DefiTuna Fusion" => Ok(PoolType::DefiTunaFusion),
        "DefiTuna Pools" => Ok(PoolType::DefiTunaPools),
        "Saros" => Ok(PoolType::Saros),
        "PancakeSwap" => Ok(PoolType::PancakeSwap),
        "Dooar" => Ok(PoolType::Dooar),
        "Pumpup" => Ok(PoolType::Pumpup),
        "Pumpup Bonding" => Ok(PoolType::PumpupBonding),
        _ => Err(TradeError::Validation(format!("unknown DEX label: {label}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_type_from_label_all() {
        let cases = [
            ("Raydium V4", PoolType::RaydiumV4),
            ("Raydium CPMM", PoolType::RaydiumCpmm),
            ("Raydium CLMM", PoolType::RaydiumCl),
            ("Raydium LP", PoolType::RaydiumLp),
            ("PumpFun", PoolType::PumpFun),
            ("PumpFun AMM", PoolType::PumpFunAmm),
            ("Meteora", PoolType::Meteora),
            ("Meteora DLMM", PoolType::MeteoraDlmm),
            ("Meteora DAMM", PoolType::MeteoraDamm),
            ("Meteora DBC", PoolType::MeteoraDbc),
            ("Orca", PoolType::Orca),
            ("FluxBeam", PoolType::FluxBeam),
            ("FlashTrade", PoolType::FlashTrade),
            ("Byreal", PoolType::Byreal),
            ("DefiTuna Fusion", PoolType::DefiTunaFusion),
            ("DefiTuna Pools", PoolType::DefiTunaPools),
            ("Saros", PoolType::Saros),
            ("PancakeSwap", PoolType::PancakeSwap),
            ("Dooar", PoolType::Dooar),
            ("Pumpup", PoolType::Pumpup),
            ("Pumpup Bonding", PoolType::PumpupBonding),
        ];
        for (label, expected) in &cases {
            let result = pool_type_from_label(label).unwrap();
            assert_eq!(result, *expected, "label={label}");
        }
    }

    #[test]
    fn native_sol_hops_wrap_the_guaranteed_amount_and_unwrap_before_a_buy() {
        let user = Pubkey::new_unique();
        let token = Pubkey::new_unique();
        // pump.fun selling into the SOL bridge wraps what hop 2 will spend
        assert_eq!(native_sol_hop_floor(PoolType::PumpFun, &SOL_NATIVE_MINT, 1_000_000, 300), 970_000);
        assert_eq!(native_sol_hop_floor(PoolType::Orca, &SOL_NATIVE_MINT, 1_000_000, 300), 0);
        assert_eq!(native_sol_hop_floor(PoolType::PumpFun, &token, 1_000_000, 300), 0);
        // pump.fun buying from the SOL bridge: WSOL unwrapped first, inside the router
        let buy = Instruction { program_id: crate::constants::PUMP_FUN_PROG_ID, accounts: vec![], data: vec![1] };
        let mut ixs = SwapInstructions { setup: vec![], swap: vec![buy], cleanup: vec![] };
        assert!(!unwrap_for_native_sol_hop(PoolType::MeteoraDbc, &SOL_NATIVE_MINT, &mut ixs, &user));
        assert!(unwrap_for_native_sol_hop(PoolType::PumpFun, &SOL_NATIVE_MINT, &mut ixs, &user));
        let wsol = spl_associated_token_account::get_associated_token_address(&user, &SOL_NATIVE_MINT);
        assert_eq!((ixs.swap.len(), ixs.swap[0].data.as_slice(), ixs.swap[0].accounts[0].pubkey), (2, [9u8].as_slice(), wsol));
        // ... so the route's cleanup must not close it again
        let other = Pubkey::new_unique();
        let mut cleanup = vec![
            spl_token::instruction::close_account(&TOKEN_PROGRAM_ID, &wsol, &user, &user, &[]).unwrap(),
            spl_token::instruction::close_account(&TOKEN_PROGRAM_ID, &other, &user, &user, &[]).unwrap(),
        ];
        drop_wsol_close(&mut cleanup, &user);
        assert_eq!(cleanup.len(), 1);
        assert_eq!(cleanup[0].accounts[0].pubkey, other);
    }

    #[test]
    fn test_pool_type_from_label_unknown() {
        assert!(pool_type_from_label("NonExistent DEX").is_err());
    }

    #[test]
    fn test_swap_request_deserialization() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000000000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "162500000",
                "minimum_out": "161687500",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.05",
                "routes": [{
                    "pool": {
                        "pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV",
                        "dex": "Raydium CPMM",
                        "input_token": "So11111111111111111111111111111111111111112",
                        "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                        "amount_in": "1000000000",
                        "amount_out": "162500000",
                        "fee": "250000",
                        "fee_token": "So11111111111111111111111111111111111111112"
                    },
                    "percent": 100
                }],
                "slot": 408947310,
                "quote_time_ms": 0.012
            },
            "auto_wrap_sol": true,
            "priority_fee": 5000
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.wallet, "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW");
        assert_eq!(req.quote.amount_out, "162500000");
        assert_eq!(req.auto_wrap_sol, Some(true));
        assert_eq!(req.priority_fee, Some(5000));
    }

    #[test]
    fn test_swap_request_two_hop_deserialization() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "TokenA111111111111111111111111111111111111111",
                "amount_in": "1000000",
                "output_token": "TokenB111111111111111111111111111111111111111",
                "amount_out": "950000",
                "minimum_out": "940000",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.15",
                "routes": [
                    {
                        "pool": {
                            "pool_address": "Pool1111111111111111111111111111111111111111",
                            "dex": "PumpFun AMM",
                            "input_token": "TokenA111111111111111111111111111111111111111",
                            "output_token": "So11111111111111111111111111111111111111112",
                            "amount_in": "1000000",
                            "amount_out": "500000",
                            "fee": "2500",
                            "fee_token": "TokenA111111111111111111111111111111111111111"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool2222222222222222222222222222222222222222",
                            "dex": "Meteora",
                            "input_token": "So11111111111111111111111111111111111111112",
                            "output_token": "TokenB111111111111111111111111111111111111111",
                            "amount_in": "500000",
                            "amount_out": "950000",
                            "fee": "1250",
                            "fee_token": "So11111111111111111111111111111111111111112"
                        },
                        "percent": 100
                    }
                ],
                "slot": 0,
                "quote_time_ms": 0.025
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.quote.routes.len(), 2);
        assert_eq!(req.quote.routes[0].pool.dex, "PumpFun AMM");
        assert_eq!(req.quote.routes[1].pool.dex, "Meteora");
        assert_eq!(req.quote.amount_out, "950000");
    }

    #[test]
    fn test_swap_request_three_hop_deserialization() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "TokenA111111111111111111111111111111111111111",
                "amount_in": "1000000",
                "output_token": "TokenD111111111111111111111111111111111111111",
                "amount_out": "880000",
                "minimum_out": "871200",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.25",
                "routes": [
                    {
                        "pool": {
                            "pool_address": "Pool1111111111111111111111111111111111111111",
                            "dex": "PumpFun AMM",
                            "input_token": "TokenA111111111111111111111111111111111111111",
                            "output_token": "So11111111111111111111111111111111111111112",
                            "amount_in": "1000000",
                            "amount_out": "950000",
                            "fee": "2500",
                            "fee_token": "TokenA111111111111111111111111111111111111111"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool2222222222222222222222222222222222222222",
                            "dex": "Meteora",
                            "input_token": "So11111111111111111111111111111111111111112",
                            "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                            "amount_in": "950000",
                            "amount_out": "920000",
                            "fee": "2375",
                            "fee_token": "So11111111111111111111111111111111111111112"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool3333333333333333333333333333333333333333",
                            "dex": "Raydium CPMM",
                            "input_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                            "output_token": "TokenD111111111111111111111111111111111111111",
                            "amount_in": "920000",
                            "amount_out": "880000",
                            "fee": "2300",
                            "fee_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                        },
                        "percent": 100
                    }
                ],
                "slot": 0,
                "quote_time_ms": 0.035
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.quote.routes.len(), 3);
        assert_eq!(req.quote.routes[0].pool.dex, "PumpFun AMM");
        assert_eq!(req.quote.routes[1].pool.dex, "Meteora");
        assert_eq!(req.quote.routes[2].pool.dex, "Raydium CPMM");
        assert_eq!(req.quote.amount_out, "880000");

        // Verify bridge continuity in deserialized data
        assert_eq!(
            req.quote.routes[0].pool.output_token,
            req.quote.routes[1].pool.input_token,
            "hop1 output must match hop2 input"
        );
        assert_eq!(
            req.quote.routes[1].pool.output_token,
            req.quote.routes[2].pool.input_token,
            "hop2 output must match hop3 input"
        );
    }

    #[test]
    fn test_get_token_program_sol() {
        // SOL should return TOKEN_PROGRAM_ID synchronously (no RPC needed)
        // We can't easily test async here, but we verify the constant mapping
        assert_eq!(SOL_NATIVE_MINT.to_string(), "So11111111111111111111111111111111111111112");
    }

    #[test]
    fn test_swap_request_with_simulate_flag() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000000000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "162500000",
                "minimum_out": "161687500",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.05",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000000000", "amount_out": "162500000", "fee": "250000", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 0,
                "quote_time_ms": 0.01
            },
            "simulate": true,
            "compute_limit": 200000,
            "priority_fee": 10000
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.simulate, Some(true));
        assert_eq!(req.compute_limit, Some(200_000));
        assert_eq!(req.priority_fee, Some(10_000));
        // dynamic_cu not set
        assert_eq!(req.dynamic_cu, None);
    }

    #[test]
    fn test_swap_request_defaults_new_fields() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "900",
                "minimum_out": "850",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.05",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000", "amount_out": "900", "fee": "25", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 0,
                "quote_time_ms": 0.01
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.simulate, None);
        assert_eq!(req.compute_limit, None);
        assert_eq!(req.dynamic_cu, None);
        assert_eq!(req.priority_fee, None);
    }

    #[test]
    fn test_swap_response_simulation_serialization() {
        let resp = SwapResponse {
            tx_version: 0,
            transaction: "base64data".to_string(),
            block_height: 12345,
            compute_limit: 89_000,
            priority_fee: 5_000,
            simulation: Some(SimulationResponse {
                success: true,
                units_consumed: 80_000,
                error: None,
                logs: vec!["Program log: Transfer 12345".to_string()],
            }),
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"simulation\""));
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"units_consumed\":80000"));
        assert!(json.contains("\"logs\""));
    }

    #[test]
    fn test_swap_response_no_simulation_skipped() {
        let resp = SwapResponse {
            tx_version: 0,
            transaction: "base64data".to_string(),
            block_height: 12345,
            compute_limit: 400_000,
            priority_fee: 5_000,
            simulation: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        // simulation field should be absent (skip_serializing_if)
        assert!(!json.contains("simulation"));
    }

    // ── Slippage enforcement: multi-hop minimum_out in deserialized quotes ──

    #[test]
    fn test_minimum_out_parsed_from_quote_correctly() {
        // Verify that minimum_out field round-trips through JSON serialization
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "950000",
                "minimum_out": "999",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.05",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000000", "amount_out": "950000", "fee": "2500", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 0,
                "quote_time_ms": 0.01
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.quote.minimum_out, "999");
    }

    #[test]
    fn test_minimum_out_zero_is_valid_for_intermediate_hops() {
        // minimum_out=0 is valid — used for intermediate hops in multi-hop swaps
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "950000",
                "minimum_out": "0",
                "mode": "ExactIn",
                "slippage_bps": 10000,
                "price_impact": "0.05",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000000", "amount_out": "950000", "fee": "2500", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 0,
                "quote_time_ms": 0.01
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.quote.minimum_out, "0");
        // This should parse as a valid u64
        let min_out: u64 = req.quote.minimum_out.parse().unwrap();
        assert_eq!(min_out, 0);
    }

    #[test]
    fn test_two_hop_quote_minimum_out_applies_to_final_output() {
        // In a 2-hop swap:
        // - hop1 has min_amount_out=0 (intermediate, no enforcement)
        // - hop2 has the real minimum_out from the quote
        // This test verifies the quote structure correctly carries minimum_out
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "TokenA111111111111111111111111111111111111111",
                "amount_in": "1000000",
                "output_token": "TokenB111111111111111111111111111111111111111",
                "amount_out": "950000",
                "minimum_out": "940500",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.15",
                "routes": [
                    {
                        "pool": {
                            "pool_address": "Pool1111111111111111111111111111111111111111",
                            "dex": "PumpFun AMM",
                            "input_token": "TokenA111111111111111111111111111111111111111",
                            "output_token": "So11111111111111111111111111111111111111112",
                            "amount_in": "1000000",
                            "amount_out": "500000",
                            "fee": "2500",
                            "fee_token": "TokenA111111111111111111111111111111111111111"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool2222222222222222222222222222222222222222",
                            "dex": "Meteora",
                            "input_token": "So11111111111111111111111111111111111111112",
                            "output_token": "TokenB111111111111111111111111111111111111111",
                            "amount_in": "500000",
                            "amount_out": "950000",
                            "fee": "1250",
                            "fee_token": "So11111111111111111111111111111111111111112"
                        },
                        "percent": 100
                    }
                ],
                "slot": 0,
                "quote_time_ms": 0.025
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        let min_out: u64 = req.quote.minimum_out.parse().unwrap();

        // Verify minimum_out = compute_threshold(950000, 100) = 950000 * 9900 / 10000 = 940500
        use crate::quote::types::compute_threshold;
        let expected = compute_threshold(950_000, 100);
        assert_eq!(expected, 940_500);
        assert_eq!(min_out, expected);

        // Verify route structure: 2 hops
        assert_eq!(req.quote.routes.len(), 2);
        // hop1 output == hop2 input (bridge continuity)
        assert_eq!(
            req.quote.routes[0].pool.output_token,
            req.quote.routes[1].pool.input_token,
        );
    }

    #[test]
    fn test_three_hop_quote_minimum_out_structure() {
        // In a 3-hop swap:
        // - hop1 has min_amount_out=0 (intermediate)
        // - hop2 has min_amount_out=0 (intermediate)
        // - hop3 has the real minimum_out from the quote
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "TokenA111111111111111111111111111111111111111",
                "amount_in": "1000000",
                "output_token": "TokenD111111111111111111111111111111111111111",
                "amount_out": "880000",
                "minimum_out": "871200",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.25",
                "routes": [
                    {
                        "pool": {
                            "pool_address": "Pool1111111111111111111111111111111111111111",
                            "dex": "PumpFun AMM",
                            "input_token": "TokenA111111111111111111111111111111111111111",
                            "output_token": "So11111111111111111111111111111111111111112",
                            "amount_in": "1000000",
                            "amount_out": "950000",
                            "fee": "2500",
                            "fee_token": "TokenA111111111111111111111111111111111111111"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool2222222222222222222222222222222222222222",
                            "dex": "Meteora",
                            "input_token": "So11111111111111111111111111111111111111112",
                            "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                            "amount_in": "950000",
                            "amount_out": "920000",
                            "fee": "2375",
                            "fee_token": "So11111111111111111111111111111111111111112"
                        },
                        "percent": 100
                    },
                    {
                        "pool": {
                            "pool_address": "Pool3333333333333333333333333333333333333333",
                            "dex": "Raydium CPMM",
                            "input_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                            "output_token": "TokenD111111111111111111111111111111111111111",
                            "amount_in": "920000",
                            "amount_out": "880000",
                            "fee": "2300",
                            "fee_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                        },
                        "percent": 100
                    }
                ],
                "slot": 0,
                "quote_time_ms": 0.035
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        let min_out: u64 = req.quote.minimum_out.parse().unwrap();

        // minimum_out = compute_threshold(880000, 100) = 880000 * 9900 / 10000 = 871200
        use crate::quote::types::compute_threshold;
        let expected = compute_threshold(880_000, 100);
        assert_eq!(expected, 871_200);
        assert_eq!(min_out, expected);

        // 3 hops with bridge continuity
        assert_eq!(req.quote.routes.len(), 3);
        assert_eq!(
            req.quote.routes[0].pool.output_token,
            req.quote.routes[1].pool.input_token,
            "hop1 output must match hop2 input"
        );
        assert_eq!(
            req.quote.routes[1].pool.output_token,
            req.quote.routes[2].pool.input_token,
            "hop2 output must match hop3 input"
        );
    }

    #[test]
    fn test_large_minimum_out_parses_correctly() {
        // Ensure large u64 values parse through the JSON roundtrip
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "18446744073709551615",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "18446744073709551615",
                "minimum_out": "18262276473072696151",
                "mode": "ExactIn",
                "slippage_bps": 100,
                "price_impact": "0.0",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "18446744073709551615", "amount_out": "18446744073709551615", "fee": "0", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 0,
                "quote_time_ms": 0.01
            }
        }"#;

        let req: SwapRequest = serde_json::from_str(json).unwrap();
        let min_out: u64 = req.quote.minimum_out.parse().unwrap();
        // u64::MAX * 9900 / 10000 = 18262276473072696150.8... → truncated to 18262276473072696150
        // But the JSON has the pre-computed value
        assert_eq!(min_out, 18_262_276_473_072_696_151);
    }

    // ── Tip field tests ──

    #[test]
    fn test_swap_request_no_tip_field() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "900",
                "minimum_out": "895",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.01",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000", "amount_out": "900", "fee": "25", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 1,
                "quote_time_ms": 0.1
            }
        }"#;
        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert!(req.tip.is_none());
    }

    #[test]
    fn test_swap_request_tip_null() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "tip": null,
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "900",
                "minimum_out": "895",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.01",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000", "amount_out": "900", "fee": "25", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 1,
                "quote_time_ms": 0.1
            }
        }"#;
        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert!(req.tip.is_none());
    }

    #[test]
    fn test_swap_request_with_tip() {
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "tip": {
                "address": "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
                "lamports": 10000
            },
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "900",
                "minimum_out": "895",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.01",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000", "amount_out": "900", "fee": "25", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 1,
                "quote_time_ms": 0.1
            }
        }"#;
        let req: SwapRequest = serde_json::from_str(json).unwrap();
        let tip = req.tip.unwrap();
        assert_eq!(tip.address, "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5");
        assert_eq!(tip.lamports, 10000);
    }

    #[test]
    fn test_swap_request_tip_is_optional_with_other_fields() {
        // All optional fields set EXCEPT tip — should deserialize fine
        let json = r#"{
            "wallet": "6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW",
            "auto_wrap_sol": true,
            "priority_fee": 5000,
            "compute_limit": 400000,
            "simulate": true,
            "quote": {
                "input_token": "So11111111111111111111111111111111111111112",
                "amount_in": "1000",
                "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "amount_out": "900",
                "minimum_out": "895",
                "mode": "ExactIn",
                "slippage_bps": 50,
                "price_impact": "0.01",
                "routes": [{"pool": {"pool_address": "HJPjoWUrhoZBKRNbtr3PFQHoMTmLLGmpvdnVaAe3KXGV", "dex": "Raydium CPMM", "input_token": "So11111111111111111111111111111111111111112", "output_token": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount_in": "1000", "amount_out": "900", "fee": "25", "fee_token": "So11111111111111111111111111111111111111112"}, "percent": 100}],
                "slot": 1,
                "quote_time_ms": 0.1
            }
        }"#;
        let req: SwapRequest = serde_json::from_str(json).unwrap();
        assert!(req.tip.is_none());
        assert_eq!(req.auto_wrap_sol, Some(true));
        assert_eq!(req.priority_fee, Some(5000));
        assert_eq!(req.compute_limit, Some(400000));
        assert_eq!(req.simulate, Some(true));
    }
}
