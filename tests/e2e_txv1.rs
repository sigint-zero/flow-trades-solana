//! LIVE: Solana transaction format v1 (SIMD-0385) on the RPC paths.
//!
//! txv1 is active on mainnet (feature `txv1aq4pp…`), so every recent block
//! carries v1 transactions. These tests prove, against the real RPC, that:
//!
//! 1. `getBlock` with the crate-wide `MAX_SUPPORTED_TX_VERSION` returns a
//!    block that deserializes with v1 transactions present, and both block
//!    consumers (swap-stream parser, pool-discovery scanner) run over it —
//!    including v1 transactions that hit DEX programs.
//! 2. NEGATIVE CONTROL: the same block requested one version lower is refused
//!    WHOLE with `-32015`.
//! 3. `blockSubscribe` over WebSocket (the no-Geyser fallback) delivers
//!    non-null blocks containing v1 transactions.
//!
//! Requires `RPC_URL` or `SOL_HTTPS_ENDPOINT` (HTTPS; the WS URL is derived).
//! Skips loudly when unset.

use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcBlockConfig, RpcBlockSubscribeFilter};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::transaction::TransactionVersion;
use solana_transaction_status_client_types::{
    EncodedTransaction, TransactionDetails, UiConfirmedBlock, UiMessage, UiTransactionEncoding,
};

use flow_trades::enrichment::PriceOracle;
use flow_trades::pool::registry::PoolRegistry;
use flow_trades::stream::block_scanner::extract_candidates_from_block;
use flow_trades::stream::swap_stream::{parse_swaps_from_block, Swap};
use flow_trades::stream::tx_version::{
    self, is_version_refusal, JSON_RPC_UNSUPPORTED_TX_VERSION, MAX_SUPPORTED_TX_VERSION,
};

fn rpc_url() -> Option<String> {
    std::env::var("RPC_URL")
        .or_else(|_| std::env::var("SOL_HTTPS_ENDPOINT"))
        .ok()
        .filter(|u| !u.is_empty())
}

fn rpc() -> Option<RpcClient> {
    let url = rpc_url()?;
    Some(RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()))
}

macro_rules! require_rpc {
    () => {
        match rpc() {
            Some(r) => r,
            None => {
                eprintln!("SKIPPED: RPC_URL / SOL_HTTPS_ENDPOINT not set");
                return;
            }
        }
    };
}

fn is_v1(tx: &solana_transaction_status_client_types::EncodedTransactionWithStatusMeta) -> bool {
    matches!(tx.version, Some(TransactionVersion::Number(1)))
}

/// Static account keys of a JSON-encoded transaction (v1 has no lookups).
fn account_keys(tx: &solana_transaction_status_client_types::EncodedTransactionWithStatusMeta) -> Vec<String> {
    match &tx.transaction {
        EncodedTransaction::Json(ui) => match &ui.message {
            UiMessage::Raw(raw) => raw.account_keys.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

const DEX_PROGRAMS: &[&str] = &[
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",  // pump.fun AMM
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // pump.fun bonding
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium V4
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  // Meteora DLMM
];

/// True when a top-level or inner instruction targets one of `DEX_PROGRAMS`.
fn invokes_dex(tx: &solana_transaction_status_client_types::EncodedTransactionWithStatusMeta) -> bool {
    use solana_transaction_status_client_types::{option_serializer::OptionSerializer, UiInstruction};
    let EncodedTransaction::Json(ui) = &tx.transaction else { return false };
    let UiMessage::Raw(raw) = &ui.message else { return false };
    let is_dex = |idx: u8| raw.account_keys.get(idx as usize).is_some_and(|k| DEX_PROGRAMS.contains(&k.as_str()));
    if raw.instructions.iter().any(|ix| is_dex(ix.program_id_index)) {
        return true;
    }
    let Some(meta) = &tx.meta else { return false };
    let OptionSerializer::Some(inner) = &meta.inner_instructions else { return false };
    inner.iter().flat_map(|ii| ii.instructions.iter()).any(|ix| match ix {
        UiInstruction::Compiled(c) => is_dex(c.program_id_index),
        _ => false,
    })
}

/// Walk back from the tip to a block with v1 transactions — preferring one
/// where a v1 transaction invokes a DEX program, so the swap-stream assertion
/// has something to bite on (v1 adoption by DEX bots is still partial).
async fn find_v1_block(rpc: &RpcClient) -> (u64, UiConfirmedBlock) {
    let tip = rpc.get_slot_with_commitment(CommitmentConfig::confirmed()).await.expect("getSlot");
    let mut fallback: Option<(u64, UiConfirmedBlock)> = None;
    for slot in (tip.saturating_sub(60)..tip.saturating_sub(3)).rev() {
        let cfg = tx_version::block_config(UiTransactionEncoding::Json, CommitmentConfig::confirmed());
        let Ok(block) = rpc.get_block_with_config(slot, cfg).await else { continue }; // skipped slot
        let txs = block.transactions.as_deref().unwrap_or(&[]);
        if !txs.iter().any(is_v1) {
            continue;
        }
        let v1_dex = txs.iter().any(|t| is_v1(t) && t.meta.as_ref().is_some_and(|m| m.err.is_none()) && invokes_dex(t));
        if v1_dex {
            return (slot, block);
        }
        if fallback.is_none() {
            fallback = Some((slot, block));
        }
    }
    fallback.expect("no block with a v1 transaction in the last 60 slots — is txv1 active on this cluster?")
}

fn swap_sink() -> (tokio::sync::broadcast::Sender<Arc<Swap>>, tokio::sync::broadcast::Receiver<Arc<Swap>>) {
    tokio::sync::broadcast::channel(65_536)
}

#[tokio::test]
async fn test_01_getblock_v1_deserializes_and_both_consumers_run() {
    let rpc = require_rpc!();
    let (slot, block) = find_v1_block(&rpc).await;
    let txs = block.transactions.as_ref().unwrap();
    let n_v1 = txs.iter().filter(|t| is_v1(t)).count();
    let n_v0 = txs.iter().filter(|t| matches!(t.version, Some(TransactionVersion::Number(0)))).count();
    let n_legacy = txs.iter().filter(|t| matches!(t.version, Some(TransactionVersion::Legacy(_)))).count();
    eprintln!("slot {slot}: {} txs — legacy {n_legacy}, v0 {n_v0}, v1 {n_v1}", txs.len());
    assert!(n_v1 > 0);
    assert_eq!(n_v1 + n_v0 + n_legacy, txs.len(), "every transaction carries a known version");

    // Every v1 tx decoded to a Raw message with inline keys (v1 has no lookup tables).
    for t in txs.iter().filter(|t| is_v1(t)) {
        let keys = account_keys(t);
        assert!(!keys.is_empty(), "v1 tx without account keys: {:?}", t.transaction);
        if let EncodedTransaction::Json(ui) = &t.transaction {
            if let UiMessage::Raw(raw) = &ui.message {
                assert!(raw.address_table_lookups.as_ref().map_or(true, |l| l.is_empty()));
                assert!(!raw.instructions.is_empty());
            }
        }
    }

    // Successful v1 transactions that INVOKE a DEX program (top-level or
    // inner) — the ones a version-0 request makes invisible by killing the
    // whole block. Merely listing a DEX program as an account is not an
    // invocation (aggregators pass venues they do not route through).
    let v1_dex_sigs: Vec<String> = txs
        .iter()
        .filter(|t| is_v1(t) && t.meta.as_ref().is_some_and(|m| m.err.is_none()))
        .filter(|t| invokes_dex(t))
        .filter_map(|t| match &t.transaction {
            EncodedTransaction::Json(ui) => ui.signatures.first().cloned(),
            _ => None,
        })
        .collect();
    eprintln!("v1 txs invoking DEX programs (successful): {}", v1_dex_sigs.len());

    // Consumer 1: swap-stream parser over the whole block.
    let oracle = PriceOracle::new();
    let (tx, mut rx) = swap_sink();
    let emitted = parse_swaps_from_block(&block, &oracle, &tx);
    eprintln!("swap-stream emitted {emitted} swaps from slot {slot}");
    assert!(emitted > 0, "a mainnet block with DEX activity must yield swaps");
    let mut from_v1 = 0usize;
    while let Ok(s) = rx.try_recv() {
        if v1_dex_sigs.contains(&s.signature) {
            from_v1 += 1;
        }
    }
    eprintln!("swaps emitted FROM v1 transactions: {from_v1}");
    // One block is a small sample: a v1 DEX invocation may be a non-swap
    // instruction (create, deposit, event). With several, some must be swaps.
    // The WS test below asserts the same over six blocks.
    if v1_dex_sigs.len() >= 3 {
        assert!(from_v1 > 0, "{} v1 DEX transactions but none produced a swap", v1_dex_sigs.len());
    }

    // Consumer 2: pool-discovery scanner (fresh registry → everything is a candidate).
    let registry = Arc::new(PoolRegistry::new());
    let candidates = extract_candidates_from_block(&block, &registry);
    eprintln!("discovery candidates from slot {slot}: {}", candidates.len());
    assert!(!candidates.is_empty());
}

#[tokio::test]
async fn test_02_negative_control_one_version_lower_is_refused_whole() {
    let rpc = require_rpc!();
    let (slot, _) = find_v1_block(&rpc).await;
    // What the pre-fix code asked for: one below the current maximum (= 0).
    let stale = RpcBlockConfig {
        encoding: Some(UiTransactionEncoding::Json),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(false),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TX_VERSION - 1),
    };
    let err = rpc
        .get_block_with_config(slot, stale)
        .await
        .expect_err("a v1-bearing block must be refused at the stale version");
    eprintln!("slot {slot} at version {}: {err}", MAX_SUPPORTED_TX_VERSION - 1);
    assert!(is_version_refusal(&err), "expected JSON-RPC {JSON_RPC_UNSUPPORTED_TX_VERSION}, got: {err}");
}

fn http_to_ws(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url.to_string()
    }
}

#[tokio::test]
async fn test_03_ws_block_subscribe_delivers_v1_blocks() {
    use futures::StreamExt;
    use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient;

    let Some(url) = rpc_url() else {
        eprintln!("SKIPPED: RPC_URL / SOL_HTTPS_ENDPOINT not set");
        return;
    };
    let ws_url = http_to_ws(&url);
    let pubsub = PubsubClient::new(&ws_url).await.expect("WS connect");
    let cfg = tx_version::block_subscribe_config(UiTransactionEncoding::Json);
    let (mut stream, _unsub) = pubsub
        .block_subscribe(RpcBlockSubscribeFilter::All, Some(cfg))
        .await
        .expect("blockSubscribe");

    let oracle = PriceOracle::new();
    let (tx, mut rx) = swap_sink();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    let (mut blocks, mut null_blocks, mut v1_total, mut swaps_total) = (0usize, 0usize, 0usize, 0usize);
    let mut v1_dex_sigs: Vec<String> = Vec::new();
    while blocks + null_blocks < 6 {
        let next = tokio::time::timeout_at(deadline, stream.next()).await;
        let Ok(Some(n)) = next else { break };
        match n.value.block {
            Some(block) => {
                blocks += 1;
                let txs = block.transactions.as_deref().unwrap_or(&[]);
                let n_v1 = txs.iter().filter(|t| is_v1(t)).count();
                v1_total += n_v1;
                v1_dex_sigs.extend(
                    txs.iter()
                        .filter(|t| is_v1(t) && t.meta.as_ref().is_some_and(|m| m.err.is_none()) && invokes_dex(t))
                        .filter_map(|t| match &t.transaction {
                            EncodedTransaction::Json(ui) => ui.signatures.first().cloned(),
                            _ => None,
                        }),
                );
                let emitted = parse_swaps_from_block(&block, &oracle, &tx);
                swaps_total += emitted;
                eprintln!("ws block slot {}: {} txs, {n_v1} v1, {emitted} swaps", n.value.slot, txs.len());
            }
            None => {
                null_blocks += 1;
                eprintln!("ws block slot {}: block=null (err={:?})", n.value.slot, n.value.err);
            }
        }
    }
    let mut from_v1 = 0usize;
    while let Ok(s) = rx.try_recv() {
        if v1_dex_sigs.contains(&s.signature) {
            from_v1 += 1;
        }
    }
    eprintln!(
        "ws: {blocks} blocks, {null_blocks} null, {v1_total} v1 txs, {swaps_total} swaps, {} v1 DEX txs → {from_v1} swaps from v1",
        v1_dex_sigs.len()
    );
    assert!(blocks >= 3, "expected ≥3 blocks over WS within 90s, got {blocks} (+{null_blocks} null)");
    assert_eq!(null_blocks, 0, "a null block over WS is the version-refusal symptom");
    assert!(v1_total > 0, "no v1 transactions seen over WS in {blocks} blocks");
    assert!(swaps_total > 0);
    if v1_dex_sigs.len() >= 3 {
        assert!(from_v1 > 0, "{} v1 DEX transactions over {blocks} blocks but none produced a swap", v1_dex_sigs.len());
    }
}
