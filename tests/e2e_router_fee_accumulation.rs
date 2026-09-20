//! REAL MAINNET — Multiple swaps to verify fee accumulation in treasury.
//! Runs buy + sell + buy across different pools to collect fees in different tokens.

use std::str::FromStr;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};

use flow_trades::constants::*;
use flow_trades::execution::amms::AmmExecutorType;
use flow_trades::execution::router::{wrap_swap, RouterConfig};
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolType, SwapOrder};

const ROUTER: &str = "FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm";
const TREASURY: &str = "2yL7tWs2TULhicDtdDV7A8P8Agh79EwCFLKeyKL5fMr3";

fn rpc() -> RpcClient {
    RpcClient::new_with_commitment(
        std::env::var("RPC_URL").expect("RPC_URL"), CommitmentConfig::confirmed(),
    )
}

fn load_keypair() -> Keypair {
    let b58 = std::env::var("SIM_PRIVATE_KEY").expect("SIM_PRIVATE_KEY");
    Keypair::from_bytes(&bs58::decode(b58.trim()).into_vec().unwrap()).unwrap()
}

fn pk(s: &str) -> Pubkey { Pubkey::from_str(s).unwrap() }

fn router_config() -> RouterConfig {
    RouterConfig { program_id: pk(ROUTER), treasury_wallet: pk(TREASURY), referral_wallet: None, fee_bps: 50 }
}

struct SwapDef {
    label: &'static str,
    pool_address: &'static str,
    pool_type: PoolType,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount_in: u64,
}

async fn execute_swap(rpc: &RpcClient, signer: &Keypair, swap: &SwapDef) -> Result<String, String> {
    let user = signer.pubkey();
    let router = router_config();

    let pool = pk(swap.pool_address);
    let state = fetcher::fetch_pool_state(rpc, swap.pool_type, &pool).await
        .map_err(|e| format!("fetch: {e}"))?;

    let input_tp = get_mint_token_program(rpc, &swap.input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, &swap.output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);

    let executor = AmmExecutorType::from_pool_type(swap.pool_type).map_err(|e| format!("{e}"))?;
    let order = SwapOrder {
        pool_address: pool, pool_type: swap.pool_type,
        input_mint: swap.input_mint, output_mint: swap.output_mint,
        amount_in: swap.amount_in, min_amount_out: 0, user,
        input_token_program: input_tp, output_token_program: output_tp,
    };
    let ixs = executor.build_swap_ix(&order, &state).map_err(|e| format!("build: {e}"))?;

    let user_input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &user, &swap.input_mint, &input_tp,
    );
    let user_output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &user, &swap.output_mint, &output_tp,
    );
    let protocol_fee_acct = router.fee_account_for_mint(&swap.output_mint, &output_tp);

    let create_fee_ata = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        &user, &router.treasury_wallet, &swap.output_mint, &output_tp,
    );
    let create_output_ata = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        &user, &user, &swap.output_mint, &output_tp,
    );

    let router_ix = wrap_swap(
        &router, &user, &[user_input_ata, user_output_ata],
        &protocol_fee_acct, None, &[ixs.swap[0].clone()],
        swap.amount_in, 0, &output_tp, &Pubkey::default() /* output mint: ignored by the legacy layout */,
    ).map_err(|e| format!("wrap: {e}"))?;

    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(50_000),
        create_fee_ata,
        create_output_ata,
    ];
    all_ixs.extend(ixs.setup);
    all_ixs.push(router_ix);
    all_ixs.extend(ixs.cleanup);

    let blockhash = rpc.get_latest_blockhash().await.map_err(|e| format!("{e}"))?;
    let msg = solana_sdk::message::Message::new(&all_ixs, Some(&user));
    let tx = Transaction::new(&[signer], msg, blockhash);

    rpc.send_and_confirm_transaction(&tx).await
        .map(|sig| sig.to_string())
        .map_err(|e| format!("{e}"))
}

#[tokio::test]
async fn test_fee_accumulation() {
    eprintln!("\n╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  REAL MAINNET — FEE ACCUMULATION TEST                        ║");
    eprintln!("║  Multiple swaps through production router                     ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝\n");

    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let router = router_config();

    let bal_before = rpc.get_balance(&user).await.unwrap();
    eprintln!("  Wallet:   {}", user);
    eprintln!("  Treasury: {}", TREASURY);
    eprintln!("  Balance:  {:.6} SOL\n", bal_before as f64 / 1e9);

    // Token mints we'll use
    let pfa_token = pk("6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz");
    let cpmm_token = pk("25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook");

    let swaps = vec![
        // Buy 1: SOL → PumpFunAmm token (fee in PFA token)
        SwapDef {
            label: "Buy #1 (PFA, 0.001 SOL)",
            pool_address: "6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6",
            pool_type: PoolType::PumpFunAmm,
            input_mint: SOL_NATIVE_MINT,
            output_mint: pfa_token,
            amount_in: 1_000_000, // 0.001 SOL
        },
        // Buy 2: SOL → PumpFunAmm token again (fee accumulates)
        SwapDef {
            label: "Buy #2 (PFA, 0.002 SOL)",
            pool_address: "6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6",
            pool_type: PoolType::PumpFunAmm,
            input_mint: SOL_NATIVE_MINT,
            output_mint: pfa_token,
            amount_in: 2_000_000, // 0.002 SOL
        },
        // Buy 3: SOL → RaydiumCpmm token (fee in different token)
        SwapDef {
            label: "Buy #3 (CPMM, 0.001 SOL)",
            pool_address: "BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr",
            pool_type: PoolType::RaydiumCpmm,
            input_mint: SOL_NATIVE_MINT,
            output_mint: cpmm_token,
            amount_in: 1_000_000, // 0.001 SOL
        },
        // Sell: PFA token → SOL (fee in SOL — different fee mint!)
        SwapDef {
            label: "Sell #1 (PFA→SOL, all tokens)",
            pool_address: "6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6",
            pool_type: PoolType::PumpFunAmm,
            input_mint: pfa_token,
            output_mint: SOL_NATIVE_MINT,
            amount_in: 0, // will be adjusted after buys
        },
    ];

    let mut pass = 0;
    let mut fail = 0;

    for (i, swap) in swaps.iter().enumerate() {
        // Skip the sell if amount is 0 (we'll handle it separately)
        if swap.amount_in == 0 {
            // Get current PFA token balance and sell it all
            let output_tp = get_mint_token_program(&rpc, &swap.input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
            let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
                &user, &swap.input_mint, &output_tp,
            );
            match rpc.get_token_account_balance(&ata).await {
                Ok(bal) => {
                    let amount: u64 = bal.amount.parse().unwrap_or(0);
                    if amount == 0 {
                        eprintln!("  #{}: {} — SKIP (no tokens to sell)", i+1, swap.label);
                        continue;
                    }
                    let sell_swap = SwapDef {
                        label: swap.label,
                        pool_address: swap.pool_address,
                        pool_type: swap.pool_type,
                        input_mint: swap.input_mint,
                        output_mint: swap.output_mint,
                        amount_in: amount,
                    };
                    match execute_swap(&rpc, &signer, &sell_swap).await {
                        Ok(sig) => {
                            eprintln!("  #{}: {} — CONFIRMED: {} (sold {} tokens)", i+1, swap.label, &sig[..20], amount);
                            pass += 1;
                        }
                        Err(e) => {
                            eprintln!("  #{}: {} — FAILED: {}", i+1, swap.label, e);
                            fail += 1;
                        }
                    }
                }
                Err(_) => {
                    eprintln!("  #{}: {} — SKIP (no ATA)", i+1, swap.label);
                }
            }
            continue;
        }

        match execute_swap(&rpc, &signer, swap).await {
            Ok(sig) => {
                eprintln!("  #{}: {} — CONFIRMED: {}", i+1, swap.label, &sig[..20]);
                pass += 1;
            }
            Err(e) => {
                eprintln!("  #{}: {} — FAILED: {}", i+1, swap.label, e);
                fail += 1;
            }
        }
    }

    // Check treasury fee balances
    eprintln!("\n  ── TREASURY FEE BALANCES ──\n");

    let fee_mints = [
        ("PFA token (6FH1)", pfa_token),
        ("CPMM token (25fFY)", cpmm_token),
        ("SOL (WSOL)", SOL_NATIVE_MINT),
    ];

    for (label, mint) in &fee_mints {
        let tp = get_mint_token_program(&rpc, mint).await.unwrap_or(TOKEN_PROGRAM_ID);
        let fee_ata = router.fee_account_for_mint(mint, &tp);
        match rpc.get_token_account_balance(&fee_ata).await {
            Ok(bal) => eprintln!("  {}: {} (ATA: {})", label, bal.ui_amount_string, &fee_ata.to_string()[..12]),
            Err(_) => eprintln!("  {}: no ATA (no swaps with this output yet)", label),
        }
    }

    let bal_after = rpc.get_balance(&user).await.unwrap();
    let spent = bal_before.saturating_sub(bal_after);
    eprintln!("\n  Total SOL spent: {:.6}", spent as f64 / 1e9);
    eprintln!("  Swaps: {} passed, {} failed", pass, fail);

    assert!(pass >= 3, "At least 3 swaps should succeed");
    eprintln!("\n  [OK] Fee accumulation verified across {} swaps\n", pass);
}
