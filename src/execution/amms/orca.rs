use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP, DISC_SWAP_V2};

pub struct OrcaExecutor;

/// Derive Orca tick array PDA (88 ticks per array, STRING seed for start_index).
fn derive_tick_array(whirlpool: &Pubkey, tick_current: i32, tick_spacing: i32, offset: i32) -> Pubkey {
    let ticks_per_array = 88 * tick_spacing;
    let start_index = if ticks_per_array == 0 {
        0
    } else {
        let array_idx = tick_current.div_euclid(ticks_per_array) + offset;
        array_idx * ticks_per_array
    };
    // Orca Whirlpool uses the STRING representation of start_index as PDA seed
    let start_str = start_index.to_string();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_str.as_bytes()],
        &ORCA_PROG_ID,
    );
    pda
}

impl AmmExecutor for OrcaExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (
            whirlpool, token_vault_a, token_vault_b,
            oracle, token_mint_a, token_mint_b,
            tick_current, tick_spacing,
        ) = match pool_state {
            PoolState::Orca {
                whirlpool, token_vault_a, token_vault_b,
                oracle, token_mint_a, token_mint_b,
                tick_current, tick_spacing, ..
            } => (
                whirlpool, token_vault_a, token_vault_b,
                oracle, token_mint_a, token_mint_b,
                *tick_current, *tick_spacing,
            ),
            _ => return Err(TradeError::Execution("expected Orca pool state".into())),
        };

        // Direction
        let a_to_b = order.input_mint == *token_mint_a;

        // Derive tick arrays based on swap direction
        let ta_current = derive_tick_array(whirlpool, tick_current, tick_spacing, 0);
        let (ta0, ta1, ta2) = if a_to_b {
            let ta_prev = derive_tick_array(whirlpool, tick_current, tick_spacing, -1);
            (ta_current, ta_prev, ta_prev)
        } else {
            let ta_next = derive_tick_array(whirlpool, tick_current, tick_spacing, 1);
            (ta_current, ta_next, ta_next)
        };

        // Determine token programs for A and B mints
        let (prog_a, prog_b) = if a_to_b {
            (order.input_token_program, order.output_token_program)
        } else {
            (order.output_token_program, order.input_token_program)
        };
        let user_token_a = get_associated_token_address_with_program_id(&order.user, token_mint_a, &prog_a);
        let user_token_b = get_associated_token_address_with_program_id(&order.user, token_mint_b, &prog_b);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, &order.output_mint, &order.output_token_program,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL handling
        if order.input_mint == SOL_NATIVE_MINT {
            let user_input_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_input_ata, order.amount_in,
            ));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_input_ata).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_input_ata, &order.user, &order.user, &[],
            ).unwrap());
        }
        if order.output_mint == SOL_NATIVE_MINT {
            let user_output_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_output_ata, &order.user, &order.user, &[],
            ).unwrap());
        }

        // sqrt_price_limit: use MIN+1 / MAX-1 to accept any price
        let sqrt_price_limit: u128 = if a_to_b {
            4295048017
        } else {
            79226673515401279992447579054
        };

        let needs_token_2022 = order.input_token_program == TOKEN_2022_PROGRAM_ID
            || order.output_token_program == TOKEN_2022_PROGRAM_ID;

        let disc = if needs_token_2022 {
            DISC_SWAP_V2
        } else {
            DISC_SWAP
        };

        let data_len = if needs_token_2022 { 43 } else { 42 };
        let mut data = Vec::with_capacity(data_len);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
        data.push(1u8); // amount_specified_is_input = true
        data.push(if a_to_b { 1u8 } else { 0u8 });
        if needs_token_2022 {
            data.push(0u8); // empty remaining_accounts_info
        }

        let accounts = if needs_token_2022 {
            // swap_v2: 15 accounts
            vec![
                AccountMeta::new_readonly(prog_a, false),
                AccountMeta::new_readonly(prog_b, false),
                AccountMeta::new_readonly(crate::constants::MEMO_PROGRAM_ID, false),
                AccountMeta::new(order.user, true),
                AccountMeta::new(*whirlpool, false),
                AccountMeta::new_readonly(*token_mint_a, false),
                AccountMeta::new_readonly(*token_mint_b, false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(*token_vault_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(*token_vault_b, false),
                AccountMeta::new(ta0, false),
                AccountMeta::new(ta1, false),
                AccountMeta::new(ta2, false),
                AccountMeta::new(*oracle, false) // writable: adaptive-fee oracle (ConstraintMut on Token-2022 pools otherwise),
            ]
        } else {
            // swap: 11 accounts
            vec![
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new(order.user, true),
                AccountMeta::new(*whirlpool, false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(*token_vault_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(*token_vault_b, false),
                AccountMeta::new(ta0, false),
                AccountMeta::new(ta1, false),
                AccountMeta::new(ta2, false),
                AccountMeta::new(*oracle, false) // writable: adaptive-fee oracle (ConstraintMut on Token-2022 pools otherwise),
            ]
        };

        let swap_ix = Instruction {
            program_id: ORCA_PROG_ID,
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
