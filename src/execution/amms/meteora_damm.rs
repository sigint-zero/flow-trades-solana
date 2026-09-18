use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP};

pub struct MeteoraDammExecutor;

impl AmmExecutor for MeteoraDammExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint) = match pool_state {
            PoolState::MeteoraDamm {
                pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint, ..
            } => (pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint),
            _ => return Err(TradeError::Execution("expected MeteoraDamm pool state".into())),
        };

        let input_prog = order.input_token_program;
        let output_prog = order.output_token_program;

        let user_source_ata = get_associated_token_address_with_program_id(&order.user, &order.input_mint, &input_prog);
        let user_dest_ata = get_associated_token_address_with_program_id(&order.user, &order.output_mint, &output_prog);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, &order.output_mint, &output_prog,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL handling
        if order.input_mint == SOL_NATIVE_MINT {
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_source_ata, order.amount_in,
            ));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_source_ata).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_source_ata, &order.user, &order.user, &[],
            ).unwrap());
        }
        if order.output_mint == SOL_NATIVE_MINT {
            let output_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &output_ata, &order.user, &order.user, &[],
            ).unwrap());
        }

        // Data: discriminator + amount_in(u64) + min_out(u64)
        let disc = DISC_SWAP;
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        // pool_authority PDA: seeds = ["pool_authority"]
        let (pool_authority, _) = Pubkey::find_program_address(
            &[b"pool_authority"],
            &METEORA_DAMM_PROG_ID,
        );

        // event_authority PDA: seeds = ["__event_authority"]
        let (event_authority, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &METEORA_DAMM_PROG_ID,
        );

        // Token programs in canonical (a, b) order
        let (token_a_program, token_b_program) = if order.input_mint == *token_a_mint {
            (input_prog, output_prog)
        } else {
            (output_prog, input_prog)
        };

        // Accounts (14) -- DAMM v2 IDL
        let accounts = vec![
            AccountMeta::new_readonly(pool_authority, false),           // [0]
            AccountMeta::new(*pool, false),                             // [1]
            AccountMeta::new(user_source_ata, false),                   // [2]
            AccountMeta::new(user_dest_ata, false),                     // [3]
            AccountMeta::new(*token_a_vault, false),                    // [4]
            AccountMeta::new(*token_b_vault, false),                    // [5]
            AccountMeta::new_readonly(*token_a_mint, false),            // [6]
            AccountMeta::new_readonly(*token_b_mint, false),            // [7]
            AccountMeta::new(order.user, true),                         // [8]
            AccountMeta::new_readonly(token_a_program, false),          // [9]
            AccountMeta::new_readonly(token_b_program, false),          // [10]
            AccountMeta::new_readonly(METEORA_DAMM_PROG_ID, false),     // [11]
            AccountMeta::new_readonly(event_authority, false),          // [12]
            AccountMeta::new_readonly(METEORA_DAMM_PROG_ID, false),     // [13]
        ];

        let swap_ix = Instruction {
            program_id: METEORA_DAMM_PROG_ID,
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
