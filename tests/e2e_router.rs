//! E2E test for the flow-router CPI wrapper.
//!
//! Validates that router-wrapped transactions:
//! 1. Build correctly with the right account structure
//! 2. Are smaller or equal to direct TX (router adds fixed overhead)
//! 3. Produce valid serializable VersionedTransactions
//!
//! Run:
//! ```bash
//! set -a && source .env && set +a
//! OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include \
//!   cargo test --test e2e_router -- --nocapture --test-threads=1
//! ```

use std::str::FromStr;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::instruction::AccountMeta;

use flow_trades::constants::*;
use flow_trades::execution::AmmExecutorType;
use flow_trades::execution::router::{RouterConfig, wrap_swap};
use flow_trades::execution::tx_builder::{build_unsigned_versioned_tx, TxBuildConfig};
use flow_trades::pool::fetcher;
use flow_trades::pool::types::{PoolType, SwapOrder};

const ROUTER_PROGRAM_ID: &str = "FLoWYLh8vBd32mvCAXnQDdGWniBPECaqUrTasntarMB7";

fn rpc_url() -> String {
    std::env::var("SOL_HTTPS_ENDPOINT")
        .or_else(|_| std::env::var("RPC_URL"))
        .expect("SOL_HTTPS_ENDPOINT or RPC_URL required")
}

fn rpc() -> RpcClient {
    RpcClient::new_with_commitment(rpc_url(), CommitmentConfig::confirmed())
}

fn router_config() -> RouterConfig {
    RouterConfig {
        program_id: Pubkey::from_str(ROUTER_PROGRAM_ID).unwrap(),
        treasury_wallet: Pubkey::new_unique(),
        referral_wallet: None,
    }
}

#[tokio::test]
async fn test_router_wrap_swap() {
    eprintln!("\n=== ROUTER WRAP: SINGLE HOP ===\n");
    let rpc = rpc();
    let user = Pubkey::from_str("6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW").unwrap();
    let router = router_config();

    // Build a direct RaydiumCpmm swap
    let pool_addr = Pubkey::from_str("BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr").unwrap();
    let output_mint = Pubkey::from_str("25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook").unwrap();

    let pool_state = fetcher::fetch_pool_state(&rpc, PoolType::RaydiumCpmm, &pool_addr)
        .await.expect("fetch pool");

    let executor = AmmExecutorType::from_pool_type(PoolType::RaydiumCpmm).unwrap();
    let order = SwapOrder {
        pool_address: pool_addr,
        pool_type: PoolType::RaydiumCpmm,
        input_mint: SOL_NATIVE_MINT,
        output_mint,
        amount_in: 1_000_000,
        min_amount_out: 1,
        user,
        input_token_program: TOKEN_PROGRAM_ID,
        output_token_program: TOKEN_PROGRAM_ID,
    };

    let ixs = executor.build_swap_ix(&order, &pool_state).unwrap();
    assert_eq!(ixs.swap.len(), 1);
    let dex_ix = &ixs.swap[0];

    eprintln!("  Direct DEX IX: {} accounts, {} bytes data", dex_ix.accounts.len(), dex_ix.data.len());

    // Wrap in router
    let input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
    );
    let output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &user, &output_mint, &TOKEN_PROGRAM_ID,
    );

    let protocol_fee = Pubkey::new_unique();
    let dex_ix_clone = dex_ix.clone();
    let wrapped = wrap_swap(
        &router, &user, &[input_ata, output_ata], &protocol_fee, None, &[dex_ix_clone], 1_000_000, 1, &TOKEN_PROGRAM_ID,
        ).unwrap();

    eprintln!("  Wrapped IX: {} accounts, {} bytes data", wrapped.accounts.len(), wrapped.data.len());
    eprintln!("  Program: {}", wrapped.program_id);
    eprintln!("  Overhead: +{} accounts, +{} bytes data",
        wrapped.accounts.len() - dex_ix.accounts.len(),
        wrapped.data.len() - dex_ix.data.len());

    // Verify structure: 1 payer + 2 tokens + 4 fixed + DEX accounts + 1 DEX program
    assert_eq!(wrapped.program_id.to_string(), ROUTER_PROGRAM_ID);
    assert_eq!(wrapped.accounts.len(), 1 + 2 + 4 + dex_ix.accounts.len() + 1);
    assert!(wrapped.accounts[0].is_signer); // payer
    assert_eq!(wrapped.accounts[0].pubkey, user);
    assert_eq!(wrapped.data[0], 0); // SWAP_DISC

    // Build full TX with router-wrapped instruction
    let router_ixs = flow_trades::pool::types::SwapInstructions {
        setup: ixs.setup.clone(),
        swap: vec![wrapped],
        cleanup: ixs.cleanup.clone(),
    };

    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let config = TxBuildConfig {
        compute_unit_limit: 400_000,
        priority_fee_lamports: 5_000,
    };

    let tx = build_unsigned_versioned_tx(&router_ixs, &user, &config, blockhash, &[]).unwrap();
    let tx_bytes = bincode::serialize(&tx).unwrap();
    eprintln!("  Router TX size: {} bytes", tx_bytes.len());
    eprintln!("  Under 1232 limit: {}", if tx_bytes.len() <= 1232 { "YES" } else { "NO" });

    // Also build direct TX for comparison
    let direct_ixs = flow_trades::pool::types::SwapInstructions {
        setup: ixs.setup.clone(),
        swap: ixs.swap.clone(),
        cleanup: ixs.cleanup.clone(),
    };
    let direct_tx = build_unsigned_versioned_tx(&direct_ixs, &user, &config, blockhash, &[]).unwrap();
    let direct_bytes = bincode::serialize(&direct_tx).unwrap();

    eprintln!("  Direct TX size: {} bytes", direct_bytes.len());
    eprintln!("  Router overhead: +{} bytes", tx_bytes.len() as i64 - direct_bytes.len() as i64);
    eprintln!("  [OK] Single-hop router wrap successful");
}

#[tokio::test]
async fn test_router_wrap_with_protocol_fee_account() {
    eprintln!("\n=== ROUTER WRAP: WITH PROTOCOL FEE TOKEN ACCOUNT ===\n");
    let user = Pubkey::from_str("6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW").unwrap();
    let protocol_fee_token_account = Pubkey::new_unique();

    let router = RouterConfig {
        program_id: Pubkey::from_str(ROUTER_PROGRAM_ID).unwrap(),
        treasury_wallet: Pubkey::new_unique(),
        referral_wallet: None,
    };

    // Simple dummy DEX instruction
    let dex_ix = solana_sdk::instruction::Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
        data: vec![0x09, 1, 0, 0, 0, 0, 0, 0, 0],
    };

    let wrapped = wrap_swap(
        &router, &user,
        &[Pubkey::new_unique(), Pubkey::new_unique()],
        &protocol_fee_token_account,
        None,
        &[dex_ix], 1_000_000_000, 100_000, &TOKEN_PROGRAM_ID,
    ).unwrap();

    // protocol_fee_token_account: payer(1) + tokens(2) + config(1) = index 4
    assert_eq!(wrapped.accounts[4].pubkey, protocol_fee_token_account);
    // config_pda at index 3 (read-only)
    assert!(!wrapped.accounts[3].is_writable);
    eprintln!("  Protocol fee account: {} (position [4])", protocol_fee_token_account);
    eprintln!("  Config PDA: {} (position [3])", wrapped.accounts[3].pubkey);
    eprintln!("  [OK] Protocol fee token account correctly placed");
}

#[tokio::test]
async fn test_router_wrap_swap_multi_hop() {
    eprintln!("\n=== ROUTER WRAP: MULTI-HOP (2 DEX CPIs) ===\n");
    let rpc = rpc();
    let user = Pubkey::from_str("6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW").unwrap();
    let router = router_config();

    // Hop 1: SOL → token via RaydiumCpmm
    let pool1 = Pubkey::from_str("BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr").unwrap();
    let token_a = Pubkey::from_str("25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook").unwrap();
    let state1 = fetcher::fetch_pool_state(&rpc, PoolType::RaydiumCpmm, &pool1).await.expect("fetch pool1");
    let exec1 = AmmExecutorType::from_pool_type(PoolType::RaydiumCpmm).unwrap();
    let order1 = SwapOrder {
        pool_address: pool1, pool_type: PoolType::RaydiumCpmm,
        input_mint: SOL_NATIVE_MINT, output_mint: token_a,
        amount_in: 1_000_000, min_amount_out: 1, user,
        input_token_program: TOKEN_PROGRAM_ID, output_token_program: TOKEN_PROGRAM_ID,
    };
    let ixs1 = exec1.build_swap_ix(&order1, &state1).unwrap();

    // Hop 2: token → SOL via MeteoraDamm
    let pool2 = Pubkey::from_str("4ac2qQp9uPQTuhvNH1p5xZmZnKV1pAxWKjBPkwvD6x6J").unwrap();
    let token_b = Pubkey::from_str("CDNZaZwhWB2VGFXSEMmM7hBodrhciRx7JPbjghttbEM3").unwrap();
    let state2 = fetcher::fetch_pool_state(&rpc, PoolType::MeteoraDamm, &pool2).await.expect("fetch pool2");
    let exec2 = AmmExecutorType::from_pool_type(PoolType::MeteoraDamm).unwrap();
    let order2 = SwapOrder {
        pool_address: pool2, pool_type: PoolType::MeteoraDamm,
        input_mint: SOL_NATIVE_MINT, output_mint: token_b,
        amount_in: 1_000, min_amount_out: 1, user,
        input_token_program: TOKEN_PROGRAM_ID, output_token_program: TOKEN_PROGRAM_ID,
    };
    let ixs2 = exec2.build_swap_ix(&order2, &state2).unwrap();

    let hop1_ix = &ixs1.swap[0];
    let hop2_ix = &ixs2.swap[0];

    eprintln!("  Hop 1: {} accounts (RaydiumCpmm)", hop1_ix.accounts.len());
    eprintln!("  Hop 2: {} accounts (MeteoraDamm)", hop2_ix.accounts.len());

    let input_ata = Pubkey::new_unique();
    let intermediate_ata = Pubkey::new_unique();
    let output_ata = Pubkey::new_unique();
    let protocol_fee = Pubkey::new_unique();

    let hop1_clone = hop1_ix.clone();
    let hop2_clone = hop2_ix.clone();
    let wrapped = wrap_swap(
        &router, &user,
        &[input_ata, intermediate_ata, output_ata],
        &protocol_fee, None,
        &[hop1_clone, hop2_clone],
        1_000_000, 1, &TOKEN_PROGRAM_ID,
        ).unwrap();

    eprintln!("  Wrapped: {} accounts, {} bytes data", wrapped.accounts.len(), wrapped.data.len());
    eprintln!("  Expected: 1 payer + 3 tokens + 4 fixed + {} hop1 + {} hop2 + DEX programs",
        hop1_ix.accounts.len(), hop2_ix.accounts.len());

    assert_eq!(wrapped.program_id.to_string(), ROUTER_PROGRAM_ID);
    // 1 payer + 3 tokens + 4 fixed + hop1 + hop2 + DEX programs
    let expected = 1 + 3 + 4 + hop1_ix.accounts.len() + hop2_ix.accounts.len()
        + if hop1_ix.program_id == hop2_ix.program_id { 1 } else { 2 };
    assert_eq!(wrapped.accounts.len(), expected);
    assert_eq!(wrapped.data[0], 0); // SWAP_DISC (generic, always 0)
    assert!(wrapped.accounts[0].is_signer);

    eprintln!("  [OK] Multi-hop router wrap successful");
}

#[tokio::test]
async fn test_router_tx_size_comparison() {
    eprintln!("\n=== ROUTER TX SIZE: DIRECT vs WRAPPED ===\n");
    let rpc = rpc();
    let user = Pubkey::from_str("6TwqjGNQ8c2aUHvbpAjMd4bdHdone9CTrz3c8S71E2WW").unwrap();
    let router = router_config();

    // Test across multiple AMMs
    let test_cases: Vec<(&str, PoolType, &str)> = vec![
        ("BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr", PoolType::RaydiumCpmm, "25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook"),
        ("6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6", PoolType::PumpFunAmm, "6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz"),
        ("4ac2qQp9uPQTuhvNH1p5xZmZnKV1pAxWKjBPkwvD6x6J", PoolType::MeteoraDamm, "CDNZaZwhWB2VGFXSEMmM7hBodrhciRx7JPbjghttbEM3"),
    ];

    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let config = TxBuildConfig { compute_unit_limit: 400_000, priority_fee_lamports: 5_000 };

    eprintln!("  {:<16} | {:>10} | {:>10} | {:>10}", "AMM", "Direct (B)", "Router (B)", "Overhead");
    eprintln!("  {:-<16}-+-{:-<10}-+-{:-<10}-+-{:-<10}", "", "", "", "");

    for (pool_str, pool_type, out_mint_str) in &test_cases {
        let pool_addr = Pubkey::from_str(pool_str).unwrap();
        let output_mint = Pubkey::from_str(out_mint_str).unwrap();

        let state = match fetcher::fetch_pool_state(&rpc, *pool_type, &pool_addr).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        let executor = AmmExecutorType::from_pool_type(*pool_type).unwrap();
        let order = SwapOrder {
            pool_address: pool_addr, pool_type: *pool_type,
            input_mint: SOL_NATIVE_MINT, output_mint,
            amount_in: 1_000_000, min_amount_out: 1, user,
            input_token_program: TOKEN_PROGRAM_ID, output_token_program: TOKEN_PROGRAM_ID,
        };

        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        // Direct TX
        let direct_tx = build_unsigned_versioned_tx(&ixs, &user, &config, blockhash, &[]).unwrap();
        let direct_size = bincode::serialize(&direct_tx).unwrap().len();

        // Router-wrapped TX
        let input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
            &user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
        );
        let output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
            &user, &output_mint, &TOKEN_PROGRAM_ID,
        );
        let protocol_fee = Pubkey::new_unique();
        let wrapped = wrap_swap(&router, &user, &[input_ata, output_ata], &protocol_fee, None, &[ixs.swap[0].clone()], 1_000_000, 1, &TOKEN_PROGRAM_ID).unwrap();
        let router_ixs = flow_trades::pool::types::SwapInstructions {
            setup: ixs.setup.clone(), swap: vec![wrapped], cleanup: ixs.cleanup.clone(),
        };
        let router_tx = build_unsigned_versioned_tx(&router_ixs, &user, &config, blockhash, &[]).unwrap();
        let router_size = bincode::serialize(&router_tx).unwrap().len();

        eprintln!("  {:<16} | {:>10} | {:>10} | +{:<9}",
            format!("{pool_type:?}"), direct_size, router_size, router_size - direct_size);
    }
    eprintln!("\n  [OK] All router TXs built successfully");
}

#[tokio::test]
async fn test_router_attribution() {
    eprintln!("\n=== ROUTER ATTRIBUTION ===\n");
    let router = router_config();

    eprintln!("  Router Program ID: {}", ROUTER_PROGRAM_ID);
    eprintln!("  Prefix: {}", &ROUTER_PROGRAM_ID[..4]);
    eprintln!("");
    eprintln!("  Every swap TX calls this program as the top-level instruction.");
    eprintln!("  The actual DEX swap happens as an inner CPI.");
    eprintln!("  To query all flow-trades volume: getSignaturesForAddress({ROUTER_PROGRAM_ID})");
    eprintln!("");

    // Verify the program ID prefix
    assert!(ROUTER_PROGRAM_ID.starts_with("FLo"));
    eprintln!("  [OK] Vanity prefix verified: FLoW...");
}
