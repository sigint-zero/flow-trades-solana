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
        let (pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint, fees) = match pool_state {
            PoolState::MeteoraDamm {
                pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint, fees, ..
            } => (pool, token_a_vault, token_b_vault, token_a_mint, token_b_mint, fees),
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
        let mut accounts = vec![
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
        // A rate-limiter pool inside its window checks, via the instructions
        // sysvar (first remaining account), that this is the only swap on the
        // pool in the transaction; without it the swap fails.
        if fees.fee_scheduler_mode == crate::quote::damm_v2::BASE_FEE_RATE_LIMITER {
            accounts.push(AccountMeta::new_readonly(solana_sdk::sysvar::instructions::ID, false));
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::types::PoolType;

    fn build(fee_scheduler_mode: u8) -> Instruction {
        let (mint_a, mint_b, pool) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let state = PoolState::MeteoraDamm {
            pool,
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0,
            fees: crate::quote::damm_v2::DammFees { fee_scheduler_mode, ..Default::default() },
            activation_point: 0, activation_type: 0, collect_fee_mode: 1, pool_status: 0,
        };
        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::MeteoraDamm,
            input_mint: mint_b,
            output_mint: mint_a,
            amount_in: 1_000,
            min_amount_out: 1,
            user: Pubkey::new_unique(),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_PROGRAM_ID,
        };
        MeteoraDammExecutor.build_swap_ix(&order, &state).unwrap().swap.remove(0)
    }

    #[test]
    fn rate_limiter_pools_get_the_instructions_sysvar() {
        assert_eq!(build(0).accounts.len(), 14);
        let ix = build(crate::quote::damm_v2::BASE_FEE_RATE_LIMITER);
        assert_eq!(ix.accounts.len(), 15);
        assert_eq!(ix.accounts[14].pubkey, solana_sdk::sysvar::instructions::ID);
        assert!(!ix.accounts[14].is_writable && !ix.accounts[14].is_signer);
    }
}
