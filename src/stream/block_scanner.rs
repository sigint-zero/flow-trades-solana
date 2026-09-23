//! Lightweight block scanner for autonomous pool discovery.
//!
//! Polls recent Solana blocks via `getBlock`, scans every transaction for
//! instructions targeting known DEX program IDs, extracts candidate pool
//! addresses, and attempts to fetch their on-chain state. New pools are
//! automatically added to the registry and cache.
//!
//! No Geyser, no external dependencies needed — just a Solana RPC endpoint.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, UiInstruction,
    EncodedTransaction, UiConfirmedBlock, UiMessage,
    UiTransactionEncoding,
};
use tracing::{debug, info, warn};

use crate::constants::*;
use crate::enrichment::PriceOracle;
use crate::stream::account_mirror::AccountMirror;
use crate::pool::cache::PoolCache;
use crate::pool::registry::{PoolEntry, PoolRegistry};
use crate::pool::types::PoolType;
use crate::storage::sqlite::PoolDb;
use crate::stream::swap_stream::{parse_swaps_from_block, Swap};
use crate::stream::types::StreamStats;

/// Optional swap-stream context for the scanner. When `Some`, every block
/// is also fed into the swap parser and resulting swaps go into the
/// broadcast channel.
#[derive(Clone)]
pub struct SwapStreamCtx {
    pub tx: tokio::sync::broadcast::Sender<std::sync::Arc<Swap>>,
    pub oracle: Arc<PriceOracle>,
}

/// Maximum number of concurrent pool-fetch tasks when discovering new pools.
const MAX_CONCURRENT_DISCOVERIES: usize = 50;

/// Convert an HTTP(S) URL to a WS(S) URL for blockSubscribe.
fn http_to_ws(url: &str) -> String {
    if url.starts_with("https://") {
        format!("wss://{}", &url["https://".len()..])
    } else if url.starts_with("http://") {
        format!("ws://{}", &url["http://".len()..])
    } else {
        url.to_string()
    }
}

/// Build the program ID -> PoolType lookup map.
fn build_program_map() -> HashMap<Pubkey, PoolType> {
    HashMap::from([
        (RAYDIUM_V4_PROG_ID, PoolType::RaydiumV4),
        (RAYDIUM_CPMM_PROG_ID, PoolType::RaydiumCpmm),
        (RAYDIUM_CL_PROG_ID, PoolType::RaydiumCl),
        (RAYDIUM_LP_PROG_ID, PoolType::RaydiumLp),
        (PUMP_FUN_PROG_ID, PoolType::PumpFun),
        (PUMP_FUN_AMM_PROG_ID, PoolType::PumpFunAmm),
        (METEORA_PROG_ID, PoolType::Meteora),
        (METEORA_DLMM_PROG_ID, PoolType::MeteoraDlmm),
        (METEORA_DAMM_PROG_ID, PoolType::MeteoraDamm),
        (METEORA_DBC_PROG_ID, PoolType::MeteoraDbc),
        (ORCA_PROG_ID, PoolType::Orca),
        (FLUXBEAM_PROG_ID, PoolType::FluxBeam),
        (SAROS_PROG_ID, PoolType::Saros),
        (DOOAR_PROG_ID, PoolType::Dooar),
        (PANCAKESWAP_PROG_ID, PoolType::PancakeSwap),
        (FLASH_TRADE_PROG_ID, PoolType::FlashTrade),
        (BYREAL_PROG_ID, PoolType::Byreal),
        (DEFITUNA_FUSION_PROG_ID, PoolType::DefiTunaFusion),
        (DEFITUNA_POOLS_PROG_ID, PoolType::DefiTunaPools),
        (PUMPUP_PROG_ID, PoolType::Pumpup),
        // OnChain Labs DEX V2 is intentionally NOT mapped to a PoolType: it's
        // an aggregator router, not a directly-quotable DEX. Including it in
        // the Geyser owner filter (see geyser.rs::all_dex_programs) means we
        // receive blocks containing OnChain Labs txs; the existing block
        // scanner walks inner instructions and registers any KNOWN-DEX pools
        // it routes through (Raydium, Orca, Meteora, etc.) — the discovery
        // benefit. We just don't try to register OnChain Labs program accounts
        // themselves as pools.
    ])
}

/// Map a DEX program ID to its PoolType.
pub fn dex_program_to_type(program_id: &Pubkey) -> Option<PoolType> {
    // Use a static map for O(1) lookups in hot path
    static PROGRAM_MAP: std::sync::OnceLock<HashMap<Pubkey, PoolType>> = std::sync::OnceLock::new();
    let map = PROGRAM_MAP.get_or_init(build_program_map);
    map.get(program_id).copied()
}

/// Extract the pool address from an instruction's account list based on the DEX type.
///
/// Each DEX puts the pool account at a specific index:
/// - accounts[1]: RaydiumV4, RaydiumCpmm, RaydiumLp, Meteora, MeteoraDlmm, FluxBeam, Saros, Dooar
/// - accounts[2]: RaydiumCl, Byreal, PumpFun, MeteoraDamm, MeteoraDbc, Orca, PancakeSwap
/// - accounts[3]: PumpupBonding (pool_sol_account)
/// - accounts[0]: PumpFunAmm (buy/sell: `[0] pool, [1] user, [2] global_config,
///   [3] base_mint, ...` — index 3 was the MINT, which made every pAMM pool
///   discovered from a block an undecodable "pool" and the swap-stream `pool`
///   field the token mint; the Geyser account stream masked it)
pub fn extract_pool_index(pool_type: PoolType) -> usize {
    match pool_type {
        PoolType::RaydiumV4 => 1,
        PoolType::RaydiumCpmm => 1,
        PoolType::RaydiumLp => 1,
        PoolType::Meteora => 1,
        PoolType::MeteoraDlmm => 1,
        PoolType::FluxBeam => 1,
        PoolType::Saros => 1,
        PoolType::Dooar => 1,
        PoolType::RaydiumCl => 2,
        PoolType::Byreal => 2,
        PoolType::PumpFun => 2,
        PoolType::MeteoraDamm => 2,
        PoolType::MeteoraDbc => 2,
        PoolType::Orca => 2,
        PoolType::PancakeSwap => 2,
        PoolType::PumpFunAmm => 0,
        // Pumpup `swap` ix puts pool at accounts[0] per IDL.
        PoolType::Pumpup => 0,
        // Pumpup `buy`/`sell` put pool_sol_account at accounts[3] per IDL.
        PoolType::PumpupBonding => 3,
        // Remaining DEXes: try accounts[1] as a reasonable default
        PoolType::FlashTrade
        | PoolType::DefiTunaFusion
        | PoolType::DefiTunaPools => 1,
        PoolType::Unknown => 1,
    }
}

/// First 8 bytes of `sha256("global:swap")` — Pumpup AMM swap.
const PUMPUP_SWAP_DISC: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
/// First 8 bytes of `sha256("global:buy")` — Pumpup bonding-curve buy.
const PUMPUP_BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// First 8 bytes of `sha256("global:sell")` — Pumpup bonding-curve sell.
const PUMPUP_SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Disambiguate a Pumpup instruction by inspecting its first 8 data bytes.
/// Returns `PoolType::Pumpup` for AMM `swap`, `PoolType::PumpupBonding` for
/// `buy`/`sell`, and `None` for any other Pumpup ix (init/migrate/etc.) so
/// the scanner skips it instead of registering an unrelated account as a pool.
pub(crate) fn pumpup_pool_type_from_ix_data(ix_data_b58: &str) -> Option<PoolType> {
    let bytes = bs58::decode(ix_data_b58).into_vec().ok()?;
    refine_pool_type(PoolType::Pumpup, &bytes)
}

/// The pool type an instruction to `base`'s program actually refers to, or
/// `None` when the instruction is not a swap whose accounts carry a pool.
///
/// Two programs need the discriminator, not just the program id:
/// - **Pumpup** shares one program across the AMM (`swap`) and the bonding
///   curve (`buy`/`sell`).
/// - **pump.fun AMM** invokes ITSELF for Anchor event emission after every
///   swap (accounts `[event_authority, program, ...]`), and also has
///   `create_pool`/`deposit`/`withdraw`. Treating those as swaps registered
///   the event authority as a "pool" and made the swap stream emit every pAMM
///   trade twice (once per distinct "pool" in the same transaction).
pub(crate) fn refine_pool_type(base: PoolType, ix_data: &[u8]) -> Option<PoolType> {
    match base {
        PoolType::Pumpup | PoolType::PumpupBonding => {
            let disc: [u8; 8] = ix_data.get(..8)?.try_into().ok()?;
            if disc == PUMPUP_SWAP_DISC {
                Some(PoolType::Pumpup)
            } else if disc == PUMPUP_BUY_DISC || disc == PUMPUP_SELL_DISC {
                Some(PoolType::PumpupBonding)
            } else {
                None
            }
        }
        PoolType::PumpFunAmm => {
            use crate::execution::amms::pumpfun_amm::{BUY_DISC, BUY_EXACT_QUOTE_IN_DISC, SELL_DISC};
            let disc: [u8; 8] = ix_data.get(..8)?.try_into().ok()?;
            (disc == BUY_DISC || disc == SELL_DISC || disc == BUY_EXACT_QUOTE_IN_DISC)
                .then_some(PoolType::PumpFunAmm)
        }
        other => Some(other),
    }
}

/// Where a SWAP instruction keeps its pool account, for every program we
/// discover from blocks — keyed by program AND instruction discriminator,
/// because one program has several swap shapes (Orca `swap` vs `swapV2`,
/// Raydium LP's four exact-in/out variants) and non-swap instructions
/// (deposit, create, event self-CPI) carry no pool at any fixed position.
/// Returns `None` for anything that is not a recognised swap.
///
/// Positions (CPMM 3, Raydium LP 4, Meteora / DLMM 0, DAMM 1, pump.fun bonding
/// 3, the SPL token-swap forks 0, DefiTuna Fusion 4, …) were verified against
/// ~200k attributed mainnet swaps.
pub fn swap_pool_index(program_id: &Pubkey, ix_data: &[u8]) -> Option<(PoolType, usize)> {
    use crate::execution::amms::pumpfun_amm::{BUY_DISC, BUY_EXACT_QUOTE_IN_DISC, SELL_DISC};
    let base = dex_program_to_type(program_id)?;
    let disc: Option<[u8; 8]> = ix_data.get(..8).and_then(|d| d.try_into().ok());
    let d = |x: [u8; 8]| disc == Some(x);
    // Anchor `global:swap` / `global:swap_v2` — shared by Raydium CLMM, Orca V1,
    // Meteora Standard/DAMM, Pumpup AMM, PancakeSwap.
    const ANCHOR_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
    const ANCHOR_SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
    const SWAP2: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136]; // DLMM swap2 / DBC swap2
    const DLMM_EXACT_OUT: [u8; 8] = [250, 73, 101, 33, 38, 207, 75, 184];
    const DLMM_WITH_PRICE: [u8; 8] = [56, 173, 230, 208, 173, 228, 156, 205];
    const CPMM_SWAP_BASE_IN: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
    const CPMM_SWAP_BASE_OUT: [u8; 8] = [55, 217, 98, 86, 163, 74, 180, 173];
    const LP_BUY_EXACT_IN: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236];
    const LP_BUY_EXACT_OUT: [u8; 8] = [24, 211, 116, 40, 105, 3, 153, 56];
    const LP_SELL_EXACT_IN: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];
    const LP_SELL_EXACT_OUT: [u8; 8] = [95, 200, 71, 34, 8, 9, 11, 166];
    const BYREAL_SWAP_V3_DYN: [u8; 8] = [229, 46, 213, 132, 105, 40, 40, 228]; // Byreal `swap_v3_dyn`
    let idx = match base {
        // Raydium V4: legacy u8 tag — 9 / 11 = swap_base_in / swap_base_out,
        // 16 / 17 = their `_v2` forms without OpenBook accounts; [1] = amm
        PoolType::RaydiumV4 => (matches!(ix_data.first(), Some(9) | Some(11) | Some(16) | Some(17))).then_some(1)?,
        PoolType::RaydiumCpmm => (d(CPMM_SWAP_BASE_IN) || d(CPMM_SWAP_BASE_OUT)).then_some(3)?,
        PoolType::RaydiumLp => (d(LP_BUY_EXACT_IN) || d(LP_BUY_EXACT_OUT) || d(LP_SELL_EXACT_IN) || d(LP_SELL_EXACT_OUT)).then_some(4)?,
        PoolType::RaydiumCl | PoolType::PancakeSwap => (d(ANCHOR_SWAP) || d(ANCHOR_SWAP_V2)).then_some(2)?,
        // Byreal (Raydium CLMM fork): swap / swap_v2 / swap_v3_dyn, [2] = pool_state
        PoolType::Byreal => (d(ANCHOR_SWAP) || d(ANCHOR_SWAP_V2) || d(BYREAL_SWAP_V3_DYN)).then_some(2)?,
        // bonding curve at [3] in buy/sell/buy_exact_sol_in, at [10] in the
        // `_v2` instructions (quote-mint aware; also used on SOL curves)
        PoolType::PumpFun => {
            use crate::execution::amms::pumpfun::BUY_EXACT_SOL_IN_DISC;
            const BUY_V2: [u8; 8] = [0xb8, 0x17, 0xee, 0x61, 0x67, 0xc5, 0xd3, 0x3d];
            const SELL_V2: [u8; 8] = [0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2];
            const BUY_EXACT_QUOTE_IN_V2: [u8; 8] = [0xc2, 0xab, 0x1c, 0x46, 0x68, 0x4d, 0x5b, 0x2f];
            if d(BUY_DISC) || d(SELL_DISC) || d(BUY_EXACT_SOL_IN_DISC) {
                3
            } else if d(BUY_V2) || d(SELL_V2) || d(BUY_EXACT_QUOTE_IN_V2) {
                10
            } else {
                return None;
            }
        }
        PoolType::PumpFunAmm => (d(BUY_DISC) || d(SELL_DISC) || d(BUY_EXACT_QUOTE_IN_DISC)).then_some(0)?,
        PoolType::Meteora => d(ANCHOR_SWAP).then_some(0)?,
        PoolType::MeteoraDlmm => (d(ANCHOR_SWAP) || d(SWAP2) || d(DLMM_EXACT_OUT) || d(DLMM_WITH_PRICE)).then_some(0)?,
        PoolType::MeteoraDamm => d(ANCHOR_SWAP).then_some(1)?,
        PoolType::MeteoraDbc => (d(SWAP2) || d(ANCHOR_SWAP)).then_some(2)?,
        PoolType::Orca => {
            if d(ANCHOR_SWAP) { 2 } else if d(ANCHOR_SWAP_V2) { 4 } else { return None }
        }
        // SPL token-swap forks: single-byte tag 1 = Swap; [0] = swap state
        PoolType::FluxBeam | PoolType::Saros | PoolType::Dooar => (ix_data.first() == Some(&1)).then_some(0)?,
        PoolType::FlashTrade | PoolType::DefiTunaPools => 0,
        PoolType::DefiTunaFusion => 4,
        // Pumpup shares one program across the AMM (`swap`, [0] pool) and the
        // bonding curve (`buy`/`sell`, [3] pool_sol_account).
        PoolType::Pumpup | PoolType::PumpupBonding => {
            return match refine_pool_type(PoolType::Pumpup, ix_data)? {
                PoolType::Pumpup => Some((PoolType::Pumpup, 0)),
                _ => Some((PoolType::PumpupBonding, 3)),
            };
        }
        PoolType::Unknown => return None,
    };
    Some((base, idx))
}

/// [`swap_pool_index`] for the RPC paths, where instruction data is base58.
pub fn swap_pool_index_b58(program_id: &Pubkey, ix_data_b58: &str) -> Option<(PoolType, usize)> {
    dex_program_to_type(program_id)?;
    let bytes = bs58::decode(ix_data_b58).into_vec().ok()?;
    swap_pool_index(program_id, &bytes)
}

/// Candidates whose state fetch failed recently (not a pool, or a transient
/// RPC error). Without this the same wrong account would be re-discovered on
/// every block that touches it and re-fetched.
static FAILED_CANDIDATES: std::sync::LazyLock<dashmap::DashMap<Pubkey, std::time::Instant>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);
const FAILED_CANDIDATE_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const FAILED_CANDIDATE_CAP: usize = 50_000;

/// Remember a candidate whose fetch failed (bounded; oldest are evicted by TTL).
pub fn note_failed_candidate(pool: Pubkey) {
    if FAILED_CANDIDATES.len() >= FAILED_CANDIDATE_CAP {
        let now = std::time::Instant::now();
        FAILED_CANDIDATES.retain(|_, t| now.duration_since(*t) < FAILED_CANDIDATE_TTL);
    }
    FAILED_CANDIDATES.insert(pool, std::time::Instant::now());
}

/// True when `pool` failed a fetch within the TTL.
pub fn recently_failed(pool: &Pubkey) -> bool {
    FAILED_CANDIDATES
        .get(pool)
        .is_some_and(|t| t.elapsed() < FAILED_CANDIDATE_TTL)
}

/// [`refine_pool_type`] for the RPC paths, where instruction data is base58.
pub(crate) fn pool_type_for_ix_b58(program_id: &Pubkey, ix_data_b58: &str) -> Option<PoolType> {
    let base = dex_program_to_type(program_id)?;
    match base {
        PoolType::Pumpup | PoolType::PumpupBonding | PoolType::PumpFunAmm => {
            let bytes = bs58::decode(ix_data_b58).into_vec().ok()?;
            refine_pool_type(base, &bytes)
        }
        other => Some(other),
    }
}

/// Extract (mint_a, mint_b) from a parsed PoolState.
pub fn extract_mints_from_state(state: &crate::pool::types::PoolState) -> Option<(Pubkey, Pubkey)> {
    use crate::pool::types::PoolState;
    match state {
        PoolState::RaydiumV4 {
            coin_mint,
            pc_mint,
            ..
        } => Some((*coin_mint, *pc_mint)),
        PoolState::RaydiumCpmm {
            token_0_mint,
            token_1_mint,
            ..
        } => Some((*token_0_mint, *token_1_mint)),
        PoolState::RaydiumClmm {
            token_mint_0,
            token_mint_1,
            ..
        } => Some((*token_mint_0, *token_mint_1)),
        PoolState::RaydiumLp {
            base_mint,
            quote_mint,
            ..
        } => Some((*base_mint, *quote_mint)),
        PoolState::PumpFun { mint, .. } => Some((*mint, SOL_NATIVE_MINT)),
        PoolState::PumpFunAmm {
            base_mint,
            quote_mint,
            ..
        } => Some((*base_mint, *quote_mint)),
        PoolState::Meteora {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::MeteoraDlmm {
            token_x_mint,
            token_y_mint,
            ..
        } => Some((*token_x_mint, *token_y_mint)),
        PoolState::MeteoraDamm {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::MeteoraDbc {
            base_mint,
            quote_mint,
            ..
        } => Some((*base_mint, *quote_mint)),
        PoolState::Orca {
            token_mint_a,
            token_mint_b,
            ..
        } => Some((*token_mint_a, *token_mint_b)),
        PoolState::FluxBeam {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::FlashTrade { token_mint, .. } => Some((*token_mint, SOL_NATIVE_MINT)),
        PoolState::Byreal {
            token_mint_a,
            token_mint_b,
            ..
        } => Some((*token_mint_a, *token_mint_b)),
        PoolState::DefiTunaFusion {
            token_mint_a,
            token_mint_b,
            ..
        } => Some((*token_mint_a, *token_mint_b)),
        PoolState::DefiTunaPools {
            token_mint_a,
            token_mint_b,
            ..
        } => Some((*token_mint_a, *token_mint_b)),
        PoolState::Saros {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::PancakeSwap {
            token_mint_a,
            token_mint_b,
            ..
        } => Some((*token_mint_a, *token_mint_b)),
        PoolState::Dooar {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::Pumpup {
            token_a_mint,
            token_b_mint,
            ..
        } => Some((*token_a_mint, *token_b_mint)),
        PoolState::PumpupBonding { mint, .. } => Some((*mint, SOL_NATIVE_MINT)),
    }
}

/// Scan a block for new pool addresses and register any that aren't already known.
///
/// Returns the number of newly discovered pools.
/// Extract new pool candidates from a block — sync, no RPC, no async.
/// Returns (pool_address, pool_type) pairs for pools not yet in the registry.
pub fn extract_candidates_from_block(
    block: &UiConfirmedBlock,
    registry: &PoolRegistry,
) -> Vec<(Pubkey, PoolType)> {
    let transactions = match &block.transactions {
        Some(txs) => txs,
        None => return Vec::new(),
    };

    let mut candidates: HashSet<(Pubkey, PoolType)> = HashSet::new();

    for encoded_tx in transactions {
        if let Some(ref meta) = encoded_tx.meta {
            if meta.err.is_some() { continue; }
        }

        let (static_keys, instructions) = match &encoded_tx.transaction {
            EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                UiMessage::Raw(raw) => (&raw.account_keys, &raw.instructions),
                UiMessage::Parsed(_) => continue,
            },
            _ => continue,
        };
        // Full key table: static keys ++ ALT-loaded writable ++ readonly — inner
        // instructions index into this, exactly as the runtime does.
        let mut account_keys: Vec<String> = static_keys.clone();
        if let Some(meta) = &encoded_tx.meta {
            if let OptionSerializer::Some(la) = &meta.loaded_addresses {
                account_keys.extend(la.writable.iter().cloned());
                account_keys.extend(la.readonly.iter().cloned());
            }
        }
        // Top-level AND inner instructions: most volume on the large pools is
        // routed through aggregators, where the DEX call is a CPI. Scanning
        // only the top level left those pools undiscovered without Geyser.
        let mut all_ixs: Vec<(u8, &Vec<u8>, &String)> =
            instructions.iter().map(|ix| (ix.program_id_index, &ix.accounts, &ix.data)).collect();
        if let Some(meta) = &encoded_tx.meta {
            if let OptionSerializer::Some(inner) = &meta.inner_instructions {
                for ii in inner {
                    for ux in &ii.instructions {
                        if let UiInstruction::Compiled(c) = ux {
                            all_ixs.push((c.program_id_index, &c.accounts, &c.data));
                        }
                    }
                }
            }
        }

        for (prog_idx_u8, ix_accounts, ix_data) in all_ixs {
            let prog_idx = prog_idx_u8 as usize;
            if prog_idx >= account_keys.len() { continue; }

            let program_id = match Pubkey::from_str(&account_keys[prog_idx]) {
                Ok(pk) => pk,
                Err(_) => continue,
            };

            // Program id + discriminator decide both the pool type and WHERE
            // the pool sits in this instruction's accounts; non-swaps are skipped.
            let (pool_type, pool_idx) = match swap_pool_index_b58(&program_id, ix_data) {
                Some(x) => x,
                None => continue,
            };
            if pool_idx >= ix_accounts.len() { continue; }

            let account_idx = ix_accounts[pool_idx] as usize;
            if account_idx >= account_keys.len() { continue; }

            let pool_address = match Pubkey::from_str(&account_keys[account_idx]) {
                Ok(pk) => pk,
                Err(_) => continue,
            };

            if registry.contains(&pool_address) || recently_failed(&pool_address) { continue; }
            candidates.insert((pool_address, pool_type));
        }
    }

    candidates.into_iter().collect()
}

/// Summary log interval in seconds.
const SUMMARY_INTERVAL_SECS: u64 = 30;

/// Run the block scanner loop. Polls recent blocks and discovers new pools.
///
/// This function runs forever (or until the task is cancelled).
///
/// Features:
/// - Exponential backoff on errors (1s -> 2s -> 4s -> ... -> 30s max, reset on success)
/// - Skip-ahead when falling behind by >100 slots (jumps to current-5)
/// - Summary logging every 30s instead of per-slot
pub async fn run_block_scanner(
    rpc: Arc<RpcClient>,
    registry: Arc<PoolRegistry>,
    cache: Arc<PoolCache>,
    stats: Arc<StreamStats>,
    pool_db: Arc<PoolDb>,
    _scan_interval_ms: u64, // unused — blockSubscribe is push-based
    swap_stream: Option<SwapStreamCtx>,
    mirror: Option<Arc<AccountMirror>>,
) {
    let refresh = mirror.map(|m| Arc::new(crate::stream::block_refresh::BlockRefreshCtx {
        rpc: Arc::clone(&rpc),
        registry: Arc::clone(&registry),
        cache: Arc::clone(&cache),
        mirror: m,
        in_flight: Arc::new(dashmap::DashSet::new()),
    }));
    let ws_url = http_to_ws(&rpc.url());

    // Summary counters (reset every SUMMARY_INTERVAL_SECS)
    let mut summary_slots_scanned: u64 = 0;
    let mut summary_pools_discovered: u64 = 0;
    let mut summary_errors: u64 = 0;
    let mut last_summary = std::time::Instant::now();

    info!(
        swap_stream_enabled = swap_stream.is_some(),
        "block scanner started (blockSubscribe — real-time, every block)"
    );

    loop {
        match run_block_subscribe(
            &ws_url, &rpc, &registry, &cache, &stats, &pool_db,
            &mut summary_slots_scanned, &mut summary_pools_discovered, &mut summary_errors,
            &mut last_summary, swap_stream.as_ref(), refresh.as_ref(),
        ).await {
            Ok(()) => info!("blockSubscribe ended, reconnecting"),
            Err(e) => warn!(error = %e, "blockSubscribe error, reconnecting in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Run a single blockSubscribe session. Returns on disconnect.
#[allow(clippy::too_many_arguments)]
async fn run_block_subscribe(
    ws_url: &str,
    rpc: &Arc<RpcClient>,
    registry: &Arc<PoolRegistry>,
    cache: &Arc<PoolCache>,
    stats: &Arc<StreamStats>,
    pool_db: &Arc<PoolDb>,
    summary_slots: &mut u64,
    summary_pools: &mut u64,
    summary_errors: &mut u64,
    last_summary: &mut std::time::Instant,
    swap_stream: Option<&SwapStreamCtx>,
    refresh: Option<&Arc<crate::stream::block_refresh::BlockRefreshCtx>>,
) -> crate::error::TradeResult<()> {
    use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient;
    use solana_client::rpc_config::RpcBlockSubscribeFilter;
    use futures::StreamExt;

    let pubsub = PubsubClient::new(ws_url)
        .await
        .map_err(|e| crate::error::TradeError::Rpc(format!("WS connect: {e}")))?;

    // The version is declared once, in `stream::tx_version` — a stale number
    // here would turn every block holding a newer transaction into `block: null`.
    let config = crate::stream::tx_version::block_subscribe_config(UiTransactionEncoding::Json);

    // Subscribe to ALL blocks. We process each block synchronously (extract pool
    // candidates = fast, no RPC), then spawn async RPC fetches for new pools in
    // the background.
    let (mut stream, _unsub) = pubsub
        .block_subscribe(RpcBlockSubscribeFilter::All, Some(config))
        .await
        .map_err(|e| crate::error::TradeError::Rpc(format!("blockSubscribe: {e}")))?;

    info!("blockSubscribe active — receiving all blocks");

    // Semaphore to cap concurrent pool-fetch tasks
    let fetch_sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DISCOVERIES));

    while let Some(notification) = stream.next().await {
        let update = notification.value;
        crate::stream::note_slot(update.slot);
        if let Some(block) = update.block {
            *summary_slots += 1;

            // Swap stream: parse + broadcast every confirmed DEX swap.
            // Lossy by design — slow consumers drop, never block.
            if let Some(ctx) = swap_stream {
                let emitted = parse_swaps_from_block(&block, &ctx.oracle, &ctx.tx);
                if emitted > 0 {
                    stats.record_swap_emitted(emitted as u64);
                }
            }

            // Block-driven freshness (the Geyser job, from the block): vault
            // balances → mirror now; touched state-priced pools re-read in the
            // background. The quote path never has to notice staleness itself.
            if let Some(ctx) = refresh {
                let pass = crate::stream::block_refresh::mirror_block(&block, &ctx.registry, &ctx.mirror);
                if pass.vault_updates > 0 {
                    stats.record_update();
                }
                if !pass.touched.is_empty() {
                    let ctx = Arc::clone(ctx);
                    tokio::spawn(async move {
                        let (states, ticks) = crate::stream::block_refresh::refresh_touched(&ctx, pass.touched).await;
                        debug!(states, ticks, "block refresh");
                    });
                }
            }

            // Extract pool candidates synchronously (fast — no RPC, no async)
            let candidates = extract_candidates_from_block(&block, registry);

            if !candidates.is_empty() {
                let placeholder = Pubkey::default();
                for (pool_address, pool_type) in candidates {
                    // Register immediately with placeholder mints.
                    // contains() will return true — prevents re-discovery.
                    // lookup() won't find it until background fetch updates mints.
                    registry.add(PoolEntry {
                        address: pool_address,
                        pool_type,
                        mint_a: placeholder,
                        mint_b: placeholder,
                    });

                    // Background: fetch real state + mints and update registry
                    let rpc = Arc::clone(rpc);
                    let registry = Arc::clone(registry);
                    let cache = Arc::clone(cache);
                    let stats = Arc::clone(stats);
                    let pool_db = Arc::clone(pool_db);
                    let sem = Arc::clone(&fetch_sem);
                    let mirror = refresh.map(|r| Arc::clone(&r.mirror));

                    tokio::spawn(async move {
                        let _permit = sem.acquire().await;
                        match crate::pool::fetcher::fetch_pool_state(&rpc, pool_type, &pool_address).await {
                            Ok(state) => {
                                if let Some(m) = &mirror {
                                    for vault in crate::stream::geyser::extract_vault_pubkeys(&state) {
                                        m.register_vault(vault, pool_address);
                                    }
                                }
                                if crate::pool::fetcher::is_state_priced(pool_type) {
                                    let _ = crate::pool::ticks::load_clmm_ticks(&rpc, &state).await;
                                }
                                if pool_type == PoolType::MeteoraDlmm {
                                    let _ = crate::pool::bins::load_dlmm_bins(&rpc, &state).await;
                                }
                                if let Some((mint_a, mint_b)) = extract_mints_from_state(&state) {
                                    // token program + Token-2022 transfer fee, once per mint
                                    crate::pool::mints::ensure_mint_info(&rpc, &[mint_a, mint_b]).await;
                                }
                                if let Some((mint_a, mint_b)) = extract_mints_from_state(&state) {
                                    let entry = PoolEntry {
                                        address: pool_address,
                                        pool_type,
                                        mint_a,
                                        mint_b,
                                    };
                                    let _ = pool_db.insert_pool(&entry);
                                    registry.add(entry); // updates mints from placeholder
                                    cache.insert(pool_address, state);
                                    stats.record_update();
                                }
                            }
                            Err(e) => {
                                // Not a valid pool (or a transient RPC error) — drop the
                                // placeholder and stop re-discovering it for a while.
                                debug!(pool = %pool_address, ?pool_type, error = %e, "candidate fetch failed");
                                registry.remove(&pool_address);
                                note_failed_candidate(pool_address);
                            }
                        }
                    });
                    *summary_pools += 1;
                }
            }
        }

        // Periodic summary
        if last_summary.elapsed().as_secs() >= SUMMARY_INTERVAL_SECS {
            if *summary_slots > 0 || *summary_pools > 0 {
                info!(
                    refresh_vaults = crate::stream::block_refresh::REFRESH_STATS.vault_updates.load(std::sync::atomic::Ordering::Relaxed),
                    refresh_touched = crate::stream::block_refresh::REFRESH_STATS.touched.load(std::sync::atomic::Ordering::Relaxed),
                    refresh_states = crate::stream::block_refresh::REFRESH_STATS.refreshed.load(std::sync::atomic::Ordering::Relaxed),
                    refresh_ticks = crate::stream::block_refresh::REFRESH_STATS.ticks.load(std::sync::atomic::Ordering::Relaxed),
                    slots = *summary_slots,
                    discovered = *summary_pools,
                    errors = *summary_errors,
                    registry = registry.len(),
                    "block scanner summary (last {}s)",
                    SUMMARY_INTERVAL_SECS
                );
            }
            *summary_slots = 0;
            *summary_pools = 0;
            *summary_errors = 0;
            *last_summary = std::time::Instant::now();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dex_program_to_type_all_programs() {
        // Verify all 19 DEX programs are mapped
        assert_eq!(dex_program_to_type(&RAYDIUM_V4_PROG_ID), Some(PoolType::RaydiumV4));
        assert_eq!(dex_program_to_type(&RAYDIUM_CPMM_PROG_ID), Some(PoolType::RaydiumCpmm));
        assert_eq!(dex_program_to_type(&RAYDIUM_CL_PROG_ID), Some(PoolType::RaydiumCl));
        assert_eq!(dex_program_to_type(&RAYDIUM_LP_PROG_ID), Some(PoolType::RaydiumLp));
        assert_eq!(dex_program_to_type(&PUMP_FUN_PROG_ID), Some(PoolType::PumpFun));
        assert_eq!(dex_program_to_type(&PUMP_FUN_AMM_PROG_ID), Some(PoolType::PumpFunAmm));
        assert_eq!(dex_program_to_type(&METEORA_PROG_ID), Some(PoolType::Meteora));
        assert_eq!(dex_program_to_type(&METEORA_DLMM_PROG_ID), Some(PoolType::MeteoraDlmm));
        assert_eq!(dex_program_to_type(&METEORA_DAMM_PROG_ID), Some(PoolType::MeteoraDamm));
        assert_eq!(dex_program_to_type(&METEORA_DBC_PROG_ID), Some(PoolType::MeteoraDbc));
        assert_eq!(dex_program_to_type(&ORCA_PROG_ID), Some(PoolType::Orca));
        assert_eq!(dex_program_to_type(&FLUXBEAM_PROG_ID), Some(PoolType::FluxBeam));
        assert_eq!(dex_program_to_type(&SAROS_PROG_ID), Some(PoolType::Saros));
        assert_eq!(dex_program_to_type(&DOOAR_PROG_ID), Some(PoolType::Dooar));
        assert_eq!(dex_program_to_type(&PANCAKESWAP_PROG_ID), Some(PoolType::PancakeSwap));
        assert_eq!(dex_program_to_type(&FLASH_TRADE_PROG_ID), Some(PoolType::FlashTrade));
        assert_eq!(dex_program_to_type(&BYREAL_PROG_ID), Some(PoolType::Byreal));
        assert_eq!(dex_program_to_type(&DEFITUNA_FUSION_PROG_ID), Some(PoolType::DefiTunaFusion));
        assert_eq!(dex_program_to_type(&DEFITUNA_POOLS_PROG_ID), Some(PoolType::DefiTunaPools));
    }

    #[test]
    fn test_dex_program_to_type_unknown() {
        let unknown = Pubkey::new_unique();
        assert_eq!(dex_program_to_type(&unknown), None);
    }

    #[test]
    fn test_dex_program_to_type_count() {
        // 19 base DEX programs + Pumpup = 20.
        // OnChain Labs DEX V2 is intentionally not mapped here (aggregator router,
        // not a directly-quotable DEX — see comment in build_program_map).
        let map = build_program_map();
        assert_eq!(map.len(), 20);
    }

    #[test]
    fn test_extract_pool_index_accounts_1() {
        // DEXes where pool address is at accounts[1]
        use crate::execution::amms::pumpfun_amm::{BUY_DISC, SELL_DISC};
        let anchor_swap = [248u8, 198, 158, 145, 225, 117, 135, 200];
        let anchor_swap_v2 = [43u8, 4, 237, 11, 26, 201, 30, 98];
        let cpmm_in = [143u8, 190, 90, 218, 196, 30, 51, 222];
        let lp_buy_in = [250u8, 234, 13, 123, 213, 156, 19, 236];
        let f = |pid: &Pubkey, data: &[u8]| swap_pool_index(pid, data);
        assert_eq!(f(&RAYDIUM_V4_PROG_ID, &[9, 0, 0]), Some((PoolType::RaydiumV4, 1)));
        assert_eq!(f(&RAYDIUM_V4_PROG_ID, &[11]), Some((PoolType::RaydiumV4, 1)));
        assert_eq!(f(&RAYDIUM_V4_PROG_ID, &[16, 0]), Some((PoolType::RaydiumV4, 1)), "swap_base_in_v2");
        assert_eq!(f(&RAYDIUM_V4_PROG_ID, &[17, 0]), Some((PoolType::RaydiumV4, 1)), "swap_base_out_v2");
        assert_eq!(f(&RAYDIUM_V4_PROG_ID, &[3]), None, "V4 deposit is not a swap");
        // Byreal (Raydium CLMM fork): [0] is the payer, [2] the pool — mainnet
        // swap_v3_dyn / swap_v2 on 27x6aSxc… and 5bWgqeKb…
        let byreal_v3_dyn = [0xe5u8, 0x2e, 0xd5, 0x84, 0x69, 0x28, 0x28, 0xe4];
        assert_eq!(f(&BYREAL_PROG_ID, &byreal_v3_dyn), Some((PoolType::Byreal, 2)));
        assert_eq!(f(&BYREAL_PROG_ID, &anchor_swap_v2), Some((PoolType::Byreal, 2)));
        assert_eq!(f(&BYREAL_PROG_ID, &anchor_swap), Some((PoolType::Byreal, 2)));
        assert_eq!(f(&BYREAL_PROG_ID, &[0x87, 0x80, 0x2f, 0x4d, 0x0f, 0x98, 0xf0, 0x31]), None, "open_position is not a swap");
        assert_eq!(f(&RAYDIUM_CPMM_PROG_ID, &cpmm_in), Some((PoolType::RaydiumCpmm, 3)));
        assert_eq!(f(&RAYDIUM_LP_PROG_ID, &lp_buy_in), Some((PoolType::RaydiumLp, 4)));
        assert_eq!(f(&RAYDIUM_CL_PROG_ID, &anchor_swap_v2), Some((PoolType::RaydiumCl, 2)));
        assert_eq!(f(&PUMP_FUN_PROG_ID, &BUY_DISC), Some((PoolType::PumpFun, 3)), "bonding curve, not the mint at [2]");
        assert_eq!(f(&PUMP_FUN_PROG_ID, &crate::execution::amms::pumpfun::BUY_EXACT_SOL_IN_DISC), Some((PoolType::PumpFun, 3)));
        // sell_v2 (live, 61xBNv9n…): the curve moves to [10]
        assert_eq!(f(&PUMP_FUN_PROG_ID, &[0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2]), Some((PoolType::PumpFun, 10)));
        assert_eq!(f(&PUMP_FUN_PROG_ID, &[0xd6, 0x90, 0x4c, 0xec, 0x5f, 0x8b, 0x31, 0xb4]), None, "create_v2 is not a swap");
        assert_eq!(f(&PUMP_FUN_AMM_PROG_ID, &SELL_DISC), Some((PoolType::PumpFunAmm, 0)));
        assert_eq!(f(&METEORA_PROG_ID, &anchor_swap), Some((PoolType::Meteora, 0)));
        assert_eq!(f(&METEORA_DLMM_PROG_ID, &anchor_swap), Some((PoolType::MeteoraDlmm, 0)));
        assert_eq!(f(&METEORA_DAMM_PROG_ID, &anchor_swap), Some((PoolType::MeteoraDamm, 1)));
        assert_eq!(f(&ORCA_PROG_ID, &anchor_swap), Some((PoolType::Orca, 2)));
        assert_eq!(f(&ORCA_PROG_ID, &anchor_swap_v2), Some((PoolType::Orca, 4)), "swapV2 moves the whirlpool to [4]");
        assert_eq!(f(&ORCA_PROG_ID, &[1, 2, 3, 4, 5, 6, 7, 8]), None, "unknown Orca ix is not a swap");
        assert_eq!(f(&FLUXBEAM_PROG_ID, &[1, 0]), Some((PoolType::FluxBeam, 0)));
        assert_eq!(f(&FLUXBEAM_PROG_ID, &[2, 0]), None, "token-swap deposit is not a swap");
        assert_eq!(f(&Pubkey::new_unique(), &anchor_swap), None, "unknown program");
        // the legacy per-program default still exists for callers without data
        assert_eq!(extract_pool_index(PoolType::PumpFunAmm), 0, "pAMM buy/sell: [0] pool ([3] is the base mint)");
    }

    #[test]
    fn pamm_only_buy_and_sell_are_swaps() {
        use crate::execution::amms::pumpfun_amm::{BUY_DISC, BUY_EXACT_QUOTE_IN_DISC, SELL_DISC};
        let mut buy = BUY_DISC.to_vec(); buy.extend([0u8; 17]);
        let mut sell = SELL_DISC.to_vec(); sell.extend([0u8; 16]);
        let mut beqi = BUY_EXACT_QUOTE_IN_DISC.to_vec(); beqi.extend([0u8; 17]);
        assert_eq!(refine_pool_type(PoolType::PumpFunAmm, &buy), Some(PoolType::PumpFunAmm));
        assert_eq!(refine_pool_type(PoolType::PumpFunAmm, &beqi), Some(PoolType::PumpFunAmm));
        assert_eq!(refine_pool_type(PoolType::PumpFunAmm, &sell), Some(PoolType::PumpFunAmm));
        // Anchor event self-CPI: `e445a52e51cb9a1d` + event bytes
        let event = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d, 1, 2, 3];
        assert_eq!(refine_pool_type(PoolType::PumpFunAmm, &event), None);
        assert_eq!(refine_pool_type(PoolType::PumpFunAmm, &[1, 2]), None, "short data");
        // other venues pass through untouched
        assert_eq!(refine_pool_type(PoolType::RaydiumCpmm, &event), Some(PoolType::RaydiumCpmm));
        let b58 = bs58::encode(&buy).into_string();
        assert_eq!(pool_type_for_ix_b58(&PUMP_FUN_AMM_PROG_ID, &b58), Some(PoolType::PumpFunAmm));
        let b58e = bs58::encode(&event).into_string();
        assert_eq!(pool_type_for_ix_b58(&PUMP_FUN_AMM_PROG_ID, &b58e), None);
    }

    #[test]
    fn test_extract_pool_index_all_variants_handled() {
        // Ensure every PoolType variant returns a valid index (no panics)
        let variants = [
            PoolType::Unknown,
            PoolType::RaydiumV4,
            PoolType::RaydiumCpmm,
            PoolType::RaydiumCl,
            PoolType::RaydiumLp,
            PoolType::PumpFun,
            PoolType::PumpFunAmm,
            PoolType::Meteora,
            PoolType::MeteoraDlmm,
            PoolType::MeteoraDamm,
            PoolType::MeteoraDbc,
            PoolType::Orca,
            PoolType::FluxBeam,
            PoolType::FlashTrade,
            PoolType::Byreal,
            PoolType::DefiTunaFusion,
            PoolType::DefiTunaPools,
            PoolType::Saros,
            PoolType::PancakeSwap,
            PoolType::Dooar,
        ];
        for v in &variants {
            let idx = extract_pool_index(*v);
            assert!(idx <= 3, "unexpected pool index {} for {:?}", idx, v);
        }
    }

    #[test]
    fn test_extract_mints_from_state_raydium_cpmm() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::RaydiumCpmm {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            token_0_vault: Pubkey::new_unique(),
            token_1_vault: Pubkey::new_unique(),
            token_0_mint: mint_a,
            token_1_mint: mint_b,
            observation: Pubkey::new_unique(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_pumpfun_amm() {
        let base = Pubkey::new_unique();
        let quote = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint: base,
            quote_mint: quote,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1000,
            quote_reserve: 2000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((base, quote)));
    }

    #[test]
    fn test_extract_mints_from_state_orca() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::Orca {
            whirlpool: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_current: 0,
            tick_spacing: 64,
            sqrt_price_x64: 0,
            liquidity: 0,
            fee_rate: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_meteora_damm() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_raydium_v4() {
        let (coin, pc) = (Pubkey::new_unique(), Pubkey::new_unique());
        let state = crate::pool::types::PoolState::RaydiumV4 {
            amm_id: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            coin_vault: Pubkey::new_unique(),
            pc_vault: Pubkey::new_unique(),
            coin_mint: coin,
            pc_mint: pc,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            need_take_pnl_coin: 0,
            need_take_pnl_pc: 0,
            status: 6,
            pool_open_time: 0,
        };
        assert_eq!(extract_mints_from_state(&state), Some((coin, pc)));
    }

    #[test]
    fn test_extract_mints_from_state_pumpfun_bonding() {
        let mint = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::PumpFun {
            global: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            mint,
            bonding_curve: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
            event_authority: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
            buyback_fee_recipient: Pubkey::new_unique(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint, SOL_NATIVE_MINT)));
    }

    #[test]
    fn test_extract_mints_from_state_meteora_dlmm() {
        let mint_x = Pubkey::new_unique();
        let mint_y = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::MeteoraDlmm {
            lb_pair: Pubkey::new_unique(),
            bin_array_bitmap_extension: Pubkey::new_unique(),
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            token_x_mint: mint_x,
            token_y_mint: mint_y,
            oracle: Pubkey::new_unique(),
            host_fee_in: Pubkey::new_unique(),
            event_authority: Pubkey::new_unique(),
            bin_arrays: vec![],
            pair: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_x, mint_y)));
    }

    #[test]
    fn test_extract_mints_from_state_all_variants() {
        // Verify that every PoolState variant is handled (no panics)
        // Build a representative state for each variant and verify it returns Some or None
        let pk = || Pubkey::new_unique();

        let states = vec![
            crate::pool::types::PoolState::RaydiumV4 {
                amm_id: pk(), authority: pk(), coin_vault: pk(), pc_vault: pk(),
                coin_mint: pk(), pc_mint: pk(), swap_fee_numerator: 25, swap_fee_denominator: 10_000,
                need_take_pnl_coin: 0, need_take_pnl_pc: 0, status: 6, pool_open_time: 0,
            },
            crate::pool::types::PoolState::RaydiumCpmm {
                pool: pk(), authority: pk(), config: pk(),
                token_0_vault: pk(), token_1_vault: pk(),
                token_0_mint: pk(), token_1_mint: pk(), observation: pk(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        },
            crate::pool::types::PoolState::RaydiumClmm {
                pool: pk(), amm_config: pk(), observation: pk(),
                token_vault_0: pk(), token_vault_1: pk(),
                tick_array_0: pk(), tick_array_1: pk(), tick_array_2: pk(),
                token_mint_0: pk(), token_mint_1: pk(),
                tick_current: 0, tick_spacing: 1,
                sqrt_price_x64: 0, liquidity: 0, fee_rate: 0, fee_ext: Default::default(),
            },
            crate::pool::types::PoolState::Orca {
                whirlpool: pk(), token_vault_a: pk(), token_vault_b: pk(),
                oracle: pk(), token_mint_a: pk(), token_mint_b: pk(),
                tick_current: 0, tick_spacing: 64,
                sqrt_price_x64: 0, liquidity: 0, fee_rate: 0,
            },
            crate::pool::types::PoolState::MeteoraDamm {
                pool: pk(), token_a_vault: pk(), token_b_vault: pk(),
                token_a_mint: pk(), token_b_mint: pk(),
                liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
            },
        ];

        for state in &states {
            // Just verify no panic
            let _ = extract_mints_from_state(state);
        }
    }

    #[test]
    fn test_registry_contains() {
        let registry = PoolRegistry::new();
        let addr = Pubkey::new_unique();
        assert!(!registry.contains(&addr));

        registry.add(PoolEntry {
            address: addr,
            pool_type: PoolType::Orca,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        });
        assert!(registry.contains(&addr));
    }

    #[test]
    fn test_registry_contains_after_remove() {
        let registry = PoolRegistry::new();
        let addr = Pubkey::new_unique();

        registry.add(PoolEntry {
            address: addr,
            pool_type: PoolType::Orca,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        });
        assert!(registry.contains(&addr));

        registry.remove(&addr);
        assert!(!registry.contains(&addr));
    }

    #[test]
    fn test_build_program_map_unique_keys() {
        let map = build_program_map();
        // All keys must be unique (HashMap guarantees this, but let's verify count).
        // 19 base + Pumpup = 20. OnChain Labs is excluded by design (aggregator).
        assert_eq!(map.len(), 20);
        // All values should be non-Unknown
        for (_, pt) in &map {
            assert_ne!(*pt, PoolType::Unknown);
        }
    }

    #[test]
    fn test_extract_mints_from_state_raydium_lp() {
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::RaydiumLp {
            pool_state: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            config_id: Pubkey::new_unique(),
            platform_id: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((base_mint, quote_mint)));
    }

    #[test]
    fn test_extract_mints_from_state_meteora_dbc() {
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::MeteoraDbc {
            pool: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            pool_authority: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            curve: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((base_mint, quote_mint)));
    }

    #[test]
    fn test_extract_mints_from_state_fluxbeam() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::FluxBeam {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            pool_token_program: Pubkey::new_unique(),
            fees: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_saros() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::Saros {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            fees: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_dooar() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::Dooar {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            fees: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_pancakeswap() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::PancakeSwap {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_current: 0,
            tick_spacing: 1,
            sqrt_price_x64: 0, liquidity: 0, fee_rate: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_defituna_fusion() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::DefiTunaFusion {
            pool: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_spacing: 1,
            tick_current_index: 0,
            sqrt_price_x64: 0, liquidity: 0, fee_rate: 0,
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_byreal() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::Byreal {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_current: 0,
            tick_spacing: 1,
            sqrt_price_x64: 0, liquidity: 0, fee_rate: 2500, fee: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_a, mint_b)));
    }

    #[test]
    fn test_extract_mints_from_state_raydium_clmm() {
        let mint_0 = Pubkey::new_unique();
        let mint_1 = Pubkey::new_unique();
        let state = crate::pool::types::PoolState::RaydiumClmm {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            tick_array_0: Pubkey::new_unique(),
            tick_array_1: Pubkey::new_unique(),
            tick_array_2: Pubkey::new_unique(),
            token_mint_0: mint_0,
            token_mint_1: mint_1,
            tick_current: 0,
            tick_spacing: 1,
            sqrt_price_x64: 0, liquidity: 0, fee_rate: 0, fee_ext: Default::default(),
        };
        let result = extract_mints_from_state(&state);
        assert_eq!(result, Some((mint_0, mint_1)));
    }
}
