use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP};

pub struct MeteoraExecutor;

impl AmmExecutor for MeteoraExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (
            pool, token_a_mint, _token_b_mint,
            a_vault, b_vault, a_token_vault, b_token_vault,
            a_vault_lp_mint, b_vault_lp_mint, a_vault_lp, b_vault_lp,
            admin_token_a_fee, admin_token_b_fee, vault_program,
        ) = match pool_state {
            PoolState::Meteora {
                pool, token_a_mint, token_b_mint,
                a_vault, b_vault, a_token_vault, b_token_vault,
                a_vault_lp_mint, b_vault_lp_mint, a_vault_lp, b_vault_lp,
                admin_token_a_fee, admin_token_b_fee, vault_program, ..
            } => (
                pool, token_a_mint, token_b_mint,
                a_vault, b_vault, a_token_vault, b_token_vault,
                a_vault_lp_mint, b_vault_lp_mint, a_vault_lp, b_vault_lp,
                admin_token_a_fee, admin_token_b_fee, vault_program,
            ),
            _ => return Err(TradeError::Execution("expected Meteora pool state".into())),
        };

        let user_source = get_associated_token_address_with_program_id(&order.user, &order.input_mint, &order.input_token_program);
        let user_dest = get_associated_token_address_with_program_id(&order.user, &order.output_mint, &order.output_token_program);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, &order.output_mint, &order.output_token_program,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL handling
        if order.input_mint == SOL_NATIVE_MINT {
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_source, order.amount_in,
            ));
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

        // Data: discriminator + in_amount(u64) + minimum_out_amount(u64)
        let disc = DISC_SWAP;
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        // Choose admin_token_fee based on input (fee charged on the input token)
        let admin_token_fee = if order.input_mint == *token_a_mint {
            *admin_token_a_fee
        } else {
            *admin_token_b_fee
        };

        // Accounts (15)
        let accounts = vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new(user_source, false),
            AccountMeta::new(user_dest, false),
            AccountMeta::new(*a_vault, false),
            AccountMeta::new(*b_vault, false),
            AccountMeta::new(*a_token_vault, false),
            AccountMeta::new(*b_token_vault, false),
            AccountMeta::new(*a_vault_lp_mint, false),
            AccountMeta::new(*b_vault_lp_mint, false),
            AccountMeta::new(*a_vault_lp, false),
            AccountMeta::new(*b_vault_lp, false),
            AccountMeta::new(admin_token_fee, false),
            AccountMeta::new(order.user, true),
            AccountMeta::new_readonly(*vault_program, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ];

        let swap_ix = Instruction {
            program_id: METEORA_PROG_ID,
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
