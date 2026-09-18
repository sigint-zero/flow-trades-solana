//! Fresh-pool validation: dynamically discover a currently active pool for
//! every AMM, run the full pipeline (fetch → build → tx → simulate) against
//! mainnet, and report per-AMM results.
//!
//! Unlike `e2e_live_swaps.rs` (hardcoded pools that drift over time), this
//! test queries `getSignaturesForAddress` for each program at run-time, picks
//! the first successful tx, extracts the pool account, and tests against a
//! currently active pool. Safe to re-run on every release without refreshing
//! fixtures.
//!
//! Run:
//! ```bash
//! set -a && source .env && set +a
//! OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include \
//!   cargo test --test e2e_fresh_markets -- --nocapture --test-threads=1
//! ```

use std::str::FromStr;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_transaction_status_client_types::{
    option_serializer::OptionSerializer, EncodedTransaction, UiInstruction, UiMessage,
    UiTransactionEncoding,
};

use flow_trades::constants::*;
use flow_trades::execution::tx_builder::{build_unsigned_versioned_tx, TxBuildConfig};
use flow_trades::execution::AmmExecutorType;
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolType, SwapOrder};

const SOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const PYUSD: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";

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

fn is_quote_mint(m: &Pubkey) -> bool {
    let mint_str = m.to_string();
    mint_str == SOL || mint_str == USDC || mint_str == USDT || mint_str == PYUSD
}

/// One AMM under test. `candidate_pool_idxs` are the account positions in
/// the swap instruction (in priority order) where the pool address lives —
/// matches `block_scanner::extract_pool_index` plus a fallback list for AMMs
/// with multiple ix shapes.
struct AmmSpec {
    label: &'static str,
    pool_type: PoolType,
    program_id: Pubkey,
    candidate_pool_idxs: &'static [usize],
    /// Hardcoded fallback pool when discovery can't find a fresh one. Used
    /// for low-volume AMMs (Saros/Dooar/FluxBeam/FlashTrade) that may not
    /// produce a hit in the recent-sig window.
    fallback: Option<(&'static str, &'static str, &'static str)>,
}

fn specs() -> Vec<AmmSpec> {
    vec![
        AmmSpec {
            label: "RaydiumV4",
            pool_type: PoolType::RaydiumV4,
            program_id: RAYDIUM_V4_PROG_ID,
            candidate_pool_idxs: &[1],
            fallback: Some(("3JDQqSxGF1yjpeStYNRmvXk76ApSGm7uE2onDQpyRvn4", SOL, "G9EFgQFiJMu4j38CF8ANRGFpdUVitcUXG51tBLYEpump")),
        },
        AmmSpec {
            label: "RaydiumCpmm",
            pool_type: PoolType::RaydiumCpmm,
            program_id: RAYDIUM_CPMM_PROG_ID,
            candidate_pool_idxs: &[3],
            fallback: Some(("BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr", SOL, "25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook")),
        },
        AmmSpec {
            label: "RaydiumCl",
            pool_type: PoolType::RaydiumCl,
            program_id: RAYDIUM_CL_PROG_ID,
            candidate_pool_idxs: &[2],
            fallback: Some(("ENQmMUSXmUYPaAL9NH79cFw3Lfht3bThmY8Zs8UwGEbr", SOL, "22r6hjfpF15dkgJzkNXthNPZny1r7TohQb1vbAEBD5Fg")),
        },
        AmmSpec {
            label: "RaydiumLp",
            pool_type: PoolType::RaydiumLp,
            program_id: RAYDIUM_LP_PROG_ID,
            candidate_pool_idxs: &[1, 2, 3],
            fallback: Some(("6Lc76tcWsCEkydyLriNaeDkUgVekusBVGgQYYDiKRZi1", SOL, "D756Z3S31AZMbU4teTu2BWK77neDArFhaPr6eZ7bonk")),
        },
        AmmSpec {
            label: "Orca",
            pool_type: PoolType::Orca,
            program_id: ORCA_PROG_ID,
            candidate_pool_idxs: &[2],
            fallback: Some(("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", SOL, USDC)),
        },
        AmmSpec {
            label: "Meteora",
            pool_type: PoolType::Meteora,
            program_id: METEORA_PROG_ID,
            candidate_pool_idxs: &[0, 1],
            fallback: Some(("BCXjm4FfSoquZQJV5Wcje1g1pSHW2hFMU9wDE98Nyatb", SOL, "STrikemJEk2tFVYpg7SMo9nGPrnJ56fHnS1K7PV2fPw")),
        },
        AmmSpec {
            label: "MeteoraDlmm",
            pool_type: PoolType::MeteoraDlmm,
            program_id: METEORA_DLMM_PROG_ID,
            candidate_pool_idxs: &[0, 1],
            fallback: Some(("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR", SOL, USDC)),
        },
        AmmSpec {
            label: "MeteoraDamm",
            pool_type: PoolType::MeteoraDamm,
            program_id: METEORA_DAMM_PROG_ID,
            candidate_pool_idxs: &[0, 1, 2],
            fallback: Some(("4ac2qQp9uPQTuhvNH1p5xZmZnKV1pAxWKjBPkwvD6x6J", SOL, "CDNZaZwhWB2VGFXSEMmM7hBodrhciRx7JPbjghttbEM3")),
        },
        AmmSpec {
            label: "MeteoraDbc",
            pool_type: PoolType::MeteoraDbc,
            program_id: METEORA_DBC_PROG_ID,
            candidate_pool_idxs: &[1],
            fallback: Some(("7TqH5rBfnJ8ykttxUJ1LZGQVi24mFph8nQFKCEPwyBJt", SOL, "A6QfoNh386MJjyCGrFJwvriMqUd9Yh4d7ZUr8SfSyhst")),
        },
        AmmSpec {
            label: "PumpFun",
            pool_type: PoolType::PumpFun,
            program_id: PUMP_FUN_PROG_ID,
            candidate_pool_idxs: &[3],
            fallback: None,
        },
        AmmSpec {
            label: "PumpFunAmm",
            pool_type: PoolType::PumpFunAmm,
            program_id: PUMP_FUN_AMM_PROG_ID,
            candidate_pool_idxs: &[0, 3],
            fallback: Some(("6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6", SOL, "6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz")),
        },
        AmmSpec {
            label: "FluxBeam",
            pool_type: PoolType::FluxBeam,
            program_id: FLUXBEAM_PROG_ID,
            candidate_pool_idxs: &[1, 2],
            fallback: Some(("6hrvHgqXna7i2Xck2859N8yxaiC5jboA1paBnTSi7FT4", SOL, "8rScidWjLJYNKJQPpV5EBP5jSbJV6CfrZZPkGubuu6ct")),
        },
        AmmSpec {
            label: "FlashTrade",
            pool_type: PoolType::FlashTrade,
            program_id: FLASH_TRADE_PROG_ID,
            candidate_pool_idxs: &[1, 2],
            fallback: None,
        },
        AmmSpec {
            label: "Byreal",
            pool_type: PoolType::Byreal,
            program_id: BYREAL_PROG_ID,
            candidate_pool_idxs: &[0, 1, 2],
            fallback: None,
        },
        AmmSpec {
            label: "DefiTunaFusion",
            pool_type: PoolType::DefiTunaFusion,
            program_id: DEFITUNA_FUSION_PROG_ID,
            candidate_pool_idxs: &[0, 1, 2],
            fallback: Some(("7VuKeevbvbQQcxz6N4SNLmuq6PYy4AcGQRDssoqo4t65", SOL, USDC)),
        },
        AmmSpec {
            label: "Saros",
            pool_type: PoolType::Saros,
            program_id: SAROS_PROG_ID,
            candidate_pool_idxs: &[1, 2],
            fallback: None,
        },
        AmmSpec {
            label: "PancakeSwap",
            pool_type: PoolType::PancakeSwap,
            program_id: PANCAKESWAP_PROG_ID,
            candidate_pool_idxs: &[2],
            fallback: None,
        },
        AmmSpec {
            label: "Dooar",
            pool_type: PoolType::Dooar,
            program_id: DOOAR_PROG_ID,
            candidate_pool_idxs: &[1, 2],
            fallback: Some(("5GGvkcqQ1554ibdc18JXiPqR8aJz6WV3JSNShoj32ufT", USDC, SOL)),
        },
        AmmSpec {
            label: "Pumpup",
            pool_type: PoolType::Pumpup,
            program_id: PUMPUP_PROG_ID,
            candidate_pool_idxs: &[0],
            fallback: Some(("7Q9RYYbijphbAXBV527Jz2QmgY4BXdaAzfXhJ3wT8hv1", USDT, "AnncZ1M8BbE8GVPrqJvecff4G7FzQvzpfMt4JvWddGai")),
        },
        AmmSpec {
            label: "PumpupBonding",
            pool_type: PoolType::PumpupBonding,
            program_id: PUMPUP_PROG_ID,
            // Bonding `buy`/`sell` puts pool_sol_account at accounts[3].
            candidate_pool_idxs: &[3],
            fallback: Some(("AQxKPt88jGP1DiwRbqweoo74Yi2o3fMTATAbDDA6BVLT", SOL, "9U3FcH1Z3vZFHvN5KrkHHkuJSPKKnBBLpPQ1FkezxAai")),
        },
    ]
}

/// Find a recent active pool by walking signatures for the program.
async fn discover_fresh_pool(
    rpc: &RpcClient,
    spec: &AmmSpec,
    sigs_to_scan: usize,
) -> Option<(Pubkey, Pubkey, Pubkey)> {
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

        // Build full account-key list (static + ALT-loaded).
        let (static_keys, ix_list) = match &tx.transaction.transaction {
            EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                UiMessage::Raw(raw) => (raw.account_keys.clone(), raw.instructions.clone()),
                _ => continue,
            },
            _ => continue,
        };
        let loaded = &meta.loaded_addresses;
        let mut keys: Vec<String> = static_keys;
        if let OptionSerializer::Some(la) = loaded {
            keys.extend(la.writable.iter().cloned());
            keys.extend(la.readonly.iter().cloned());
        }

        // Mints involved in the tx — used to pick base/quote.
        let mut mints: Vec<Pubkey> = Vec::new();
        if let OptionSerializer::Some(post) =
            &meta.post_token_balances
        {
            for tb in post {
                if let Ok(m) = tb.mint.parse::<Pubkey>() {
                    if !mints.contains(&m) {
                        mints.push(m);
                    }
                }
            }
        }
        if let OptionSerializer::Some(pre) =
            &meta.pre_token_balances
        {
            for tb in pre {
                if let Ok(m) = tb.mint.parse::<Pubkey>() {
                    if !mints.contains(&m) {
                        mints.push(m);
                    }
                }
            }
        }

        // Walk top-level + inner instructions.
        let mut all_ixs: Vec<solana_transaction_status_client_types::UiCompiledInstruction> =
            ix_list;
        if let OptionSerializer::Some(ii_list) =
            &meta.inner_instructions
        {
            for ii in ii_list {
                for ux in &ii.instructions {
                    if let UiInstruction::Compiled(c) = ux {
                        all_ixs.push(c.clone());
                    }
                }
            }
        }

        let prog_str = spec.program_id.to_string();
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
                // Verify the account is owned by the program.
                let ai = match rpc.get_account(&pool).await {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                if ai.owner != spec.program_id {
                    continue;
                }
                // Verify our parser actually accepts this account. Some
                // programs have multiple ix shapes that put unrelated
                // PDAs at the same index (e.g. MeteoraDbc `swap` vs
                // `migrate_pool`). Catching parse failure here filters
                // those out and lets us try the next signature.
                if fetcher::fetch_pool_state(rpc, spec.pool_type, &pool)
                    .await
                    .is_err()
                {
                    continue;
                }
                // Pick a quote mint (prefer SOL > USDC > USDT > PYUSD) and a
                // base mint from the tx's token-balance entries.
                let mut quote: Option<Pubkey> = None;
                let mut base: Option<Pubkey> = None;
                for m in &mints {
                    if is_quote_mint(m) {
                        if quote.is_none() || m.to_string() == SOL {
                            quote = Some(*m);
                        }
                    } else if base.is_none() {
                        base = Some(*m);
                    }
                }
                let quote = quote.unwrap_or_else(|| pk(SOL));
                let base = match base {
                    Some(b) => b,
                    None => continue,
                };
                return Some((pool, quote, base));
            }
        }
    }
    None
}

/// Run fetch → build IX → build TX → simulate against one (PoolType, pool, quote, base).
async fn run_full_pipeline(
    rpc: &RpcClient,
    user: Pubkey,
    pool_type: PoolType,
    pool: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
) -> Result<String, String> {
    let fetch_start = Instant::now();
    let pool_state = fetcher::fetch_pool_state(rpc, pool_type, &pool)
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    let fetch_ms = fetch_start.elapsed().as_millis();

    let input_tp = get_mint_token_program(rpc, &input_mint)
        .await
        .unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, &output_mint)
        .await
        .unwrap_or(TOKEN_PROGRAM_ID);

    let executor = AmmExecutorType::from_pool_type(pool_type)
        .map_err(|e| format!("executor: {e}"))?;

    // Use a sensible amount: 0.001 SOL for SOL-input, 0.05 USDT for stable-input.
    let amount_in = if input_mint == SOL_NATIVE_MINT {
        1_000_000u64
    } else {
        50_000u64
    };

    let order = SwapOrder {
        pool_address: pool,
        pool_type,
        input_mint,
        output_mint,
        amount_in,
        min_amount_out: 1,
        user,
        input_token_program: input_tp,
        output_token_program: output_tp,
    };

    let build_start = Instant::now();
    let ixs = executor
        .build_swap_ix(&order, &pool_state)
        .map_err(|e| format!("build_ix: {e}"))?;
    let build_us = build_start.elapsed().as_micros();
    let ix_count = format!("{}+{}+{}", ixs.setup.len(), ixs.swap.len(), ixs.cleanup.len());

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| format!("blockhash: {e}"))?;
    let cfg = TxBuildConfig {
        compute_unit_limit: 400_000,
        priority_fee_lamports: 5_000,
    };
    let vtx = build_unsigned_versioned_tx(&ixs, &user, &cfg, blockhash, &[])
        .map_err(|e| format!("build_tx: {e}"))?;
    let tx_bytes = bincode::serialize(&vtx).map_err(|e| format!("serialize: {e}"))?;

    let sim_cfg = solana_client::rpc_config::RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };
    let sim = rpc
        .simulate_transaction_with_config(&vtx, sim_cfg)
        .await
        .map_err(|e| format!("simulate RPC: {e}"))?;
    let cu = sim.value.units_consumed.unwrap_or(0);
    let sim_status = match &sim.value.err {
        Some(e) => format!("ProgramError({:?})", e),
        None => "Passed".to_string(),
    };

    Ok(format!(
        "fetch={}ms build={}µs ix={} tx={}B sim={} cu={}",
        fetch_ms, build_us, ix_count, tx_bytes.len(), sim_status, cu
    ))
}

#[tokio::test]
async fn test_fresh_market_validation() {
    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let bal = rpc.get_balance(&user).await.unwrap_or(0);

    eprintln!("\n+-{:-<88}-+", " FRESH-POOL VALIDATION ");
    eprintln!("| wallet:  {}", user);
    eprintln!("| balance: {} lamports ({:.6} SOL)", bal, bal as f64 / 1e9);
    eprintln!("| AMMs:    {}", specs().len());
    eprintln!("+-{:-<88}-+", "");

    let mut pass = 0;
    let mut sim_passed = 0;
    let mut fail = 0;
    let mut skip = 0;

    eprintln!(
        "\n  {:<16} | {:>8} | {:<44} | {}",
        "AMM", "Source", "Pool", "Result"
    );
    eprintln!("  {:-<16}-+-{:-<8}-+-{:-<44}-+-{:-<70}", "", "", "", "");

    for spec in specs() {
        // Try fresh discovery first; fall back to hardcoded pool.
        let (pool, input_mint, output_mint, source) = match discover_fresh_pool(&rpc, &spec, 30).await {
            Some((p, q, b)) => (p, q, b, "fresh"),
            None => match spec.fallback {
                Some((p, q, b)) => (pk(p), pk(q), pk(b), "fallback"),
                None => {
                    eprintln!(
                        "  {:<16} | {:<8} | {:<44} | NO POOL FOUND (skip)",
                        spec.label, "—", "—"
                    );
                    skip += 1;
                    continue;
                }
            },
        };

        match run_full_pipeline(&rpc, user, spec.pool_type, pool, input_mint, output_mint).await {
            Ok(result) => {
                eprintln!("  {:<16} | {:<8} | {:<44} | {}", spec.label, source, pool, result);
                pass += 1;
                if result.contains("sim=Passed") {
                    sim_passed += 1;
                }
            }
            Err(e) => {
                eprintln!("  {:<16} | {:<8} | {:<44} | FAIL: {}", spec.label, source, pool, e);
                fail += 1;
            }
        }
    }

    eprintln!("\n+-{:-<88}-+", " SUMMARY ");
    eprintln!(
        "| {} pipeline-pass / {} simulate-pass / {} skip / {} fail (of {})",
        pass, sim_passed, skip, fail, specs().len()
    );
    eprintln!("+-{:-<88}-+", "");

    assert_eq!(fail, 0, "no AMMs should hard-fail the pipeline");
    assert!(
        pass >= 17,
        "at least 17 of {} AMMs should complete the pipeline",
        specs().len()
    );
}
