use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::AmmExecutor;

pub struct FluxBeamExecutor;

// FluxBeam swap instruction discriminator (SPL Token Swap compatible)
const SWAP_DISC: u8 = 1;

impl AmmExecutor for FluxBeamExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, authority, token_a_vault, token_b_vault, pool_mint,
             fee_account, token_a_mint, _token_b_mint, pool_token_program) =
            match pool_state {
                PoolState::FluxBeam {
                    pool, authority, token_a_vault, token_b_vault,
                    pool_mint, fee_account, token_a_mint, token_b_mint,
                    pool_token_program, ..
                } => (pool, authority, token_a_vault, token_b_vault, pool_mint,
                      fee_account, token_a_mint, token_b_mint, pool_token_program),
                _ => return Err(TradeError::Execution("expected FluxBeam pool state".into())),
            };

        let is_a_to_b = order.input_mint == *token_a_mint;
        let (source_vault, dest_vault) = if is_a_to_b {
            (token_a_vault, token_b_vault)
        } else {
            (token_b_vault, token_a_vault)
        };

        let user_source = get_associated_token_address_with_program_id(&order.user, &order.input_mint, &order.input_token_program);
        let user_dest = get_associated_token_address_with_program_id(&order.user, &order.output_mint, &order.output_token_program);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, &order.output_mint, &order.output_token_program,
            ),
        ];
        let mut cleanup = Vec::new();
        add_wsol_handling(&mut setup, &mut cleanup, order);

        // Data: [1 (swap)] + amount_in(u64) + min_amount_out(u64)
        let mut data = Vec::with_capacity(17);
        data.push(SWAP_DISC);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        // FluxBeam is a Token-2022 aware SPL Token Swap fork -- 15 accounts.
        let source_mint = order.input_mint;
        let dest_mint = order.output_mint;
        let source_token_program = order.input_token_program;
        let dest_token_program = order.output_token_program;

        let accounts = vec![
            AccountMeta::new_readonly(*pool, false),                         // [0]
            AccountMeta::new_readonly(*authority, false),                    // [1]
            AccountMeta::new_readonly(order.user, true),                    // [2]
            AccountMeta::new(user_source, false),                           // [3]
            AccountMeta::new(*source_vault, false),                         // [4]
            AccountMeta::new(*dest_vault, false),                           // [5]
            AccountMeta::new(user_dest, false),                             // [6]
            AccountMeta::new(*pool_mint, false),                            // [7]
            AccountMeta::new(*fee_account, false),                          // [8]
            AccountMeta::new_readonly(source_mint, false),                  // [9]
            AccountMeta::new_readonly(dest_mint, false),                    // [10]
            AccountMeta::new_readonly(source_token_program, false),         // [11]
            AccountMeta::new_readonly(dest_token_program, false),           // [12]
            AccountMeta::new_readonly(*pool_token_program, false),          // [13]
            AccountMeta::new(*fee_account, false),                          // [14] host_fee = fee_account
        ];

        let swap_ix = Instruction {
            program_id: FLUXBEAM_PROG_ID,
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

fn add_wsol_handling(
    setup: &mut Vec<solana_sdk::instruction::Instruction>,
    cleanup: &mut Vec<solana_sdk::instruction::Instruction>,
    order: &SwapOrder,
) {
    if order.input_mint == SOL_NATIVE_MINT {
        let wsol_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
        setup.insert(0, create_associated_token_account_idempotent(
            &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
        ));
        setup.push(solana_sdk::system_instruction::transfer(
            &order.user, &wsol_ata, order.amount_in,
        ));
        setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &wsol_ata).unwrap());
        cleanup.push(spl_token::instruction::close_account(
            &TOKEN_PROGRAM_ID, &wsol_ata, &order.user, &order.user, &[],
        ).unwrap());
    }
    if order.output_mint == SOL_NATIVE_MINT {
        let wsol_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
        cleanup.push(spl_token::instruction::close_account(
            &TOKEN_PROGRAM_ID, &wsol_ata, &order.user, &order.user, &[],
        ).unwrap());
    }
}
