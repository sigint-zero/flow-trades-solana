use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP, DISC_SWAP_V2};

pub struct PancakeSwapExecutor;


impl AmmExecutor for PancakeSwapExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, amm_config, token_vault_a, token_vault_b, observation,
             token_mint_a, token_mint_b, tick_current, tick_spacing) =
            match pool_state {
                PoolState::PancakeSwap {
                    pool, amm_config, token_vault_a, token_vault_b, observation,
                    token_mint_a, token_mint_b, tick_current, tick_spacing, ..
                } => (pool, amm_config, token_vault_a, token_vault_b, observation,
                      token_mint_a, token_mint_b, *tick_current, *tick_spacing),
                _ => return Err(TradeError::Execution("expected PancakeSwap pool state".into())),
            };

        let a_to_b = order.input_mint == *token_mint_a;

        // Tick arrays for the swap direction, as Raydium CLMM: the current
        // array plus the next two in the direction the price moves
        // (a_to_b: price down → [0, -1, -2]; b_to_a: price up → [0, +1, +2]).
        // Same rules as Raydium CLMM (bitmap extension first, then initialised
        // arrays in walk order) — see `raydium_clmm::clmm_swap_tick_arrays`.
        let (bitmap_ext, tick_arrays) = super::raydium_clmm::clmm_swap_tick_arrays(&PANCAKESWAP_PROG_ID, pool, tick_current, tick_spacing, a_to_b);

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

        if order.input_mint == SOL_NATIVE_MINT {
            let ui = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            setup.insert(0, create_associated_token_account_idempotent(
                &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            ));
            setup.push(solana_sdk::system_instruction::transfer(&order.user, &ui, order.amount_in));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &ui).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &ui, &order.user, &order.user, &[],
            ).unwrap());
        }
        if order.output_mint == SOL_NATIVE_MINT {
            let uo = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &uo, &order.user, &order.user, &[],
            ).unwrap());
        }

        // sqrt_price_limit: boundary values
        let sqrt_price_limit: u128 = if a_to_b { 4295048017 } else { 79226673515401279992447579054 };

        // Raydium-CLMM fork: Token-2022 on either side needs `swap_v2`, which
        // takes the 2022 program, the memo program and both mints after the
        // token program (plain `swap` fails AccountOwnedByWrongProgram on the
        // user's Token-2022 account).
        let needs_token_2022 = order.input_token_program == TOKEN_2022_PROGRAM_ID
            || order.output_token_program == TOKEN_2022_PROGRAM_ID;
        let disc = if needs_token_2022 { DISC_SWAP_V2 } else { DISC_SWAP };
        let mut data = Vec::with_capacity(41);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
        data.push(1u8); // is_base_input = true

        // Determine input/output vaults based on direction
        let (input_vault, output_vault) = if a_to_b {
            (*token_vault_a, *token_vault_b)
        } else {
            (*token_vault_b, *token_vault_a)
        };

        let (input_ata, output_ata) = if a_to_b {
            (user_token_a, user_token_b)
        } else {
            (user_token_b, user_token_a)
        };

        // Accounts -- PancakeSwap CLMM Swap / SwapV2 (Raydium CLMM fork)
        let mut accounts = vec![
            AccountMeta::new(order.user, true),                     // [0] payer
            AccountMeta::new_readonly(*amm_config, false),          // [1] amm_config
            AccountMeta::new(*pool, false),                         // [2] pool_state
            AccountMeta::new(input_ata, false),                     // [3] input_token_account
            AccountMeta::new(output_ata, false),                    // [4] output_token_account
            AccountMeta::new(input_vault, false),                   // [5] input_vault
            AccountMeta::new(output_vault, false),                  // [6] output_vault
            AccountMeta::new(*observation, false),                  // [7] observation_state
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),     // [8] token_program
        ];
        if needs_token_2022 {
            accounts.push(AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false)); // token_program_2022
            accounts.push(AccountMeta::new_readonly(MEMO_PROGRAM_ID, false));       // memo_program
            accounts.push(AccountMeta::new_readonly(order.input_mint, false));      // input_vault_mint
            accounts.push(AccountMeta::new_readonly(order.output_mint, false));     // output_vault_mint
        }
        // v1: named tick_array, then remaining [ext, arrays…]; v2: remaining [ext, arrays…]
        super::raydium_clmm::push_tick_array_accounts(&mut accounts, needs_token_2022, bitmap_ext, &tick_arrays);

        let swap_ix = Instruction {
            program_id: PANCAKESWAP_PROG_ID,
            accounts,
            data,
        };

        Ok(SwapInstructions { setup, swap: vec![swap_ix], cleanup })
    }
}
