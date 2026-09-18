//! REAL MAINNET SWAP — actually submits a transaction through the production router.
//! This spends real SOL. Run manually only.
//!
//! Requires: RPC_URL + SIM_PRIVATE_KEY

use std::str::FromStr;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{Transaction, VersionedTransaction},
};

use flow_trades::constants::*;
use flow_trades::execution::amms::AmmExecutorType;
use flow_trades::execution::router::{wrap_swap, RouterConfig};
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolType, SwapOrder};

const ROUTER: &str = "FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm";
const TREASURY: &str = "2yL7tWs2TULhicDtdDV7A8P8Agh79EwCFLKeyKL5fMr3";

fn rpc() -> RpcClient {
    let url = std::env::var("RPC_URL").expect("RPC_URL required");
    RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())
}

fn load_keypair() -> Keypair {
    let b58 = std::env::var("SIM_PRIVATE_KEY").expect("SIM_PRIVATE_KEY required");
    Keypair::from_bytes(&bs58::decode(b58.trim()).into_vec().unwrap()).unwrap()
}

fn pk(s: &str) -> Pubkey { Pubkey::from_str(s).unwrap() }

#[tokio::test]
async fn test_real_swap_single_hop() {
    eprintln!("\n╔══════════════════════════════════════════════════════╗");
    eprintln!("║  REAL MAINNET SWAP — 0.001 SOL via PumpFunAmm        ║");
    eprintln!("║  Through production router with fee collection        ║");
    eprintln!("╚══════════════════════════════════════════════════════╝\n");

    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let router = RouterConfig {
        program_id: pk(ROUTER),
        treasury_wallet: pk(TREASURY),
        referral_wallet: None,
    };

    let input_mint = SOL_NATIVE_MINT;
    let output_mint = pk("6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz");

    eprintln!("  Wallet:    {}", user);
    eprintln!("  Router:    {}", ROUTER);
    eprintln!("  Treasury:  {}", TREASURY);
    eprintln!("  Swap:      0.001 SOL → 6FH1 token via PumpFunAmm");

    let bal_before = rpc.get_balance(&user).await.unwrap();
    eprintln!("  Balance:   {:.6} SOL", bal_before as f64 / 1e9);

    // Fetch pool
    let pool_addr = pk("6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6");
    let pool_state = fetcher::fetch_pool_state(&rpc, PoolType::PumpFunAmm, &pool_addr).await
        .expect("fetch pool");

    let output_tp = get_mint_token_program(&rpc, &output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);

    // Build DEX IX
    let executor = AmmExecutorType::from_pool_type(PoolType::PumpFunAmm).unwrap();
    let order = SwapOrder {
        pool_address: pool_addr,
        pool_type: PoolType::PumpFunAmm,
        input_mint, output_mint,
        amount_in: 1_000_000, // 0.001 SOL
        min_amount_out: 0,
        user,
        input_token_program: TOKEN_PROGRAM_ID,
        output_token_program: output_tp,
    };
    let ixs = executor.build_swap_ix(&order, &pool_state).expect("build ix");

    // Build router-wrapped TX
    let user_input_ata = spl_associated_token_account::get_associated_token_address(&user, &input_mint);
    let user_output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user, &output_mint, &output_tp);
    let protocol_fee_acct = router.fee_account_for_mint(&output_mint, &output_tp);

    let create_fee_ata = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        &user, &router.treasury_wallet, &output_mint, &output_tp,
    );
    let create_output_ata = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        &user, &user, &output_mint, &output_tp,
    );

    let router_ix = wrap_swap(
        &router, &user, &[user_input_ata, user_output_ata],
        &protocol_fee_acct, None, &[ixs.swap[0].clone()],
        1_000_000, 0, &output_tp, &output_mint,
    ).expect("wrap_swap");

    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(50_000),
        create_fee_ata,
        create_output_ata,
    ];
    all_ixs.extend(ixs.setup);
    all_ixs.push(router_ix);
    all_ixs.extend(ixs.cleanup);

    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let msg = solana_sdk::message::Message::new(&all_ixs, Some(&user));
    let tx = Transaction::new(&[&signer], msg, blockhash);

    let tx_bytes = bincode::serialize(&tx).unwrap().len();
    eprintln!("  TX size:   {} bytes", tx_bytes);
    assert!(tx_bytes <= 1232, "TX too large: {}", tx_bytes);

    // SUBMIT FOR REAL
    eprintln!("  Submitting...");
    match rpc.send_and_confirm_transaction(&tx).await {
        Ok(sig) => {
            eprintln!("  TX CONFIRMED: {}", sig);

            let bal_after = rpc.get_balance(&user).await.unwrap();
            let spent = bal_before.saturating_sub(bal_after);
            eprintln!("  SOL spent: {:.6} (includes rent + fees + swap)", spent as f64 / 1e9);

            // Check if treasury got fee tokens
            match rpc.get_token_account_balance(&protocol_fee_acct).await {
                Ok(bal) => eprintln!("  Treasury fee balance: {} tokens", bal.ui_amount_string),
                Err(_) => eprintln!("  Treasury fee ATA: not yet funded (first swap creates it)"),
            }

            eprintln!("\n  [OK] REAL SWAP CONFIRMED ON MAINNET");
        }
        Err(e) => {
            eprintln!("  TX FAILED: {}", e);
            eprintln!("\n  [FAIL] Swap did not land");
            panic!("Real swap failed: {}", e);
        }
    }
}
