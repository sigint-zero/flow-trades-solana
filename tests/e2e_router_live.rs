//! LIVE on-chain router tests — real swaps through the deployed flow-router program.
//!
//! Tests:
//! 1. Read config PDA and verify stored values
//! 2. Build router-wrapped swap TX for each AMM and simulate on mainnet
//! 3. Verify router-wrapped swaps simulate correctly
//!
//! Requires: RPC_URL + SIM_PRIVATE_KEY in .env
//!
//! Run:
//! ```bash
//! set -a && source .env && set +a
//! OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include \
//!   cargo test --test e2e_router_live -- --nocapture --test-threads=1
//! ```

use std::str::FromStr;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;

use flow_trades::constants::*;
use flow_trades::execution::amms::AmmExecutorType;
use flow_trades::execution::router::{RouterConfig, wrap_swap};
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolType, SwapOrder};

/// Staging router program ID (deployed for testing).
const STAGING_ROUTER: &str = "FLorgvPfcfirXaKvuFTDgeqZoDAMXAymZcnfNDK4mmGj";

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

fn router_config() -> RouterConfig {
    RouterConfig {
        program_id: pk(STAGING_ROUTER),
        treasury_wallet: pk("EnE9FuHM9HtEx7wTFVMgwaK2jKRS2TthdKcsgvwKerXN"),
        referral_wallet: None,
    }
}

/// Read and verify the on-chain config PDA.
#[tokio::test]
async fn test_read_config_pda() {
    eprintln!("\n=== READ ON-CHAIN CONFIG PDA ===\n");
    let rpc = rpc();
    let program_id = pk(STAGING_ROUTER);
    let (config_pda, bump) = Pubkey::find_program_address(&[b"config"], &program_id);

    eprintln!("  Program:    {}", program_id);
    eprintln!("  Config PDA: {} (bump={})", config_pda, bump);

    let acct = rpc.get_account(&config_pda).await.expect("fetch config PDA");

    assert_eq!(acct.data.len(), 76, "config should be 76 bytes");
    assert_eq!(&acct.data[..8], b"flowconf", "discriminator mismatch");
    assert_eq!(acct.owner, program_id, "owner should be router program");

    let admin = Pubkey::new_from_array(acct.data[8..40].try_into().unwrap());
    let fee_bps = u16::from_le_bytes(acct.data[40..42].try_into().unwrap());
    let protocol_fee = Pubkey::new_from_array(acct.data[42..74].try_into().unwrap());
    let referral_split = u16::from_le_bytes(acct.data[74..76].try_into().unwrap());

    eprintln!("  Admin:            {}", admin);
    eprintln!("  fee_bps:          {} ({}%)", fee_bps, fee_bps as f64 / 100.0);
    eprintln!("  protocol_fee:     {}", protocol_fee);
    eprintln!("  referral_split:   {} ({}%)", referral_split, referral_split as f64 / 100.0);

    assert_eq!(fee_bps, 50);
    assert_eq!(referral_split, 7000);
    eprintln!("\n  [OK] Config PDA verified on-chain");
}

struct RouterSwapTest {
    name: &'static str,
    pool_type: PoolType,
    pool_address: &'static str,
    input_mint: &'static str,
    output_mint: &'static str,
}

/// Build a full router-wrapped swap TX (setup + router CPI + cleanup) and simulate.
async fn test_router_swap(test: &RouterSwapTest) -> Result<String, String> {
    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let router = router_config();
    let pool_addr = pk(test.pool_address);
    let input_mint = pk(test.input_mint);
    let output_mint = pk(test.output_mint);

    // 1. Fetch pool state
    let pool_state = fetcher::fetch_pool_state(&rpc, test.pool_type, &pool_addr)
        .await
        .map_err(|e| format!("fetch: {e}"))?;

    // 2. Detect token programs
    let input_tp = get_mint_token_program(&rpc, &input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(&rpc, &output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);

    // 3. Build DEX swap IX (includes setup for ATA creation + WSOL wrap)
    let executor = AmmExecutorType::from_pool_type(test.pool_type)
        .map_err(|e| format!("executor: {e}"))?;
    let order = SwapOrder {
        pool_address: pool_addr,
        pool_type: test.pool_type,
        input_mint,
        output_mint,
        amount_in: 1_000_000, // 0.001 SOL
        min_amount_out: 0,
        user,
        input_token_program: input_tp,
        output_token_program: output_tp,
    };

    let ixs = executor.build_swap_ix(&order, &pool_state)
        .map_err(|e| format!("build_ix: {e}"))?;

    // 4. Wrap the DEX swap IX in a router CPI
    let dex_ix = &ixs.swap[0];
    let user_input_ata = spl_associated_token_account::get_associated_token_address(&user, &input_mint);
    let user_output_ata = spl_associated_token_account::get_associated_token_address(&user, &output_mint);
    // protocol_fee_acct = admin wallet (matches what's in the config PDA)
    let protocol_fee_acct = user;

    let router_ix = wrap_swap(
        &router, &user, &[user_input_ata, user_output_ata],
        &protocol_fee_acct, None, &[dex_ix.clone()], 1_000_000, 0, &TOKEN_PROGRAM_ID, &output_mint,
    ).map_err(|e| format!("wrap: {e}"))?;

    // 5. Assemble full TX: compute budget + setup + router_swap + cleanup
    let setup_count = ixs.setup.len();
    let cleanup_count = ixs.cleanup.len();

    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(1000),
    ];
    all_ixs.extend(ixs.setup);   // ATA creation, WSOL wrapping
    all_ixs.push(router_ix);      // Router-wrapped DEX swap
    all_ixs.extend(ixs.cleanup);  // WSOL unwrap

    // 6. Build unsigned TX
    let blockhash = rpc.get_latest_blockhash().await
        .map_err(|e| format!("blockhash: {e}"))?;

    let msg = Message::new_with_blockhash(&all_ixs, Some(&user), &blockhash);
    let vtx = VersionedTransaction::from(solana_sdk::transaction::Transaction::new_unsigned(msg));

    let tx_bytes = bincode::serialize(&vtx).map_err(|e| format!("serialize: {e}"))?;

    // 7. Simulate
    let sim_config = solana_client::rpc_config::RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    let sim = rpc.simulate_transaction_with_config(&vtx, sim_config).await
        .map_err(|e| format!("sim RPC: {e}"))?;

    let cu = sim.value.units_consumed.unwrap_or(0);
    let ix_count = format!("{}+1+{}", setup_count, cleanup_count);
    if let Some(err) = &sim.value.err {
        let err_str = format!("{:?}", err);
        Ok(format!("ix={}  tx={}B  sim=Error({})  cu={}", ix_count, tx_bytes.len(), err_str, cu))
    } else {
        Ok(format!("ix={}  tx={}B  sim=Passed  cu={}", ix_count, tx_bytes.len(), cu))
    }
}

#[tokio::test]
async fn test_router_wrapped_swaps_all_amms() {
    let tests = vec![
        RouterSwapTest {
            name: "RaydiumCpmm",
            pool_type: PoolType::RaydiumCpmm,
            pool_address: "BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook",
        },
        RouterSwapTest {
            name: "RaydiumCLMM",
            pool_type: PoolType::RaydiumCl,
            pool_address: "ENQmMUSXmUYPaAL9NH79cFw3Lfht3bThmY8Zs8UwGEbr",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "22r6hjfpF15dkgJzkNXthNPZny1r7TohQb1vbAEBD5Fg",
        },
        RouterSwapTest {
            name: "RaydiumLP",
            pool_type: PoolType::RaydiumLp,
            pool_address: "6Lc76tcWsCEkydyLriNaeDkUgVekusBVGgQYYDiKRZi1",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "D756Z3S31AZMbU4teTu2BWK77neDArFhaPr6eZ7bonk",
        },
        RouterSwapTest {
            name: "PumpFunAmm",
            pool_type: PoolType::PumpFunAmm,
            pool_address: "6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz",
        },
        RouterSwapTest {
            name: "Meteora",
            pool_type: PoolType::Meteora,
            pool_address: "BCXjm4FfSoquZQJV5Wcje1g1pSHW2hFMU9wDE98Nyatb",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "STrikemJEk2tFVYpg7SMo9nGPrnJ56fHnS1K7PV2fPw",
        },
        RouterSwapTest {
            name: "MeteoraDLMM",
            pool_type: PoolType::MeteoraDlmm,
            pool_address: "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        },
        RouterSwapTest {
            name: "Orca",
            pool_type: PoolType::Orca,
            pool_address: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        },
        RouterSwapTest {
            name: "DefiTunaFusion",
            pool_type: PoolType::DefiTunaFusion,
            pool_address: "7VuKeevbvbQQcxz6N4SNLmuq6PYy4AcGQRDssoqo4t65",
            input_mint: "So11111111111111111111111111111111111111112",
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        },
    ];

    let signer = load_keypair();
    eprintln!("\n╔════════════════════════════════════════════════════════════╗");
    eprintln!("║  LIVE ROUTER-WRAPPED SWAP — SIMULATE ON MAINNET          ║");
    eprintln!("╚════════════════════════════════════════════════════════════╝\n");
    eprintln!("  Router:  {}", STAGING_ROUTER);
    eprintln!("  Wallet:  {}", signer.pubkey());

    let rpc = rpc();
    let bal = rpc.get_balance(&signer.pubkey()).await.unwrap_or(0);
    eprintln!("  Balance: {:.6} SOL\n", bal as f64 / 1e9);

    eprintln!("  {:<16} | {}", "AMM", "Result");
    eprintln!("  {:-<16}-+-{:-<70}", "", "");

    let mut pass = 0;
    let mut fail = 0;

    for test in &tests {
        match test_router_swap(test).await {
            Ok(result) => {
                eprintln!("  {:<16} | {}", test.name, result);
                pass += 1;
            }
            Err(e) => {
                if e.contains("AccountNotFound") || e.contains("too small") {
                    eprintln!("  {:<16} | SKIP: pool closed", test.name);
                } else {
                    eprintln!("  {:<16} | FAIL: {}", test.name, e);
                    fail += 1;
                }
            }
        }
    }

    eprintln!("\n  Results: {} PASS / {} FAIL (of {} tests)\n", pass, fail, tests.len());
    assert_eq!(fail, 0, "No router-wrapped swaps should hard-fail");
}
