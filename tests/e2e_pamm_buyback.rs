//! LIVE (simulate-only, never submits): pump.fun AMM swaps with the pump_fees
//! buyback remaining-accounts, through the on-chain flow-router.
//!
//! 1. Find a SOL-quoted pool with a recent successful direct BUY by walking the
//!    pAMM program's recent signatures; that buyer is the simulation payer
//!    (has SOL, and now holds the token — so it can also simulate the sell).
//! 2. `fetch_pool_state` resolves the buyback accounts + current fee recipient,
//!    and the Geyser re-parse carry-over restores them.
//! 3. A BUY (SOL → token) and a SELL (token → SOL) built by the executor,
//!    wrapped in the router and packed the way `/swap` packs them (v0 with the
//!    production ALTs), SIMULATE cleanly (`sigVerify=false`, blockhash
//!    replaced — no keys, no funds).
//! 4. DEFECT REINTRODUCED: the same buy with the pre-buyback account layout
//!    (trailing `pool_v2` instead of the buyback accounts) FAILS on-chain.
//! 5. The quote engine's number for the buy equals what the executor encodes,
//!    and the derived `minimum_out` sits at or below it.
//!
//! Requires `RPC_URL` or `SOL_HTTPS_ENDPOINT`. Skips loudly when unset.

use std::str::FromStr;
use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, EncodedTransaction, UiInstruction, UiMessage,
    UiParsedInstruction, UiTransactionEncoding,
};

use flow_trades::constants::*;
use flow_trades::execution::address_lookup::AltCache;
use flow_trades::execution::amms::pumpfun_amm::{pamm_quote_out, FEE_PROGRAM};
use flow_trades::execution::amms::AmmExecutorType;
use flow_trades::execution::router::{wrap_swap, RouterConfig};
use flow_trades::execution::tx_builder::{build_unsigned_versioned_tx, TxBuildConfig};
use flow_trades::pool::cache::PoolCache;
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::registry::{PoolEntry, PoolRegistry};
use flow_trades::pool::types::{PoolState, PoolType, SwapInstructions, SwapOrder};
use flow_trades::quote::{QuoteRequest, Quoter};
use flow_trades::stream::tx_version;

const ROUTER: &str = "FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm";
const TREASURY: &str = "2yL7tWs2TULhicDtdDV7A8P8Agh79EwCFLKeyKL5fMr3";
/// The production lookup tables (`.env.example` `ALT_ADDRESSES`).
const PROD_ALTS: [&str; 2] = [
    "BrQp6dwBFCdUfrvgnqzw9tc9kLPXTn16AmjZE8xJanMM",
    "AoRtqBqk7Ysf3cd5NWjs93E2ekmz9KG1wLV84v7Xa1KK",
];
const BUY_DISC: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
const BUY_LAMPORTS: u64 = 1_000_000; // 0.001 SOL
/// Solana's packet limit for a serialized transaction.
const MAX_TX_BYTES: usize = 1232;

fn rpc() -> Option<Arc<RpcClient>> {
    let url = std::env::var("RPC_URL")
        .or_else(|_| std::env::var("SOL_HTTPS_ENDPOINT"))
        .ok()
        .filter(|u| !u.is_empty())?;
    Some(Arc::new(RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())))
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

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

fn router_config() -> RouterConfig {
    RouterConfig { program_id: pk(ROUTER), treasury_wallet: pk(TREASURY), referral_wallet: None }
}

/// A SOL-quoted pool with a recent SUCCESSFUL direct buy, and that buyer.
#[derive(Debug)]
struct LivePool {
    pool: Pubkey,
    buyer: Pubkey,
    buy_sig: String,
}

static LIVE: tokio::sync::OnceCell<LivePool> = tokio::sync::OnceCell::const_new();

async fn live_pool(rpc: &RpcClient) -> &'static LivePool {
    LIVE.get_or_init(|| find_live_pool(rpc)).await
}

async fn find_live_pool(rpc: &RpcClient) -> LivePool {
    // Three attempts: the program's signature index is heavy and the RPC
    // occasionally times out on it.
    let mut sigs = Vec::new();
    for attempt in 1..=3 {
        match rpc
            .get_signatures_for_address_with_config(
                &PUMP_FUN_AMM_PROG_ID,
                GetConfirmedSignaturesForAddress2Config {
                    limit: Some(200),
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(s) => {
                sigs = s;
                break;
            }
            Err(e) if attempt < 3 => {
                eprintln!("getSignaturesForAddress(pAMM) attempt {attempt} failed: {e}; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            Err(e) => panic!("getSignaturesForAddress(pAMM): {e}"),
        }
    }
    let pamm = PUMP_FUN_AMM_PROG_ID.to_string();
    let wsol = SOL_NATIVE_MINT.to_string();
    let mut fetched = 0;
    for si in sigs.into_iter().filter(|s| s.err.is_none()) {
        if fetched >= 60 {
            break;
        }
        let sig = Signature::from_str(&si.signature).unwrap();
        let cfg = tx_version::transaction_config(UiTransactionEncoding::JsonParsed, CommitmentConfig::confirmed());
        let Ok(tx) = rpc.get_transaction_with_config(&sig, cfg).await else { continue };
        fetched += 1;
        let EncodedTransaction::Json(ui) = tx.transaction.transaction else { continue };
        let UiMessage::Parsed(parsed) = ui.message else { continue };
        let fee_payer = parsed.account_keys.first().map(|k| k.pubkey.clone()).unwrap_or_default();
        let mut all = parsed.instructions.clone();
        if let Some(meta) = tx.transaction.meta {
            if let OptionSerializer::Some(inner) = meta.inner_instructions {
                for ii in inner {
                    all.extend(ii.instructions);
                }
            }
        }
        for ix in all {
            let UiInstruction::Parsed(UiParsedInstruction::PartiallyDecoded(p)) = ix else { continue };
            if p.program_id != pamm || p.accounts.len() < 23 {
                continue;
            }
            let data = bs58::decode(&p.data).into_vec().unwrap_or_default();
            if data.len() < 8 || data[..8] != BUY_DISC {
                continue;
            }
            // Direct buy by the fee payer (user = account[1]) on a SOL-quoted
            // pool (quote_mint = account[4]).
            if p.accounts[1] != fee_payer || p.accounts[4] != wsol {
                continue;
            }
            // The buyer is our simulation payer: it must still hold enough SOL
            // for the buy, the fee-ATA rent and fees, or the simulation fails on
            // the System transfer (Custom(1), insufficient lamports).
            let payer = pk(&fee_payer);
            match rpc.get_balance(&payer).await {
                Ok(lamports) if lamports >= 50_000_000 => {}
                _ => continue,
            }
            let lp = LivePool { pool: pk(&p.accounts[0]), buyer: payer, buy_sig: si.signature.clone() };
            eprintln!("live pool {} — recent direct buy {} by {}", lp.pool, lp.buy_sig, lp.buyer);
            return lp;
        }
    }
    panic!("no recent successful direct pAMM buy on a SOL-quoted pool in the last 200 program signatures");
}

async fn simulate(rpc: &RpcClient, vtx: &VersionedTransaction, label: &str) -> (bool, Vec<String>) {
    let size = bincode::serialize(vtx).unwrap().len();
    assert!(size <= MAX_TX_BYTES, "{label}: {size} bytes exceeds the {MAX_TX_BYTES}-byte transaction limit");
    let sim = rpc
        .simulate_transaction_with_config(
            vtx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(CommitmentConfig::confirmed()),
                accounts: None,
                min_context_slot: None,
                inner_instructions: false,
                encoding: None,
            },
        )
        .await
        .expect("simulateTransaction RPC");
    let logs = sim.value.logs.unwrap_or_default();
    match sim.value.err {
        None => {
            eprintln!("  {label}: sim OK, cu={}, tx={size}B", sim.value.units_consumed.unwrap_or(0));
            (true, logs)
        }
        Some(e) => {
            eprintln!("  {label}: sim ERROR {e:?}, tx={size}B");
            for l in logs.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
                eprintln!("    {l}");
            }
            (false, logs)
        }
    }
}

/// Pack a DEX swap the way `/swap` does: fee-ATA create + setup, the router
/// CPI wrapping the DEX instruction, cleanup — as a v0 transaction against
/// the production lookup tables.
async fn router_vtx(
    rpc: &RpcClient,
    alts: &AltCache,
    swap: &SwapInstructions,
    user: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    input_tp: &Pubkey,
    output_tp: &Pubkey,
    amount_in: u64,
    min_out: u64,
) -> VersionedTransaction {
    let router = router_config();
    let user_in = spl_associated_token_account::get_associated_token_address_with_program_id(user, input_mint, input_tp);
    let user_out = spl_associated_token_account::get_associated_token_address_with_program_id(user, output_mint, output_tp);
    let fee_acct = router.fee_account_for_mint(output_mint, output_tp);
    let create_fee_ata = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        user, &router.treasury_wallet, output_mint, output_tp,
    );
    let router_ix = wrap_swap(&router, user, &[user_in, user_out], &fee_acct, None, &swap.swap, amount_in, min_out, output_tp, &Pubkey::default() /* output mint: ignored by the legacy layout */)
        .expect("wrap_swap");
    let mut setup = vec![create_fee_ata];
    setup.extend(swap.setup.iter().cloned());
    let packed = SwapInstructions { setup, swap: vec![router_ix], cleanup: swap.cleanup.clone() };
    let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
    build_unsigned_versioned_tx(&packed, user, &TxBuildConfig::default(), blockhash, &alts.all_tables())
        .expect("build v0 tx")
}

async fn prod_alts(rpc: &RpcClient) -> AltCache {
    let cache = AltCache::new();
    let addrs: Vec<Pubkey> = PROD_ALTS.iter().map(|a| pk(a)).collect();
    let loaded = cache.load_alts(rpc, &addrs).await;
    eprintln!("  ALTs loaded: {loaded} tables, {} addresses", cache.total_addresses());
    assert_eq!(loaded, PROD_ALTS.len(), "production lookup tables must load");
    cache
}

fn pamm_fields(state: &PoolState) -> (Pubkey, Pubkey, u64, u64, Pubkey, Vec<(Pubkey, bool)>) {
    match state {
        PoolState::PumpFunAmm { base_mint, quote_mint, base_reserve, quote_reserve, protocol_fee_recipient, buyback_accounts, .. } => {
            (*base_mint, *quote_mint, *base_reserve, *quote_reserve, *protocol_fee_recipient, buyback_accounts.clone())
        }
        other => panic!("expected PumpFunAmm, got {other:?}"),
    }
}

#[tokio::test]
async fn test_01_fetch_resolves_buyback_accounts_and_recipient() {
    let rpc = require_rpc!();
    let live = live_pool(&rpc).await;
    let state = fetcher::fetch_pool_state(&rpc, PoolType::PumpFunAmm, &live.pool).await.expect("fetch pool state");
    let (base, quote, br, qr, recipient, bb) = pamm_fields(&state);
    eprintln!("  base {base} quote {quote} reserves {br}/{qr}");
    eprintln!("  protocol_fee_recipient {recipient}");
    for (pk, w) in &bb {
        eprintln!("  buyback account {pk} writable={w}");
    }
    assert!(!bb.is_empty(), "buyback accounts must resolve for a pool with recent swaps");
    assert_ne!(recipient, Pubkey::default(), "current fee recipient must resolve");
    assert!(br > 0 && qr > 0);
    assert!(!state.needs_pamm_fee_accounts());
    assert_eq!(quote, SOL_NATIVE_MINT);

    // The Geyser inline re-parse loses these; the carry-over must restore them.
    let account = rpc.get_account(&live.pool).await.unwrap();
    let mut reparsed = fetcher::parse_pumpfun_amm_with_balances(&live.pool, &account.data, Some(br), Some(qr)).unwrap();
    assert!(reparsed.needs_pamm_fee_accounts(), "a raw re-parse has no buyback accounts");
    reparsed.carry_over_pamm_fee_accounts(&state);
    let (_, _, _, _, r2, bb2) = pamm_fields(&reparsed);
    assert_eq!(bb2, bb);
    assert_eq!(r2, recipient);
}

#[tokio::test]
async fn test_02_buy_and_sell_simulate_through_router() {
    let rpc = require_rpc!();
    let live = live_pool(&rpc).await;
    let state = fetcher::fetch_pool_state(&rpc, PoolType::PumpFunAmm, &live.pool).await.expect("fetch pool state");
    let (base, quote, _, _, _, _) = pamm_fields(&state);
    let base_tp = get_mint_token_program(&rpc, &base).await.unwrap_or(TOKEN_PROGRAM_ID);
    let sol_tp = TOKEN_PROGRAM_ID;
    let exec = AmmExecutorType::from_pool_type(PoolType::PumpFunAmm).unwrap();
    let alts = prod_alts(&rpc).await;

    // BUY: SOL → token, payer = the recent buyer (has SOL).
    let buy = SwapOrder {
        pool_address: live.pool, pool_type: PoolType::PumpFunAmm,
        input_mint: quote, output_mint: base, amount_in: BUY_LAMPORTS, min_amount_out: 0,
        user: live.buyer, input_token_program: sol_tp, output_token_program: base_tp,
    };
    // The floor the quote engine derives: the pool's tier-fee constant-product
    // output less 1% slippage. It goes both into the router and, because the
    // buy is exact-input, into the pAMM instruction itself.
    // Layout proof only: young pools charge an effective 3–36% on buys (the
    // buyback mechanic) which the server learns from streamed swaps; this test
    // has no stream, so the floor is the tier quote less 40%.
    let (expected_out, fee_bps) = pamm_quote_out(&state, &quote, BUY_LAMPORTS).expect("quote");
    let min_out = expected_out - expected_out * 40 / 100;
    eprintln!("  quote: {expected_out} base atoms at {fee_bps} bps → floor {min_out}");
    let buy = SwapOrder { min_amount_out: min_out, ..buy };
    let buy_ixs = exec.build_swap_ix(&buy, &state).expect("build buy");
    assert_eq!(u64::from_le_bytes(buy_ixs.swap[0].data[8..16].try_into().unwrap()), BUY_LAMPORTS, "exact-input");
    assert_eq!(u64::from_le_bytes(buy_ixs.swap[0].data[16..24].try_into().unwrap()), min_out);
    let vtx = router_vtx(&rpc, &alts, &buy_ixs, &live.buyer, &quote, &base, &sol_tp, &base_tp, BUY_LAMPORTS, min_out).await;
    let (ok, logs) = simulate(&rpc, &vtx, "BUY via router (buyback layout, v0+ALT)").await;
    assert!(ok, "buy simulation failed:\n{}", logs.join("\n"));
    assert!(logs.iter().any(|l| l.contains("Instruction: BuyExactQuoteIn")), "pAMM BuyExactQuoteIn did not run");
    if let Some(l) = logs.iter().find(|l| l.contains("flow-router:") && l.contains("out (min")) {
        eprintln!("  {l}");
    }

    // SELL: token → SOL, a tenth of what the buyer holds.
    let buyer_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&live.buyer, &base, &base_tp);
    let held: u64 = rpc.get_token_account_balance(&buyer_ata).await.map(|b| b.amount.parse().unwrap_or(0)).unwrap_or(0);
    eprintln!("  buyer holds {held} base atoms");
    if held == 0 {
        eprintln!("  buyer already sold — SELL simulation skipped (buy path proven)");
        return;
    }
    let sell_amt = (held / 10).max(1);
    let sell = SwapOrder {
        pool_address: live.pool, pool_type: PoolType::PumpFunAmm,
        input_mint: base, output_mint: quote, amount_in: sell_amt, min_amount_out: 1,
        user: live.buyer, input_token_program: base_tp, output_token_program: sol_tp,
    };
    let sell_ixs = exec.build_swap_ix(&sell, &state).expect("build sell");
    let vtx = router_vtx(&rpc, &alts, &sell_ixs, &live.buyer, &base, &quote, &base_tp, &sol_tp, sell_amt, 1).await;
    let (ok, logs) = simulate(&rpc, &vtx, "SELL via router (buyback layout, v0+ALT)").await;
    assert!(ok, "sell simulation failed:\n{}", logs.join("\n"));
    assert!(logs.iter().any(|l| l.contains("Instruction: Sell")), "pAMM Sell did not run");
}

#[tokio::test]
async fn test_03_defect_reintroduced_old_pool_v2_layout_fails() {
    let rpc = require_rpc!();
    let live = live_pool(&rpc).await;
    let state = fetcher::fetch_pool_state(&rpc, PoolType::PumpFunAmm, &live.pool).await.expect("fetch pool state");
    let (base, quote, _, _, _, _) = pamm_fields(&state);
    let base_tp = get_mint_token_program(&rpc, &base).await.unwrap_or(TOKEN_PROGRAM_ID);
    let exec = AmmExecutorType::from_pool_type(PoolType::PumpFunAmm).unwrap();
    let alts = prod_alts(&rpc).await;
    let buy = SwapOrder {
        pool_address: live.pool, pool_type: PoolType::PumpFunAmm,
        input_mint: quote, output_mint: base, amount_in: BUY_LAMPORTS, min_amount_out: 0,
        user: live.buyer, input_token_program: TOKEN_PROGRAM_ID, output_token_program: base_tp,
    };
    let mut buy_ixs = exec.build_swap_ix(&buy, &state).expect("build buy");

    // Rewrite the account tail exactly as the pre-fix executor built it:
    // [..., fee_config, fee_program, pool_v2] with no buyback accounts.
    let ix: &mut Instruction = &mut buy_ixs.swap[0];
    let fp = ix.accounts.iter().position(|a| a.pubkey == FEE_PROGRAM).unwrap();
    ix.accounts.truncate(fp + 1);
    let (pool_v2, _) = Pubkey::find_program_address(&[b"pool-v2", base.as_ref()], &PUMP_FUN_AMM_PROG_ID);
    ix.accounts.push(AccountMeta::new_readonly(pool_v2, false));

    let vtx = router_vtx(&rpc, &alts, &buy_ixs, &live.buyer, &quote, &base, &TOKEN_PROGRAM_ID, &base_tp, BUY_LAMPORTS, 0).await;
    let (ok, logs) = simulate(&rpc, &vtx, "BUY via router (OLD pool_v2 layout)").await;
    assert!(!ok, "the pre-buyback layout must be rejected on-chain");
    let pamm_failed = logs.iter().any(|l| l.contains(&PUMP_FUN_AMM_PROG_ID.to_string()) && l.contains("failed"));
    assert!(pamm_failed, "expected the pAMM program itself to fail:\n{}", logs.join("\n"));
    // 6058 BuybackFeeRecipientMissing when the buyback check runs first; the
    // exact-input path can trip an earlier check on the truncated account set.
    let anchor_err = logs.iter().any(|l| l.contains("AnchorError") || l.contains("custom program error"));
    assert!(anchor_err, "expected a pAMM program error:\n{}", logs.join("\n"));
}

#[tokio::test]
async fn test_04_quote_equals_executor_exact_out() {
    let rpc = require_rpc!();
    let live = live_pool(&rpc).await;
    let state = fetcher::fetch_pool_state(&rpc, PoolType::PumpFunAmm, &live.pool).await.expect("fetch pool state");
    let (base, quote, br, qr, _, _) = pamm_fields(&state);

    let registry = Arc::new(PoolRegistry::new());
    registry.add(PoolEntry { address: live.pool, pool_type: PoolType::PumpFunAmm, mint_a: base, mint_b: quote });
    let cache = Arc::new(PoolCache::new(600_000));
    cache.insert(live.pool, state.clone());
    let quoter = Quoter::new(registry, cache, Arc::clone(&rpc));

    let req = QuoteRequest {
        input_mint: quote, output_mint: base, amount: BUY_LAMPORTS, slippage_bps: 100,
        only_direct_routes: true, exclude_dexes: vec![], dexes: vec![], max_accounts: 64,
    };
    let resp = quoter.quote(&req).await.expect("quote");
    let quoted: u64 = resp.amount_out.parse().unwrap();
    let minimum: u64 = resp.minimum_out.parse().unwrap();
    let (expected, fee_bps) = pamm_quote_out(&state, &quote, BUY_LAMPORTS).unwrap();
    eprintln!("  quote out={quoted} minimum_out={minimum} tier fee={fee_bps} bps (reserves {br}/{qr})");
    assert_eq!(quoted, expected, "quote must use the pool's tier fee");
    assert!(minimum <= expected, "router floor above the expected output");
    assert_eq!(resp.routes[0].pool.fee, ((BUY_LAMPORTS as u128 * fee_bps as u128) / 10_000).to_string(), "fee reported at the tier rate");

    // and the executor, given this state, encodes exactly that number
    let exec = AmmExecutorType::from_pool_type(PoolType::PumpFunAmm).unwrap();
    let order = SwapOrder {
        pool_address: live.pool, pool_type: PoolType::PumpFunAmm,
        input_mint: quote, output_mint: base, amount_in: BUY_LAMPORTS, min_amount_out: minimum,
        user: live.buyer, input_token_program: TOKEN_PROGRAM_ID, output_token_program: TOKEN_PROGRAM_ID,
    };
    let ixs = exec.build_swap_ix(&order, &state).unwrap();
    assert_eq!(u64::from_le_bytes(ixs.swap[0].data[8..16].try_into().unwrap()), BUY_LAMPORTS, "exact-input");
    assert_eq!(u64::from_le_bytes(ixs.swap[0].data[16..24].try_into().unwrap()), minimum, "the quote's floor is in the instruction");
}
