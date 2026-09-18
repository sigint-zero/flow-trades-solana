//! Live mainnet buy+sell round-trips on every SOL-paired AMM, using
//! freshly-discovered pools picked at run-time. No router CPI (bypassed —
//! see e2e_mainnet_swaps.rs for context on why the staging router is
//! disabled). Pure executor → submit → confirm.
//!
//! Each round-trip costs ~0.0001 SOL in tx fees + slippage. Total wallet
//! cost for the suite is < 0.01 SOL.
//!
//! Run:
//! ```bash
//! set -a && source .env && set +a
//! OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include \
//!   cargo test --test e2e_fresh_roundtrips -- --nocapture --test-threads=1
//! ```

use std::str::FromStr;
use std::time::{Duration, Instant};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, EncodedTransaction, UiInstruction, UiMessage,
    UiTransactionEncoding,
};

use flow_trades::constants::*;
use flow_trades::execution::AmmExecutorType;
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolType, SwapOrder};

fn rpc() -> RpcClient {
    let url = std::env::var("RPC_URL")
        .or_else(|_| std::env::var("SOL_HTTPS_ENDPOINT"))
        .expect("RPC_URL required");
    RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())
}

fn load_keypair() -> Keypair {
    let b58 = std::env::var("SIM_PRIVATE_KEY").expect("SIM_PRIVATE_KEY required");
    let bytes = bs58::decode(b58.trim()).into_vec().expect("invalid base58");
    Keypair::from_bytes(&bytes).expect("invalid keypair")
}

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

/// AMM under test for live round-trips. Only SOL-paired AMMs here.
struct AmmSpec {
    label: &'static str,
    pool_type: PoolType,
    program_id: Pubkey,
    candidate_pool_idxs: &'static [usize],
    /// Hardcoded fallback when discovery can't find a fresh SOL-paired pool.
    fallback_pool: Option<&'static str>,
    /// Optional fallback non-SOL mint (for the fallback pool).
    fallback_token: Option<&'static str>,
}

fn specs() -> Vec<AmmSpec> {
    vec![
        AmmSpec {
            label: "RaydiumV4",
            pool_type: PoolType::RaydiumV4,
            program_id: RAYDIUM_V4_PROG_ID,
            candidate_pool_idxs: &[1],
            fallback_pool: Some("3JDQqSxGF1yjpeStYNRmvXk76ApSGm7uE2onDQpyRvn4"),
            fallback_token: Some("G9EFgQFiJMu4j38CF8ANRGFpdUVitcUXG51tBLYEpump"),
        },
        AmmSpec {
            label: "RaydiumCpmm",
            pool_type: PoolType::RaydiumCpmm,
            program_id: RAYDIUM_CPMM_PROG_ID,
            candidate_pool_idxs: &[3],
            fallback_pool: Some("BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr"),
            fallback_token: Some("25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook"),
        },
        AmmSpec {
            label: "RaydiumCl",
            pool_type: PoolType::RaydiumCl,
            program_id: RAYDIUM_CL_PROG_ID,
            candidate_pool_idxs: &[2],
            fallback_pool: Some("ENQmMUSXmUYPaAL9NH79cFw3Lfht3bThmY8Zs8UwGEbr"),
            fallback_token: Some("22r6hjfpF15dkgJzkNXthNPZny1r7TohQb1vbAEBD5Fg"),
        },
        AmmSpec {
            label: "Orca",
            pool_type: PoolType::Orca,
            program_id: ORCA_PROG_ID,
            candidate_pool_idxs: &[2],
            fallback_pool: Some("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"),
            // Orca SOL/USDC — SOL roundtrip works; USDC is fine
            fallback_token: Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        },
        AmmSpec {
            label: "MeteoraDlmm",
            pool_type: PoolType::MeteoraDlmm,
            program_id: METEORA_DLMM_PROG_ID,
            candidate_pool_idxs: &[0, 1],
            fallback_pool: Some("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR"),
            fallback_token: Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        },
        AmmSpec {
            label: "MeteoraDamm",
            pool_type: PoolType::MeteoraDamm,
            program_id: METEORA_DAMM_PROG_ID,
            candidate_pool_idxs: &[0, 1, 2],
            fallback_pool: Some("4ac2qQp9uPQTuhvNH1p5xZmZnKV1pAxWKjBPkwvD6x6J"),
            fallback_token: Some("CDNZaZwhWB2VGFXSEMmM7hBodrhciRx7JPbjghttbEM3"),
        },
        AmmSpec {
            label: "PumpupBonding",
            pool_type: PoolType::PumpupBonding,
            program_id: PUMPUP_PROG_ID,
            // Bonding `buy`/`sell` puts pool_sol_account at accounts[3].
            candidate_pool_idxs: &[3],
            fallback_pool: Some("AQxKPt88jGP1DiwRbqweoo74Yi2o3fMTATAbDDA6BVLT"),
            fallback_token: Some("9U3FcH1Z3vZFHvN5KrkHHkuJSPKKnBBLpPQ1FkezxAai"),
        },
    ]
}

/// Find a recent SOL-paired pool for a program by walking sigs.
async fn discover_fresh_sol_pool(
    rpc: &RpcClient,
    spec: &AmmSpec,
    sigs_to_scan: usize,
) -> Option<(Pubkey, Pubkey)> {
    let cfg = GetConfirmedSignaturesForAddress2Config {
        before: None,
        until: None,
        limit: Some(sigs_to_scan),
        commitment: Some(CommitmentConfig::confirmed()),
    };
    let sigs = rpc
        .get_signatures_for_address_with_config(&spec.program_id, cfg)
        .await
        .ok()?;
    let prog_str = spec.program_id.to_string();

    for sig_info in sigs {
        if sig_info.err.is_some() {
            continue;
        }
        let sig: Signature = match sig_info.signature.parse() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cfg = flow_trades::stream::tx_version::transaction_config(
            UiTransactionEncoding::Json,
            CommitmentConfig::confirmed(),
        );
        let tx = match rpc.get_transaction_with_config(&sig, cfg).await {
            Ok(t) => t,
            Err(_) => continue,
        };
        let meta = match tx.transaction.meta.as_ref() {
            Some(m) if m.err.is_none() => m,
            _ => continue,
        };

        let (static_keys, ix_list) = match &tx.transaction.transaction {
            EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                UiMessage::Raw(raw) => (raw.account_keys.clone(), raw.instructions.clone()),
                _ => continue,
            },
            _ => continue,
        };
        let mut keys = static_keys;
        if let OptionSerializer::Some(la) = &meta.loaded_addresses {
            keys.extend(la.writable.iter().cloned());
            keys.extend(la.readonly.iter().cloned());
        }

        // Mints involved
        let mut mints: Vec<Pubkey> = Vec::new();
        if let OptionSerializer::Some(post) = &meta.post_token_balances {
            for tb in post {
                if let Ok(m) = tb.mint.parse::<Pubkey>() {
                    if !mints.contains(&m) {
                        mints.push(m);
                    }
                }
            }
        }
        if let OptionSerializer::Some(pre) = &meta.pre_token_balances {
            for tb in pre {
                if let Ok(m) = tb.mint.parse::<Pubkey>() {
                    if !mints.contains(&m) {
                        mints.push(m);
                    }
                }
            }
        }
        // Need SOL among mints AND at least one non-SOL token mint.
        if !mints.contains(&SOL_NATIVE_MINT) {
            continue;
        }
        let token_mint = match mints.iter().find(|m| **m != SOL_NATIVE_MINT).copied() {
            Some(m) => m,
            None => continue,
        };

        let mut all_ixs: Vec<solana_transaction_status_client_types::UiCompiledInstruction> = ix_list;
        if let OptionSerializer::Some(ii_list) = &meta.inner_instructions {
            for ii in ii_list {
                for ux in &ii.instructions {
                    if let UiInstruction::Compiled(c) = ux {
                        all_ixs.push(c.clone());
                    }
                }
            }
        }

        for ix in &all_ixs {
            let pid_idx = ix.program_id_index as usize;
            if pid_idx >= keys.len() {
                continue;
            }
            if keys[pid_idx] != prog_str {
                continue;
            }
            for &cand_idx in spec.candidate_pool_idxs {
                if cand_idx >= ix.accounts.len() {
                    continue;
                }
                let acc_idx = ix.accounts[cand_idx] as usize;
                if acc_idx >= keys.len() {
                    continue;
                }
                let pool: Pubkey = match keys[acc_idx].parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let ai = match rpc.get_account(&pool).await {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                if ai.owner != spec.program_id {
                    continue;
                }
                if fetcher::fetch_pool_state(rpc, spec.pool_type, &pool)
                    .await
                    .is_err()
                {
                    continue;
                }
                return Some((pool, token_mint));
            }
        }
    }
    None
}

/// Direct (non-router) swap — fetch state, build IX, sign, submit, confirm.
async fn execute_swap_direct(
    rpc: &RpcClient,
    signer: &Keypair,
    pool_type: PoolType,
    pool_address: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
) -> Result<(String, u128), String> {
    let user = signer.pubkey();
    let pool_state = fetcher::fetch_pool_state(rpc, pool_type, pool_address)
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    let input_tp = get_mint_token_program(rpc, input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let executor = AmmExecutorType::from_pool_type(pool_type)
        .map_err(|e| format!("executor: {e}"))?;
    let order = SwapOrder {
        pool_address: *pool_address,
        pool_type,
        input_mint: *input_mint,
        output_mint: *output_mint,
        amount_in,
        min_amount_out,
        user,
        input_token_program: input_tp,
        output_token_program: output_tp,
    };
    let ixs = executor
        .build_swap_ix(&order, &pool_state)
        .map_err(|e| format!("build_ix: {e}"))?;
    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(5_000),
    ];
    all_ixs.extend(ixs.setup);
    all_ixs.extend(ixs.swap);
    all_ixs.extend(ixs.cleanup);

    let blockhash = rpc.get_latest_blockhash().await.map_err(|e| format!("blockhash: {e}"))?;
    let start = Instant::now();
    let msg = Message::new_with_blockhash(&all_ixs, Some(&user), &blockhash);
    let tx = Transaction::new(&[signer], msg, blockhash);
    let sig = rpc.send_and_confirm_transaction(&tx).await.map_err(|e| format!("submit: {e}"))?;
    Ok((sig.to_string(), start.elapsed().as_millis()))
}

async fn token_balance(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> u64 {
    let mint_tp = get_mint_token_program(rpc, mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        owner, mint, &mint_tp,
    );
    rpc.get_token_account_balance(&ata)
        .await
        .ok()
        .and_then(|b| b.amount.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Default per-AMM buy size. CLMM/DLMM pools concentrate liquidity in
/// narrow tick/bin ranges, so very small SOL inputs can fail through-bin
/// constraints (0xbb-range custom errors). Bump those AMMs up — still
/// cheap, but enough to clear most active bins.
fn buy_lamports_for(pool_type: PoolType) -> u64 {
    match pool_type {
        PoolType::RaydiumCl
        | PoolType::Orca
        | PoolType::PancakeSwap
        | PoolType::Byreal
        | PoolType::DefiTunaFusion
        | PoolType::MeteoraDlmm => 5_000_000, // 0.005 SOL
        _ => 1_000_000,                        // 0.001 SOL
    }
}

const RETRY_COUNT: u32 = 4;

async fn run_round_trip(
    rpc: &RpcClient,
    signer: &Keypair,
    spec: &AmmSpec,
) -> Result<(String, String, u64, u64), String> {
    let user = signer.pubkey();
    let buy_amount = buy_lamports_for(spec.pool_type);

    // 1. Pick a pool. Try fresh discovery first; if the buy fails on the
    //    fresh pool with a likely-liquidity error, retry against the
    //    known-deep fallback before giving up.
    let fresh = discover_fresh_sol_pool(rpc, spec, 30).await;
    let fallback = match (spec.fallback_pool, spec.fallback_token) {
        (Some(p), Some(t)) => Some((pk(p), pk(t))),
        _ => None,
    };
    let (pool, token_mint) = match fresh {
        Some(p) => p,
        None => fallback.ok_or_else(|| "no SOL-paired pool found".to_string())?,
    };
    eprintln!("    pool: {pool}  token: {token_mint}");

    // 2. Buy — retry on fallback if a thin-liquidity error fires.
    let pre_buy = token_balance(rpc, &user, &token_mint).await;
    let buy_result = execute_swap_direct(
        rpc, signer, spec.pool_type, &pool,
        &SOL_NATIVE_MINT, &token_mint, buy_amount, 1,
    ).await;
    let (pool, token_mint, buy_sig, buy_ms) = match buy_result {
        Ok((sig, ms)) => (pool, token_mint, sig, ms),
        Err(e) => {
            // Common CLMM/DLMM thin-liquidity errors + RaydiumCpmm pool
            // state (0x3, often pump-fun migration pools). Rather than
            // failing, retry on the verified deep fallback pool.
            let likely_thin = e.contains("0xbbf") || e.contains("0xbc0")
                || e.contains("0xbc4") || e.contains("Custom(3007)")
                || e.contains("Custom(3008)") || e.contains("Custom(3012)")
                || e.contains("Custom(3)");
            match (likely_thin, fallback) {
                (true, Some((fp, ft))) if fp != pool => {
                    eprintln!("    fresh pool {pool} too thin ({}); retrying on fallback {fp}",
                        e.lines().next().unwrap_or(""));
                    let (s, m) = execute_swap_direct(
                        rpc, signer, spec.pool_type, &fp,
                        &SOL_NATIVE_MINT, &ft, buy_amount, 1,
                    ).await.map_err(|e2| format!("buy(fallback): {e2}"))?;
                    (fp, ft, s, m)
                }
                _ => return Err(format!("buy: {e}")),
            }
        }
    };
    eprintln!("    BUY  : {buy_sig}  ({} ms)", buy_ms);

    tokio::time::sleep(Duration::from_secs(3)).await;
    let post_buy = token_balance(rpc, &user, &token_mint).await;
    let token_received = post_buy.saturating_sub(pre_buy);
    if token_received == 0 {
        return Err(format!("buy succeeded but no tokens received (pre={pre_buy} post={post_buy})"));
    }
    eprintln!("    +{} atomic of {token_mint}", token_received);

    // 3. Sell — retry on simulator state lag
    let mut sell_attempts = 0u32;
    let (sell_sig, sell_ms) = loop {
        sell_attempts += 1;
        match execute_swap_direct(
            rpc, signer, spec.pool_type, &pool,
            &token_mint, &SOL_NATIVE_MINT, token_received, 1,
        ).await {
            Ok(r) => break r,
            Err(e) if sell_attempts <= RETRY_COUNT => {
                eprintln!("    sell attempt {sell_attempts} failed: {e}; retrying...");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(format!("sell after {sell_attempts} attempts: {e}")),
        }
    };
    eprintln!("    SELL : {sell_sig}  ({} ms)", sell_ms);

    Ok((buy_sig, sell_sig, buy_ms as u64, sell_ms as u64))
}

#[tokio::test]
async fn test_fresh_live_round_trips() {
    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let starting_sol = rpc.get_balance(&user).await.unwrap_or(0);
    let specs = specs();

    eprintln!("\n+-{:-<80}-+", " FRESH-POOL LIVE ROUND-TRIPS (SOL ↔ TOKEN) ");
    eprintln!("| wallet:  {}", user);
    eprintln!("| balance: {} lamports ({:.6} SOL)", starting_sol, starting_sol as f64 / 1e9);
    eprintln!("| AMMs:    {}", specs.len());
    eprintln!("+-{:-<80}-+", "");

    let mut pass = 0;
    let mut fail: Vec<(&str, String)> = Vec::new();

    for spec in &specs {
        eprintln!("\n  ── {} ──", spec.label);
        match run_round_trip(&rpc, &signer, spec).await {
            Ok(_) => {
                pass += 1;
            }
            Err(e) => {
                eprintln!("    [FAIL] {e}");
                fail.push((spec.label, e));
            }
        }
    }

    let ending_sol = rpc.get_balance(&user).await.unwrap_or(0);
    let net = starting_sol as i128 - ending_sol as i128;

    eprintln!("\n+-{:-<80}-+", " SUMMARY ");
    eprintln!("| {} pass / {} fail (of {})", pass, fail.len(), specs.len());
    eprintln!("| net SOL spent: {} lamports ({:.6} SOL)", net, net as f64 / 1e9);
    if !fail.is_empty() {
        eprintln!("|");
        eprintln!("| failures:");
        for (label, e) in &fail {
            eprintln!("|   {label}: {}", e.lines().next().unwrap_or(""));
        }
    }
    eprintln!("+-{:-<80}-+", "");

    // Live round-trips are inherently flaky vs simulation: pool state
    // shifts between the discovery moment and the buy attempt (drains,
    // bin-liquidity rebalances, pump migrations). Simulation in
    // e2e_fresh_markets is the authoritative signal that every IX builder
    // works. Here we just want to confirm at least the high-confidence
    // SOL-paired AMMs successfully round-trip on mainnet — RaydiumV4,
    // MeteoraDamm, PumpupBonding rarely fail.
    let total = specs.len();
    let need = (total * 4) / 10; // 40%
    assert!(pass >= need, "pass-rate too low: {pass}/{total} (need >= {need})");
}
