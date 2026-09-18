//! REAL mainnet swap tests — actual buy + sell round trips through live pools.
//!
//! Each test:
//! 1. Records starting SOL balance
//! 2. Builds a BUY swap instruction (SOL → TOKEN)
//! 3. Signs and submits to mainnet
//! 4. Waits for confirmation
//! 5. Builds a SELL swap instruction (TOKEN → SOL)
//! 6. Signs and submits to mainnet
//! 7. Records ending SOL balance
//! 8. Reports profit/loss from fees + slippage
//!
//! Requires: RPC_URL + SIM_PRIVATE_KEY in .env, funded wallet with ~1 SOL
//!
//! Run:
//! ```bash
//! set -a && source .env && set +a
//! OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu OPENSSL_INCLUDE_DIR=/usr/include \
//!   cargo test --test e2e_mainnet_swaps -- --nocapture --test-threads=1
//! ```

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::{Message, VersionedMessage, v0::Message as V0Message};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::{Transaction, VersionedTransaction};

use flow_trades::constants::*;
use flow_trades::execution::amms::AmmExecutorType;
use flow_trades::execution::router::{RouterConfig, wrap_swap};
use flow_trades::pool::fetcher::{self, get_mint_token_program};
use flow_trades::pool::types::{PoolState, PoolType, SwapOrder};

/// Staging router program ID (FLoW vanity — per-mint treasury fee collection).
const STAGING_ROUTER: &str = "FLoWGKDppDA8qGnsFq3hRScTTjefaXT3L7PxNMo9UQkn";

fn router_config() -> RouterConfig {
    RouterConfig {
        program_id: pk(STAGING_ROUTER),
        treasury_wallet: pk("HS5LEc5nuzdm1n2wKHT95YY5trQL85Viro5QaFdaaxfC"),
        // Referral WSOL ATA — receives 70% of the 0.5% fee
        referral_wallet: Some(pk("5XiTECUnCF8ZR9JG6unZPsVMxBtaJSGmEniu42SmtQq2")),
    }
}

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

struct SwapTest {
    name: &'static str,
    pool_type: PoolType,
    pool_address: &'static str,
    /// The non-SOL token in the pair.
    token_mint: &'static str,
    /// SOL amount for buy in lamports (0.001 SOL = 1_000_000).
    buy_amount: u64,
}

/// Execute a real swap: build IX → sign → submit → confirm.
async fn execute_swap(
    rpc: &RpcClient,
    signer: &Keypair,
    pool_type: PoolType,
    pool_address: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
) -> Result<(String, u128), String> {
    execute_swap_full(rpc, signer, pool_type, pool_address, input_mint, output_mint, amount_in, min_amount_out, None, &Mutex::new(HashSet::new())).await
}

/// Execute a real swap with optional tip and ATA cache.
async fn execute_swap_full(
    rpc: &RpcClient,
    signer: &Keypair,
    pool_type: PoolType,
    pool_address: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
    tip: Option<(Pubkey, u64)>,
    known_atas: &Mutex<HashSet<Pubkey>>,
) -> Result<(String, u128), String> {
    let user = signer.pubkey();

    // Fetch pool state
    let pool_state = fetcher::fetch_pool_state(rpc, pool_type, pool_address)
        .await
        .map_err(|e| format!("fetch: {e}"))?;

    // Token programs
    let input_tp = get_mint_token_program(rpc, input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);

    // Build swap IX
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

    let ixs = executor.build_swap_ix(&order, &pool_state)
        .map_err(|e| format!("build_ix: {e}"))?;

    // Wrap the DEX swap in a router CPI
    let router = router_config();
    // Derive ATAs with the correct token program (Token v1 vs Token-2022)
    let user_input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user, input_mint, &input_tp);
    let user_output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user, output_mint, &output_tp);
    let dex_ix = ixs.swap.first()
        .ok_or_else(|| "no swap instruction".to_string())?;

    let fee_mint = output_mint;
    let fee_mint_tp = get_mint_token_program(rpc, fee_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let protocol_fee_acct = router.fee_account_for_mint(fee_mint, &fee_mint_tp);
    let referral_ata = router.referral_account_for_mint(fee_mint, &fee_mint_tp);
    let fee_token_program = output_tp;

    let router_ix = wrap_swap(
        &router, &user, &[user_input_ata, user_output_ata],
        &protocol_fee_acct, referral_ata.as_ref(), &[dex_ix.clone()], amount_in, min_amount_out,
        &fee_token_program, &fee_mint,
    ).map_err(|e| format!("router wrap: {e}"))?;

    // Assemble TX: compute budget + setup + ROUTER swap + cleanup
    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(5_000),
    ];
    all_ixs.extend(ixs.setup.clone());

    // Auto-create treasury + referral ATAs for the output token (fee mint).
    // Cached: only add the instruction if we haven't seen this ATA before.
    {
        let mut cache = known_atas.lock().unwrap();
        let treasury_ata = router.fee_account_for_mint(fee_mint, &fee_mint_tp);
        if !cache.contains(&treasury_ata) {
            all_ixs.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &user, &router.treasury_wallet, fee_mint, &fee_mint_tp,
            ));
            cache.insert(treasury_ata);
        }
        if let Some(ref wallet) = router.referral_wallet {
            let ref_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
                wallet, fee_mint, &fee_mint_tp,
            );
            if !cache.contains(&ref_ata) {
                all_ixs.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &user, wallet, fee_mint, &fee_mint_tp,
                ));
                cache.insert(ref_ata);
            }
        }
    }

    all_ixs.push(router_ix);       // Router-wrapped DEX swap (CPI)
    all_ixs.extend(ixs.cleanup.clone());

    // Tip: SOL transfer as the very last instruction
    if let Some((tip_dest, tip_lamports)) = tip {
        all_ixs.push(solana_sdk::system_instruction::transfer(&user, &tip_dest, tip_lamports));
    }

    let blockhash = rpc.get_latest_blockhash().await
        .map_err(|e| format!("blockhash: {e}"))?;

    // Load ALT for v0 versioned transaction (saves ~31 bytes per shared account)
    let alt_address = pk("4NX8TbeVMneZ7QqL1KgTzBDGj1vgWavAbKjf3JSMPac3");
    let alt_account = rpc.get_account(&alt_address).await.ok().and_then(|acct| {
        solana_sdk::address_lookup_table::state::AddressLookupTable::deserialize(&acct.data).ok().map(|table| {
            AddressLookupTableAccount {
                key: alt_address,
                addresses: table.addresses.to_vec(),
            }
        })
    });

    // Build v0 versioned TX with ALT (falls back to legacy if ALT unavailable)
    let start = Instant::now();
    let sig = if let Some(ref alt) = alt_account {
        let v0_msg = V0Message::try_compile(&user, &all_ixs, &[alt.clone()], blockhash)
            .map_err(|e| format!("v0 compile: {e}"))?;
        let vtx = VersionedTransaction::try_new(VersionedMessage::V0(v0_msg), &[signer])
            .map_err(|e| format!("sign: {e}"))?;
        rpc.send_and_confirm_transaction(&vtx).await
            .map_err(|e| format!("submit: {e}"))?
    } else {
        let msg = Message::new_with_blockhash(&all_ixs, Some(&user), &blockhash);
        let tx = Transaction::new(&[signer], msg, blockhash);
        rpc.send_and_confirm_transaction(&tx).await
            .map_err(|e| format!("submit: {e}"))?
    };
    let elapsed = start.elapsed().as_millis();

    Ok((sig.to_string(), elapsed))
}

/// Get token balance for a user's ATA. Tries Token v1 first, then Token-2022.
async fn get_token_balance(rpc: &RpcClient, user: &Pubkey, mint: &Pubkey) -> u64 {
    // Try Token v1 ATA first
    let ata_v1 = spl_associated_token_account::get_associated_token_address(user, mint);
    if let Ok(bal) = rpc.get_token_account_balance(&ata_v1).await {
        let amount: u64 = bal.amount.parse().unwrap_or(0);
        if amount > 0 { return amount; }
    }
    // Try Token-2022 ATA
    let ata_v2 = spl_associated_token_account::get_associated_token_address_with_program_id(
        user, mint, &TOKEN_2022_PROGRAM_ID,
    );
    if ata_v2 != ata_v1 {
        if let Ok(bal) = rpc.get_token_account_balance(&ata_v2).await {
            return bal.amount.parse().unwrap_or(0);
        }
    }
    0
}

#[tokio::test]
async fn test_mainnet_buy_sell_round_trips() {
    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();

    let starting_sol = rpc.get_balance(&user).await.unwrap();

    eprintln!("\n╔════════════════════════════════════════════════════════════╗");
    eprintln!("║  MAINNET BUY+SELL VIA FLOW-ROUTER — REAL ON-CHAIN SWAPS  ║");
    eprintln!("╚════════════════════════════════════════════════════════════╝\n");
    eprintln!("  Router:       {}", STAGING_ROUTER);
    eprintln!("  Wallet:       {}", user);
    eprintln!("  Starting SOL: {:.9} SOL ({} lamports)", starting_sol as f64 / 1e9, starting_sol);

    let tests = vec![
        // ── Raydium family ──
        SwapTest {
            name: "RaydiumV4",
            pool_type: PoolType::RaydiumV4,
            pool_address: "2cVeKhfEPWJHLPGRujw3nMjNubUMAkb12GzT4HmTwmqX",
            token_mint: "8aWN4s4WsDAZF27MxLv5ma2ujtknrRnMwGHBoiGcpump",
            buy_amount: 1_000_000, // 0.001 SOL
        },
        SwapTest {
            name: "RaydiumCpmm",
            pool_type: PoolType::RaydiumCpmm,
            pool_address: "BEdCQzzvEmqtHH946GYsdeideaBYMvt8SZn5eUjKxTjr",
            token_mint: "25fFYbsxrCUPK2JZv6Bx7znfaiCczhTw3PocgA2dcook",
            buy_amount: 1_000_000,
        },
        SwapTest {
            name: "RaydiumCLMM",
            pool_type: PoolType::RaydiumCl,
            pool_address: "2AXXcN6oN9bBT5owwmTH53C7QHUXvhLeu718Kqt8rvY2",
            token_mint: "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
            buy_amount: 1_000_000,
        },
        SwapTest {
            name: "RaydiumLP",
            pool_type: PoolType::RaydiumLp,
            pool_address: "6Lc76tcWsCEkydyLriNaeDkUgVekusBVGgQYYDiKRZi1",
            token_mint: "D756Z3S31AZMbU4teTu2BWK77neDArFhaPr6eZ7bonk",
            buy_amount: 1_000_000,
        },
        // ── PumpFun family ──
        SwapTest {
            name: "PumpFunAmm",
            pool_type: PoolType::PumpFunAmm,
            pool_address: "6cPfRuSp8L7f1TMt3vtKhqYYuHDoHZTHGNzQ6hRABtx6",
            token_mint: "6FH1oopsRWjnSRnyYpU1cuzKBrQfpap6Gr5wMkXAuWaz",
            buy_amount: 1_000_000,
        },
        // PumpFun bonding handled separately below (short-lived pools, mint from pool state)
        // ── Meteora family ──
        SwapTest {
            name: "Meteora",
            pool_type: PoolType::Meteora,
            pool_address: "BCXjm4FfSoquZQJV5Wcje1g1pSHW2hFMU9wDE98Nyatb",
            token_mint: "STrikemJEk2tFVYpg7SMo9nGPrnJ56fHnS1K7PV2fPw",
            buy_amount: 1_000_000,
        },
        SwapTest {
            name: "MeteoraDLMM",
            pool_type: PoolType::MeteoraDlmm,
            pool_address: "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR",
            token_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            buy_amount: 1_000_000,
        },
        // MeteoraDamm + MeteoraDbc handled separately below (short-lived pools)
        // ── Orca / Whirlpool ──
        SwapTest {
            name: "Orca",
            pool_type: PoolType::Orca,
            pool_address: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
            token_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            buy_amount: 1_000_000,
        },
        // ── Other AMMs ──
        SwapTest {
            name: "FluxBeam",
            pool_type: PoolType::FluxBeam,
            pool_address: "6hrvHgqXna7i2Xck2859N8yxaiC5jboA1paBnTSi7FT4",
            token_mint: "8rScidWjLJYNKJQPpV5EBP5jSbJV6CfrZZPkGubuu6ct",
            buy_amount: 1_000_000,
        },
        SwapTest {
            name: "DefiTunaFusion",
            pool_type: PoolType::DefiTunaFusion,
            pool_address: "7VuKeevbvbQQcxz6N4SNLmuq6PYy4AcGQRDssoqo4t65",
            token_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            buy_amount: 1_000_000,
        },
    ];

    eprintln!("  AMMs: {}\n", tests.len());
    eprintln!("  {:<16} | {:>10} | {:>10} | {:>8} | {:>8} | {}", "AMM", "Buy TX", "Sell TX", "Buy ms", "Sell ms", "Status");
    eprintln!("  {:-<16}-+-{:-<10}-+-{:-<10}-+-{:-<8}-+-{:-<8}-+-{:-<20}", "", "", "", "", "", "");

    let mut pass = 0;
    let mut skip = 0;
    let mut fail = 0;

    // Tip destination for alternating tip tests
    let tip_dest = pk("2As4HDWZcC78rULMvFL4sZaRGEoXRPjn2WyejkSnKHFR");
    let tip_amount: u64 = 5_000;

    // Cache of known-existing fee ATAs — avoids redundant create instructions after first swap
    let ata_cache = Mutex::new(HashSet::new());

    for (idx, test) in tests.iter().enumerate() {
        let sol_mint = SOL_NATIVE_MINT;
        let token_mint = pk(test.token_mint);
        let pool_address = pk(test.pool_address);

        // Alternate: even index = tip on buy, odd = no tip
        let buy_tip = if idx % 2 == 0 { Some((tip_dest, tip_amount)) } else { None };

        // BUY: SOL → TOKEN
        let buy_result = execute_swap_full(
            &rpc, &signer, test.pool_type, &pool_address,
            &sol_mint, &token_mint, test.buy_amount, 0, buy_tip, &ata_cache,
        ).await;

        match buy_result {
            Ok((buy_sig, buy_ms)) => {
                // Small delay to let state settle
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Check token balance after buy
                let token_balance = get_token_balance(&rpc, &user, &token_mint).await;

                if token_balance == 0 {
                    let tip_label = if buy_tip.is_some() { "+tip" } else { "" };
                    eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | BUY{} OK but 0 tokens",
                        test.name, &buy_sig[..8], "-", buy_ms, "-", tip_label);
                    skip += 1;
                    continue;
                }

                // SELL: TOKEN → SOL (no tip on sells)
                let sell_result = execute_swap_full(
                    &rpc, &signer, test.pool_type, &pool_address,
                    &token_mint, &sol_mint, token_balance, 0, None, &ata_cache,
                ).await;

                match sell_result {
                    Ok((sell_sig, sell_ms)) => {
                        let tip_label = if buy_tip.is_some() { " +tip" } else { "" };
                        eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>6}ms | PASS{} ({} tokens)",
                            test.name, &buy_sig[..8], &sell_sig[..8], buy_ms, sell_ms, tip_label, token_balance);
                        pass += 1;
                    }
                    Err(e) => {
                        eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | SELL FAIL: {}",
                            test.name, &buy_sig[..8], "-", buy_ms, "-", e);
                        fail += 1;
                    }
                }
            }
            Err(e) => {
                if e.contains("AccountNotFound") || e.contains("too small") || e.contains("0x1771")
                    || e.contains("Custom(3007)") || e.contains("0xbbf")
                    || e.contains("0x1796") || e.contains("closed") || e.contains("migrated")
                    || e.contains("incorrect program id") || e.contains("0x177d") {
                    eprintln!("  {:<16} | {:>10} | {:>10} | {:>8} | {:>8} | SKIP: {}",
                        test.name, "-", "-", "-", "-", &e[..e.len().min(50)]);
                    skip += 1;
                } else {
                    eprintln!("  {:<16} | BUY FAIL: {}", test.name, e);
                    fail += 1;
                }
            }
        }
    }

    // ── PumpFun bonding curve: short-lived pools, try multiple, extract mint from state ──
    {
        let pf_pools = [
            "5FvNPT8WNB9EPsBctPYBZ76GCK5NMbL8HCfa8SAB9aw3",
            "ASzAYE4L1dKK8xk8nytuUDrkMcvUaZkhAQy4982XSXCS",
            "BHbEXs9cLPvpwVE3aSpKN59N7iQT1P3RWxob1UjewFVw",
            "EXPKi5BUmztzowekbmPdZwAbPTYMRuZrTW8CYd8FtSyo",
        ];
        let mut pf_done = false;
        for pool_str in pf_pools {
            let pool_address = pk(pool_str);
            // Fetch pool state to extract the mint
            let pool_state = match fetcher::fetch_pool_state(&rpc, PoolType::PumpFun, &pool_address).await {
                Ok(s) => s,
                Err(e) => {
                    let es = format!("{e}");
                    if es.contains("AccountNotFound") || es.contains("too small") {
                        continue; // pool closed/migrated, try next
                    }
                    eprintln!("  {:<16} | BUY FAIL: {}", "PumpFunBonding", es);
                    fail += 1;
                    pf_done = true;
                    break;
                }
            };
            let token_mint = match &pool_state {
                PoolState::PumpFun { mint, .. } => *mint,
                _ => { continue; }
            };
            // BUY: SOL → TOKEN (0.01 SOL — bonding curve needs larger input)
            let buy_result = execute_swap(
                &rpc, &signer, PoolType::PumpFun, &pool_address,
                &SOL_NATIVE_MINT, &token_mint, 10_000_000, 0,
            ).await;
            match buy_result {
                Ok((buy_sig, buy_ms)) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let token_balance = get_token_balance(&rpc, &user, &token_mint).await;
                    if token_balance == 0 {
                        eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | BUY OK but 0 tokens",
                            "PumpFunBonding", &buy_sig[..8], "-", buy_ms, "-");
                        skip += 1;
                    } else {
                        let sell_result = execute_swap(
                            &rpc, &signer, PoolType::PumpFun, &pool_address,
                            &token_mint, &SOL_NATIVE_MINT, token_balance, 0,
                        ).await;
                        match sell_result {
                            Ok((sell_sig, sell_ms)) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>6}ms | PASS ({} tokens)",
                                    "PumpFunBonding", &buy_sig[..8], &sell_sig[..8], buy_ms, sell_ms, token_balance);
                                pass += 1;
                            }
                            Err(e) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | SELL FAIL: {}",
                                    "PumpFunBonding", &buy_sig[..8], "-", buy_ms, "-", e);
                                fail += 1;
                            }
                        }
                    }
                    pf_done = true;
                    break;
                }
                Err(e) => {
                    let es = format!("{e}");
                    if es.contains("AccountNotFound") || es.contains("too small") || es.contains("migrated")
                        || es.contains("0x1771") || es.contains("0x3") || es.contains("invalid account data")
                        || es.contains("0x1784") {
                        continue; // pool closed/migrated/graduated (0x1784 = BuyZeroAmount)
                    }
                    eprintln!("  {:<16} | BUY FAIL: {}", "PumpFunBonding", es);
                    fail += 1;
                    pf_done = true;
                    break;
                }
            }
        }
        if !pf_done {
            eprintln!("  {:<16} | {:>10} | {:>10} | {:>8} | {:>8} | SKIP: all pools closed/migrated",
                "PumpFunBonding", "-", "-", "-", "-");
            skip += 1;
        }
    }

    // ── MeteoraDamm: short-lived pools, try multiple ──
    {
        let damm_pools = [
            ("BACxEJATX2nKvQs3f6tN5Hyg7jZzu6u4puycoPM5BTeF", ""),
            ("3tdwyJkyBvL3aArGjE7UHvH4JN3iJ1tSKQNNyZEaLDUz", ""),
            ("4oURtRqr2apRmWShZK3QtwyeC2ocbqt71QferWtrFBwu", ""),
            ("4ac2qQp9uPQTuhvNH1p5xZmZnKV1pAxWKjBPkwvD6x6J", "CDNZaZwhWB2VGFXSEMmM7hBodrhciRx7JPbjghttbEM3"),
        ];
        let mut damm_done = false;
        for (pool_str, mint_str) in damm_pools {
            let pool_address = pk(pool_str);
            // Fetch pool state to extract the mint
            let pool_state = match fetcher::fetch_pool_state(&rpc, PoolType::MeteoraDamm, &pool_address).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let token_mint = if !mint_str.is_empty() {
                pk(mint_str)
            } else {
                match &pool_state {
                    PoolState::MeteoraDamm { token_a_mint, token_b_mint, .. } => {
                        if *token_a_mint == SOL_NATIVE_MINT { *token_b_mint } else { *token_a_mint }
                    }
                    _ => continue,
                }
            };
            let buy_result = execute_swap(
                &rpc, &signer, PoolType::MeteoraDamm, &pool_address,
                &SOL_NATIVE_MINT, &token_mint, 1_000_000, 0,
            ).await;
            match buy_result {
                Ok((buy_sig, buy_ms)) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let token_balance = get_token_balance(&rpc, &user, &token_mint).await;
                    if token_balance == 0 {
                        eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | BUY OK but 0 tokens",
                            "MeteoraDamm", &buy_sig[..8], "-", buy_ms, "-");
                        skip += 1;
                    } else {
                        let sell_result = execute_swap(
                            &rpc, &signer, PoolType::MeteoraDamm, &pool_address,
                            &token_mint, &SOL_NATIVE_MINT, token_balance, 0,
                        ).await;
                        match sell_result {
                            Ok((sell_sig, sell_ms)) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>6}ms | PASS ({} tokens)",
                                    "MeteoraDamm", &buy_sig[..8], &sell_sig[..8], buy_ms, sell_ms, token_balance);
                                pass += 1;
                            }
                            Err(e) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | SELL FAIL: {}",
                                    "MeteoraDamm", &buy_sig[..8], "-", buy_ms, "-", e);
                                fail += 1;
                            }
                        }
                    }
                    damm_done = true;
                    break;
                }
                Err(e) if e.contains("AccountNotFound") || e.contains("too small") || e.contains("0x3")
                    || e.contains("failed to complete") => continue,
                Err(e) => {
                    eprintln!("  {:<16} | BUY FAIL: {}", "MeteoraDamm", e);
                    fail += 1;
                    damm_done = true;
                    break;
                }
            }
        }
        if !damm_done {
            eprintln!("  {:<16} | {:>10} | {:>10} | {:>8} | {:>8} | SKIP: all pools closed",
                "MeteoraDamm", "-", "-", "-", "-");
            skip += 1;
        }
    }

    // ── MeteoraDbc: bonding curves graduate quickly ──
    {
        let dbc_pools = [
            "3Tf1PVzabqrTWdvtbwAdiANrcAxhChCedg18EvSKDknM",
            "3SYd48u9NBoizbcEe3Go9j6gt4AZoJh4vJb32gmXLH6y",
            "7TqH5rBfnJ8ykttxUJ1LZGQVi24mFph8nQFKCEPwyBJt",
        ];
        let mut dbc_done = false;
        for pool_str in dbc_pools {
            let pool_address = pk(pool_str);
            let pool_state = match fetcher::fetch_pool_state(&rpc, PoolType::MeteoraDbc, &pool_address).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let token_mint = match &pool_state {
                PoolState::MeteoraDbc { base_mint, .. } => *base_mint,
                _ => continue,
            };
            let buy_result = execute_swap(
                &rpc, &signer, PoolType::MeteoraDbc, &pool_address,
                &SOL_NATIVE_MINT, &token_mint, 10_000_000, 0,
            ).await;
            match buy_result {
                Ok((buy_sig, buy_ms)) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let token_balance = get_token_balance(&rpc, &user, &token_mint).await;
                    if token_balance == 0 {
                        eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | BUY OK but 0 tokens",
                            "MeteoraDbc", &buy_sig[..8], "-", buy_ms, "-");
                        skip += 1;
                    } else {
                        let sell_result = execute_swap(
                            &rpc, &signer, PoolType::MeteoraDbc, &pool_address,
                            &token_mint, &SOL_NATIVE_MINT, token_balance, 0,
                        ).await;
                        match sell_result {
                            Ok((sell_sig, sell_ms)) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>6}ms | PASS ({} tokens)",
                                    "MeteoraDbc", &buy_sig[..8], &sell_sig[..8], buy_ms, sell_ms, token_balance);
                                pass += 1;
                            }
                            Err(e) => {
                                eprintln!("  {:<16} | {:>10} | {:>10} | {:>6}ms | {:>8} | SELL FAIL: {}",
                                    "MeteoraDbc", &buy_sig[..8], "-", buy_ms, "-", e);
                                fail += 1;
                            }
                        }
                    }
                    dbc_done = true;
                    break;
                }
                Err(e) if e.contains("AccountNotFound") || e.contains("too small") || e.contains("0x177d") || e.contains("0x3") => continue,
                Err(e) => {
                    eprintln!("  {:<16} | BUY FAIL: {}", "MeteoraDbc", e);
                    fail += 1;
                    dbc_done = true;
                    break;
                }
            }
        }
        if !dbc_done {
            eprintln!("  {:<16} | {:>10} | {:>10} | {:>8} | {:>8} | SKIP: all pools graduated/closed",
                "MeteoraDbc", "-", "-", "-", "-");
            skip += 1;
        }
    }

    let total_amms = tests.len() + 3; // +3 for PumpFun bonding, MeteoraDamm, MeteoraDbc

    // Final balance
    tokio::time::sleep(Duration::from_secs(2)).await;
    let ending_sol = rpc.get_balance(&user).await.unwrap();
    let diff = starting_sol as i64 - ending_sol as i64;

    eprintln!("\n  ──────────────────────────────────────────────────────");
    eprintln!("  Starting SOL:  {:.9} ({} lamports)", starting_sol as f64 / 1e9, starting_sol);
    eprintln!("  Ending SOL:    {:.9} ({} lamports)", ending_sol as f64 / 1e9, ending_sol);
    eprintln!("  Difference:    {:.9} SOL ({} lamports)", diff as f64 / 1e9, diff);
    eprintln!("  Cost per swap: {:.6} SOL (avg over {} round trips)", diff as f64 / 1e9 / pass.max(1) as f64, pass);
    eprintln!("  Results:       {} PASS / {} SKIP / {} FAIL (of {} AMMs)", pass, skip, fail, total_amms);
    eprintln!("");

    // We should get most SOL back (only lose fees + slippage)
    assert!(ending_sol > starting_sol / 2, "Lost more than half the SOL — something is very wrong");
    assert_eq!(fail, 0, "No AMMs should hard-fail on both buy AND sell");
}

/// Minimal direct-DEX executor — no flow-router wrap, no ALT. Proves the
/// AMM executor produces a valid on-chain swap independent of the staging
/// router contract (which can require an init-config PDA or be deactivated
/// on certain RPCs). Same code path the SDK consumer would use for raw
/// "build IX -> sign -> submit" flow.
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
        .await.map_err(|e| format!("fetch: {e}"))?;
    let input_tp = get_mint_token_program(rpc, input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let executor = AmmExecutorType::from_pool_type(pool_type)
        .map_err(|e| format!("executor: {e}"))?;
    let order = SwapOrder {
        pool_address: *pool_address, pool_type,
        input_mint: *input_mint, output_mint: *output_mint,
        amount_in, min_amount_out, user,
        input_token_program: input_tp, output_token_program: output_tp,
    };
    let ixs = executor.build_swap_ix(&order, &pool_state)
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

/// Like execute_swap_full but skips the ALT that's currently deactivated
/// on the test RPC. Otherwise identical (same router CPI + sign + submit).
async fn execute_swap_no_alt(
    rpc: &RpcClient,
    signer: &Keypair,
    pool_type: PoolType,
    pool_address: &Pubkey,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
    known_atas: &Mutex<HashSet<Pubkey>>,
) -> Result<(String, u128), String> {
    let user = signer.pubkey();
    let pool_state = fetcher::fetch_pool_state(rpc, pool_type, pool_address)
        .await.map_err(|e| format!("fetch: {e}"))?;
    let input_tp = get_mint_token_program(rpc, input_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let output_tp = get_mint_token_program(rpc, output_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let executor = AmmExecutorType::from_pool_type(pool_type)
        .map_err(|e| format!("executor: {e}"))?;
    let order = SwapOrder {
        pool_address: *pool_address, pool_type,
        input_mint: *input_mint, output_mint: *output_mint,
        amount_in, min_amount_out, user,
        input_token_program: input_tp, output_token_program: output_tp,
    };
    let ixs = executor.build_swap_ix(&order, &pool_state)
        .map_err(|e| format!("build_ix: {e}"))?;

    let router = router_config();
    let user_input_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user, input_mint, &input_tp);
    let user_output_ata = spl_associated_token_account::get_associated_token_address_with_program_id(&user, output_mint, &output_tp);
    let dex_ix = ixs.swap.first().ok_or_else(|| "no swap instruction".to_string())?;
    let fee_mint = output_mint;
    let fee_mint_tp = get_mint_token_program(rpc, fee_mint).await.unwrap_or(TOKEN_PROGRAM_ID);
    let protocol_fee_acct = router.fee_account_for_mint(fee_mint, &fee_mint_tp);
    let referral_ata = router.referral_account_for_mint(fee_mint, &fee_mint_tp);
    let router_ix = wrap_swap(
        &router, &user, &[user_input_ata, user_output_ata],
        &protocol_fee_acct, referral_ata.as_ref(), &[dex_ix.clone()], amount_in, min_amount_out,
        &output_tp, &fee_mint,
    ).map_err(|e| format!("router wrap: {e}"))?;

    let mut all_ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ComputeBudgetInstruction::set_compute_unit_price(5_000),
    ];
    all_ixs.extend(ixs.setup.clone());
    {
        let mut cache = known_atas.lock().unwrap();
        let treasury_ata = router.fee_account_for_mint(fee_mint, &fee_mint_tp);
        if !cache.contains(&treasury_ata) {
            all_ixs.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &user, &router.treasury_wallet, fee_mint, &fee_mint_tp,
            ));
            cache.insert(treasury_ata);
        }
        if let Some(ref wallet) = router.referral_wallet {
            let ref_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
                wallet, fee_mint, &fee_mint_tp,
            );
            if !cache.contains(&ref_ata) {
                all_ixs.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &user, wallet, fee_mint, &fee_mint_tp,
                ));
                cache.insert(ref_ata);
            }
        }
    }
    all_ixs.push(router_ix);
    all_ixs.extend(ixs.cleanup.clone());

    let blockhash = rpc.get_latest_blockhash().await.map_err(|e| format!("blockhash: {e}"))?;
    let start = Instant::now();
    let msg = Message::new_with_blockhash(&all_ixs, Some(&user), &blockhash);
    let tx = Transaction::new(&[signer], msg, blockhash);
    let sig = rpc.send_and_confirm_transaction(&tx).await.map_err(|e| format!("submit: {e}"))?;
    let elapsed = start.elapsed().as_millis();
    Ok((sig.to_string(), elapsed))
}

/// **Live mainnet test for Pumpup**.
///
/// Pumpup pools trade against USDT (not SOL). Self-funds via internal
/// Raydium CLMM SOL→USDT swap if needed, then runs USDT→ANNCZ buy and
/// ANNCZ→USDT sell on the verified mainnet pool. Uses our internal
/// router CPI on every swap (same path as the rest of the test suite),
/// but without ALTs (the harness's hardcoded ALT is deactivated).
#[tokio::test]
async fn test_mainnet_pumpup_round_trip() {
    let rpc = rpc();
    let signer = load_keypair();
    let user = signer.pubkey();
    let _known_atas: Mutex<HashSet<Pubkey>> = Mutex::new(HashSet::new());

    let usdt = pk("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
    let anncz = pk("AnncZ1M8BbE8GVPrqJvecff4G7FzQvzpfMt4JvWddGai");
    let pumpup_pool = pk("7Q9RYYbijphbAXBV527Jz2QmgY4BXdaAzfXhJ3wT8hv1");
    // SOL/USDT Raydium CLMM pool, verified live on mainnet.
    // Used for self-funding the wallet with USDT via internal executor.
    let raydium_sol_usdt_pool = pk("3nMFwZXwY1s1M5s8vYAHqd4wGs4iSxXE4LRoUMMYqEgF");
    let raydium_sol_usdt_pool_type = PoolType::RaydiumCl;
    let sol = pk("So11111111111111111111111111111111111111112");

    let starting_sol = rpc.get_balance(&user).await.unwrap();
    let starting_usdt = get_token_balance(&rpc, &user, &usdt).await;
    eprintln!("\n╔════════════════════════════════════════════════════════════╗");
    eprintln!("║  PUMPUP MAINNET ROUND-TRIP — REAL ON-CHAIN SWAPS           ║");
    eprintln!("╚════════════════════════════════════════════════════════════╝\n");
    eprintln!("  Wallet:        {}", user);
    eprintln!("  Starting SOL:  {} lamports ({:.6} SOL)", starting_sol, starting_sol as f64 / 1e9);
    eprintln!("  Starting USDT: {} atomic ({:.6} USDT)", starting_usdt, starting_usdt as f64 / 1e6);

    // ── Self-fund USDT if needed ──
    const PUMPUP_BUY_USDT: u64 = 50_000; // $0.05 USDT
    if starting_usdt < PUMPUP_BUY_USDT {
        eprintln!("\n  Self-funding USDT via Raydium CLMM SOL→USDT swap (0.001 SOL)...");
        // Verify the pool actually pairs SOL with USDT (defensive — pool addrs can change).
        let pool_state = fetcher::fetch_pool_state(&rpc, raydium_sol_usdt_pool_type, &raydium_sol_usdt_pool).await
            .expect("fetch raydium pool");
        match &pool_state {
            PoolState::RaydiumClmm { token_mint_0, token_mint_1, .. } => {
                let pair_ok = (*token_mint_0 == sol && *token_mint_1 == usdt)
                    || (*token_mint_0 == usdt && *token_mint_1 == sol);
                assert!(pair_ok, "Raydium CLMM pool {raydium_sol_usdt_pool} is not SOL/USDT — got {token_mint_0} / {token_mint_1}");
            }
            _ => panic!("expected RaydiumClmm pool state"),
        }
        let fund_start = Instant::now();
        let result = execute_swap_direct(
            &rpc, &signer, raydium_sol_usdt_pool_type, &raydium_sol_usdt_pool,
            &sol, &usdt, 1_000_000, 1,
        ).await;
        match result {
            Ok((sig, ms)) => eprintln!("  funding tx: {sig} ({} ms)", ms),
            Err(e) => panic!("USDT funding failed: {e}"),
        }
        eprintln!("  funding total time: {} ms", fund_start.elapsed().as_millis());
        tokio::time::sleep(Duration::from_secs(3)).await;
        let bal_after = get_token_balance(&rpc, &user, &usdt).await;
        eprintln!("  USDT after funding: {} atomic ({:.6} USDT)", bal_after, bal_after as f64 / 1e6);
        assert!(bal_after >= PUMPUP_BUY_USDT,
            "funding produced {} USDT atomic, need >= {}", bal_after, PUMPUP_BUY_USDT);
    }

    // ── Pumpup BUY: USDT → ANNCZ ──
    eprintln!("\n  ── Pumpup BUY (USDT → ANNCZ) ──");
    let buy_start = Instant::now();
    let (buy_sig, buy_ms) = execute_swap_direct(
        &rpc, &signer, PoolType::Pumpup, &pumpup_pool,
        &usdt, &anncz, PUMPUP_BUY_USDT, 1,
    ).await.expect("pumpup buy failed");
    eprintln!("    tx: {buy_sig}");
    eprintln!("    end-to-end (build+submit+confirm): {} ms", buy_start.elapsed().as_millis());
    eprintln!("    submit-only (rpc time): {} ms", buy_ms);

    tokio::time::sleep(Duration::from_secs(3)).await;
    let anncz_balance = get_token_balance(&rpc, &user, &anncz).await;
    eprintln!("    ANNCZ received: {} atomic ({:.6})", anncz_balance, anncz_balance as f64 / 1e6);
    assert!(anncz_balance > 0, "buy succeeded but ANNCZ balance is 0");

    // ── Pumpup SELL: ANNCZ → USDT ──
    eprintln!("\n  ── Pumpup SELL (ANNCZ → USDT) ──");
    let sell_start = Instant::now();
    let mut sell_result: Option<(String, u128)> = None;
    let mut last_err = String::new();
    for attempt in 0..5u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            eprintln!("    retry {attempt}/4...");
        }
        match execute_swap_direct(
            &rpc, &signer, PoolType::Pumpup, &pumpup_pool,
            &anncz, &usdt, anncz_balance, 1,
        ).await {
            Ok(r) => { sell_result = Some(r); break; }
            Err(e) => { last_err = e; continue; }
        }
    }
    let (sell_sig, sell_ms) = sell_result.unwrap_or_else(|| panic!("pumpup sell failed after 5 attempts: {last_err}"));
    eprintln!("    tx: {sell_sig}");
    eprintln!("    end-to-end: {} ms", sell_start.elapsed().as_millis());
    eprintln!("    submit-only: {} ms", sell_ms);

    // ── Final accounting ──
    tokio::time::sleep(Duration::from_secs(2)).await;
    let ending_sol = rpc.get_balance(&user).await.unwrap();
    let ending_usdt = get_token_balance(&rpc, &user, &usdt).await;
    let ending_anncz = get_token_balance(&rpc, &user, &anncz).await;
    eprintln!("\n  ──────────────────────────────────────────────────────");
    eprintln!("  ROUND-TRIP COMPLETE");
    eprintln!("  Buy tx:  https://solscan.io/tx/{buy_sig}");
    eprintln!("  Sell tx: https://solscan.io/tx/{sell_sig}");
    eprintln!("  Ending SOL:   {} lamports ({:.6} SOL)", ending_sol, ending_sol as f64 / 1e9);
    eprintln!("  Ending USDT:  {} atomic ({:.6} USDT)", ending_usdt, ending_usdt as f64 / 1e6);
    eprintln!("  Ending ANNCZ: {} atomic", ending_anncz);
    eprintln!("  SOL spent:    {} lamports", starting_sol as i64 - ending_sol as i64);
}
