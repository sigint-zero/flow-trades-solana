use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP_BASE_INPUT};

pub struct RaydiumCpmmExecutor;

impl AmmExecutor for RaydiumCpmmExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (
            pool, authority, config,
            token_0_vault, token_1_vault,
            token_0_mint, token_1_mint,
            observation,
        ) = match pool_state {
            PoolState::RaydiumCpmm {
                pool, authority, config,
                token_0_vault, token_1_vault,
                token_0_mint, token_1_mint,
                observation, ..
            } => (
                pool, authority, config,
                token_0_vault, token_1_vault,
                token_0_mint, token_1_mint,
                observation,
            ),
            _ => return Err(TradeError::Execution(
                "expected RaydiumCpmm pool state".into(),
            )),
        };

        // Determine which vault is input/output by matching input_mint to token_0/token_1
        let (input_vault, output_vault, input_mint, output_mint) =
            if order.input_mint == *token_0_mint {
                (token_0_vault, token_1_vault, token_0_mint, token_1_mint)
            } else if order.input_mint == *token_1_mint {
                (token_1_vault, token_0_vault, token_1_mint, token_0_mint)
            } else {
                return Err(TradeError::Execution(format!(
                    "input mint {} does not match pool token_0 {} or token_1 {}",
                    order.input_mint, token_0_mint, token_1_mint,
                )));
            };

        // Derive user ATAs for input and output mints
        let user_input_ata = get_associated_token_address_with_program_id(&order.user, &order.input_mint, &order.input_token_program);
        let user_output_ata = get_associated_token_address_with_program_id(&order.user, &order.output_mint, &order.output_token_program);

        // Setup: ensure output ATA exists
        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user,
                &order.user,
                &order.output_mint,
                &order.output_token_program,
            ),
        ];

        // Cleanup: close WSOL accounts after swap
        let mut cleanup = Vec::new();

        // WSOL wrapping: if input is native SOL, create WSOL ATA, transfer SOL, sync
        if order.input_mint == SOL_NATIVE_MINT {
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_input_ata, order.amount_in,
            ));
            setup.push(
                spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_input_ata)
                    .unwrap(),
            );
            cleanup.push(
                spl_token::instruction::close_account(
                    &TOKEN_PROGRAM_ID, &user_input_ata, &order.user, &order.user, &[],
                )
                .unwrap(),
            );
        }

        // WSOL unwrapping: if output is native SOL, close WSOL ATA to reclaim lamports
        if order.output_mint == SOL_NATIVE_MINT {
            cleanup.push(
                spl_token::instruction::close_account(
                    &TOKEN_PROGRAM_ID, &user_output_ata, &order.user, &order.user, &[],
                )
                .unwrap(),
            );
        }

        // Build instruction data: discriminator(8) + amount_in(u64 LE) + min_amount_out(u64 LE)
        let disc = DISC_SWAP_BASE_INPUT;
        let mut data = Vec::with_capacity(8 + 8 + 8);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        // 13 accounts for swap_base_input
        let accounts = vec![
            AccountMeta::new(order.user, true),                     // 1. payer (signer, writable)
            AccountMeta::new_readonly(*authority, false),            // 2. authority (PDA)
            AccountMeta::new_readonly(*config, false),               // 3. amm_config
            AccountMeta::new(*pool, false),                          // 4. pool_state
            AccountMeta::new(user_input_ata, false),                 // 5. input_token_account
            AccountMeta::new(user_output_ata, false),                // 6. output_token_account
            AccountMeta::new(*input_vault, false),                   // 7. input_vault
            AccountMeta::new(*output_vault, false),                  // 8. output_vault
            AccountMeta::new_readonly(order.input_token_program, false), // 9. input_token_program
            AccountMeta::new_readonly(order.output_token_program, false), // 10. output_token_program
            AccountMeta::new_readonly(*input_mint, false),           // 11. input_token_mint
            AccountMeta::new_readonly(*output_mint, false),          // 12. output_token_mint
            AccountMeta::new(*observation, false),                   // 13. observation_state
        ];

        let swap_ix = Instruction {
            program_id: RAYDIUM_CPMM_PROG_ID,
            accounts,
            data,
        };

        Ok(SwapInstructions {
            setup,
            swap: vec![swap_ix],
            cleanup,
        })
    }
}
