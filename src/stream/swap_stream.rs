//! Live swap stream — parses confirmed DEX swaps off the block stream and
//! broadcasts them to subscribers.
//!
//! ### Parser strategy: balance-delta aggregation
//! Rather than per-DEX bytecode parsers, we use the
//! `pre_token_balances` / `post_token_balances` and `pre_balances` /
//! `post_balances` arrays already attached to every confirmed tx. For
//! the fee payer, we compute per-mint signed deltas; the most-negative
//! mint is the input, the most-positive is the output. This handles
//! SPL Token, Token-2022 (including transfer-fee extensions), wrapped
//! SOL, and native-SOL paths (PumpFun bonding, Pumpup bonding) with
//! one code path. Pool address comes from the per-DEX
//! `extract_pool_index` already used by the discovery scanner.
//!
//! ### Fan-out
//! Parsed swaps are sent into a `tokio::sync::broadcast` channel. Slow
//! consumers lag — the broadcast channel drops oldest messages for that
//! subscriber rather than blocking the parsing pipeline.

use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, EncodedTransaction, UiInstruction, UiMessage,
};
use yellowstone_grpc_proto::geyser::SubscribeUpdateBlock;
use yellowstone_grpc_proto::solana::storage::confirmed_block::{
    InnerInstruction as YsInnerInstruction, TokenBalance as YsTokenBalance,
    Transaction as YsTransaction, TransactionStatusMeta as YsMeta,
};

use crate::constants::{ONCHAIN_LABS_DEX_V2_PROG_ID, PYUSD_MINT, SOL_NATIVE_MINT, USDC_MINT, USDT_MINT};
use crate::enrichment::PriceOracle;
use crate::stream::block_scanner::{
    swap_pool_index_b58,
};
use crate::quote::router::label_for_pool_type;
use crate::pool::types::PoolType;

/// One side of a swap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapSide {
    pub mint: String,
    /// Atomic amount as a string (avoids JSON-number precision loss).
    pub amount: String,
    pub decimals: u8,
    /// `amount / 10^decimals` formatted as a decimal string.
    pub ui_amount: String,
}

impl SwapSide {
    pub fn new(mint: Pubkey, amount: u64, decimals: u8) -> Self {
        let ui = if decimals == 0 {
            format!("{amount}")
        } else {
            let scale = 10u128.pow(decimals as u32) as f64;
            format!("{:.*}", decimals as usize, amount as f64 / scale)
        };
        SwapSide {
            mint: mint.to_string(),
            amount: amount.to_string(),
            decimals,
            ui_amount: ui,
        }
    }
}

/// One observed swap. Wire format for `/swap-stream` subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Swap {
    /// Discriminator field for the WS wire format. Always `"swap"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub signature: String,
    pub slot: u64,
    /// Block time as Unix epoch seconds. Optional — RPC sometimes omits.
    pub block_time: Option<i64>,
    /// Human-readable DEX label (matches `program_id_to_label`).
    pub dex: String,
    /// Pool address (or pool_sol_account for bonding curves).
    pub pool: String,
    /// Fee payer of the underlying transaction (the "user" doing the swap).
    pub user: String,
    pub input: SwapSide,
    pub output: SwapSide,
    /// `output_ui / input_ui` — output-token per input-token.
    pub price_native: Option<String>,
    /// `input_ui / output_ui` — input-token per output-token.
    pub price_native_inverted: Option<String>,
    /// USD per output token (one-sided). Null if neither side is a known
    /// quote mint (SOL/USDC/USDT/PYUSD) and SOL price isn't available.
    pub price_usd: Option<String>,
    /// Total trade size in USD.
    pub amount_usd: Option<String>,
}

/// Subscriber filter (all fields optional, AND'd).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwapFilter {
    /// DEX labels to allow (empty / missing = allow all).
    #[serde(default)]
    pub dex: Vec<String>,
    /// Match swaps where input or output equals this mint.
    pub mint: Option<String>,
    /// Match swaps for this pool only.
    pub pool: Option<String>,
    /// Minimum trade size in USD (filters out dust). Swaps without
    /// `amount_usd` (no price reference) are dropped when this is set.
    pub min_amount_usd: Option<f64>,
}

impl SwapFilter {
    pub fn matches(&self, swap: &Swap) -> bool {
        if !self.dex.is_empty() && !self.dex.iter().any(|d| d == &swap.dex) {
            return false;
        }
        if let Some(ref m) = self.mint {
            if &swap.input.mint != m && &swap.output.mint != m {
                return false;
            }
        }
        if let Some(ref p) = self.pool {
            if &swap.pool != p {
                return false;
            }
        }
        if let Some(min) = self.min_amount_usd {
            match swap.amount_usd.as_ref().and_then(|s| s.parse::<f64>().ok()) {
                Some(v) if v >= min => (),
                _ => return false,
            }
        }
        true
    }
}

const STABLE_MINTS: [Pubkey; 3] = [USDC_MINT, USDT_MINT, PYUSD_MINT];

fn is_stable(mint: &Pubkey) -> bool {
    STABLE_MINTS.contains(mint)
}

/// Normalized per-tx facts the parser actually needs. Both the RPC
/// `UiConfirmedBlock` path and the Yellowstone gRPC `SubscribeUpdateBlock`
/// path build this then call into `parse_one_tx`. Everything in here is
/// already-validated (no failed txs, no votes).
struct TxFacts<'a> {
    sig: String,
    block_time: Option<i64>,
    /// Full account-key list (static + ALT-loaded), as Pubkeys.
    keys: &'a [Pubkey],
    fee_payer: Pubkey,
    /// Pre-computed per-mint signed atomic deltas for the fee payer.
    deltas: Deltas,
    /// Every token-balance observation in the tx (vaults included).
    obs: Vec<BalanceObs>,
    /// Signed lamport delta per account index (native SOL legs of bonding curves).
    lamport_deltas: Vec<i128>,
    /// (program_id, accounts indices, ix_data_b58) for every ix
    /// (top-level first, then inner) — bytes already converted.
    ixs: Vec<IxView<'a>>,
}

struct IxView<'a> {
    program_id: Pubkey,
    accounts: &'a [u8],
    /// Base58-encoded ix data — we only need the first 8 bytes for
    /// disambiguation, but the whole thing is already small.
    data_b58: String,
}

/// Walk every (top-level + inner) instruction in a tx. For each ix that
/// targets a known DEX program, run the balance-delta parser and emit
/// at most one Swap per (signature, pool). Swaps are sent into the
/// provided broadcast sender — slow subscribers lag, never block.
///
/// `oracle` provides SOL/USD for USD enrichment when one side of the
/// swap is SOL. Stable mints are treated as $1.00. If neither side is
/// SOL nor a stable, USD fields stay null.
pub fn parse_swaps_from_block(
    block: &solana_transaction_status_client_types::UiConfirmedBlock,
    oracle: &PriceOracle,
    out: &tokio::sync::broadcast::Sender<Arc<Swap>>,
) -> usize {
    let block_time = block.block_time;
    let txs = match &block.transactions {
        Some(txs) => txs,
        None => return 0,
    };
    let slot = block.parent_slot.saturating_add(1);
    let mut emitted = 0usize;

    for encoded in txs {
        // Skip failed txs.
        let meta = match &encoded.meta {
            Some(m) if m.err.is_none() => m,
            _ => continue,
        };

        let sig = match &encoded.transaction {
            EncodedTransaction::Json(ui_tx) => {
                ui_tx.signatures.first().cloned().unwrap_or_default()
            }
            _ => continue,
        };

        let (account_keys, top_ixs) = match &encoded.transaction {
            EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                UiMessage::Raw(raw) => (raw.account_keys.clone(), raw.instructions.clone()),
                UiMessage::Parsed(_) => continue,
            },
            _ => continue,
        };
        let mut keys_str: Vec<String> = account_keys;
        if let OptionSerializer::Some(la) = &meta.loaded_addresses {
            keys_str.extend(la.writable.iter().cloned());
            keys_str.extend(la.readonly.iter().cloned());
        }
        let keys: Vec<Pubkey> = keys_str
            .iter()
            .filter_map(|s| Pubkey::from_str(s).ok())
            .collect();
        if keys.len() != keys_str.len() {
            continue; // bad key encoding somewhere; skip rather than misalign
        }
        let fee_payer = match keys.first() {
            Some(p) => *p,
            None => continue,
        };

        // Build observations from pre/post_token_balances + native lamports.
        let mut obs: Vec<BalanceObs> = Vec::new();
        if let OptionSerializer::Some(pre) = &meta.pre_token_balances {
            for tb in pre {
                if let Some(o) = ui_balance_to_obs_pre(tb) {
                    obs.push(o);
                }
            }
        }
        if let OptionSerializer::Some(post) = &meta.post_token_balances {
            for tb in post {
                merge_obs_post(&mut obs, ui_balance_to_obs_post(tb));
            }
        }
        let deltas = compute_fee_payer_deltas(
            &obs,
            &fee_payer,
            meta.pre_balances.first().copied().unwrap_or(0),
            meta.post_balances.first().copied().unwrap_or(0),
            meta.fee,
        );
        if deltas.is_empty() {
            continue;
        }

        // Build IxView vec (top-level + inner)
        let mut ixs: Vec<IxView> = Vec::with_capacity(top_ixs.len() + 8);
        for ix in &top_ixs {
            if let Some(view) = ui_compiled_to_view(ix, &keys) {
                ixs.push(view);
            }
        }
        if let OptionSerializer::Some(inner_list) = &meta.inner_instructions {
            for ii in inner_list {
                for ux in &ii.instructions {
                    if let UiInstruction::Compiled(c) = ux {
                        if let Some(view) = ui_compiled_to_view(c, &keys) {
                            ixs.push(view);
                        }
                    }
                }
            }
        }

        let lamport_deltas: Vec<i128> = meta
            .pre_balances
            .iter()
            .zip(meta.post_balances.iter())
            .map(|(a, b)| *b as i128 - *a as i128)
            .collect();
        let facts = TxFacts {
            sig,
            block_time,
            keys: &keys,
            fee_payer,
            deltas,
            obs,
            lamport_deltas,
            ixs,
        };
        emitted += parse_one_tx(&facts, slot, oracle, out);
    }

    emitted
}

/// Yellowstone gRPC variant — same parsing logic, different proto types.
pub fn parse_swaps_from_yellowstone_block(
    block: &SubscribeUpdateBlock,
    oracle: &PriceOracle,
    out: &tokio::sync::broadcast::Sender<Arc<Swap>>,
) -> usize {
    let block_time = block.block_time.as_ref().map(|t| t.timestamp);
    let slot = block.slot;
    let mut emitted = 0usize;

    for tx_info in &block.transactions {
        if tx_info.is_vote {
            continue;
        }
        let tx: &YsTransaction = match &tx_info.transaction {
            Some(t) => t,
            None => continue,
        };
        let meta: &YsMeta = match &tx_info.meta {
            Some(m) if m.err.is_none() => m,
            _ => continue,
        };
        let msg = match &tx.message {
            Some(m) => m,
            None => continue,
        };

        // Account keys: static + ALT-loaded
        let mut keys: Vec<Pubkey> = Vec::with_capacity(
            msg.account_keys.len()
                + meta.loaded_writable_addresses.len()
                + meta.loaded_readonly_addresses.len(),
        );
        for k in &msg.account_keys {
            if let Ok(b) = <[u8; 32]>::try_from(k.as_slice()) {
                keys.push(Pubkey::new_from_array(b));
            } else {
                keys.clear();
                break;
            }
        }
        if keys.is_empty() {
            continue;
        }
        for k in &meta.loaded_writable_addresses {
            if let Ok(b) = <[u8; 32]>::try_from(k.as_slice()) {
                keys.push(Pubkey::new_from_array(b));
            }
        }
        for k in &meta.loaded_readonly_addresses {
            if let Ok(b) = <[u8; 32]>::try_from(k.as_slice()) {
                keys.push(Pubkey::new_from_array(b));
            }
        }

        let fee_payer = match keys.first() {
            Some(p) => *p,
            None => continue,
        };
        let sig = bs58::encode(&tx_info.signature).into_string();

        // Build observations
        let mut obs: Vec<BalanceObs> = Vec::new();
        for tb in &meta.pre_token_balances {
            if let Some(o) = ys_balance_to_obs_pre(tb) {
                obs.push(o);
            }
        }
        for tb in &meta.post_token_balances {
            merge_obs_post(&mut obs, ys_balance_to_obs_post(tb));
        }
        let deltas = compute_fee_payer_deltas(
            &obs,
            &fee_payer,
            meta.pre_balances.first().copied().unwrap_or(0),
            meta.post_balances.first().copied().unwrap_or(0),
            meta.fee,
        );
        if deltas.is_empty() {
            continue;
        }

        // IxView
        let mut ixs: Vec<IxView> = Vec::with_capacity(msg.instructions.len() + 8);
        for ix in &msg.instructions {
            if let Some(view) = ys_compiled_to_view(ix, &keys) {
                ixs.push(view);
            }
        }
        for ii in &meta.inner_instructions {
            for inner in &ii.instructions {
                if let Some(view) = ys_inner_to_view(inner, &keys) {
                    ixs.push(view);
                }
            }
        }

        let lamport_deltas: Vec<i128> = meta
            .pre_balances
            .iter()
            .zip(meta.post_balances.iter())
            .map(|(a, b)| *b as i128 - *a as i128)
            .collect();
        let facts = TxFacts {
            sig,
            block_time,
            keys: &keys,
            fee_payer,
            deltas,
            obs,
            lamport_deltas,
            ixs,
        };
        emitted += parse_one_tx(&facts, slot, oracle, out);
    }

    emitted
}

/// Shared core: dispatch every IxView and emit one Swap per pool.
///
/// Amounts come from the pool's own vault balance deltas when the instruction's
/// accounts let us identify them (leg-accurate, so a 2-hop route yields two
/// swaps with their real legs). When they cannot be identified the fee payer's
/// net deltas are used — once per transaction, never again for a second pool
/// (otherwise a routed swap is counted once per pool it touches).
fn parse_one_tx(
    facts: &TxFacts,
    slot: u64,
    oracle: &PriceOracle,
    out: &tokio::sync::broadcast::Sender<Arc<Swap>>,
) -> usize {
    let mut seen_pools: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let mut emitted = 0usize;
    let mut fallback_used = false;

    // How many recognised swap instructions this tx carries: with exactly one,
    // the fee payer's input delta is what the trader paid in total, which is
    // the right base for the observed-fee estimate.
    let n_swaps = facts
        .ixs
        .iter()
        .filter(|ix| swap_pool_index_b58(&ix.program_id, &ix.data_b58).is_some() || ix.program_id == ONCHAIN_LABS_DEX_V2_PROG_ID)
        .count();

    for ix in &facts.ixs {
        // OnChain Labs DEX V2: an aggregator into private venues — no pool to
        // quote, but the trade is real. Streamed under the program id with the
        // fee payer's net legs.
        if ix.program_id == ONCHAIN_LABS_DEX_V2_PROG_ID {
            if fallback_used || !seen_pools.insert(ONCHAIN_LABS_DEX_V2_PROG_ID) {
                continue;
            }
            if let Some(swap) = build_swap_from_deltas(
                &facts.sig, slot, facts.block_time, "OnChain Labs DEX V2", ONCHAIN_LABS_DEX_V2_PROG_ID, facts.fee_payer, &facts.deltas, oracle,
            ) {
                fallback_used = true;
                if out.send(Arc::new(swap)).is_ok() {
                    emitted += 1;
                }
            }
            continue;
        }
        let (pt, pool_idx) = match swap_pool_index_b58(&ix.program_id, &ix.data_b58) {
            Some(x) => x,
            None => continue,
        };
        let pool = match ix.accounts.get(pool_idx).and_then(|i| facts.keys.get(*i as usize)) {
            Some(p) => *p,
            None => continue,
        };
        if !seen_pools.insert(pool) {
            continue;
        }
        let dex = label_for_pool_type(pt);
        let swap = match vault_legs(pool, ix.accounts, facts) {
            Some(legs) => {
                if is_cp_for_fee_estimate(pt) {
                    // The trader's total input when this is the only swap in the
                    // tx (fees skimmed before the vault count), else what reached
                    // the vault (a lower bound).
                    let paid_in = if n_swaps == 1 {
                        facts
                            .deltas
                            .entries
                            .iter()
                            .find(|(m, d, _)| *m == legs.in_mint && *d < 0)
                            .map(|(_, d, _)| d.unsigned_abs())
                            .filter(|p| *p >= legs.in_amount as u128)
                            .unwrap_or(legs.in_amount as u128)
                    } else {
                        legs.in_amount as u128
                    };
                    if let Some(ppm) = crate::stream::observed_fees::implied_fee_ppm(
                        paid_in, legs.out_amount as u128, legs.r_in_pre, legs.r_out_pre,
                    ) {
                        crate::stream::observed_fees::record(pool, ppm);
                    }
                }
                build_swap(&facts.sig, slot, facts.block_time, dex, pool, facts.fee_payer,
                    legs.in_mint, legs.in_amount, legs.in_dec, legs.out_mint, legs.out_amount, legs.out_dec, oracle)
            }
            None => {
                if fallback_used {
                    continue;
                }
                match build_swap_from_deltas(&facts.sig, slot, facts.block_time, dex, pool, facts.fee_payer, &facts.deltas, oracle) {
                    Some(sw) => {
                        fallback_used = true;
                        Some(sw)
                    }
                    None => None,
                }
            }
        };
        if let Some(swap) = swap {
            if out.send(Arc::new(swap)).is_ok() {
                emitted += 1;
            }
        }
    }
    emitted
}

/// Venues whose swap is x·y=k on the two vault balances, so an observed swap
/// yields an effective fee the quoter can reuse.
fn is_cp_for_fee_estimate(pt: PoolType) -> bool {
    matches!(
        pt,
        PoolType::PumpFunAmm
            | PoolType::RaydiumCpmm
            | PoolType::MeteoraDamm
            | PoolType::Meteora
            | PoolType::FluxBeam
            | PoolType::Saros
            | PoolType::Dooar
            | PoolType::Pumpup
    )
}

/// The two legs of one pool's swap, read from its own vault balances.
struct VaultLegs {
    in_mint: Pubkey,
    in_amount: u64,
    in_dec: u8,
    /// Vault balance BEFORE the swap on the input side.
    r_in_pre: u128,
    out_mint: Pubkey,
    out_amount: u64,
    out_dec: u8,
    r_out_pre: u128,
}

/// Identify the pool's vaults among the instruction's accounts: token accounts
/// whose owner is the pool itself, or (Raydium CPMM/LP, Meteora DAMM/DBC, the
/// SPL token-swap forks) an authority that is itself one of the instruction's
/// accounts — never the fee payer. Exactly one vault must have gained and one
/// lost, on different mints. A bonding curve's native-SOL side is taken from
/// the pool account's lamport delta when only the token vault is found.
fn vault_legs(pool: Pubkey, ix_accounts: &[u8], facts: &TxFacts) -> Option<VaultLegs> {
    let idx_set: std::collections::HashSet<u32> = ix_accounts.iter().map(|i| *i as u32).collect();
    let key_set: std::collections::HashSet<Pubkey> =
        ix_accounts.iter().filter_map(|i| facts.keys.get(*i as usize).copied()).collect();
    let pick = |owned_by_pool_only: bool| -> Vec<&BalanceObs> {
        facts
            .obs
            .iter()
            .filter(|o| idx_set.contains(&o.account_index))
            .filter(|o| o.owner != facts.fee_payer)
            .filter(|o| if owned_by_pool_only { o.owner == pool } else { key_set.contains(&o.owner) })
            .filter(|o| o.post_atom != o.pre_atom)
            .collect()
    };
    let mut cands = pick(true);
    if cands.len() != 2 {
        cands = pick(false);
    }
    let gained: Vec<&&BalanceObs> = cands.iter().filter(|o| o.post_atom > o.pre_atom).collect();
    let lost: Vec<&&BalanceObs> = cands.iter().filter(|o| o.post_atom < o.pre_atom).collect();
    match (gained.as_slice(), lost.as_slice()) {
        ([g], [l]) if g.mint != l.mint => Some(VaultLegs {
            in_mint: g.mint,
            in_amount: (g.post_atom - g.pre_atom).min(u64::MAX as u128) as u64,
            in_dec: g.decimals,
            r_in_pre: g.pre_atom,
            out_mint: l.mint,
            out_amount: (l.pre_atom - l.post_atom).min(u64::MAX as u128) as u64,
            out_dec: l.decimals,
            r_out_pre: l.pre_atom,
        }),
        // Bonding curves (pump.fun, Pumpup): one token vault + native SOL on the pool account.
        ([g], []) | ([], [g]) => {
            let pool_idx = facts.keys.iter().position(|k| *k == pool)?;
            let lam = *facts.lamport_deltas.get(pool_idx)?;
            if lam == 0 {
                return None;
            }
            let token_gained = g.post_atom > g.pre_atom;
            if token_gained == (lam > 0) {
                return None; // both sides moved the same way — not a swap
            }
            let tok_amt = g.post_atom.abs_diff(g.pre_atom).min(u64::MAX as u128) as u64;
            let sol_amt = lam.unsigned_abs().min(u64::MAX as u128) as u64;
            Some(if token_gained {
                VaultLegs { in_mint: g.mint, in_amount: tok_amt, in_dec: g.decimals, r_in_pre: g.pre_atom,
                            out_mint: SOL_NATIVE_MINT, out_amount: sol_amt, out_dec: 9, r_out_pre: 0 }
            } else {
                VaultLegs { in_mint: SOL_NATIVE_MINT, in_amount: sol_amt, in_dec: 9, r_in_pre: 0,
                            out_mint: g.mint, out_amount: tok_amt, out_dec: g.decimals, r_out_pre: g.pre_atom }
            })
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_swap(
    sig: &str,
    slot: u64,
    block_time: Option<i64>,
    dex: &str,
    pool: Pubkey,
    user: Pubkey,
    input_mint: Pubkey,
    input_amount: u64,
    input_dec: u8,
    output_mint: Pubkey,
    output_amount: u64,
    output_dec: u8,
    oracle: &PriceOracle,
) -> Option<Swap> {
    if input_mint == output_mint || input_amount == 0 || output_amount == 0 {
        return None;
    }
    let input = SwapSide::new(input_mint, input_amount, input_dec);
    let output = SwapSide::new(output_mint, output_amount, output_dec);
    let (price_native, price_native_inverted) = native_price(input_amount, input_dec, output_amount, output_dec);
    let (price_usd, amount_usd) = usd_price(&input_mint, &output_mint, input_amount, input_dec, output_amount, output_dec, oracle);
    Some(Swap {
        kind: "swap",
        signature: sig.to_string(),
        slot,
        block_time,
        dex: dex.to_string(),
        pool: pool.to_string(),
        user: user.to_string(),
        input,
        output,
        price_native,
        price_native_inverted,
        price_usd,
        amount_usd,
    })
}

/// Pre/post token-balance observation, normalized across both block formats.
struct BalanceObs {
    account_index: u32,
    mint: Pubkey,
    owner: Pubkey,
    pre_atom: u128,
    post_atom: u128,
    decimals: u8,
}

fn ui_balance_to_obs_pre(
    tb: &solana_transaction_status_client_types::UiTransactionTokenBalance,
) -> Option<BalanceObs> {
    let mint = Pubkey::from_str(&tb.mint).ok()?;
    let owner = match &tb.owner {
        OptionSerializer::Some(o) => Pubkey::from_str(o).ok()?,
        _ => return None,
    };
    let pre_atom = tb.ui_token_amount.amount.parse::<u128>().unwrap_or(0);
    Some(BalanceObs {
        account_index: tb.account_index as u32,
        mint,
        owner,
        pre_atom,
        post_atom: 0,
        decimals: tb.ui_token_amount.decimals,
    })
}

fn ui_balance_to_obs_post(
    tb: &solana_transaction_status_client_types::UiTransactionTokenBalance,
) -> Option<BalanceObs> {
    let mint = Pubkey::from_str(&tb.mint).ok()?;
    let owner = match &tb.owner {
        OptionSerializer::Some(o) => Pubkey::from_str(o).ok()?,
        _ => return None,
    };
    let post_atom = tb.ui_token_amount.amount.parse::<u128>().unwrap_or(0);
    Some(BalanceObs {
        account_index: tb.account_index as u32,
        mint,
        owner,
        pre_atom: 0,
        post_atom,
        decimals: tb.ui_token_amount.decimals,
    })
}

fn ys_balance_to_obs_pre(tb: &YsTokenBalance) -> Option<BalanceObs> {
    let mint = Pubkey::from_str(&tb.mint).ok()?;
    let owner = Pubkey::from_str(&tb.owner).ok()?;
    let amt = tb.ui_token_amount.as_ref()?;
    let pre_atom = amt.amount.parse::<u128>().unwrap_or(0);
    Some(BalanceObs {
        account_index: tb.account_index,
        mint,
        owner,
        pre_atom,
        post_atom: 0,
        decimals: amt.decimals as u8,
    })
}

fn ys_balance_to_obs_post(tb: &YsTokenBalance) -> Option<BalanceObs> {
    let mint = Pubkey::from_str(&tb.mint).ok()?;
    let owner = Pubkey::from_str(&tb.owner).ok()?;
    let amt = tb.ui_token_amount.as_ref()?;
    let post_atom = amt.amount.parse::<u128>().unwrap_or(0);
    Some(BalanceObs {
        account_index: tb.account_index,
        mint,
        owner,
        pre_atom: 0,
        post_atom,
        decimals: amt.decimals as u8,
    })
}

/// Merge a post observation into an existing pre observation list (matched by
/// account_index). If no pre entry exists we push a synthetic one.
fn merge_obs_post(obs: &mut Vec<BalanceObs>, post: Option<BalanceObs>) {
    let post = match post {
        Some(p) => p,
        None => return,
    };
    if let Some(existing) = obs.iter_mut().find(|o| o.account_index == post.account_index) {
        existing.post_atom = post.post_atom;
        if existing.decimals == 0 {
            existing.decimals = post.decimals;
        }
    } else {
        obs.push(post);
    }
}

fn ui_compiled_to_view<'a>(
    ix: &'a solana_transaction_status_client_types::UiCompiledInstruction,
    keys: &[Pubkey],
) -> Option<IxView<'a>> {
    let pid_idx = ix.program_id_index as usize;
    if pid_idx >= keys.len() {
        return None;
    }
    Some(IxView {
        program_id: keys[pid_idx],
        accounts: &ix.accounts,
        data_b58: ix.data.clone(),
    })
}

fn ys_compiled_to_view<'a>(
    ix: &'a yellowstone_grpc_proto::solana::storage::confirmed_block::CompiledInstruction,
    keys: &[Pubkey],
) -> Option<IxView<'a>> {
    let pid_idx = ix.program_id_index as usize;
    if pid_idx >= keys.len() {
        return None;
    }
    Some(IxView {
        program_id: keys[pid_idx],
        accounts: &ix.accounts,
        data_b58: bs58::encode(&ix.data).into_string(),
    })
}

fn ys_inner_to_view<'a>(ix: &'a YsInnerInstruction, keys: &[Pubkey]) -> Option<IxView<'a>> {
    let pid_idx = ix.program_id_index as usize;
    if pid_idx >= keys.len() {
        return None;
    }
    Some(IxView {
        program_id: keys[pid_idx],
        accounts: &ix.accounts,
        data_b58: bs58::encode(&ix.data).into_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_swap_from_deltas(
    sig: &str,
    slot: u64,
    block_time: Option<i64>,
    dex: &str,
    pool: Pubkey,
    fee_payer: Pubkey,
    deltas: &Deltas,
    oracle: &PriceOracle,
) -> Option<Swap> {
    // Pick most-negative as input, most-positive as output.
    let (input_mint, input_amount, input_dec) = deltas.most_negative()?;
    let (output_mint, output_amount, output_dec) = deltas.most_positive()?;
    build_swap(sig, slot, block_time, dex, pool, fee_payer, input_mint, input_amount, input_dec, output_mint, output_amount, output_dec, oracle)
}


/// Per-mint signed deltas for the fee payer.
#[derive(Debug, Default)]
struct Deltas {
    /// (mint, signed_atomic_delta, decimals)
    entries: Vec<(Pubkey, i128, u8)>,
}

impl Deltas {
    fn most_negative(&self) -> Option<(Pubkey, u64, u8)> {
        self.entries
            .iter()
            .filter(|(_, d, _)| *d < 0)
            .min_by_key(|(_, d, _)| *d)
            .map(|(m, d, dec)| (*m, d.unsigned_abs() as u64, *dec))
    }
    fn most_positive(&self) -> Option<(Pubkey, u64, u8)> {
        self.entries
            .iter()
            .filter(|(_, d, _)| *d > 0)
            .max_by_key(|(_, d, _)| *d)
            .map(|(m, d, dec)| (*m, *d as u64, *dec))
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compute the fee payer's per-mint signed atomic deltas. Sums across multiple
/// token accounts of the same (owner, mint). Native SOL is included via the
/// lamport balance, minus tx fee (so the swap-only delta isn't polluted by gas).
/// PumpFun AMM-style wraps that net to zero correctly cancel out.
fn compute_fee_payer_deltas(
    obs: &[BalanceObs],
    fee_payer: &Pubkey,
    pre_lamports: u64,
    post_lamports: u64,
    fee: u64,
) -> Deltas {
    use std::collections::HashMap;
    let mut by_mint: HashMap<Pubkey, (i128, u8)> = HashMap::new();

    for o in obs {
        if &o.owner != fee_payer {
            continue;
        }
        let delta = o.post_atom as i128 - o.pre_atom as i128;
        if delta == 0 {
            continue;
        }
        let entry = by_mint.entry(o.mint).or_insert((0, o.decimals));
        entry.0 += delta;
        if entry.1 == 0 {
            entry.1 = o.decimals;
        }
    }

    // Native SOL leg: lamport delta + fee (fee is always paid regardless of swap).
    let raw_delta = post_lamports as i128 - pre_lamports as i128;
    let delta_excl_fee = raw_delta + fee as i128;
    if delta_excl_fee != 0 {
        let entry = by_mint.entry(SOL_NATIVE_MINT).or_insert((0, 9));
        entry.0 += delta_excl_fee;
        if entry.1 == 0 {
            entry.1 = 9;
        }
    }

    let mut entries: Vec<(Pubkey, i128, u8)> = by_mint
        .into_iter()
        .filter(|(_, (d, _))| *d != 0)
        .map(|(m, (d, dec))| (m, d, dec))
        .collect();
    entries.sort_by_key(|(_, d, _)| *d);
    Deltas { entries }
}

/// Compute native price strings (output_ui per input_ui, and inverse).
/// Returns (`price_native`, `price_native_inverted`).
fn native_price(
    in_atom: u64,
    in_dec: u8,
    out_atom: u64,
    out_dec: u8,
) -> (Option<String>, Option<String>) {
    if in_atom == 0 || out_atom == 0 {
        return (None, None);
    }
    let in_ui = atomic_to_ui(in_atom, in_dec);
    let out_ui = atomic_to_ui(out_atom, out_dec);
    if in_ui == 0.0 || out_ui == 0.0 {
        return (None, None);
    }
    let p = out_ui / in_ui;
    let pi = in_ui / out_ui;
    (Some(format_price(p)), Some(format_price(pi)))
}

/// USD enrichment. Returns (price_per_output_unit_in_usd, total_trade_value_usd).
fn usd_price(
    in_mint: &Pubkey,
    out_mint: &Pubkey,
    in_atom: u64,
    in_dec: u8,
    out_atom: u64,
    out_dec: u8,
    oracle: &PriceOracle,
) -> (Option<String>, Option<String>) {
    let in_ui = atomic_to_ui(in_atom, in_dec);
    let out_ui = atomic_to_ui(out_atom, out_dec);
    if in_ui == 0.0 || out_ui == 0.0 {
        return (None, None);
    }

    // Determine a USD value for the input side (the user's spend).
    let input_usd = if is_stable(in_mint) {
        Some(in_ui)
    } else if *in_mint == SOL_NATIVE_MINT {
        oracle.get_sol_usd_price().map(|sol| sol * in_ui)
    } else if is_stable(out_mint) {
        Some(out_ui)
    } else if *out_mint == SOL_NATIVE_MINT {
        oracle.get_sol_usd_price().map(|sol| sol * out_ui)
    } else {
        None
    };

    let amount_usd = match input_usd {
        Some(v) => v,
        None => return (None, None),
    };
    let price_per_output = amount_usd / out_ui;
    (
        Some(format_price(price_per_output)),
        Some(format!("{:.4}", amount_usd)),
    )
}

fn atomic_to_ui(atom: u64, decimals: u8) -> f64 {
    if decimals == 0 {
        return atom as f64;
    }
    let scale = 10u128.pow(decimals as u32) as f64;
    atom as f64 / scale
}

fn format_price(p: f64) -> String {
    if !p.is_finite() {
        return "0".to_string();
    }
    if p.abs() >= 1.0 {
        format!("{:.6}", p)
    } else if p.abs() >= 0.0001 {
        format!("{:.8}", p)
    } else {
        format!("{:.12}", p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey;

    #[test]
    fn test_swap_side_ui_amount() {
        let mint = pubkey!("So11111111111111111111111111111111111111112");
        let s = SwapSide::new(mint, 1_000_000, 9);
        assert_eq!(s.amount, "1000000");
        assert_eq!(s.decimals, 9);
        assert!(s.ui_amount.starts_with("0.001"));
    }

    #[test]
    fn test_native_price_basic() {
        // 1 SOL in, 100 USDC out → output_per_input = 100 USDC / 1 SOL
        let (p, pi) = native_price(1_000_000_000, 9, 100_000_000, 6);
        assert_eq!(p.unwrap(), "100.000000");
        assert_eq!(pi.unwrap(), "0.01000000");
    }

    #[test]
    fn test_native_price_zero_returns_none() {
        let (p, pi) = native_price(0, 9, 1, 6);
        assert!(p.is_none());
        assert!(pi.is_none());
    }

    #[test]
    fn test_usd_price_with_stable_input() {
        let oracle = PriceOracle::new();
        // 50 USDC → 1B atomic of some token (6 decimals → 1000 ui units)
        let token = Pubkey::new_unique();
        let (price_usd, amt_usd) = usd_price(
            &USDC_MINT, &token, 50_000_000, 6, 1_000_000_000, 6, &oracle,
        );
        // amount_usd = 50 USDC = $50.00
        // price per output token = 50 / 1000 = $0.05
        assert_eq!(amt_usd.unwrap(), "50.0000");
        assert_eq!(price_usd.unwrap(), "0.05000000");
    }

    #[test]
    fn test_usd_price_with_sol_input_and_oracle() {
        let oracle = PriceOracle::new();
        oracle.set_for_tests(150.0);
        let token = Pubkey::new_unique();
        // 1 SOL → 100 of token (6 decimals)
        let (price_usd, amt_usd) = usd_price(
            &SOL_NATIVE_MINT, &token, 1_000_000_000, 9, 100_000_000, 6, &oracle,
        );
        // $150 / 100 ui = $1.50 per output token; amount = $150
        assert_eq!(amt_usd.unwrap(), "150.0000");
        assert_eq!(price_usd.unwrap(), "1.500000");
    }

    #[test]
    fn test_usd_price_no_reference_returns_none() {
        let oracle = PriceOracle::new(); // never refreshed
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let (p, amt) = usd_price(&a, &b, 1, 6, 1, 6, &oracle);
        assert!(p.is_none());
        assert!(amt.is_none());
    }

    #[test]
    fn test_filter_match_dex() {
        let mut swap = sample_swap();
        let f = SwapFilter {
            dex: vec!["Pumpup Bonding".into()],
            ..Default::default()
        };
        swap.dex = "Orca".into();
        assert!(!f.matches(&swap));
        swap.dex = "Pumpup Bonding".into();
        assert!(f.matches(&swap));
    }

    #[test]
    fn test_filter_match_mint() {
        let swap = sample_swap();
        let f = SwapFilter {
            mint: Some(swap.input.mint.clone()),
            ..Default::default()
        };
        assert!(f.matches(&swap));
        let f = SwapFilter {
            mint: Some("11111111111111111111111111111111".into()),
            ..Default::default()
        };
        assert!(!f.matches(&swap));
    }

    #[test]
    fn test_filter_match_min_amount_usd() {
        let mut swap = sample_swap();
        swap.amount_usd = Some("0.50".into());
        let f = SwapFilter {
            min_amount_usd: Some(1.0),
            ..Default::default()
        };
        assert!(!f.matches(&swap));
        swap.amount_usd = Some("5.50".into());
        assert!(f.matches(&swap));
        // No amount_usd + filter set → reject
        swap.amount_usd = None;
        assert!(!f.matches(&swap));
    }

    fn sample_swap() -> Swap {
        Swap {
            kind: "swap",
            signature: "abc".into(),
            slot: 1,
            block_time: None,
            dex: "Orca".into(),
            pool: "POOL".into(),
            user: "USER".into(),
            input: SwapSide::new(SOL_NATIVE_MINT, 1, 9),
            output: SwapSide::new(USDC_MINT, 1, 6),
            price_native: Some("1".into()),
            price_native_inverted: Some("1".into()),
            price_usd: Some("1".into()),
            amount_usd: Some("1".into()),
        }
    }

}
