use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::AmmExecutor;

pub struct SarosExecutor;

// Saros uses SPL Token Swap compatible instruction layout
const SWAP_DISC: u8 = 1;

impl AmmExecutor for SarosExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, authority, token_a_vault, token_b_vault, pool_mint, fee_account, token_a_mint, _token_b_mint) =
            match pool_state {
                PoolState::Saros {
                    pool, authority, token_a_vault, token_b_vault,
                    pool_mint, fee_account, token_a_mint, token_b_mint, ..
                } => (pool, authority, token_a_vault, token_b_vault, pool_mint, fee_account, token_a_mint, token_b_mint),
                _ => return Err(TradeError::Execution("expected Saros pool state".into())),
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

        if order.input_mint == SOL_NATIVE_MINT {
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(&order.user, &user_source, order.amount_in));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_source).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_source, &order.user, &order.user, &[],
            ).unwrap());
        }
        if order.output_mint == SOL_NATIVE_MINT {
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_dest, &order.user, &order.user, &[],
            ).unwrap());
        }

        let mut data = Vec::with_capacity(17);
        data.push(SWAP_DISC);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        let accounts = vec![
            AccountMeta::new_readonly(*pool, false),
            AccountMeta::new_readonly(*authority, false),
            AccountMeta::new_readonly(order.user, true),
            AccountMeta::new(user_source, false),
            AccountMeta::new(*source_vault, false),
            AccountMeta::new(*dest_vault, false),
            AccountMeta::new(user_dest, false),
            AccountMeta::new(*pool_mint, false),
            AccountMeta::new(*fee_account, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ];

        let swap_ix = Instruction {
            program_id: SAROS_PROG_ID,
            accounts,
            data,
        };

        Ok(SwapInstructions { setup, swap: vec![swap_ix], cleanup })
    }
}
