use std::sync::Arc;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::constants::BRIDGE_MINTS;
use crate::error::{TradeError, TradeResult};
use crate::pool::cache::PoolCache;
use crate::pool::registry::{PoolEntry, PoolRegistry};
use crate::pool::types::{PoolState, PoolType};

use crate::execution::amms::pumpfun_amm::pamm_total_fee_bps;
use super::clmm;
use super::math::{compute_constant_product_out, compute_fee_amount, compute_price_impact_for_type, estimate_price_impact, extract_clmm_params};
use super::types::{
    PlatformFee, QuoteRequest, QuoteResponse, RouteStep, PoolRoute, compute_threshold,
};

/// Default fee in basis points for constant-product AMMs that don't expose their fee on-chain.
const DEFAULT_FEE_BPS: u16 = 25;

/// Platform fee in basis points, as the on-chain router config states it.
/// Set once at startup from the config PDA (`set_platform_fee_bps`); 50 until then.
static PLATFORM_FEE_BPS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(50);

pub fn set_platform_fee_bps(bps: u16) {
    PLATFORM_FEE_BPS.store(bps.min(10_000), std::sync::atomic::Ordering::Relaxed);
}

pub fn platform_fee_bps() -> u16 {
    PLATFORM_FEE_BPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Compute the platform fee for a quote. Always taken from the output token.
fn compute_platform_fee(
    amount_out: u64,
    output_mint: &Pubkey,
) -> PlatformFee {
    let bps = platform_fee_bps();
    let fee_amount = (amount_out as u128 * bps as u128 / 10_000) as u64;
    PlatformFee {
        amount: fee_amount.to_string(),
        fee_bps: bps,
        fee_token: output_mint.to_string(),
        side: "output".to_string(),
    }
}

/// Pool types that use constant-product math (x * y = k).
fn is_constant_product(pool_type: PoolType) -> bool {
    matches!(
        pool_type,
        PoolType::RaydiumV4
            | PoolType::RaydiumCpmm
            | PoolType::PumpFunAmm
            | PoolType::Saros
            | PoolType::Dooar
            | PoolType::PumpupBonding
            | PoolType::Pumpup
            | PoolType::FluxBeam
        // NOT here, quoted with their own math in `quote_state`: Meteora DAMM v2
        // (single-range sqrt-price curve, `quote::damm_v2`), Meteora DLMM (bin
        // walk, `quote::dlmm`), Raydium LaunchLab
        // (virtual-reserve bonding curve, `quote::launchlab`), Meteora Standard
        // (LP share of dynamic vaults, `quote::meteora_std`), every CLMM venue
        // (`quote::clmm` tick walk), pump.fun bonding (`quote::pump_bonding`)
        // and Meteora DBC (`quote::dbc`).
    )
}

/// Pool types that use CLMM (concentrated liquidity) math.
fn is_clmm(pool_type: PoolType) -> bool {
    matches!(
        pool_type,
        PoolType::RaydiumCl
            | PoolType::Orca
            | PoolType::PancakeSwap
            | PoolType::Byreal
            | PoolType::DefiTunaFusion
    )
}

/// Check if a pool type is quotable (constant product or CLMM).
fn is_quotable(pool_type: PoolType) -> bool {
    is_constant_product(pool_type) || is_clmm(pool_type) || matches!(pool_type, PoolType::MeteoraDamm | PoolType::RaydiumLp | PoolType::Meteora | PoolType::MeteoraDlmm | PoolType::PumpFun | PoolType::MeteoraDbc)
}

/// Get the label for a pool type from the program_id_to_label mapping.
pub(crate) fn label_for_pool_type(pool_type: PoolType) -> &'static str {
    match pool_type {
        PoolType::RaydiumV4 => "Raydium V4",
        PoolType::RaydiumCpmm => "Raydium CPMM",
        PoolType::RaydiumCl => "Raydium CLMM",
        PoolType::RaydiumLp => "Raydium LP",
        PoolType::PumpFun => "PumpFun",
        PoolType::PumpFunAmm => "PumpFun AMM",
        PoolType::Meteora => "Meteora",
        PoolType::MeteoraDlmm => "Meteora DLMM",
        PoolType::MeteoraDamm => "Meteora DAMM",
        PoolType::MeteoraDbc => "Meteora DBC",
        PoolType::Orca => "Orca",
        PoolType::FluxBeam => "FluxBeam",
        PoolType::FlashTrade => "FlashTrade",
        PoolType::Byreal => "Byreal",
        PoolType::DefiTunaFusion => "DefiTuna Fusion",
        PoolType::DefiTunaPools => "DefiTuna Pools",
        PoolType::Saros => "Saros",
        PoolType::PancakeSwap => "PancakeSwap",
        PoolType::Dooar => "Dooar",
        PoolType::Pumpup => "Pumpup",
        PoolType::PumpupBonding => "Pumpup Bonding",
        _ => "Unknown",
    }
}

/// (amount_out, fee_amount, reserve_in, reserve_out) of one priced leg.
type LegResult = (u64, u64, u128, u128);

/// Outcome of trying to price a pool from memory.
enum Eval {
    /// Priced (or known unpriceable: `None`) without leaving memory.
    Quoted(Option<LegResult>),
    /// Needs the cold path (RPC).
    Cold,
}

/// A computed direct route candidate with output amount and metadata.
#[derive(Debug, Clone)]
struct DirectRoute {
    pool_address: Pubkey,
    pool_type: PoolType,
    out_amount: u64,
    fee_amount: u64,
    reserve_in: u128,
    reserve_out: u128,
    /// The venue fee this pool charges (bps) — per-pool for pump.fun AMM
    /// (market-cap tier), the protocol default elsewhere. Read by the
    /// split-route evaluator, which is compiled for tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    fee_bps: u16,
}

/// What a hop is GUARANTEED to deliver at the route's slippage: the amount the
/// swap builder spends on the next hop. Later hops are quoted on this, not on
/// the expected output, so the quote equals what the router will deliver when
/// every hop lands at or above its floor; the difference stays in the user's
/// intermediate token account.
pub fn guaranteed(quoted_out: u64, slippage_bps: u16) -> u64 {
    let bps = (slippage_bps as u128).min(9_999);
    ((quoted_out as u128) * (10_000 - bps) / 10_000).max(1) as u64
}

/// A computed 2-hop route candidate.
#[derive(Debug, Clone)]
struct TwoHopRoute {
    hop1_entry: PoolEntry,
    hop2_entry: PoolEntry,
    bridge_mint: Pubkey,
    hop1_amount_out: u64,
    hop1_fee_amount: u64,
    hop1_reserve_in: u128,
    hop1_reserve_out: u128,
    final_amount_out: u64,
    hop2_fee_amount: u64,
    hop2_reserve_in: u128,
    hop2_reserve_out: u128,
}

/// A computed 3-hop route candidate.
#[derive(Debug, Clone)]
struct ThreeHopRoute {
    hop1_entry: PoolEntry,
    hop2_entry: PoolEntry,
    hop3_entry: PoolEntry,
    bridge1_mint: Pubkey,
    bridge2_mint: Pubkey,
    hop1_amount_out: u64,
    hop1_fee_amount: u64,
    hop1_reserve_in: u128,
    hop1_reserve_out: u128,
    hop2_amount_out: u64,
    hop2_fee_amount: u64,
    hop2_reserve_in: u128,
    hop2_reserve_out: u128,
    final_amount_out: u64,
    hop3_fee_amount: u64,
    hop3_reserve_in: u128,
    hop3_reserve_out: u128,
}

/// A computed split route: divide input across 2 pools for better output on large trades.
#[derive(Debug, Clone)]
struct SplitRoute {
    pool_a: DirectRoute,
    pool_b: DirectRoute,
    pct_a: u8,
    pct_b: u8,
    amount_a: u64,
    amount_b: u64,
    out_a: u64,
    out_b: u64,
    total_out: u64,
    fee_a: u64,
    fee_b: u64,
}

/// The Quoter finds the best swap route for a given token pair.
pub struct Quoter {
    registry: Arc<PoolRegistry>,
    cache: Arc<PoolCache>,
    rpc: Arc<RpcClient>,
    mirror: Option<Arc<crate::stream::account_mirror::AccountMirror>>,
    /// Stale-while-revalidate: when set, quoting a pool whose cached state is
    /// older than this schedules a BACKGROUND re-read and answers from the
    /// cached state immediately. The quote path itself never waits on RPC for
    /// a pool it already knows (the block-driven refresher keeps traded pools
    /// fresh; this covers quiet ones).
    pub revalidate_after: Option<std::time::Duration>,
    /// Pools with a background re-read in flight.
    revalidating: Arc<dashmap::DashSet<Pubkey>>,
}

impl Quoter {
    pub fn new(
        registry: Arc<PoolRegistry>,
        cache: Arc<PoolCache>,
        rpc: Arc<RpcClient>,
    ) -> Self {
        Self {
            registry,
            cache,
            rpc,
            mirror: None,
            revalidate_after: None,
            revalidating: Arc::new(dashmap::DashSet::new()),
        }
    }

    /// Create a Quoter with an AccountMirror for zero-RPC vault balance lookups.
    pub fn with_mirror(
        registry: Arc<PoolRegistry>,
        cache: Arc<PoolCache>,
        rpc: Arc<RpcClient>,
        mirror: Arc<crate::stream::account_mirror::AccountMirror>,
    ) -> Self {
        Self {
            registry,
            cache,
            rpc,
            mirror: Some(mirror),
            revalidate_after: None,
            revalidating: Arc::new(dashmap::DashSet::new()),
        }
    }

    /// Find the best quote for a swap request.
    pub async fn quote(&self, req: &QuoteRequest) -> TradeResult<QuoteResponse> {
        let start = Instant::now();

        // 1. Evaluate direct routes
        let t_phase = std::time::Instant::now();
        let direct_routes = self.evaluate_direct_routes(req).await;
        let ms_direct = t_phase.elapsed().as_secs_f64() * 1e3;

        // Split route quoting disabled — the swap builder cannot execute splits yet.
        // When split execution is implemented, re-enable this line:
        // let best_split = evaluate_split_routes(&direct_routes, req.amount);
        let best_split: Option<SplitRoute> = None;

        // 3. Evaluate 2-hop routes (unless only_direct_routes is true)
        let two_hop_routes = if req.only_direct_routes {
            Vec::new()
        } else {
            self.evaluate_two_hop_routes(req).await
        };

        let ms_two = t_phase.elapsed().as_secs_f64() * 1e3 - ms_direct;

        // 4. Evaluate 3-hop routes (unless only_direct_routes is true)
        let three_hop_routes = if req.only_direct_routes {
            Vec::new()
        } else {
            self.evaluate_three_hop_routes(req).await
        };

        let ms_three = t_phase.elapsed().as_secs_f64() * 1e3 - ms_direct - ms_two;
        if t_phase.elapsed().as_millis() >= 20 {
            tracing::info!(direct_ms = format!("{ms_direct:.1}"), two_hop_ms = format!("{ms_two:.1}"), three_hop_ms = format!("{ms_three:.1}"),
                direct = direct_routes.len(), two_hop = two_hop_routes.len(), three_hop = three_hop_routes.len(), "slow quote phases");
        }

        // 5. Pick the best route (direct, split, 2-hop, or 3-hop)
        let best_direct = direct_routes.iter().max_by_key(|r| r.out_amount);
        let best_two_hop = two_hop_routes.iter().max_by_key(|r| r.final_amount_out);
        let best_three_hop = three_hop_routes.iter().max_by_key(|r| r.final_amount_out);

        let context_slot: u64 = 0;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0; // milliseconds

        // Find the overall best output amount
        let direct_out = best_direct.map(|r| r.out_amount).unwrap_or(0);
        let split_out = best_split.as_ref().map(|r| r.total_out).unwrap_or(0);
        let two_hop_out = best_two_hop.map(|r| r.final_amount_out).unwrap_or(0);
        let three_hop_out = best_three_hop.map(|r| r.final_amount_out).unwrap_or(0);

        let max_out = direct_out.max(split_out).max(two_hop_out).max(three_hop_out);

        if max_out == 0 {
            return Err(TradeError::NoRoute {
                input_mint: req.input_mint.to_string(),
                output_mint: req.output_mint.to_string(),
            });
        }

        if max_out == split_out {
            if let Some(ref split) = best_split {
                return Ok(self.build_split_response(req, split, context_slot, elapsed));
            }
        }

        if max_out == three_hop_out {
            if let Some(three_hop) = best_three_hop {
                return Ok(self.build_three_hop_response(req, three_hop, context_slot, elapsed));
            }
        }

        if max_out == two_hop_out {
            if let Some(two_hop) = best_two_hop {
                return Ok(self.build_two_hop_response(req, two_hop, context_slot, elapsed));
            }
        }

        if let Some(direct) = best_direct {
            return Ok(self.build_direct_response(req, direct, context_slot, elapsed));
        }

        Err(TradeError::NoRoute {
            input_mint: req.input_mint.to_string(),
            output_mint: req.output_mint.to_string(),
        })
    }

    /// Evaluate all direct routes for the given token pair.
    async fn evaluate_direct_routes(&self, req: &QuoteRequest) -> Vec<DirectRoute> {
        let entries = self.filter_entries(
            &req.input_mint,
            &req.output_mint,
            &req.dexes,
            &req.exclude_dexes,
        );

        let mut candidates: Vec<DirectRoute> = self
            .evaluate_many(&entries, &req.input_mint, &req.output_mint, req.amount)
            .await
            .into_iter()
            .map(|(i, (out_amount, fee_amount, reserve_in, reserve_out))| {
                let entry = &entries[i];
                DirectRoute {
                    pool_address: entry.address,
                    pool_type: entry.pool_type,
                    out_amount,
                    fee_amount,
                    reserve_in,
                    reserve_out,
                    fee_bps: fee_for_pool_type(entry.pool_type),
                }
            })
            .collect();

        // Sort by best output descending
        candidates.sort_by(|a, b| b.out_amount.cmp(&a.out_amount));
        candidates
    }

    /// Evaluate 2-hop routes through bridge mints (SOL, USDC, USDT).
    async fn evaluate_two_hop_routes(&self, req: &QuoteRequest) -> Vec<TwoHopRoute> {
        let mut routes = Vec::new();

        for bridge in &BRIDGE_MINTS {
            // Skip if bridge is already the input or output mint
            if *bridge == req.input_mint || *bridge == req.output_mint {
                continue;
            }

            // Find pools for hop1 (input -> bridge) and hop2 (bridge -> output)
            let hop1_entries = self.filter_entries(
                &req.input_mint,
                bridge,
                &req.dexes,
                &req.exclude_dexes,
            );
            let hop2_entries = self.filter_entries(
                bridge,
                &req.output_mint,
                &req.dexes,
                &req.exclude_dexes,
            );

            if hop1_entries.is_empty() || hop2_entries.is_empty() {
                continue;
            }

            // Output is monotone in input, so for ANY hop-2 pool the best route
            // through this bridge starts with the hop-1 pool that pays the most:
            // evaluate hop 1 once, then every hop-2 pool once with that amount —
            // O(H1 + H2) instead of the former O(H1 × H2) sequential awaits.
            let h1_results = self.evaluate_many(&hop1_entries, &req.input_mint, bridge, req.amount).await;
            let Some(&(h1_idx, (h1_out, h1_fee, h1_res_in, h1_res_out))) = h1_results.iter().max_by_key(|(_, r)| r.0) else {
                continue;
            };
            let h1 = &hop1_entries[h1_idx];
            let h2_in = guaranteed(h1_out, req.slippage_bps);
            for (h2_idx, (h2_out, h2_fee, h2_res_in, h2_res_out)) in
                self.evaluate_many(&hop2_entries, bridge, &req.output_mint, h2_in).await
            {
                routes.push(TwoHopRoute {
                    hop1_entry: h1.clone(),
                    hop2_entry: hop2_entries[h2_idx].clone(),
                    bridge_mint: *bridge,
                    hop1_amount_out: h1_out,
                    hop1_fee_amount: h1_fee,
                    hop1_reserve_in: h1_res_in,
                    hop1_reserve_out: h1_res_out,
                    final_amount_out: h2_out,
                    hop2_fee_amount: h2_fee,
                    hop2_reserve_in: h2_res_in,
                    hop2_reserve_out: h2_res_out,
                });
            }
        }

        routes
    }

    /// Evaluate 3-hop routes through pairs of bridge mints.
    ///
    /// For each (bridge1, bridge2) pair from BRIDGE_MINTS where bridge1 != bridge2
    /// and neither equals input or output:
    ///   hop1: input -> bridge1
    ///   hop2: bridge1 -> bridge2
    ///   hop3: bridge2 -> output
    ///
    /// To avoid combinatorial explosion, we only evaluate the best hop1 pool (by output)
    /// per bridge1, and the best hop2 pool per bridge pair. This keeps it O(bridges^2).
    async fn evaluate_three_hop_routes(&self, req: &QuoteRequest) -> Vec<ThreeHopRoute> {
        let mut routes = Vec::new();

        for bridge1 in &BRIDGE_MINTS {
            // Skip if bridge1 is already the input or output mint
            if *bridge1 == req.input_mint || *bridge1 == req.output_mint {
                continue;
            }

            // Find the best hop1 pool: input -> bridge1
            let hop1_entries = self.filter_entries(
                &req.input_mint,
                bridge1,
                &req.dexes,
                &req.exclude_dexes,
            );

            if hop1_entries.is_empty() {
                continue;
            }

            // Best hop-1 pool by output (hot pools synchronously, cold ones concurrently).
            let h1_results = self.evaluate_many(&hop1_entries, &req.input_mint, bridge1, req.amount).await;
            let Some(&(h1_idx, (h1_out, h1_fee, h1_res_in, h1_res_out))) = h1_results.iter().max_by_key(|(_, r)| r.0) else {
                continue;
            };
            let h1_entry = &hop1_entries[h1_idx];

            for bridge2 in &BRIDGE_MINTS {
                // Skip if bridge2 equals bridge1, input, or output
                if *bridge2 == *bridge1 || *bridge2 == req.input_mint || *bridge2 == req.output_mint {
                    continue;
                }

                // Find the best hop2 pool: bridge1 -> bridge2
                let hop2_entries = self.filter_entries(
                    bridge1,
                    bridge2,
                    &req.dexes,
                    &req.exclude_dexes,
                );

                if hop2_entries.is_empty() {
                    continue;
                }

                let h2_in = guaranteed(h1_out, req.slippage_bps);
                let h2_results = self.evaluate_many(&hop2_entries, bridge1, bridge2, h2_in).await;
                let Some(&(h2_idx, (h2_out, h2_fee, h2_res_in, h2_res_out))) = h2_results.iter().max_by_key(|(_, r)| r.0) else {
                    continue;
                };
                let h2_entry = &hop2_entries[h2_idx];

                // Find hop3 pools: bridge2 -> output
                let hop3_entries = self.filter_entries(
                    bridge2,
                    &req.output_mint,
                    &req.dexes,
                    &req.exclude_dexes,
                );

                let h3_in = guaranteed(h2_out, req.slippage_bps);
                for (h3_idx, (h3_out, h3_fee, h3_res_in, h3_res_out)) in
                    self.evaluate_many(&hop3_entries, bridge2, &req.output_mint, h3_in).await
                {
                    routes.push(ThreeHopRoute {
                        hop1_entry: h1_entry.clone(),
                        hop2_entry: h2_entry.clone(),
                        hop3_entry: hop3_entries[h3_idx].clone(),
                        bridge1_mint: *bridge1,
                        bridge2_mint: *bridge2,
                        hop1_amount_out: h1_out,
                        hop1_fee_amount: h1_fee,
                        hop1_reserve_in: h1_res_in,
                        hop1_reserve_out: h1_res_out,
                        hop2_amount_out: h2_out,
                        hop2_fee_amount: h2_fee,
                        hop2_reserve_in: h2_res_in,
                        hop2_reserve_out: h2_res_out,
                        final_amount_out: h3_out,
                        hop3_fee_amount: h3_fee,
                        hop3_reserve_in: h3_res_in,
                        hop3_reserve_out: h3_res_out,
                    });
                }
            }
        }

        routes
    }

    /// HOT PATH. Price one pool from memory only: no `await`, no clone, no RPC.
    /// `Cold` means the pool cannot be priced from memory (state never fetched,
    /// or a constant-product pool whose vault balances are not mirrored yet).
    fn evaluate_hot(&self, entry: &PoolEntry, input_mint: &Pubkey, amount: u64) -> Eval {
        let r = self.cache.with_state(&entry.address, |state, age| {
            if self.revalidate_after.is_some_and(|d| age > d) {
                self.revalidate_in_background(entry);
            }
            self.quote_state(state, entry, input_mint, amount)
        });
        r.unwrap_or(Eval::Cold)
    }

    /// Pure pricing of a pool state. CLMM venues use their inline sqrt-price /
    /// liquidity; constant-product venues use mirrored vault balances (fed by
    /// every block) and fall back to the reserves inline in the state.
    fn quote_state(&self, state: &PoolState, entry: &PoolEntry, input_mint: &Pubkey, amount: u64) -> Eval {
        if is_clmm(entry.pool_type) {
            let params = match extract_clmm_params(state, input_mint) {
                Some(p) if p.sqrt_price_x64 > 0 => p,
                _ => return Eval::Quoted(None),
            };
            // Exact tick walk when the pool's tick arrays are in memory;
            // without them the only honest answer is "not yet" (the cold path
            // loads them). The single-range approximation over-quotes as soon
            // as a swap crosses into thinner liquidity, so it is never used.
            let (layout, _, _, tick_current, tick_spacing) = match crate::pool::ticks::tick_source(state) {
                Some(t) => t,
                None => return Eval::Quoted(None),
            };
            let ticks = match clmm::TICKS.get(&entry.address) {
                Some(t) if t.fetched_at.elapsed() <= self.cache.ttl() => Arc::clone(&t),
                _ => return Eval::Cold,
            };
            // Byreal dynamic-fee pools: a per-swap rate from vaults + oracle prices
            let fee_ppm = match super::byreal_fee::swap_fee_ppm(state, &entry.address, params.a_to_b, amount, params.fee_ppm) {
                super::byreal_fee::SwapFee::Base => params.fee_ppm,
                super::byreal_fee::SwapFee::Rate(r) => r,
                super::byreal_fee::SwapFee::Unavailable => return Eval::Quoted(None),
            };
            let fee_ext = match state {
                PoolState::RaydiumClmm { fee_ext, .. } => *fee_ext,
                _ => Default::default(),
            };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let pool = clmm::ClmmPool {
                layout, sqrt_price_x64: params.sqrt_price_x64, liquidity: params.liquidity, tick_current, tick_spacing, fee_ppm, fee_ext,
                adaptive_fee: ticks.adaptive_fee, now,
            };
            let out = match clmm::swap_exact_in_pool(&pool, &ticks, params.a_to_b, amount) {
                Some(r) if r.amount_out > 0 => r.amount_out,
                _ => return Eval::Quoted(None),
            };
            let fee_amount = (amount as u128 * fee_ppm as u128).div_ceil(clmm::FEE_DENOMINATOR_PPM) as u64;
            let q64: u128 = 1u128 << 64;
            let reserve_a = params.liquidity.saturating_mul(q64) / params.sqrt_price_x64.max(1);
            let reserve_b = params.liquidity.saturating_mul(params.sqrt_price_x64) / q64.max(1);
            let (reserve_in, reserve_out) = if params.a_to_b { (reserve_a, reserve_b) } else { (reserve_b, reserve_a) };
            return Eval::Quoted(Some((out, fee_amount, reserve_in, reserve_out)));
        }

        if let PoolState::PumpFunAmm { pool_base_vault, pool_quote_vault, base_reserve, quote_reserve, .. } = state {
            // Vault balances from the mirror (block-fresh) if it has them, else
            // the state's; the exact curve + fee model lives in the executor module.
            let (rb, rq) = match self.mirror.as_ref() {
                Some(m) => match (m.get_vault_balance(pool_base_vault), m.get_vault_balance(pool_quote_vault)) {
                    (Some(b), Some(q)) => (b, q),
                    _ => (*base_reserve, *quote_reserve),
                },
                None => (*base_reserve, *quote_reserve),
            };
            if rb == 0 || rq == 0 {
                return Eval::Cold;
            }
            let mut st = state.clone_shallow_pamm(rb, rq);
            let q = crate::execution::amms::pumpfun_amm::pamm_quote_exact_in(&st, input_mint, amount);
            let PoolState::PumpFunAmm { base_mint, virtual_quote_reserve, .. } = &mut st else { unreachable!() };
            let (rin, rout) = if input_mint == base_mint { (rb as u128, rq as u128 + *virtual_quote_reserve as u128) } else { (rq as u128 + *virtual_quote_reserve as u128, rb as u128) };
            return Eval::Quoted(q.map(|(out, fee)| (out, fee, rin, rout)));
        }

        if let PoolState::Meteora { token_a_mint, token_b_mint, reserves, .. } = state {
            let a_to_b = if input_mint == token_a_mint { true } else if input_mint == token_b_mint { false } else { return Eval::Quoted(None) };
            if reserves.computed_at == 0 {
                return Eval::Cold; // reserves never computed (old warm file)
            }
            let now = crate::stream::chain_unix_time();
            return Eval::Quoted(super::meteora_std::swap_exact_in(reserves, a_to_b, amount, now).map(|(out, fee)| {
                let (rin, rout) = if a_to_b { (reserves.token_a_amount, reserves.token_b_amount) } else { (reserves.token_b_amount, reserves.token_a_amount) };
                (out, fee, rin as u128, rout as u128)
            }));
        }

        if let PoolState::RaydiumLp { base_mint, quote_mint, curve, .. } = state {
            let q = if input_mint == quote_mint {
                curve.buy_exact_in(amount)
            } else if input_mint == base_mint {
                curve.sell_exact_in(amount)
            } else {
                None
            };
            return Eval::Quoted(q.map(|q| {
                let quote_res = curve.virtual_quote as u128 + curve.real_quote as u128;
                let base_res = (curve.virtual_base.saturating_sub(curve.real_base)) as u128;
                let (rin, rout) = if input_mint == quote_mint { (quote_res, base_res) } else { (base_res, quote_res) };
                (q.amount_out, q.fee, rin, rout)
            }));
        }

        if let PoolState::MeteoraDlmm { lb_pair, token_x_mint, token_y_mint, pair, .. } = state {
            // Exact bin walk over the pair's own snapshot (LbPair + bin arrays
            // read together); without a fresh one the answer is "not yet".
            let swap_for_y = if input_mint == token_x_mint { true } else if input_mint == token_y_mint { false } else { return Eval::Quoted(None) };
            let bins = match super::dlmm::BINS.get(lb_pair) {
                Some(b) if b.is_current(pair, self.cache.ttl()) => b,
                _ => return Eval::Cold,
            };
            let slot = crate::stream::latest_slot().max(bins.slot);
            return match super::dlmm::swap_exact_in(&bins, swap_for_y, amount, bins.chain_now(), slot, super::dlmm::SWAP_ARRAYS) {
                Ok(q) if q.amount_out > 0 => {
                    let (rin, rout) = bins.implied_reserves(swap_for_y);
                    Eval::Quoted(Some((q.amount_out, q.fee, rin, rout)))
                }
                // an array the snapshot lacks (the pair moved since): re-read
                Err(super::dlmm::WalkError::NotLoaded) => Eval::Cold,
                // liquidity runs out within the arrays a swap can carry, or disabled
                _ => Eval::Quoted(None),
            };
        }

        if let PoolState::PumpFun { mint, curve, .. } = state {
            let q = if *input_mint == crate::constants::SOL_NATIVE_MINT {
                curve.buy_exact_in(amount)
            } else if input_mint == mint {
                curve.sell_exact_in(amount)
            } else {
                None
            };
            return Eval::Quoted(q.map(|q| {
                let (sol, tok) = (curve.virtual_sol_reserves as u128, curve.virtual_token_reserves as u128);
                let (rin, rout) = if input_mint == mint { (tok, sol) } else { (sol, tok) };
                (q.amount_out, q.fee, rin, rout)
            }));
        }

        if let PoolState::MeteoraDbc { base_mint, quote_mint, curve, .. } = state {
            return Eval::Quoted(quote_dbc(input_mint, base_mint, quote_mint, curve, amount));
        }

        if let PoolState::MeteoraDamm { token_a_mint, token_b_mint, liquidity, sqrt_price, sqrt_min_price, sqrt_max_price, token_a_amount, token_b_amount, fees, activation_point, activation_type, collect_fee_mode, pool_status, .. } = state {
            return Eval::Quoted(quote_damm_v2(
                input_mint, token_a_mint, token_b_mint, *liquidity, *sqrt_price, *sqrt_min_price, *sqrt_max_price, (*token_a_amount, *token_b_amount), fees, *activation_point, *activation_type, *collect_fee_mode, *pool_status, amount,
            ));
        }

        // Mirrored vault balances are as fresh as the last block; inline
        // reserves are as old as the last state fetch.
        let reserves = self
            .get_reserves_from_mirror(state, input_mint)
            .or_else(|| extract_reserves_inline(state, input_mint));
        match reserves {
            Some((reserve_in, reserve_out)) => Eval::Quoted(self.price_cp(state, entry, input_mint, amount, reserve_in, reserve_out)),
            None => Eval::Cold,
        }
    }

    /// Constant-product leg with the fee THIS pool charges.
    fn price_cp(&self, state: &PoolState, entry: &PoolEntry, input_mint: &Pubkey, amount: u64, reserve_in: u128, reserve_out: u128) -> Option<LegResult> {
        // Raydium CPMM vaults also hold accrued protocol + fund fees, which the
        // program excludes from the curve; quoting on the raw vault balance
        // over-quotes by their share.
        let (reserve_in, reserve_out) = match state {
            PoolState::RaydiumCpmm { token_0_mint, protocol_fees_0, protocol_fees_1, fund_fees_0, fund_fees_1, .. } => {
                let (fee_in, fee_out) = if input_mint == token_0_mint {
                    (*protocol_fees_0 as u128 + *fund_fees_0 as u128, *protocol_fees_1 as u128 + *fund_fees_1 as u128)
                } else {
                    (*protocol_fees_1 as u128 + *fund_fees_1 as u128, *protocol_fees_0 as u128 + *fund_fees_0 as u128)
                };
                (reserve_in.saturating_sub(fee_in), reserve_out.saturating_sub(fee_out))
            }
            _ => (reserve_in, reserve_out),
        };
        // Raydium CPMM: the program's own arithmetic — trade fee (ceil, /1e6) and,
        // when enabled, the creator fee on the input or output side.
        if let PoolState::RaydiumCpmm { token_0_mint, trade_fee_bps, creator_fee_ppm, enable_creator_fee, creator_fee_on, .. } = state {
            if *trade_fee_bps > 0 {
                let is_token0_in = input_mint == token_0_mint;
                let creator_ppm = if *enable_creator_fee { *creator_fee_ppm as u128 } else { 0 };
                let creator_on_input = creator_ppm > 0 && match creator_fee_on { 0 => true, 1 => is_token0_in, _ => !is_token0_in };
                let ceil_ppm = |a: u128, ppm: u128| (a * ppm).div_ceil(1_000_000);
                let trade_fee = ceil_ppm(amount as u128, *trade_fee_bps as u128 * 100);
                let creator_in = if creator_on_input { ceil_ppm(amount as u128, creator_ppm) } else { 0 };
                let in_less = (amount as u128).checked_sub(trade_fee + creator_in)?;
                let out = reserve_out.checked_mul(in_less)? / reserve_in.checked_add(in_less)?;
                let creator_out = if creator_ppm > 0 && !creator_on_input { ceil_ppm(out, creator_ppm) } else { 0 };
                let out = u64::try_from(out.checked_sub(creator_out)?).ok()?;
                if out == 0 {
                    return None;
                }
                return Some((out, (trade_fee + creator_in) as u64, reserve_in, reserve_out));
            }
        }
        // Raydium AMM v4: the program's own arithmetic on `vault − need_take_pnl`.
        if let PoolState::RaydiumV4 { coin_mint, swap_fee_numerator, swap_fee_denominator, need_take_pnl_coin, need_take_pnl_pc, status, pool_open_time, .. } = state {
            let now = crate::stream::chain_unix_time();
            if !crate::execution::amms::raydium_v4::can_swap(*status, *pool_open_time, now) {
                return None;
            }
            let (pnl_in, pnl_out) = if input_mint == coin_mint { (*need_take_pnl_coin, *need_take_pnl_pc) } else { (*need_take_pnl_pc, *need_take_pnl_coin) };
            let reserve_in = reserve_in.checked_sub(pnl_in as u128)?;
            let reserve_out = reserve_out.checked_sub(pnl_out as u128)?;
            let (out, fee) = crate::execution::amms::raydium_v4::swap_base_in_out(reserve_in, reserve_out, amount, *swap_fee_numerator, *swap_fee_denominator)?;
            return Some((out, fee, reserve_in, reserve_out));
        }
        // SPL token-swap forks: the pool's own fee schedule (trade + owner
        // trade, off the input), constant-product curve only. Saros is not
        // here: its pools deliver ≈ curve(in − owner_fee); the trade fee in its
        // state is not taken from the swap, so Saros uses the observed-fee path.
        if let PoolState::Dooar { fees, .. } | PoolState::FluxBeam { fees, .. } = state {
            if let Some(fee) = fees.total_fee(amount) {
                if fees.curve_type != 0 {
                    return None;
                }
                let in_less = (amount as u128).checked_sub(fee as u128)?;
                let out = reserve_out.checked_mul(in_less)? / reserve_in.checked_add(in_less)?;
                let out = u64::try_from(out).ok()?;
                if out == 0 {
                    return None;
                }
                return Some((out, fee, reserve_in, reserve_out));
            }
        }
        // A fee measured from this pool's own recent swaps, at ppm resolution.
        if let Some(ppm) = crate::stream::observed_fees::get_fresh_ppm(&entry.address, OBSERVED_FEE_MAX_AGE) {
            let fee = (amount as u128 * ppm as u128).div_ceil(1_000_000);
            let in_less = (amount as u128).checked_sub(fee)?;
            let out = u64::try_from(reserve_out.checked_mul(in_less)? / reserve_in.checked_add(in_less)?).ok()?;
            return (out > 0).then_some((out, fee as u64, reserve_in, reserve_out));
        }
        let fee_bps = venue_fee_bps(state, entry.pool_type, &entry.address);
        match leg_out(fee_bps, reserve_in, reserve_out, amount) {
            Some((o, f)) if o > 0 => Some((o, f, reserve_in, reserve_out)),
            _ => None,
        }
    }

    /// Re-read a quiet pool off the quote path. At most one in flight per pool.
    fn revalidate_in_background(&self, entry: &PoolEntry) {
        if !self.revalidating.insert(entry.address) {
            return;
        }
        let (rpc, cache, inflight, mirror) = (Arc::clone(&self.rpc), Arc::clone(&self.cache), Arc::clone(&self.revalidating), self.mirror.clone());
        let (addr, pool_type) = (entry.address, entry.pool_type);
        tokio::spawn(async move {
            let prev = cache.get(&addr);
            let refreshed = match prev.clone() {
                // pAMM: two balance reads; the full fetch would re-run the
                // buyback-account resolve (getSignatures + getTransaction × N).
                Some(mut st @ PoolState::PumpFunAmm { .. }) => {
                    // an unknown supply means the fee tier falls back to the most
                    // expensive one: retry it
                    if let PoolState::PumpFunAmm { base_mint, base_supply, .. } = &mut st {
                        if *base_supply == 0 {
                            if let Ok(sup) = crate::pool::fetcher::fetch_mint_supply(&rpc, base_mint).await {
                                *base_supply = sup;
                            }
                        }
                    }
                    crate::pool::fetcher::refresh_pamm_reserves(&rpc, &mut st).await.map(|_| st)
                }
                _ => crate::pool::fetcher::fetch_pool_state(&rpc, pool_type, &addr).await.map(|mut fresh| {
                    if let Some(p) = &prev {
                        fresh.carry_over_pamm_fee_accounts(p);
                    }
                    fresh
                }),
            };
            if let Ok(fresh) = &refreshed {
                if is_clmm(pool_type) {
                    // one getMultipleAccounts: the ±3 tick arrays + bitmap extension
                    let _ = crate::pool::ticks::load_clmm_ticks(&rpc, fresh).await;
                }
                if pool_type == PoolType::MeteoraDlmm {
                    let _ = crate::pool::bins::load_dlmm_bins(&rpc, fresh).await;
                }
            }
            if let Ok(fresh) = refreshed {
                if let (Some(m), PoolState::PumpFunAmm { pool_base_vault, pool_quote_vault, base_reserve, quote_reserve, .. }) = (mirror.as_ref(), &fresh) {
                    m.update_vault_balance(*pool_base_vault, *base_reserve);
                    m.update_vault_balance(*pool_quote_vault, *quote_reserve);
                }
                cache.insert(addr, fresh);
            }
            inflight.remove(&addr);
        });
    }

    /// Price many pools for the same (input, amount): every pool that can be
    /// answered from memory is, synchronously and in order; only the cold ones
    /// are awaited, concurrently. Returns `(index into entries, result)`.
    async fn evaluate_many(&self, entries: &[PoolEntry], input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Vec<(usize, LegResult)> {
        // Token-2022 transfer fees: the DEX receives `amount − fee` and the user
        // receives `out − fee(out)`; the router's slippage check sees the latter.
        let amount_eff = amount.saturating_sub(crate::pool::mints::transfer_fee(input_mint).map(|f| f.fee(amount)).unwrap_or(0));
        let net_out = |r: LegResult| -> LegResult { (crate::pool::mints::net_of_transfer_fee(output_mint, r.0), r.1, r.2, r.3) };
        let mut out = Vec::with_capacity(entries.len());
        let mut cold: Vec<usize> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            if self.cache.is_dormant(&e.address) {
                continue;
            }
            match self.evaluate_hot(e, input_mint, amount_eff) {
                Eval::Quoted(Some(r)) => out.push((i, net_out(r))),
                Eval::Quoted(None) => {}
                Eval::Cold => cold.push(i),
            }
        }
        if !cold.is_empty() {
            // Unknown mints are looked up once (transfer fee, token program);
            // the input's fee is only known after that.
            let mut amount_eff = amount_eff;
            if !crate::pool::mints::is_known(input_mint) || !crate::pool::mints::is_known(output_mint) {
                crate::pool::mints::ensure_mint_info(&self.rpc, &[*input_mint, *output_mint]).await;
                amount_eff = amount.saturating_sub(crate::pool::mints::transfer_fee(input_mint).map(|f| f.fee(amount)).unwrap_or(0));
            }
            let amount_eff = amount_eff;
            let t0 = std::time::Instant::now();
            let futs = cold.iter().map(|&i| async move {
                (i, self.evaluate_cold(&entries[i], input_mint, output_mint, amount_eff).await)
            });
            let mut ok = 0usize;
            for (i, r) in futures::future::join_all(futs).await {
                if let Some(r) = r {
                    ok += 1;
                    out.push((i, net_out(r)));
                }
            }
            // Steady state is zero cold pools per quote; anything else is a
            // pool the block-driven refresh has not covered yet.
            tracing::info!(cold = cold.len(), priced = ok, ms = t0.elapsed().as_millis() as u64,
                pools = ?cold.iter().take(4).map(|&i| (entries[i].pool_type, entries[i].address.to_string())).collect::<Vec<_>>(), "quote cold path");
        }
        out
    }

    /// COLD PATH. The pool's state was never fetched, or its vault balances are
    /// not mirrored: read them over RPC (1–3 round trips), seed cache + mirror
    /// so the next quote is hot, then price.
    async fn evaluate_cold(&self, entry: &PoolEntry, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Option<LegResult> {
        let r = self.evaluate_cold_inner(entry, input_mint, output_mint, amount).await;
        // A pool that keeps failing its cold path (dead account, unparseable
        // layout, tick arrays that never load) would otherwise cost an RPC
        // round trip on EVERY quote that touches its pair. After
        // `DORMANT_THRESHOLD` consecutive failures it is skipped until a
        // background refresh brings it back.
        match r {
            Some(_) => self.cache.reset_failure(&entry.address),
            None => self.cache.record_failure(&entry.address),
        }
        r
    }

    async fn evaluate_cold_inner(&self, entry: &PoolEntry, input_mint: &Pubkey, _output_mint: &Pubkey, amount: u64) -> Option<LegResult> {
        let t0 = std::time::Instant::now();
        let mut state = match self.cache.get(&entry.address) {
            Some(s) => s,
            None => match crate::pool::fetcher::fetch_pool_state(&self.rpc, entry.pool_type, &entry.address).await {
                Ok(s) => {
                    self.cache.insert(entry.address, s.clone());
                    s
                }
                Err(e) => {
                    debug!(pool = %entry.address, ?entry.pool_type, error = %e, "cold: state fetch failed");
                    return None;
                }
            },
        };
        if is_clmm(entry.pool_type) {
            let fresh_ticks = clmm::TICKS.get(&entry.address).map(|t| t.fetched_at.elapsed() <= self.cache.ttl()).unwrap_or(false);
            if !fresh_ticks {
                if let Err(e) = crate::pool::ticks::load_clmm_ticks(&self.rpc, &state).await {
                    debug!(pool = %entry.address, error = %e, "cold: tick arrays failed");
                    return None;
                }
            }
        }
        if let PoolState::MeteoraDlmm { pair, .. } = &state {
            let fresh_bins = super::dlmm::BINS.get(&entry.address).map(|b| b.is_current(pair, self.cache.ttl())).unwrap_or(false);
            if !fresh_bins {
                if let Err(e) = crate::pool::bins::load_dlmm_bins(&self.rpc, &state).await {
                    debug!(pool = %entry.address, error = %e, "cold: dlmm bin arrays failed");
                    return None;
                }
            }
        }
        // Meteora Standard whose vault-share reserves were never computed (old
        // warm file): one batched read of its 6 accounts, then cache.
        if let PoolState::Meteora { reserves, .. } = &state {
            if reserves.computed_at == 0 {
                let keys = crate::pool::fetcher::meteora_std_refresh_keys(&state)?;
                let accts = self.rpc.get_multiple_accounts(&keys).await.ok()?;
                if !crate::pool::fetcher::refresh_meteora_std_from(&mut state, &accts) {
                    return None;
                }
                self.cache.insert(entry.address, state.clone());
            }
        }
        let quoted = match self.quote_state(&state, entry, input_mint, amount) {
            Eval::Quoted(r) => Some(r),
            Eval::Cold => None,
        };
        if let Some(r) = quoted {
            debug!(pool = %entry.address, ?entry.pool_type, ms = t0.elapsed().as_millis() as u64, "cold: state-priced");
            return r;
        }
        // Constant-product pool without mirrored balances: fetch them once.
        let (reserve_in, reserve_out) = fetch_reserves(&self.rpc, &state, input_mint).await?;
        if let (Some(mirror), Some((va, vb, ma, _mb))) = (self.mirror.as_ref(), extract_vault_mints(&state)) {
            let (bal_a, bal_b) = if *input_mint == ma { (reserve_in as u64, reserve_out as u64) } else { (reserve_out as u64, reserve_in as u64) };
            mirror.update_vault_balance(va, bal_a);
            mirror.update_vault_balance(vb, bal_b);
            if !mirror.is_vault(&va) {
                mirror.register_vault(va, entry.address);
            }
            if !mirror.is_vault(&vb) {
                mirror.register_vault(vb, entry.address);
            }
        }
        debug!(pool = %entry.address, ?entry.pool_type, ms = t0.elapsed().as_millis() as u64, "cold: vault balances");
        self.price_cp(&state, entry, input_mint, amount, reserve_in, reserve_out)
    }

    /// Try to get reserves from the AccountMirror's vault balance cache.
    /// Returns (reserve_in, reserve_out) if both vault balances are available.
    /// Zero RPC — reads from the in-memory mirror fed by Geyser.
    fn get_reserves_from_mirror(
        &self,
        state: &PoolState,
        input_mint: &Pubkey,
    ) -> Option<(u128, u128)> {
        let mirror = self.mirror.as_ref()?;
        let (vault_a, vault_b, mint_a, mint_b) = extract_vault_mints(state)?;

        // Can't determine direction without mints
        if mint_a == Pubkey::default() && mint_b == Pubkey::default() {
            return None;
        }

        let bal_a = match mirror.get_vault_balance(&vault_a) {
            Some(b) => b as u128,
            None => {
                debug!(vault = %vault_a, "mirror miss: vault A balance not cached");
                return None;
            }
        };
        let bal_b = match mirror.get_vault_balance(&vault_b) {
            Some(b) => b as u128,
            None => {
                debug!(vault = %vault_b, "mirror miss: vault B balance not cached");
                return None;
            }
        };

        if *input_mint == mint_a {
            Some((bal_a, bal_b))
        } else {
            Some((bal_b, bal_a))
        }
    }

    /// Filter pool entries by mint pair + dex whitelist/blacklist.
    fn filter_entries(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        dexes: &[String],
        exclude_dexes: &[String],
    ) -> Vec<PoolEntry> {
        self.registry
            .lookup(input_mint, output_mint)
            .into_iter()
            .filter(|e| {
                // Skip pools we can't quote (non-CP and non-CLMM)
                if !is_quotable(e.pool_type) {
                    return false;
                }

                let label = label_for_pool_type(e.pool_type);

                // Apply dex whitelist
                if !dexes.is_empty() && !dexes.iter().any(|d| d == label) {
                    return false;
                }

                // Apply dex blacklist
                if exclude_dexes.iter().any(|d| d == label) {
                    return false;
                }

                true
            })
            .collect()
    }

    /// Build a QuoteResponse from a direct route.
    fn build_direct_response(
        &self,
        req: &QuoteRequest,
        route: &DirectRoute,
        context_slot: u64,
        elapsed: f64,
    ) -> QuoteResponse {
        let threshold = compute_threshold(route.out_amount, req.slippage_bps);
        let price_impact = compute_price_impact_for_type(
            route.pool_type,
            req.amount,
            route.out_amount,
            route.reserve_in,
            route.reserve_out,
        );
        let platform_fee = compute_platform_fee(route.out_amount, &req.output_mint);

        QuoteResponse {
            input_token: req.input_mint.to_string(),
            amount_in: req.amount.to_string(),
            output_token: req.output_mint.to_string(),
            amount_out: route.out_amount.to_string(),
            minimum_out: threshold.to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: req.slippage_bps,
            price_impact: price_impact,
            routes: vec![RouteStep {
                pool: PoolRoute {
                    pool_address: route.pool_address.to_string(),
                    dex: label_for_pool_type(route.pool_type).to_string(),
                    input_token: req.input_mint.to_string(),
                    output_token: req.output_mint.to_string(),
                    amount_in: req.amount.to_string(),
                    amount_out: route.out_amount.to_string(),
                    fee: route.fee_amount.to_string(),
                    fee_token: req.input_mint.to_string(),
                },
                percent: 100,
            }],
            slot: context_slot,
            quote_time_ms: elapsed,
            platform_fee: Some(platform_fee),
        }
    }

    /// Build a QuoteResponse from a 2-hop route.
    fn build_two_hop_response(
        &self,
        req: &QuoteRequest,
        route: &TwoHopRoute,
        context_slot: u64,
        elapsed: f64,
    ) -> QuoteResponse {
        let threshold = compute_threshold(route.final_amount_out, req.slippage_bps);

        // Price impact for multi-hop: use combined impact
        // Approximate: use the product of (1-impact) for each hop
        let impact1 = estimate_price_impact(
            route.hop1_reserve_in,
            route.hop1_reserve_out,
            req.amount,
            route.hop1_amount_out,
        );
        let impact2 = estimate_price_impact(
            route.hop2_reserve_in,
            route.hop2_reserve_out,
            route.hop1_amount_out,
            route.final_amount_out,
        );

        // Combine impacts: total_impact = 1 - (1 - impact1) * (1 - impact2)
        let i1: f64 = impact1.parse().unwrap_or(0.0);
        let i2: f64 = impact2.parse().unwrap_or(0.0);
        let combined = 1.0 - (1.0 - i1 / 100.0) * (1.0 - i2 / 100.0);
        let combined_pct = (combined * 100.0).max(0.0);
        let price_impact = format!("{:.2}", combined_pct);

        let platform_fee = compute_platform_fee(route.final_amount_out, &req.output_mint);

        QuoteResponse {
            input_token: req.input_mint.to_string(),
            amount_in: req.amount.to_string(),
            output_token: req.output_mint.to_string(),
            amount_out: route.final_amount_out.to_string(),
            minimum_out: threshold.to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: req.slippage_bps,
            price_impact: price_impact,
            routes: vec![
                RouteStep {
                    pool: PoolRoute {
                        pool_address: route.hop1_entry.address.to_string(),
                        dex: label_for_pool_type(route.hop1_entry.pool_type).to_string(),
                        input_token: req.input_mint.to_string(),
                        output_token: route.bridge_mint.to_string(),
                        amount_in: req.amount.to_string(),
                        amount_out: route.hop1_amount_out.to_string(),
                        fee: route.hop1_fee_amount.to_string(),
                        fee_token: req.input_mint.to_string(),
                    },
                    percent: 100,
                },
                RouteStep {
                    pool: PoolRoute {
                        pool_address: route.hop2_entry.address.to_string(),
                        dex: label_for_pool_type(route.hop2_entry.pool_type).to_string(),
                        input_token: route.bridge_mint.to_string(),
                        output_token: req.output_mint.to_string(),
                        amount_in: guaranteed(route.hop1_amount_out, req.slippage_bps).to_string(),
                        amount_out: route.final_amount_out.to_string(),
                        fee: route.hop2_fee_amount.to_string(),
                        fee_token: route.bridge_mint.to_string(),
                    },
                    percent: 100,
                },
            ],
            slot: context_slot,
            quote_time_ms: elapsed,
            platform_fee: Some(platform_fee),
        }
    }

    /// Build a QuoteResponse from a 3-hop route.
    fn build_three_hop_response(
        &self,
        req: &QuoteRequest,
        route: &ThreeHopRoute,
        context_slot: u64,
        elapsed: f64,
    ) -> QuoteResponse {
        let threshold = compute_threshold(route.final_amount_out, req.slippage_bps);

        // Price impact for 3-hop: product of (1-impact) for each hop
        let impact1 = estimate_price_impact(
            route.hop1_reserve_in,
            route.hop1_reserve_out,
            req.amount,
            route.hop1_amount_out,
        );
        let impact2 = estimate_price_impact(
            route.hop2_reserve_in,
            route.hop2_reserve_out,
            route.hop1_amount_out,
            route.hop2_amount_out,
        );
        let impact3 = estimate_price_impact(
            route.hop3_reserve_in,
            route.hop3_reserve_out,
            route.hop2_amount_out,
            route.final_amount_out,
        );

        // Combine impacts: total = 1 - (1-i1)(1-i2)(1-i3)
        let i1: f64 = impact1.parse().unwrap_or(0.0);
        let i2: f64 = impact2.parse().unwrap_or(0.0);
        let i3: f64 = impact3.parse().unwrap_or(0.0);
        let combined = 1.0 - (1.0 - i1 / 100.0) * (1.0 - i2 / 100.0) * (1.0 - i3 / 100.0);
        let combined_pct = (combined * 100.0).max(0.0);
        let price_impact = format!("{:.2}", combined_pct);

        let platform_fee = compute_platform_fee(route.final_amount_out, &req.output_mint);

        QuoteResponse {
            input_token: req.input_mint.to_string(),
            amount_in: req.amount.to_string(),
            output_token: req.output_mint.to_string(),
            amount_out: route.final_amount_out.to_string(),
            minimum_out: threshold.to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: req.slippage_bps,
            price_impact,
            routes: vec![
                RouteStep {
                    pool: PoolRoute {
                        pool_address: route.hop1_entry.address.to_string(),
                        dex: label_for_pool_type(route.hop1_entry.pool_type).to_string(),
                        input_token: req.input_mint.to_string(),
                        output_token: route.bridge1_mint.to_string(),
                        amount_in: req.amount.to_string(),
                        amount_out: route.hop1_amount_out.to_string(),
                        fee: route.hop1_fee_amount.to_string(),
                        fee_token: req.input_mint.to_string(),
                    },
                    percent: 100,
                },
                RouteStep {
                    pool: PoolRoute {
                        pool_address: route.hop2_entry.address.to_string(),
                        dex: label_for_pool_type(route.hop2_entry.pool_type).to_string(),
                        input_token: route.bridge1_mint.to_string(),
                        output_token: route.bridge2_mint.to_string(),
                        amount_in: guaranteed(route.hop1_amount_out, req.slippage_bps).to_string(),
                        amount_out: route.hop2_amount_out.to_string(),
                        fee: route.hop2_fee_amount.to_string(),
                        fee_token: route.bridge1_mint.to_string(),
                    },
                    percent: 100,
                },
                RouteStep {
                    pool: PoolRoute {
                        pool_address: route.hop3_entry.address.to_string(),
                        dex: label_for_pool_type(route.hop3_entry.pool_type).to_string(),
                        input_token: route.bridge2_mint.to_string(),
                        output_token: req.output_mint.to_string(),
                        amount_in: guaranteed(route.hop2_amount_out, req.slippage_bps).to_string(),
                        amount_out: route.final_amount_out.to_string(),
                        fee: route.hop3_fee_amount.to_string(),
                        fee_token: route.bridge2_mint.to_string(),
                    },
                    percent: 100,
                },
            ],
            slot: context_slot,
            quote_time_ms: elapsed,
            platform_fee: Some(platform_fee),
        }
    }

    /// Build a QuoteResponse from a split route.
    fn build_split_response(
        &self,
        req: &QuoteRequest,
        split: &SplitRoute,
        context_slot: u64,
        elapsed: f64,
    ) -> QuoteResponse {
        let threshold = compute_threshold(split.total_out, req.slippage_bps);

        // Combined price impact from both legs (weighted)
        let impact_a = estimate_price_impact(
            split.pool_a.reserve_in,
            split.pool_a.reserve_out,
            split.amount_a,
            split.out_a,
        );
        let impact_b = estimate_price_impact(
            split.pool_b.reserve_in,
            split.pool_b.reserve_out,
            split.amount_b,
            split.out_b,
        );

        // Weighted average of the two impacts
        let ia: f64 = impact_a.parse().unwrap_or(0.0);
        let ib: f64 = impact_b.parse().unwrap_or(0.0);
        let weighted = (ia * split.pct_a as f64 + ib * split.pct_b as f64) / 100.0;
        let price_impact = format!("{:.2}", weighted);

        let platform_fee = compute_platform_fee(split.total_out, &req.output_mint);

        QuoteResponse {
            input_token: req.input_mint.to_string(),
            amount_in: req.amount.to_string(),
            output_token: req.output_mint.to_string(),
            amount_out: split.total_out.to_string(),
            minimum_out: threshold.to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: req.slippage_bps,
            price_impact: price_impact,
            routes: vec![
                RouteStep {
                    pool: PoolRoute {
                        pool_address: split.pool_a.pool_address.to_string(),
                        dex: label_for_pool_type(split.pool_a.pool_type).to_string(),
                        input_token: req.input_mint.to_string(),
                        output_token: req.output_mint.to_string(),
                        amount_in: split.amount_a.to_string(),
                        amount_out: split.out_a.to_string(),
                        fee: split.fee_a.to_string(),
                        fee_token: req.input_mint.to_string(),
                    },
                    percent: split.pct_a,
                },
                RouteStep {
                    pool: PoolRoute {
                        pool_address: split.pool_b.pool_address.to_string(),
                        dex: label_for_pool_type(split.pool_b.pool_type).to_string(),
                        input_token: req.input_mint.to_string(),
                        output_token: req.output_mint.to_string(),
                        amount_in: split.amount_b.to_string(),
                        amount_out: split.out_b.to_string(),
                        fee: split.fee_b.to_string(),
                        fee_token: req.input_mint.to_string(),
                    },
                    percent: split.pct_b,
                },
            ],
            slot: context_slot,
            quote_time_ms: elapsed,
            platform_fee: Some(platform_fee),
        }
    }
}

/// Evaluate split routes across pairs of direct-route pools.
///
/// When multiple pools exist for the same pair, splitting input across two pools
/// can yield more output because each pool absorbs less price impact.
///
/// Tries splits: 100/0, 90/10, 80/20, ..., 50/50 for each pool pair.
/// Returns the best split if it beats the best single-pool route, otherwise None.
#[cfg(test)]
fn evaluate_split_routes(
    direct_routes: &[DirectRoute],
    amount: u64,
) -> Option<SplitRoute> {
    if direct_routes.len() < 2 {
        return None;
    }

    // Best single-pool output
    let best_single = direct_routes.iter().map(|r| r.out_amount).max().unwrap_or(0);

    let mut best_split: Option<SplitRoute> = None;

    // Try all pairs of pools
    for (i, pool_a) in direct_routes.iter().enumerate() {
        for pool_b in direct_routes.iter().skip(i + 1) {
            // Try split percentages: 90/10, 80/20, ..., 50/50, 40/60, ..., 10/90
            for pct_a in (10u8..=90).step_by(10) {
                let pct_b = 100 - pct_a;
                let amount_a = (amount as u128 * pct_a as u128 / 100) as u64;
                let amount_b = amount.saturating_sub(amount_a);

                if amount_a == 0 || amount_b == 0 {
                    continue;
                }

                let (out_a, fee_a) = match leg_out(pool_a.fee_bps, pool_a.reserve_in, pool_a.reserve_out, amount_a) {
                    Some((o, f)) if o > 0 => (o, f),
                    _ => continue,
                };
                let (out_b, fee_b) = match leg_out(pool_b.fee_bps, pool_b.reserve_in, pool_b.reserve_out, amount_b) {
                    Some((o, f)) if o > 0 => (o, f),
                    _ => continue,
                };

                let total_out = out_a + out_b;

                // Only keep if it beats best single and current best split
                if total_out > best_single {
                    let current_best = best_split.as_ref().map(|s| s.total_out).unwrap_or(0);
                    if total_out > current_best {
                        best_split = Some(SplitRoute {
                            pool_a: pool_a.clone(),
                            pool_b: pool_b.clone(),
                            pct_a,
                            pct_b,
                            amount_a,
                            amount_b,
                            out_a,
                            out_b,
                            total_out,
                            fee_a,
                            fee_b,
                        });
                    }
                }
            }
        }
    }

    best_split
}

// ── Vault Balance Fetching ──

/// Fetch the token balance of an SPL token account.
async fn fetch_vault_balance(rpc: &RpcClient, vault: &Pubkey) -> Option<u64> {
    rpc.get_token_account_balance(vault)
        .await
        .ok()
        .and_then(|b| b.amount.parse::<u64>().ok())
}

/// Extract (vault_a, vault_b, mint_a, mint_b) from a PoolState for vault balance fetching.
/// Returns None for unsupported variants (PumpFun bonding, FlashTrade, CLMM, etc.)
fn extract_vault_mints(pool_state: &PoolState) -> Option<(Pubkey, Pubkey, Pubkey, Pubkey)> {
    match pool_state {
        PoolState::RaydiumCpmm {
            token_0_vault,
            token_1_vault,
            token_0_mint,
            token_1_mint,
            ..
        } => Some((*token_0_vault, *token_1_vault, *token_0_mint, *token_1_mint)),

        PoolState::RaydiumLp {
            base_vault,
            quote_vault,
            base_mint,
            quote_mint,
            ..
        } => Some((*base_vault, *quote_vault, *base_mint, *quote_mint)),

        PoolState::RaydiumV4 {
            coin_vault,
            pc_vault,
            coin_mint,
            pc_mint,
            ..
        } => Some((*coin_vault, *pc_vault, *coin_mint, *pc_mint)),

        PoolState::Meteora {
            a_token_vault,
            b_token_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*a_token_vault, *b_token_vault, *token_a_mint, *token_b_mint)),

        PoolState::MeteoraDamm {
            token_a_vault,
            token_b_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_vault, *token_b_vault, *token_a_mint, *token_b_mint)),

        PoolState::MeteoraDbc {
            base_vault,
            quote_vault,
            base_mint,
            quote_mint,
            ..
        } => Some((*base_vault, *quote_vault, *base_mint, *quote_mint)),

        PoolState::FluxBeam {
            token_a_vault,
            token_b_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_vault, *token_b_vault, *token_a_mint, *token_b_mint)),

        PoolState::Saros {
            token_a_vault,
            token_b_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_vault, *token_b_vault, *token_a_mint, *token_b_mint)),

        PoolState::Dooar {
            token_a_vault,
            token_b_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_vault, *token_b_vault, *token_a_mint, *token_b_mint)),

        // Pumpup has inline reserves — vault fetch not needed for quoting
        // (handled by extract_reserves_inline). We still expose vaults+mints
        // here so cold paths or 2-hop routing that bypasses inline can fall
        // back to vault RPC fetch.
        PoolState::Pumpup {
            token_a_vault,
            token_b_vault,
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_vault, *token_b_vault, *token_a_mint, *token_b_mint)),

        // PumpFunAmm has inline reserves — no vault fetch needed (handled by extract_reserves_inline)
        // PumpFun bonding, FlashTrade, CLMM pools — not supported for vault balance fetching
        _ => None,
    }
}

/// Extract (reserve_in, reserve_out) from inline pool data (no RPC needed).
/// Only PumpFunAmm stores reserves directly in PoolState.
fn extract_reserves_inline(
    state: &PoolState,
    input_mint: &Pubkey,
) -> Option<(u128, u128)> {
    match state {
        PoolState::PumpFunAmm {
            base_reserve,
            quote_reserve,
            base_mint,
            quote_mint,
            ..
        } => {
            let (res_in, res_out) = if *input_mint == *base_mint {
                (*base_reserve as u128, *quote_reserve as u128)
            } else if *input_mint == *quote_mint {
                (*quote_reserve as u128, *base_reserve as u128)
            } else {
                return None;
            };
            Some((res_in, res_out))
        }
        PoolState::Pumpup {
            token_a_mint,
            token_b_mint,
            token_a_reserve,
            token_b_reserve,
            ..
        } => {
            let (res_in, res_out) = if *input_mint == *token_a_mint {
                (*token_a_reserve as u128, *token_b_reserve as u128)
            } else if *input_mint == *token_b_mint {
                (*token_b_reserve as u128, *token_a_reserve as u128)
            } else {
                return None;
            };
            Some((res_in, res_out))
        }
        // Pumpup pre-graduation bonding curve. SOL side uses
        // `virtual_sol + real_sol` to mirror the program's internal
        // constant-product math (matches PumpFun bonding pattern).
        PoolState::PumpupBonding {
            mint,
            virtual_sol,
            real_sol,
            pool_token_reserves,
            ..
        } => {
            use crate::constants::SOL_NATIVE_MINT;
            let sol_side = (*virtual_sol as u128).saturating_add(*real_sol as u128);
            let token_side = *pool_token_reserves as u128;
            let (res_in, res_out) = if *input_mint == SOL_NATIVE_MINT {
                (sol_side, token_side)
            } else if *input_mint == *mint {
                (token_side, sol_side)
            } else {
                return None;
            };
            Some((res_in, res_out))
        }
        _ => None,
    }
}

/// Fetch (reserve_in, reserve_out) for a pool by fetching vault balances.
/// For PumpFunAmm, returns inline reserves directly (no RPC).
/// Uses tokio::join! for parallel vault balance fetches.
async fn fetch_reserves(
    rpc: &RpcClient,
    pool_state: &PoolState,
    input_mint: &Pubkey,
) -> Option<(u128, u128)> {
    let (vault_a, vault_b, mint_a, mint_b) = extract_vault_mints(pool_state)?;

    let (bal_a, bal_b) = tokio::join!(
        fetch_vault_balance(rpc, &vault_a),
        fetch_vault_balance(rpc, &vault_b),
    );

    let (ra, rb) = (bal_a? as u128, bal_b? as u128);

    // Return in (input_reserve, output_reserve) order
    if *input_mint == mint_a {
        Some((ra, rb))
    } else {
        Some((rb, ra))
    }
}

/// Pumpup bonding curve: the executor (`amms/pumpup_bonding.rs`) sizes its
/// exact-output request as `gross * 99 / 100` to cover the curve's per-side
/// fee — quoting with the same 1% keeps `/quote` and the instruction in step.
pub const PUMPUP_BONDING_FEE_BPS: u16 = 100;

/// Fee in basis points for each constant-product pool type — the number the
/// quote engine subtracts from the input before the x·y=k step.
///
/// Listed exhaustively, never behind `_`: a new venue must be an explicit
/// decision, because a fee borrowed from another protocol is a guessed
/// `minimum_out`. Fees are per-pool on several venues (Raydium CPMM configs,
/// Meteora); the 25 bps values are the protocol defaults and stand until the
/// per-pool field is read from the account.
fn fee_for_pool_type(pool_type: PoolType) -> u16 {
    match pool_type {
        PoolType::RaydiumV4 => 25,      // 0.25%
        PoolType::RaydiumCpmm => 25,    // varies by config, default 0.25%
        PoolType::RaydiumLp => 25,      // 0.25%
        // per-pool market-cap tier — see `venue_fee_bps`; this is the top tier
        PoolType::PumpFunAmm => 30,
        PoolType::Meteora => 25,        // varies
        PoolType::MeteoraDamm => 25,    // varies
        PoolType::FluxBeam => 25,       // 0.25%
        PoolType::Saros => 25,          // 0.25%
        PoolType::Dooar => 25,          // 0.25%
        PoolType::PumpupBonding => PUMPUP_BONDING_FEE_BPS,
        // Pumpup post-graduation AMM: no verified fee source (two unparsed u16
        // fields at offsets 224/259 are candidates). Quoted at a deliberately
        // HIGH 1% so a wrong guess under-quotes rather than reverts; every pool
        // with a streamed swap uses its observed fee instead (`venue_fee_bps`).
        PoolType::Pumpup => 100,
        // CLMM venues carry their fee in the pool account (`extract_clmm_params`);
        // the rest are not quoted by the constant-product path at all.
        PoolType::RaydiumCl
        | PoolType::Orca
        | PoolType::MeteoraDlmm
        | PoolType::PancakeSwap
        | PoolType::Byreal
        | PoolType::DefiTunaFusion
        | PoolType::PumpFun
        | PoolType::MeteoraDbc
        | PoolType::FlashTrade
        | PoolType::DefiTunaPools
        | PoolType::Unknown => DEFAULT_FEE_BPS,
    }
}

/// The fee (bps) a specific pool charges: pump.fun AMM reads its market-cap
/// tier from the pool state; every other venue uses the protocol default.
/// How long a streamed fee observation stays authoritative.
const OBSERVED_FEE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(600);

/// Meteora DBC leg: segment walk + base/dynamic fee at the current point
/// (slot or unix time per the config). Without a known slot the activation
/// point is used: the scheduler's highest fee, the conservative side.
fn quote_dbc(input_mint: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey, curve: &super::dbc::DbcCurve, amount: u64) -> Option<LegResult> {
    let quote_to_base = if input_mint == quote_mint { true } else if input_mint == base_mint { false } else { return None };
    let current_point = match curve.config.activation_type {
        1 => crate::stream::chain_unix_time(),
        _ => {
            let s = crate::stream::latest_slot();
            if s == 0 { curve.activation_point } else { s }
        }
    };
    let q = curve.swap_exact_in(quote_to_base, amount, current_point)?;
    let (res_quote, res_base) = curve.implied_reserves();
    let (reserve_in, reserve_out) = if quote_to_base { (res_quote, res_base) } else { (res_base, res_quote) };
    Some((q.amount_out, q.fee, reserve_in, reserve_out))
}

/// Meteora DAMM v2 leg: single-range sqrt-price curve (or constant product on
/// the tracked `reserves` for a compounding pool) + base/dynamic fee.
/// Without a known current point the CLIFF (highest) fee is assumed — the
/// conservative side for a min_out.
#[allow(clippy::too_many_arguments)]
fn quote_damm_v2(
    input_mint: &Pubkey, mint_a: &Pubkey, mint_b: &Pubkey,
    liquidity: u128, sqrt_price: u128, sqrt_min: u128, sqrt_max: u128, reserves: (u64, u64),
    fees: &super::damm_v2::DammFees, activation_point: u64, activation_type: u8, collect_fee_mode: u8, pool_status: u8,
    amount: u64,
) -> Option<LegResult> {
    if pool_status != 0 || liquidity == 0 || sqrt_price == 0 {
        return None;
    }
    let a_to_b = if input_mint == mint_a { true } else if input_mint == mint_b { false } else { return None };
    let current_point = match activation_type {
        1 => crate::stream::chain_unix_time(),
        _ => {
            let s = crate::stream::latest_slot();
            if s == 0 { activation_point } else { s }
        }
    };
    if current_point < activation_point {
        return None; // not tradable yet
    }
    let curve = if collect_fee_mode == super::damm_v2::COLLECT_FEE_MODE_COMPOUNDING {
        super::damm_v2::DammCurve::Compounding { reserve_a: reserves.0, reserve_b: reserves.1 }
    } else {
        super::damm_v2::DammCurve::Concentrated { liquidity, sqrt_price, sqrt_min, sqrt_max }
    };
    let fee_num = fees.total_fee_numerator(current_point, activation_point, a_to_b, amount, sqrt_price)?;
    let q = super::damm_v2::swap_exact_in(&curve, fee_num, collect_fee_mode, a_to_b, amount)?;
    if q.amount_out == 0 {
        return None;
    }
    // Reserves implied by the curve (for price-impact reporting).
    let (res_a, res_b) = curve.reserves();
    let (reserve_in, reserve_out) = if a_to_b { (res_a as u128, res_b as u128) } else { (res_b as u128, res_a as u128) };
    Some((q.amount_out, q.fee, reserve_in, reserve_out))
}

fn venue_fee_bps(state: &PoolState, pool_type: PoolType, pool: &Pubkey) -> u16 {
    // An exact on-chain config beats a measurement; a measurement from this
    // pool's own recent swaps beats any table (it captures schedulers, buyback
    // pricing and per-pool configs alike).
    if let PoolState::RaydiumCpmm { trade_fee_bps, .. } = state {
        if *trade_fee_bps > 0 {
            return *trade_fee_bps;
        }
    }
    if let Some(bps) = crate::stream::observed_fees::get_fresh(pool, OBSERVED_FEE_MAX_AGE) {
        return bps;
    }
    match state {
        PoolState::PumpFunAmm { .. } => pamm_total_fee_bps(state),
        _ => fee_for_pool_type(pool_type),
    }
}

/// Output and fee of one constant-product leg: x·y=k with the fee taken from
/// the input first — the ONE place the quote engine decides what a leg pays
/// out, shared by direct, split and multi-hop routes.
fn leg_out(fee_bps: u16, reserve_in: u128, reserve_out: u128, amount: u64) -> Option<(u64, u64)> {
    let out = compute_constant_product_out(reserve_in, reserve_out, amount, fee_bps)?;
    Some((out, compute_fee_amount(amount, fee_bps)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{SOL_NATIVE_MINT, USDC_MINT};

    #[test]
    fn test_is_constant_product() {
        assert!(is_constant_product(PoolType::RaydiumV4));
        assert!(is_constant_product(PoolType::RaydiumCpmm));
        assert!(is_constant_product(PoolType::PumpFunAmm));
        assert!(!is_constant_product(PoolType::Meteora), "dynamic vaults — no reserve math yet");
        assert!(is_constant_product(PoolType::FluxBeam), "SPL token-swap fee schedule read from the pool");
        assert!(is_quotable(PoolType::FluxBeam));
        assert!(is_constant_product(PoolType::Dooar));

        // CLMM pools are NOT constant product
        assert!(!is_constant_product(PoolType::Orca));
        assert!(!is_constant_product(PoolType::RaydiumCl));
        assert!(!is_constant_product(PoolType::MeteoraDlmm));
        assert!(!is_constant_product(PoolType::PancakeSwap));
    }

    #[test]
    fn test_label_for_pool_type() {
        assert_eq!(label_for_pool_type(PoolType::RaydiumCpmm), "Raydium CPMM");
        assert_eq!(label_for_pool_type(PoolType::Orca), "Orca");
        assert_eq!(label_for_pool_type(PoolType::PumpFunAmm), "PumpFun AMM");
        assert_eq!(label_for_pool_type(PoolType::MeteoraDlmm), "Meteora DLMM");
    }

    #[test]
    fn test_fee_for_pool_type() {
        assert_eq!(fee_for_pool_type(PoolType::RaydiumV4), 25);
        assert_eq!(fee_for_pool_type(PoolType::PumpFunAmm), 30, "top tier; per-pool via venue_fee_bps");
        assert_eq!(fee_for_pool_type(PoolType::PumpupBonding), PUMPUP_BONDING_FEE_BPS);
        assert_eq!(fee_for_pool_type(PoolType::Orca), DEFAULT_FEE_BPS);
        assert!(is_quotable(PoolType::Pumpup));
        assert_eq!(fee_for_pool_type(PoolType::Pumpup), 100, "conservative default until observed");
    }

    /// A pump.fun AMM leg is quoted with the pool's own market-cap tier fee,
    /// taken from the input side — the same expression the program settles with.
    #[test]
    fn test_pamm_leg_uses_the_pool_tier_fee() {
        let base_mint = Pubkey::new_unique();
        let st = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(), base_mint, quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(), pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 200_000_000_000_000, quote_reserve: 100_000_000_000, // mcap 500 SOL → 120 bps
            protocol_fee_recipient: Pubkey::default(), buyback_accounts: Vec::new(),
            base_supply: 1_000_000_000_000_000,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        };
        assert_eq!(venue_fee_bps(&st, PoolType::PumpFunAmm, &Pubkey::new_unique()), 120);
        // a streamed observation overrides the table
        let p = Pubkey::new_unique();
        crate::stream::observed_fees::record(p, 33_300);
        assert_eq!(venue_fee_bps(&st, PoolType::PumpFunAmm, &p), 333);
        let (out, fee) = leg_out(120, 100_000_000_000, 200_000_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(out, compute_constant_product_out(100_000_000_000, 200_000_000_000_000, 1_000_000_000, 120).unwrap());
        assert_eq!(fee, compute_fee_amount(1_000_000_000, 120));
        // a flat 30 bps would over-quote this young pool by ~0.9%
        let naive = compute_constant_product_out(100_000_000_000, 200_000_000_000_000, 1_000_000_000, 30).unwrap();
        assert!(naive > out);
        // other venues keep the protocol default
        assert_eq!(fee_for_pool_type(PoolType::RaydiumCpmm), 25);
    }

    #[test]
    fn test_extract_reserves_inline_pumpfun_amm_forward() {
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let state = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1_000_000,
            quote_reserve: 500_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        };

        let result = extract_reserves_inline(&state, &base_mint);
        assert_eq!(result, Some((1_000_000, 500_000)));
    }

    #[test]
    fn test_extract_reserves_inline_pumpfun_amm_reverse() {
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let state = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1_000_000,
            quote_reserve: 500_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        };

        let result = extract_reserves_inline(&state, &quote_mint);
        assert_eq!(result, Some((500_000, 1_000_000)));
    }

    #[test]
    fn test_extract_reserves_inline_non_pumpfun_returns_none() {
        let state = PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let input_mint = Pubkey::new_unique();
        let result = extract_reserves_inline(&state, &input_mint);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_vault_mints_raydium_cpmm() {
        let vault0 = Pubkey::new_unique();
        let vault1 = Pubkey::new_unique();
        let mint0 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        let state = PoolState::RaydiumCpmm {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            token_0_vault: vault0,
            token_1_vault: vault1,
            token_0_mint: mint0,
            token_1_mint: mint1,
            observation: Pubkey::new_unique(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        };

        let (va, vb, ma, mb) = extract_vault_mints(&state).unwrap();
        assert_eq!(va, vault0);
        assert_eq!(vb, vault1);
        assert_eq!(ma, mint0);
        assert_eq!(mb, mint1);
    }

    #[test]
    fn test_extract_vault_mints_raydium_lp() {
        let bv = Pubkey::new_unique();
        let qv = Pubkey::new_unique();
        let bm = Pubkey::new_unique();
        let qm = Pubkey::new_unique();
        let state = PoolState::RaydiumLp {
            pool_state: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            base_vault: bv,
            quote_vault: qv,
            base_mint: bm,
            quote_mint: qm,
            config_id: Pubkey::new_unique(),
            platform_id: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
        };

        let (va, vb, ma, mb) = extract_vault_mints(&state).unwrap();
        assert_eq!(va, bv);
        assert_eq!(vb, qv);
        assert_eq!(ma, bm);
        assert_eq!(mb, qm);
    }

    #[test]
    fn test_extract_vault_mints_meteora() {
        let va = Pubkey::new_unique();
        let vb = Pubkey::new_unique();
        let ma = Pubkey::new_unique();
        let mb = Pubkey::new_unique();
        let state = PoolState::Meteora {
            pool: Pubkey::new_unique(),
            token_a_mint: ma,
            token_b_mint: mb,
            a_vault: Pubkey::new_unique(),
            b_vault: Pubkey::new_unique(),
            a_token_vault: va,
            b_token_vault: vb,
            a_vault_lp_mint: Pubkey::new_unique(),
            b_vault_lp_mint: Pubkey::new_unique(),
            a_vault_lp: Pubkey::new_unique(),
            b_vault_lp: Pubkey::new_unique(),
            admin_token_a_fee: Pubkey::new_unique(),
            admin_token_b_fee: Pubkey::new_unique(),
            vault_program: Pubkey::new_unique(),
            reserves: Default::default(),
        };

        let (v_a, v_b, m_a, m_b) = extract_vault_mints(&state).unwrap();
        assert_eq!(v_a, va);
        assert_eq!(v_b, vb);
        assert_eq!(m_a, ma);
        assert_eq!(m_b, mb);
    }

    #[test]
    fn test_extract_vault_mints_meteora_damm() {
        let va = Pubkey::new_unique();
        let vb = Pubkey::new_unique();
        let ma = Pubkey::new_unique();
        let mb = Pubkey::new_unique();
        let state = PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: va,
            token_b_vault: vb,
            token_a_mint: ma,
            token_b_mint: mb,
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };

        let (v_a, v_b, m_a, m_b) = extract_vault_mints(&state).unwrap();
        assert_eq!(v_a, va);
        assert_eq!(v_b, vb);
        assert_eq!(m_a, ma);
        assert_eq!(m_b, mb);
    }

    #[test]
    fn test_extract_vault_mints_meteora_dbc() {
        let bv = Pubkey::new_unique();
        let qv = Pubkey::new_unique();
        let bm = Pubkey::new_unique();
        let qm = Pubkey::new_unique();
        let state = PoolState::MeteoraDbc {
            pool: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            pool_authority: Pubkey::new_unique(),
            base_vault: bv,
            quote_vault: qv,
            base_mint: bm,
            quote_mint: qm,
            curve: Default::default(),
        };

        let (va, vb, ma, mb) = extract_vault_mints(&state).unwrap();
        assert_eq!(va, bv);
        assert_eq!(vb, qv);
        assert_eq!(ma, bm);
        assert_eq!(mb, qm);
    }

    #[test]
    fn test_extract_vault_mints_fluxbeam() {
        let va = Pubkey::new_unique();
        let vb = Pubkey::new_unique();
        let ma = Pubkey::new_unique();
        let mb = Pubkey::new_unique();
        let state = PoolState::FluxBeam {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: va,
            token_b_vault: vb,
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: ma,
            token_b_mint: mb,
            pool_token_program: Pubkey::new_unique(),
            fees: Default::default(),
        };

        let (v_a, v_b, m_a, m_b) = extract_vault_mints(&state).unwrap();
        assert_eq!(v_a, va);
        assert_eq!(v_b, vb);
        assert_eq!(m_a, ma);
        assert_eq!(m_b, mb);
    }

    #[test]
    fn test_extract_vault_mints_saros() {
        let va = Pubkey::new_unique();
        let vb = Pubkey::new_unique();
        let ma = Pubkey::new_unique();
        let mb = Pubkey::new_unique();
        let state = PoolState::Saros {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: va,
            token_b_vault: vb,
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: ma,
            token_b_mint: mb,
            fees: Default::default(),
        };

        let (v_a, v_b, m_a, m_b) = extract_vault_mints(&state).unwrap();
        assert_eq!(v_a, va);
        assert_eq!(v_b, vb);
        assert_eq!(m_a, ma);
        assert_eq!(m_b, mb);
    }

    #[test]
    fn test_extract_vault_mints_dooar() {
        let va = Pubkey::new_unique();
        let vb = Pubkey::new_unique();
        let ma = Pubkey::new_unique();
        let mb = Pubkey::new_unique();
        let state = PoolState::Dooar {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: va,
            token_b_vault: vb,
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: ma,
            token_b_mint: mb,
            fees: Default::default(),
        };

        let (v_a, v_b, m_a, m_b) = extract_vault_mints(&state).unwrap();
        assert_eq!(v_a, va);
        assert_eq!(v_b, vb);
        assert_eq!(m_a, ma);
        assert_eq!(m_b, mb);
    }

    #[test]
    fn test_extract_vault_mints_raydium_v4() {
        let cv = Pubkey::new_unique();
        let pv = Pubkey::new_unique();
        let (cm, pm) = (Pubkey::new_unique(), Pubkey::new_unique());
        let state = v4_state(cv, pv, cm, pm, 0, 0);
        assert_eq!(extract_vault_mints(&state), Some((cv, pv, cm, pm)));
    }

    fn v4_state(coin_vault: Pubkey, pc_vault: Pubkey, coin_mint: Pubkey, pc_mint: Pubkey, pnl_coin: u64, pnl_pc: u64) -> PoolState {
        PoolState::RaydiumV4 {
            amm_id: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            coin_vault,
            pc_vault,
            coin_mint,
            pc_mint,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            need_take_pnl_coin: pnl_coin,
            need_take_pnl_pc: pnl_pc,
            status: 6,
            pool_open_time: 0,
        }
    }

    fn v4_quoter() -> (Arc<PoolRegistry>, Arc<crate::pool::cache::PoolCache>, Arc<crate::stream::account_mirror::AccountMirror>, Quoter) {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let mirror = Arc::new(crate::stream::account_mirror::AccountMirror::new());
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::with_mirror(Arc::clone(&registry), Arc::clone(&cache), rpc, Arc::clone(&mirror));
        (registry, cache, mirror, quoter)
    }

    fn v4_pool(registry: &PoolRegistry, cache: &crate::pool::cache::PoolCache, mirror: &crate::stream::account_mirror::AccountMirror, coin: Pubkey, pc: Pubkey, vaults: (u64, u64), pnl: (u64, u64)) -> Pubkey {
        let (pool, cv, pv) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        registry.add(PoolEntry { address: pool, pool_type: PoolType::RaydiumV4, mint_a: coin, mint_b: pc });
        cache.insert(pool, v4_state(cv, pv, coin, pc, pnl.0, pnl.1));
        mirror.update_vault_balance(cv, vaults.0);
        mirror.update_vault_balance(pv, vaults.1);
        pool
    }

    fn quote_req(input_mint: Pubkey, output_mint: Pubkey, amount: u64, only_direct_routes: bool) -> QuoteRequest {
        QuoteRequest { input_mint, output_mint, amount, slippage_bps: 50, only_direct_routes, exclude_dexes: vec![], dexes: vec![], max_accounts: 64 }
    }

    #[tokio::test]
    async fn raydium_v4_quote_matches_the_mainnet_ray_log_both_ways() {
        // 58oQChx4… SOL/USDC, tx 75pWHXMn…: vault balances before the swap, the
        // pool's need_take_pnl, and the ray_log out amounts.
        let (registry, cache, mirror, quoter) = v4_quoter();
        v4_pool(&registry, &cache, &mirror, SOL_NATIVE_MINT, USDC_MINT, (178_470_889_557_717, 21_208_903_794_498), (39_925_487, 4_739_163));
        let out: u64 = quoter.quote(&quote_req(SOL_NATIVE_MINT, USDC_MINT, 33_157_350, true)).await.unwrap().amount_out.parse().unwrap();
        assert_eq!(out, 3_930_460);
        // pc → coin on the same reserves (tx 4r7FMgx8…: its own pre-balances)
        let (registry, cache, mirror, quoter) = v4_quoter();
        v4_pool(&registry, &cache, &mirror, SOL_NATIVE_MINT, USDC_MINT, (178_424_565_685_852, 21_214_253_619_335), (39_925_487, 4_739_163));
        let resp = quoter.quote(&quote_req(USDC_MINT, SOL_NATIVE_MINT, 250_000_000, true)).await.unwrap();
        assert_eq!(resp.amount_out, "2097368298");
        assert_eq!(resp.routes[0].pool.dex, "Raydium V4");
    }

    #[tokio::test]
    async fn raydium_v4_pools_route_as_a_two_hop_leg() {
        let (registry, cache, mirror, quoter) = v4_quoter();
        let token = Pubkey::new_unique();
        v4_pool(&registry, &cache, &mirror, token, SOL_NATIVE_MINT, (5_000_000_000_000, 100_000_000_000), (0, 0));
        v4_pool(&registry, &cache, &mirror, SOL_NATIVE_MINT, USDC_MINT, (178_470_889_557_717, 21_208_903_794_498), (39_925_487, 4_739_163));
        let resp = quoter.quote(&quote_req(token, USDC_MINT, 1_000_000_000, false)).await.unwrap();
        assert_eq!(resp.routes.len(), 2);
        assert!(resp.routes.iter().all(|r| r.pool.dex == "Raydium V4"));
        let hop1: u64 = resp.routes[0].pool.amount_out.parse().unwrap();
        let (h1, _) = crate::execution::amms::raydium_v4::swap_base_in_out(5_000_000_000_000, 100_000_000_000, 1_000_000_000, 25, 10_000).unwrap();
        assert_eq!(hop1, h1);
    }

    #[tokio::test]
    async fn raydium_v4_closed_pool_is_not_quoted() {
        let (registry, cache, mirror, quoter) = v4_quoter();
        let pool = v4_pool(&registry, &cache, &mirror, SOL_NATIVE_MINT, USDC_MINT, (1_000_000_000, 1_000_000_000), (0, 0));
        if let Some(PoolState::RaydiumV4 { amm_id, authority, coin_vault, pc_vault, coin_mint, pc_mint, .. }) = cache.get(&pool) {
            cache.insert(pool, PoolState::RaydiumV4 { amm_id, authority, coin_vault, pc_vault, coin_mint, pc_mint, swap_fee_numerator: 25, swap_fee_denominator: 10_000, need_take_pnl_coin: 0, need_take_pnl_pc: 0, status: 3, pool_open_time: 0 });
        }
        assert!(quoter.quote(&quote_req(SOL_NATIVE_MINT, USDC_MINT, 1_000, true)).await.is_err(), "WithdrawOnly pool");
    }

    #[test]
    fn test_extract_vault_mints_unsupported_pumpfun() {
        let state = PoolState::PumpFun {
            global: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            bonding_curve: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
            event_authority: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
            buyback_fee_recipient: Pubkey::new_unique(),
        };
        assert!(extract_vault_mints(&state).is_none());
    }

    #[test]
    fn test_extract_vault_mints_unsupported_flash_trade() {
        let state = PoolState::FlashTrade {
            pool: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            custody: Pubkey::new_unique(),
            token_mint: Pubkey::new_unique(),
        };
        assert!(extract_vault_mints(&state).is_none());
    }

    #[test]
    fn test_extract_vault_mints_unsupported_orca_clmm() {
        let state = PoolState::Orca {
            whirlpool: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            token_mint_a: Pubkey::new_unique(),
            token_mint_b: Pubkey::new_unique(),
            tick_current: 0,
            tick_spacing: 64,
            sqrt_price_x64: 0,
            liquidity: 0,
            fee_rate: 0,
        };
        assert!(extract_vault_mints(&state).is_none());
    }

    #[test]
    fn test_extract_vault_mints_unsupported_pumpfun_amm() {
        // PumpFunAmm uses inline reserves, not vault fetching
        let state = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: Pubkey::new_unique(),
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1000,
            quote_reserve: 2000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        };
        assert!(extract_vault_mints(&state).is_none());
    }

    #[test]
    fn test_route_candidate_sorting() {
        let mut candidates = vec![
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                out_amount: 100,
                fee_amount: 1,
                reserve_in: 1000,
                reserve_out: 1000,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::PumpFunAmm,
                out_amount: 200,
                fee_amount: 2,
                reserve_in: 2000,
                reserve_out: 2000,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                out_amount: 150,
                fee_amount: 1,
                reserve_in: 1500,
                reserve_out: 1500,
                fee_bps: 25,
            },
        ];

        candidates.sort_by(|a, b| b.out_amount.cmp(&a.out_amount));
        assert_eq!(candidates[0].out_amount, 200);
        assert_eq!(candidates[1].out_amount, 150);
        assert_eq!(candidates[2].out_amount, 100);
    }

    // ── Multi-hop routing unit tests ──

    #[test]
    fn test_bridge_mints_skip_input() {
        use crate::constants::{SOL_NATIVE_MINT, USDT_MINT, PYUSD_MINT};
        // When input_mint is SOL, SOL should be skipped as bridge
        let input_mint = SOL_NATIVE_MINT;
        let bridges: Vec<&Pubkey> = BRIDGE_MINTS
            .iter()
            .filter(|b| **b != input_mint)
            .collect();
        // SOL filtered out, USDC, USDT, and PYUSD remain
        assert_eq!(bridges.len(), 3);
        // USDT should be the second one
        assert_eq!(*bridges[1], USDT_MINT);
        // PYUSD should be the third one
        assert_eq!(*bridges[2], PYUSD_MINT);
    }

    #[test]
    fn test_bridge_mints_skip_output() {
        use crate::constants::USDT_MINT;
        let output_mint = USDT_MINT;
        let bridges: Vec<&Pubkey> = BRIDGE_MINTS
            .iter()
            .filter(|b| **b != output_mint)
            .collect();
        // USDT filtered out, SOL, USDC, and PYUSD remain
        assert_eq!(bridges.len(), 3);
    }

    #[test]
    fn test_two_hop_route_struct() {
        let h1 = PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        };
        let h2 = PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::Meteora,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        };
        let route = TwoHopRoute {
            hop1_entry: h1,
            hop2_entry: h2,
            bridge_mint: Pubkey::new_unique(),
            hop1_amount_out: 1000,
            hop1_fee_amount: 3,
            hop1_reserve_in: 100_000,
            hop1_reserve_out: 200_000,
            final_amount_out: 950,
            hop2_fee_amount: 3,
            hop2_reserve_in: 150_000,
            hop2_reserve_out: 300_000,
        };

        assert_eq!(route.final_amount_out, 950);
        assert_eq!(route.hop1_amount_out, 1000);
    }

    #[tokio::test]
    async fn test_quoter_no_route_error() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let req = QuoteRequest {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            amount: 1000,
            slippage_bps: 50,
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let result = quoter.quote(&req).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            TradeError::NoRoute { .. } => {}
            other => panic!("expected NoRoute, got: {other}"),
        }
    }

    #[test]
    fn test_filter_entries_dex_whitelist() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), cache, rpc);

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a,
            mint_b,
        });
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::MeteoraDamm,
            mint_a,
            mint_b,
        });

        // Whitelist only Meteora DAMM (Meteora Standard is streamed, not quoted)
        let entries = quoter.filter_entries(
            &mint_a,
            &mint_b,
            &["Meteora DAMM".to_string()],
            &[],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pool_type, PoolType::MeteoraDamm);
    }

    #[test]
    fn test_filter_entries_dex_blacklist() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), cache, rpc);

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a,
            mint_b,
        });
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::Meteora,
            mint_a,
            mint_b,
        });

        // Blacklist Meteora
        let entries = quoter.filter_entries(
            &mint_a,
            &mint_b,
            &[],
            &["Meteora".to_string()],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pool_type, PoolType::RaydiumCpmm);
    }

    #[test]
    fn test_filter_entries_includes_clmm() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), cache, rpc);

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::Orca, // CLMM — now quotable
            mint_a,
            mint_b,
        });
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a,
            mint_b,
        });

        let entries = quoter.filter_entries(&mint_a, &mint_b, &[], &[]);
        assert_eq!(entries.len(), 2, "both CP and CLMM pools should be included");
        let types: Vec<PoolType> = entries.iter().map(|e| e.pool_type).collect();
        assert!(types.contains(&PoolType::Orca));
        assert!(types.contains(&PoolType::RaydiumCpmm));
    }

    #[test]
    fn test_filter_entries_skips_unsupported() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), cache, rpc);

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        // DefiTuna Pools is not supported for quoting
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::DefiTunaPools,
            mint_a,
            mint_b,
        });
        registry.add(PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a,
            mint_b,
        });

        let entries = quoter.filter_entries(&mint_a, &mint_b, &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pool_type, PoolType::RaydiumCpmm);
    }

    #[test]
    fn test_build_direct_response_structure() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let input_mint = Pubkey::new_unique();
        let output_mint = Pubkey::new_unique();
        let req = QuoteRequest {
            input_mint,
            output_mint,
            amount: 1_000_000,
            slippage_bps: 50,
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            out_amount: 990_000,
            fee_amount: 2_500,
            reserve_in: 10_000_000,
            reserve_out: 10_000_000,
            fee_bps: 25,
        };

        let resp = quoter.build_direct_response(&req, &route, 0, 0.01);
        assert_eq!(resp.routes.len(), 1);
        assert_eq!(resp.amount_out, "990000");
        assert_eq!(resp.routes[0].percent, 100);
        assert_eq!(resp.routes[0].pool.dex, "Raydium CPMM");
    }

    #[test]
    fn test_build_two_hop_response_structure() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let input_mint = Pubkey::new_unique();
        let output_mint = Pubkey::new_unique();
        let bridge_mint = Pubkey::new_unique();
        let req = QuoteRequest {
            input_mint,
            output_mint,
            amount: 1_000_000,
            slippage_bps: 50,
            only_direct_routes: false,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = TwoHopRoute {
            hop1_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                mint_a: input_mint,
                mint_b: bridge_mint,
            },
            hop2_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                mint_a: bridge_mint,
                mint_b: output_mint,
            },
            bridge_mint,
            hop1_amount_out: 500_000,
            hop1_fee_amount: 1_250,
            hop1_reserve_in: 10_000_000,
            hop1_reserve_out: 5_000_000,
            final_amount_out: 490_000,
            hop2_fee_amount: 1_250,
            hop2_reserve_in: 5_000_000,
            hop2_reserve_out: 10_000_000,
        };

        let resp = quoter.build_two_hop_response(&req, &route, 0, 0.01);
        assert_eq!(resp.routes.len(), 2);
        assert_eq!(resp.amount_out, "490000");

        // Hop 1: input -> bridge
        assert_eq!(resp.routes[0].pool.input_token, input_mint.to_string());
        assert_eq!(resp.routes[0].pool.output_token, bridge_mint.to_string());
        assert_eq!(resp.routes[0].pool.dex, "Raydium CPMM");

        // Hop 2: bridge -> output
        assert_eq!(resp.routes[1].pool.input_token, bridge_mint.to_string());
        assert_eq!(resp.routes[1].pool.output_token, output_mint.to_string());
        assert_eq!(resp.routes[1].pool.dex, "Meteora");
    }

    #[tokio::test]
    async fn test_quoter_direct_route_pumpfun_amm() {
        // PumpFunAmm has inline reserves, so we can test the full path
        // without real RPC vault balance fetches.
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), Arc::clone(&cache), Arc::clone(&rpc));

        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool_addr = Pubkey::new_unique();

        // Register pool
        registry.add(PoolEntry {
            address: pool_addr,
            pool_type: PoolType::PumpFunAmm,
            mint_a: base_mint,
            mint_b: quote_mint,
        });

        // Pre-populate cache with pool state (inline reserves)
        cache.insert(pool_addr, PoolState::PumpFunAmm {
            pool: pool_addr,
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000,
            quote_reserve: 5_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        let req = QuoteRequest {
            input_mint: base_mint,
            output_mint: quote_mint,
            amount: 100_000,
            slippage_bps: 50,
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let result = quoter.quote(&req).await;
        assert!(result.is_ok(), "quote should succeed with cached PumpFunAmm: {:?}", result.err());
        let resp = result.unwrap();
        assert_eq!(resp.routes.len(), 1);
        let out: u64 = resp.amount_out.parse().unwrap();
        assert!(out > 0, "out_amount should be > 0");
        assert!(out < 100_000, "out_amount should be less than input (different reserves)");
    }

    #[tokio::test]
    async fn test_quoter_bonding_curves_from_memory() {
        use crate::constants::SOL_NATIVE_MINT;
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), Arc::clone(&cache), Arc::clone(&rpc));
        let req = |input_mint, output_mint, amount| QuoteRequest {
            input_mint, output_mint, amount, slippage_bps: 100, only_direct_routes: true, exclude_dexes: vec![], dexes: vec![], max_accounts: 64,
        };

        // pump.fun: a live pre-trade curve (FSP4kDr3…, buy_exact_sol_in 0.1 SOL)
        let (mint, curve_addr) = (Pubkey::new_unique(), Pubkey::new_unique());
        registry.add(PoolEntry { address: curve_addr, pool_type: PoolType::PumpFun, mint_a: mint, mint_b: SOL_NATIVE_MINT });
        let curve = crate::quote::pump_bonding::PumpCurve {
            virtual_sol_reserves: 41_556_085_197,
            virtual_token_reserves: 774_615_796_118_287,
            real_sol_reserves: 11_556_085_197,
            real_token_reserves: 494_715_796_118_287,
            token_total_supply: 1_000_000_000_000_000,
            has_creator: true,
            ..Default::default()
        };
        let pump = |curve| PoolState::PumpFun {
            global: Pubkey::new_unique(), fee_account: Pubkey::new_unique(), mint, bonding_curve: curve_addr,
            associated_bonding_curve: Pubkey::new_unique(), event_authority: Pubkey::new_unique(), creator: Pubkey::new_unique(),
            curve, buyback_fee_recipient: Pubkey::new_unique(),
        };
        cache.insert(curve_addr, pump(curve));
        let buy = quoter.quote(&req(SOL_NATIVE_MINT, mint, 100_000_000)).await.unwrap();
        assert_eq!((buy.amount_out.as_str(), buy.routes[0].pool.dex.as_str()), ("1836647138012", "PumpFun"));
        let sell = quoter.quote(&req(mint, SOL_NATIVE_MINT, 1_836_647_138_012)).await.unwrap();
        assert!(sell.amount_out.parse::<u64>().unwrap() < 100_000_000);
        // a completed curve is not quoted
        cache.insert(curve_addr, pump(crate::quote::pump_bonding::PumpCurve { complete: true, ..curve }));
        assert!(quoter.quote(&req(SOL_NATIVE_MINT, mint, 100_000_000)).await.is_err());

        // Meteora DBC: a flat-fee config, both directions, migrated → refused
        let (base, pool) = (Pubkey::new_unique(), Pubkey::new_unique());
        registry.add(PoolEntry { address: pool, pool_type: PoolType::MeteoraDbc, mint_a: base, mint_b: SOL_NATIVE_MINT });
        let dbc_curve = crate::quote::dbc::DbcCurve {
            sqrt_price: 3_262_546_676_709_219,
            quote_reserve: 1_399_303_498,
            activation_point: 0,
            config: crate::quote::dbc::DbcConfig {
                activation_type: 1,
                migration_quote_threshold: 85_000_000_000,
                sqrt_start_price: 3_141_367_320_245_630,
                curve: vec![
                    (6_401_204_812_200_420, 3_929_368_168_768_468_756_200_000_000_000_000),
                    (13_043_817_825_332_782, 2_425_988_008_058_820_449_100_000_000_000_000),
                ],
                cliff_fee_numerator: 20_000_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let dbc = |curve| PoolState::MeteoraDbc {
            pool, config: Pubkey::new_unique(), pool_authority: Pubkey::new_unique(), base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(), base_mint: base, quote_mint: SOL_NATIVE_MINT, curve,
        };
        cache.insert(pool, dbc(dbc_curve.clone()));
        let buy = quoter.quote(&req(SOL_NATIVE_MINT, base, 40_204_811)).await.unwrap();
        assert_eq!(buy.amount_out, "1258276557343754", "658ZWbNq… EvtSwap2");
        assert!(quoter.quote(&req(base, SOL_NATIVE_MINT, 1_000_000_000_000)).await.is_ok());
        cache.insert(pool, dbc(crate::quote::dbc::DbcCurve { is_migrated: true, ..dbc_curve }));
        assert!(quoter.quote(&req(base, SOL_NATIVE_MINT, 1_000_000_000_000)).await.is_err());
    }

    #[tokio::test]
    async fn test_quoter_two_hop_route_pumpfun_amm() {
        // Test 2-hop routing: TOKEN_A -> SOL -> TOKEN_B
        // using PumpFunAmm pools (inline reserves, no RPC needed)
        use crate::constants::SOL_NATIVE_MINT;

        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), Arc::clone(&cache), Arc::clone(&rpc));

        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool1_addr = Pubkey::new_unique();
        let pool2_addr = Pubkey::new_unique();

        // Pool 1: TOKEN_A / SOL
        registry.add(PoolEntry {
            address: pool1_addr,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_a,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool1_addr, PoolState::PumpFunAmm {
            pool: pool1_addr,
            base_mint: token_a,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000_000, // 10B token
            quote_reserve: 50_000_000_000, // 50 SOL (in lamports),
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        // Pool 2: TOKEN_B / SOL
        registry.add(PoolEntry {
            address: pool2_addr,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_b,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool2_addr, PoolState::PumpFunAmm {
            pool: pool2_addr,
            base_mint: token_b,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 20_000_000_000, // 20B token
            quote_reserve: 100_000_000_000, // 100 SOL (in lamports),
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        // Quote: TOKEN_A -> TOKEN_B (no direct pool, must route through SOL)
        let req = QuoteRequest {
            input_mint: token_a,
            output_mint: token_b,
            amount: 1_000_000, // 1M token_a
            slippage_bps: 50,
            only_direct_routes: false,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let result = quoter.quote(&req).await;
        assert!(result.is_ok(), "2-hop quote should succeed: {:?}", result.err());
        let resp = result.unwrap();

        // Should be a 2-hop route
        assert_eq!(resp.routes.len(), 2, "should be a 2-hop route");

        // Hop 1: TOKEN_A -> SOL
        assert_eq!(resp.routes[0].pool.input_token, token_a.to_string());
        assert_eq!(resp.routes[0].pool.output_token, SOL_NATIVE_MINT.to_string());

        // Hop 2: SOL -> TOKEN_B
        assert_eq!(resp.routes[1].pool.input_token, SOL_NATIVE_MINT.to_string());
        assert_eq!(resp.routes[1].pool.output_token, token_b.to_string());

        let out: u64 = resp.amount_out.parse().unwrap();
        assert!(out > 0, "final output should be > 0");
    }

    #[tokio::test]
    async fn test_quoter_only_direct_routes_skips_two_hop() {
        use crate::constants::SOL_NATIVE_MINT;

        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), Arc::clone(&cache), Arc::clone(&rpc));

        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool1_addr = Pubkey::new_unique();
        let pool2_addr = Pubkey::new_unique();

        // Same pools as above but only_direct_routes = true
        registry.add(PoolEntry {
            address: pool1_addr,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_a,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool1_addr, PoolState::PumpFunAmm {
            pool: pool1_addr,
            base_mint: token_a,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000_000,
            quote_reserve: 50_000_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        registry.add(PoolEntry {
            address: pool2_addr,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_b,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool2_addr, PoolState::PumpFunAmm {
            pool: pool2_addr,
            base_mint: token_b,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 20_000_000_000,
            quote_reserve: 100_000_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        let req = QuoteRequest {
            input_mint: token_a,
            output_mint: token_b,
            amount: 1_000_000,
            slippage_bps: 50,
            only_direct_routes: true, // This should skip 2-hop
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let result = quoter.quote(&req).await;
        // Should get NoRoute since there's no direct TOKEN_A -> TOKEN_B pool
        assert!(result.is_err());
        match result.unwrap_err() {
            TradeError::NoRoute { .. } => {}
            other => panic!("expected NoRoute, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_quoter_direct_beats_two_hop() {
        use crate::constants::SOL_NATIVE_MINT;

        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(60_000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(Arc::clone(&registry), Arc::clone(&cache), Arc::clone(&rpc));

        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();

        // Direct pool: TOKEN_A / TOKEN_B with excellent reserves
        let direct_pool = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: direct_pool,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_a,
            mint_b: token_b,
        });
        cache.insert(direct_pool, PoolState::PumpFunAmm {
            pool: direct_pool,
            base_mint: token_a,
            quote_mint: token_b,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1_000_000_000,
            quote_reserve: 1_000_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        // Indirect pools: TOKEN_A / SOL and TOKEN_B / SOL (worse total output due to double fees)
        let pool1 = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: pool1,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_a,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool1, PoolState::PumpFunAmm {
            pool: pool1,
            base_mint: token_a,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 100_000_000,
            quote_reserve: 100_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        let pool2 = Pubkey::new_unique();
        registry.add(PoolEntry {
            address: pool2,
            pool_type: PoolType::PumpFunAmm,
            mint_a: token_b,
            mint_b: SOL_NATIVE_MINT,
        });
        cache.insert(pool2, PoolState::PumpFunAmm {
            pool: pool2,
            base_mint: token_b,
            quote_mint: SOL_NATIVE_MINT,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 100_000_000,
            quote_reserve: 100_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        });

        let req = QuoteRequest {
            input_mint: token_a,
            output_mint: token_b,
            amount: 1_000,
            slippage_bps: 50,
            only_direct_routes: false,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let result = quoter.quote(&req).await;
        assert!(result.is_ok());
        let resp = result.unwrap();

        // Direct route should win (1 hop, better for small amounts with large reserves)
        assert_eq!(resp.routes.len(), 1, "direct route should be preferred");
    }

    // ── Split Route Tests ──

    #[test]
    fn test_split_no_routes_returns_none() {
        let routes: Vec<DirectRoute> = Vec::new();
        assert!(evaluate_split_routes(&routes, 1000).is_none());
    }

    #[test]
    fn test_split_single_route_returns_none() {
        let routes = vec![DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            out_amount: 9000,
            fee_amount: 25,
            reserve_in: 1_000_000,
            reserve_out: 1_000_000,
            fee_bps: 25,
        }];
        assert!(evaluate_split_routes(&routes, 10000).is_none());
    }

    #[test]
    fn test_split_large_trade_benefits_from_split() {
        // Two equal pools with 1M reserves each.
        // A large trade (100K = 10% of reserves) will have high impact in one pool.
        // Splitting across both pools should give more output.
        let pool_a = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            out_amount: 0, // will be recomputed
            fee_amount: 0,
            reserve_in: 1_000_000,
            reserve_out: 1_000_000,
            fee_bps: 25,
        };
        let pool_b = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::Orca,
            out_amount: 0,
            fee_amount: 0,
            reserve_in: 1_000_000,
            reserve_out: 1_000_000,
            fee_bps: 25,
        };

        // Compute single-pool outputs first
        let amount = 100_000u64;
        let single_a = compute_constant_product_out(1_000_000, 1_000_000, amount, 25).unwrap();
        let single_b = compute_constant_product_out(1_000_000, 1_000_000, amount, 25).unwrap();
        let best_single = single_a.max(single_b);

        let routes = vec![
            DirectRoute { out_amount: single_a, fee_amount: compute_fee_amount(amount, 25), ..pool_a },
            DirectRoute { out_amount: single_b, fee_amount: compute_fee_amount(amount, 25), ..pool_b },
        ];

        let split = evaluate_split_routes(&routes, amount);
        assert!(split.is_some(), "split should be found for large trade");
        let split = split.unwrap();
        assert!(
            split.total_out > best_single,
            "split output {} should beat single pool {}",
            split.total_out,
            best_single
        );
        assert_eq!(split.pct_a + split.pct_b, 100);
    }

    #[test]
    fn test_split_small_trade_no_benefit() {
        // Two pools with huge reserves. A tiny trade has negligible impact.
        // Splitting provides no benefit (and evaluate_split_routes returns None).
        let amount = 100u64;
        let reserve = 1_000_000_000u128; // huge reserves

        let single_out = compute_constant_product_out(reserve, reserve, amount, 25).unwrap();
        let routes = vec![
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::Orca,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
        ];

        // For very small amounts relative to reserves, split should not beat single
        let split = evaluate_split_routes(&routes, amount);
        // With equal reserves, 50/50 split output = same as single pool (no improvement)
        assert!(split.is_none(), "tiny trade should not benefit from split");
    }

    #[test]
    fn test_split_50_50_not_always_best() {
        // Two pools with different reserve sizes.
        // 50/50 split is not optimal when pools are unequal.
        let amount = 100_000u64;

        // Pool A: large reserves (less impact)
        let large_reserve = 10_000_000u128;
        let single_a = compute_constant_product_out(large_reserve, large_reserve, amount, 25).unwrap();

        // Pool B: small reserves (more impact)
        let small_reserve = 500_000u128;
        let single_b = compute_constant_product_out(small_reserve, small_reserve, amount, 25).unwrap();

        let routes = vec![
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                out_amount: single_a,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: large_reserve,
                reserve_out: large_reserve,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                out_amount: single_b,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: small_reserve,
                reserve_out: small_reserve,
                fee_bps: 25,
            },
        ];

        let split = evaluate_split_routes(&routes, amount);
        if let Some(ref s) = split {
            // For unequal pools, the bigger pool should get more volume
            assert!(s.pct_a > s.pct_b || s.pct_b > s.pct_a, "unequal pools should have unequal split");
        }
        // Whether or not split is found, the best single (pool A) should be good
        assert!(single_a > single_b, "large pool should give better single output");
    }

    #[test]
    fn test_split_percents_sum_to_100() {
        let amount = 50_000u64;
        let reserve = 500_000u128;
        let single_out = compute_constant_product_out(reserve, reserve, amount, 25).unwrap();

        let routes = vec![
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
        ];

        if let Some(split) = evaluate_split_routes(&routes, amount) {
            assert_eq!(split.pct_a + split.pct_b, 100, "split percentages must sum to 100");
            assert_eq!(split.amount_a + split.amount_b, amount, "split amounts must sum to total");
            assert!(split.total_out > 0, "total output must be positive");
        }
    }

    #[test]
    fn test_split_route_amounts_are_correct() {
        // Verify that split amounts are recomputed correctly (not just reusing pool_a/pool_b out_amount)
        let amount = 100_000u64;
        let reserve = 1_000_000u128;
        let single_out = compute_constant_product_out(reserve, reserve, amount, 25).unwrap();

        let routes = vec![
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
            DirectRoute {
                pool_address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                out_amount: single_out,
                fee_amount: compute_fee_amount(amount, 25),
                reserve_in: reserve,
                reserve_out: reserve,
                fee_bps: 25,
            },
        ];

        if let Some(split) = evaluate_split_routes(&routes, amount) {
            // Verify out_a and out_b are individually correct
            let check_a = compute_constant_product_out(reserve, reserve, split.amount_a, 25).unwrap();
            let check_b = compute_constant_product_out(reserve, reserve, split.amount_b, 25).unwrap();
            assert_eq!(split.out_a, check_a, "out_a should match recomputed value");
            assert_eq!(split.out_b, check_b, "out_b should match recomputed value");
            assert_eq!(split.total_out, check_a + check_b, "total should be sum of parts");
        }
    }

    // ── Three-Hop Route Tests ──

    #[test]
    fn test_three_hop_route_struct() {
        let h1 = PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        };
        let h2 = PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::Meteora,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        };
        let h3 = PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: PoolType::PumpFunAmm,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        };
        let bridge1 = Pubkey::new_unique();
        let bridge2 = Pubkey::new_unique();

        let route = ThreeHopRoute {
            hop1_entry: h1,
            hop2_entry: h2,
            hop3_entry: h3,
            bridge1_mint: bridge1,
            bridge2_mint: bridge2,
            hop1_amount_out: 5000,
            hop1_fee_amount: 13,
            hop1_reserve_in: 100_000,
            hop1_reserve_out: 200_000,
            hop2_amount_out: 4800,
            hop2_fee_amount: 12,
            hop2_reserve_in: 150_000,
            hop2_reserve_out: 250_000,
            final_amount_out: 4600,
            hop3_fee_amount: 12,
            hop3_reserve_in: 200_000,
            hop3_reserve_out: 300_000,
        };

        assert_eq!(route.hop1_amount_out, 5000);
        assert_eq!(route.hop2_amount_out, 4800);
        assert_eq!(route.final_amount_out, 4600);
        assert_eq!(route.bridge1_mint, bridge1);
        assert_eq!(route.bridge2_mint, bridge2);
        assert_eq!(route.hop1_entry.pool_type, PoolType::RaydiumCpmm);
        assert_eq!(route.hop2_entry.pool_type, PoolType::Meteora);
        assert_eq!(route.hop3_entry.pool_type, PoolType::PumpFunAmm);
    }

    #[test]
    fn test_three_hop_bridge_pair_filtering() {
        use crate::constants::{SOL_NATIVE_MINT, USDC_MINT, USDT_MINT, PYUSD_MINT};

        let input_mint = SOL_NATIVE_MINT;
        let output_mint = USDC_MINT;

        // Collect valid (bridge1, bridge2) pairs
        let mut pairs: Vec<(Pubkey, Pubkey)> = Vec::new();
        for bridge1 in &BRIDGE_MINTS {
            if *bridge1 == input_mint || *bridge1 == output_mint {
                continue;
            }
            for bridge2 in &BRIDGE_MINTS {
                if *bridge2 == *bridge1 || *bridge2 == input_mint || *bridge2 == output_mint {
                    continue;
                }
                pairs.push((*bridge1, *bridge2));
            }
        }

        // With input=SOL and output=USDC, valid bridges are USDT and PYUSD
        // Valid pairs: (USDT, PYUSD), (PYUSD, USDT)
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&(USDT_MINT, PYUSD_MINT)));
        assert!(pairs.contains(&(PYUSD_MINT, USDT_MINT)));

        // Verify no pair has bridge == input or output
        for (b1, b2) in &pairs {
            assert_ne!(*b1, input_mint, "bridge1 must not equal input");
            assert_ne!(*b1, output_mint, "bridge1 must not equal output");
            assert_ne!(*b2, input_mint, "bridge2 must not equal input");
            assert_ne!(*b2, output_mint, "bridge2 must not equal output");
            assert_ne!(*b1, *b2, "bridge1 must not equal bridge2");
        }
    }

    #[test]
    fn test_build_three_hop_response_structure() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let input_mint = Pubkey::new_unique();
        let output_mint = Pubkey::new_unique();
        let bridge1_mint = Pubkey::new_unique();
        let bridge2_mint = Pubkey::new_unique();
        let req = QuoteRequest {
            input_mint,
            output_mint,
            amount: 1_000_000,
            slippage_bps: 50,
            only_direct_routes: false,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = ThreeHopRoute {
            hop1_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                mint_a: input_mint,
                mint_b: bridge1_mint,
            },
            hop2_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                mint_a: bridge1_mint,
                mint_b: bridge2_mint,
            },
            hop3_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::PumpFunAmm,
                mint_a: bridge2_mint,
                mint_b: output_mint,
            },
            bridge1_mint,
            bridge2_mint,
            hop1_amount_out: 500_000,
            hop1_fee_amount: 1_250,
            hop1_reserve_in: 10_000_000,
            hop1_reserve_out: 5_000_000,
            hop2_amount_out: 480_000,
            hop2_fee_amount: 1_200,
            hop2_reserve_in: 5_000_000,
            hop2_reserve_out: 5_000_000,
            final_amount_out: 460_000,
            hop3_fee_amount: 1_200,
            hop3_reserve_in: 5_000_000,
            hop3_reserve_out: 10_000_000,
        };

        let resp = quoter.build_three_hop_response(&req, &route, 0, 0.01);

        // Must have exactly 3 route steps
        assert_eq!(resp.routes.len(), 3);
        assert_eq!(resp.amount_out, "460000");

        // Hop 1: input -> bridge1
        assert_eq!(resp.routes[0].pool.input_token, input_mint.to_string());
        assert_eq!(resp.routes[0].pool.output_token, bridge1_mint.to_string());
        assert_eq!(resp.routes[0].pool.dex, "Raydium CPMM");
        assert_eq!(resp.routes[0].pool.amount_in, "1000000");
        assert_eq!(resp.routes[0].pool.amount_out, "500000");
        assert_eq!(resp.routes[0].percent, 100);

        // Hop 2: bridge1 -> bridge2
        assert_eq!(resp.routes[1].pool.input_token, bridge1_mint.to_string());
        assert_eq!(resp.routes[1].pool.output_token, bridge2_mint.to_string());
        assert_eq!(resp.routes[1].pool.dex, "Meteora");
        // later hops spend what the previous hop is guaranteed to deliver (50 bps slippage here)
        assert_eq!(resp.routes[1].pool.amount_in, guaranteed(500_000, req.slippage_bps).to_string());
        assert_eq!(resp.routes[1].pool.amount_out, "480000");
        assert_eq!(resp.routes[1].percent, 100);

        // Hop 3: bridge2 -> output
        assert_eq!(resp.routes[2].pool.input_token, bridge2_mint.to_string());
        assert_eq!(resp.routes[2].pool.output_token, output_mint.to_string());
        assert_eq!(resp.routes[2].pool.dex, "PumpFun AMM");
        assert_eq!(resp.routes[2].pool.amount_in, guaranteed(480_000, req.slippage_bps).to_string());
        assert_eq!(resp.routes[2].pool.amount_out, "460000");
        assert_eq!(resp.routes[2].percent, 100);

        // Price impact should be a parseable positive number
        let pi: f64 = resp.price_impact.parse().unwrap();
        assert!(pi >= 0.0, "price impact must be non-negative");
    }

    // ── Platform Fee Tests (always output side) ──

    #[test]
    fn test_compute_platform_fee_sol_output() {
        let output = SOL_NATIVE_MINT;
        let pf = compute_platform_fee(1_000_000_000, &output);
        // 1B * 50 / 10000 = 5_000_000
        assert_eq!(pf.amount, "5000000");
        assert_eq!(pf.fee_bps, 50);
        assert_eq!(pf.fee_token, SOL_NATIVE_MINT.to_string());
        assert_eq!(pf.side, "output");
    }

    #[test]
    fn test_compute_platform_fee_usdc_output() {
        let output = USDC_MINT;
        let pf = compute_platform_fee(100_000_000, &output);
        // 100M * 50 / 10000 = 500_000
        assert_eq!(pf.amount, "500000");
        assert_eq!(pf.fee_token, USDC_MINT.to_string());
        assert_eq!(pf.side, "output");
    }

    #[test]
    fn test_compute_platform_fee_random_token_output() {
        let output = Pubkey::new_unique();
        let pf = compute_platform_fee(5_000_000, &output);
        // 5M * 50 / 10000 = 25_000
        assert_eq!(pf.amount, "25000");
        assert_eq!(pf.fee_token, output.to_string());
        assert_eq!(pf.side, "output");
    }

    #[test]
    fn test_compute_platform_fee_zero_amount() {
        let output = USDC_MINT;
        let pf = compute_platform_fee(0, &output);
        assert_eq!(pf.amount, "0");
        assert_eq!(pf.fee_token, USDC_MINT.to_string());
    }

    #[test]
    fn test_compute_platform_fee_small_amount_rounds_down() {
        let output = USDC_MINT;
        // 99 * 50 / 10000 = 0 (rounds down)
        let pf = compute_platform_fee(99, &output);
        assert_eq!(pf.amount, "0");
    }

    #[test]
    fn test_compute_platform_fee_exact_boundary() {
        let output = USDC_MINT;
        // 200 * 50 / 10000 = 1 (exact)
        let pf = compute_platform_fee(200, &output);
        assert_eq!(pf.amount, "1");
    }

    #[test]
    fn test_compute_platform_fee_large_amount_no_overflow() {
        let output = USDC_MINT;
        // u64::MAX * 50 / 10000 — should not overflow due to u128 intermediate
        let pf = compute_platform_fee(u64::MAX, &output);
        let expected = (u64::MAX as u128 * 50 / 10_000) as u64;
        assert_eq!(pf.amount, expected.to_string());
    }

    #[test]
    fn zero_platform_fee_reports_zero_amount() {
        let prev = platform_fee_bps();
        set_platform_fee_bps(0);
        let pf = compute_platform_fee(1_000_000_000, &Pubkey::new_unique());
        assert_eq!(pf.amount, "0");
        assert_eq!(pf.fee_bps, 0);
        set_platform_fee_bps(prev);
    }

    #[test]
    fn test_platform_fee_bps_is_constant() {
        let pf = compute_platform_fee(1000, &SOL_NATIVE_MINT);
        assert_eq!(pf.fee_bps, 50);
        assert_eq!(pf.fee_bps, platform_fee_bps());
    }

    // ── Slippage enforcement: minimum_out in build_direct_response ──

    #[test]
    fn test_direct_response_minimum_out_matches_threshold() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let input_mint = Pubkey::new_unique();
        let output_mint = Pubkey::new_unique();
        let req = QuoteRequest {
            input_mint,
            output_mint,
            amount: 1_000_000,
            slippage_bps: 100, // 1%
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::RaydiumCpmm,
            out_amount: 500_000,
            fee_amount: 2_500,
            reserve_in: 10_000_000,
            reserve_out: 10_000_000,
            fee_bps: 25,
        };

        let resp = quoter.build_direct_response(&req, &route, 0, 0.01);

        let expected_threshold = compute_threshold(500_000, 100);
        assert_eq!(expected_threshold, 495_000); // 500000 * 9900 / 10000
        assert_eq!(resp.minimum_out, expected_threshold.to_string());
    }

    #[test]
    fn test_direct_response_zero_slippage_minimum_equals_output() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let req = QuoteRequest {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            amount: 1_000_000,
            slippage_bps: 0, // zero slippage
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::Meteora,
            out_amount: 750_000,
            fee_amount: 1_875,
            reserve_in: 5_000_000,
            reserve_out: 5_000_000,
            fee_bps: 25,
        };

        let resp = quoter.build_direct_response(&req, &route, 0, 0.01);

        // With zero slippage, minimum_out should equal amount_out
        assert_eq!(resp.minimum_out, resp.amount_out);
        assert_eq!(resp.minimum_out, "750000");
    }

    #[test]
    fn test_direct_response_high_slippage_minimum_near_zero() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let req = QuoteRequest {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            amount: 1_000_000,
            slippage_bps: 9999, // 99.99% slippage
            only_direct_routes: true,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = DirectRoute {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::PumpFunAmm,
            out_amount: 1_000_000,
            fee_amount: 2_500,
            reserve_in: 10_000_000,
            reserve_out: 10_000_000,
            fee_bps: 25,
        };

        let resp = quoter.build_direct_response(&req, &route, 0, 0.01);

        // 1_000_000 * (10000 - 9999) / 10000 = 1_000_000 * 1 / 10000 = 100
        let expected = compute_threshold(1_000_000, 9999);
        assert_eq!(expected, 100);
        assert_eq!(resp.minimum_out, "100");
    }

    #[test]
    fn test_two_hop_response_minimum_out_matches_threshold() {
        let registry = Arc::new(PoolRegistry::new());
        let cache = Arc::new(crate::pool::cache::PoolCache::new(2000));
        let rpc = Arc::new(RpcClient::new("http://localhost:8899".to_string()));
        let quoter = Quoter::new(registry, cache, rpc);

        let input_mint = Pubkey::new_unique();
        let output_mint = Pubkey::new_unique();
        let bridge_mint = Pubkey::new_unique();
        let req = QuoteRequest {
            input_mint,
            output_mint,
            amount: 1_000_000,
            slippage_bps: 200, // 2%
            only_direct_routes: false,
            exclude_dexes: vec![],
            dexes: vec![],
            max_accounts: 64,
        };

        let route = TwoHopRoute {
            hop1_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::RaydiumCpmm,
                mint_a: input_mint,
                mint_b: bridge_mint,
            },
            hop2_entry: PoolEntry {
                address: Pubkey::new_unique(),
                pool_type: PoolType::Meteora,
                mint_a: bridge_mint,
                mint_b: output_mint,
            },
            bridge_mint,
            hop1_amount_out: 500_000,
            hop1_fee_amount: 1_250,
            hop1_reserve_in: 10_000_000,
            hop1_reserve_out: 5_000_000,
            final_amount_out: 490_000,
            hop2_fee_amount: 1_250,
            hop2_reserve_in: 5_000_000,
            hop2_reserve_out: 10_000_000,
        };

        let resp = quoter.build_two_hop_response(&req, &route, 0, 0.01);

        // threshold = 490000 * (10000 - 200) / 10000 = 490000 * 9800 / 10000 = 480200
        let expected_threshold = compute_threshold(490_000, 200);
        assert_eq!(expected_threshold, 480_200);
        assert_eq!(resp.minimum_out, expected_threshold.to_string());
    }
}
