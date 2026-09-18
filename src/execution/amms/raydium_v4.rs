use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::{RAYDIUM_V4_PROG_ID, SOL_NATIVE_MINT, TOKEN_PROGRAM_ID};

use super::AmmExecutor;

/// Raydium V4 (legacy AMM) swap instruction builder.
pub struct RaydiumV4Executor;

/// SPL Token `SyncNative` instruction index.
const SPL_SYNC_NATIVE_IX: u8 = 17;

/// SPL Token `CloseAccount` instruction index.
const SPL_CLOSE_ACCOUNT_IX: u8 = 9;

impl AmmExecutor for RaydiumV4Executor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let PoolState::RaydiumV4 {
            amm_id,
            authority,
            open_orders,
            target_orders,
            coin_vault,
            pc_vault,
            serum_program,
            serum_market,
            serum_bids,
            serum_asks,
            serum_event_queue,
            serum_coin_vault,
            serum_pc_vault,
            serum_vault_signer,
        } = pool_state
        else {
            return Err(TradeError::Execution(
                "RaydiumV4Executor requires PoolState::RaydiumV4".into(),
            ));
        };

        let user = order.user;
        let input_is_sol = order.input_mint == SOL_NATIVE_MINT;
        let output_is_sol = order.output_mint == SOL_NATIVE_MINT;

        let user_source_ata = get_associated_token_address_with_program_id(&user, &order.input_mint, &order.input_token_program);
        let user_dest_ata = get_associated_token_address_with_program_id(&user, &order.output_mint, &order.output_token_program);

        // -- Setup: create ATAs + WSOL wrapping --
        let mut setup = Vec::new();

        // Always ensure the source ATA exists (idempotent).
        setup.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &order.input_mint,
            &order.input_token_program,
        ));

        // Always ensure the destination ATA exists (idempotent).
        setup.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &order.output_mint,
            &order.output_token_program,
        ));

        // If swapping native SOL, fund the WSOL ATA and sync its balance.
        if input_is_sol {
            // Transfer lamports into the WSOL ATA.
            setup.push(system_instruction::transfer(
                &user,
                &user_source_ata,
                order.amount_in,
            ));

            // SyncNative: tells the token program to update the token balance
            // to match the account's lamport balance.
            setup.push(Instruction {
                program_id: TOKEN_PROGRAM_ID,
                accounts: vec![AccountMeta::new(user_source_ata, false)],
                data: vec![SPL_SYNC_NATIVE_IX],
            });
        }

        // -- Swap instruction --
        // Instruction data: discriminator 0x09 ++ amount_in (u64 LE) ++ min_amount_out (u64 LE)
        let mut ix_data = Vec::with_capacity(1 + 8 + 8);
        ix_data.push(0x09);
        ix_data.extend_from_slice(&order.amount_in.to_le_bytes());
        ix_data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        let swap_ix = Instruction {
            program_id: RAYDIUM_V4_PROG_ID,
            accounts: vec![
                // 1.  TOKEN_PROGRAM_ID
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                // 2.  amm_id (the pool, writable for state updates)
                AccountMeta::new(*amm_id, false),
                // 3.  authority (PDA, read-only)
                AccountMeta::new_readonly(*authority, false),
                // 4.  open_orders (writable)
                AccountMeta::new(*open_orders, false),
                // 5.  target_orders (writable)
                AccountMeta::new(*target_orders, false),
                // 6.  coin_vault (writable)
                AccountMeta::new(*coin_vault, false),
                // 7.  pc_vault (writable)
                AccountMeta::new(*pc_vault, false),
                // 8.  serum_program (read-only)
                AccountMeta::new_readonly(*serum_program, false),
                // 9.  serum_market (writable)
                AccountMeta::new(*serum_market, false),
                // 10. serum_bids (writable)
                AccountMeta::new(*serum_bids, false),
                // 11. serum_asks (writable)
                AccountMeta::new(*serum_asks, false),
                // 12. serum_event_queue (writable)
                AccountMeta::new(*serum_event_queue, false),
                // 13. serum_coin_vault (writable)
                AccountMeta::new(*serum_coin_vault, false),
                // 14. serum_pc_vault (writable)
                AccountMeta::new(*serum_pc_vault, false),
                // 15. serum_vault_signer (read-only PDA)
                AccountMeta::new_readonly(*serum_vault_signer, false),
                // 16. user_source_token (writable)
                AccountMeta::new(user_source_ata, false),
                // 17. user_dest_token (writable)
                AccountMeta::new(user_dest_ata, false),
                // 18. user (signer)
                AccountMeta::new_readonly(user, true),
            ],
            data: ix_data,
        };

        // -- Cleanup: close WSOL account(s) to reclaim lamports --
        let mut cleanup = Vec::new();

        if input_is_sol {
            cleanup.push(build_close_account_ix(
                &user_source_ata,
                &user,
                &user,
            ));
        }

        if output_is_sol {
            cleanup.push(build_close_account_ix(
                &user_dest_ata,
                &user,
                &user,
            ));
        }

        Ok(SwapInstructions {
            setup,
            swap: vec![swap_ix],
            cleanup,
        })
    }
}

/// Build an SPL Token `CloseAccount` instruction.
fn build_close_account_ix(
    account: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data: vec![SPL_CLOSE_ACCOUNT_IX],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::types::PoolType;
    use solana_sdk::pubkey::Pubkey;

    fn dummy_pubkey(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn make_pool_state() -> PoolState {
        PoolState::RaydiumV4 {
            amm_id: dummy_pubkey(1),
            authority: dummy_pubkey(2),
            open_orders: dummy_pubkey(3),
            target_orders: dummy_pubkey(4),
            coin_vault: dummy_pubkey(5),
            pc_vault: dummy_pubkey(6),
            serum_program: dummy_pubkey(14),
            serum_market: dummy_pubkey(7),
            serum_bids: dummy_pubkey(8),
            serum_asks: dummy_pubkey(9),
            serum_event_queue: dummy_pubkey(10),
            serum_coin_vault: dummy_pubkey(11),
            serum_pc_vault: dummy_pubkey(12),
            serum_vault_signer: dummy_pubkey(13),
        }
    }

    fn make_order(input_mint: Pubkey, output_mint: Pubkey) -> SwapOrder {
        SwapOrder {
            pool_address: dummy_pubkey(1),
            pool_type: PoolType::RaydiumV4,
            input_mint,
            output_mint,
            amount_in: 1_000_000_000,
            min_amount_out: 500_000,
            user: dummy_pubkey(99),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_PROGRAM_ID,
        }
    }

    #[test]
    fn test_token_to_token_swap() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state();
        let order = make_order(dummy_pubkey(20), dummy_pubkey(21));

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA-idempotent (no WSOL wrapping)
        assert_eq!(ixs.setup.len(), 2);
        // Swap: exactly 1 instruction
        assert_eq!(ixs.swap.len(), 1);
        // Cleanup: none (no WSOL)
        assert_eq!(ixs.cleanup.len(), 0);

        // Verify swap instruction data
        let swap_data = &ixs.swap[0].data;
        assert_eq!(swap_data[0], 0x09);
        let amount_in = u64::from_le_bytes(swap_data[1..9].try_into().unwrap());
        let min_out = u64::from_le_bytes(swap_data[9..17].try_into().unwrap());
        assert_eq!(amount_in, 1_000_000_000);
        assert_eq!(min_out, 500_000);

        // Verify 18 accounts on swap instruction
        assert_eq!(ixs.swap[0].accounts.len(), 18);
        // First account is TOKEN_PROGRAM_ID
        assert_eq!(ixs.swap[0].accounts[0].pubkey, TOKEN_PROGRAM_ID);
        // Last account is user (signer)
        assert!(ixs.swap[0].accounts[17].is_signer);
    }

    #[test]
    fn test_sol_input_wrapping() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state();
        let order = make_order(SOL_NATIVE_MINT, dummy_pubkey(21));

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA + 1 system transfer + 1 SyncNative = 4
        assert_eq!(ixs.setup.len(), 4);
        // Cleanup: 1 close WSOL source ATA
        assert_eq!(ixs.cleanup.len(), 1);
    }

    #[test]
    fn test_sol_output_unwrapping() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state();
        let order = make_order(dummy_pubkey(20), SOL_NATIVE_MINT);

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA (no wrapping needed for output)
        assert_eq!(ixs.setup.len(), 2);
        // Cleanup: 1 close WSOL dest ATA
        assert_eq!(ixs.cleanup.len(), 1);
    }

    #[test]
    fn test_sol_to_sol_both_sides() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state();
        let order = make_order(SOL_NATIVE_MINT, SOL_NATIVE_MINT);

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA + 1 transfer + 1 SyncNative = 4
        assert_eq!(ixs.setup.len(), 4);
        // Cleanup: 2 close (source + dest)
        assert_eq!(ixs.cleanup.len(), 2);
    }

    #[test]
    fn test_wrong_pool_state_variant() {
        let executor = RaydiumV4Executor;
        let wrong_pool = PoolState::MeteoraDamm {
            pool: dummy_pubkey(1),
            token_a_vault: dummy_pubkey(2),
            token_b_vault: dummy_pubkey(3),
            token_a_mint: dummy_pubkey(4),
            token_b_mint: dummy_pubkey(5),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let order = make_order(dummy_pubkey(20), dummy_pubkey(21));

        let result = executor.build_swap_ix(&order, &wrong_pool);
        assert!(result.is_err());
    }
}
