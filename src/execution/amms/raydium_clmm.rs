use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP, DISC_SWAP_V2};

pub struct RaydiumClmmExecutor;

/// Derive a tick array PDA for Raydium CLMM pools.
/// Tick arrays for a Raydium-style CLMM swap: `(bitmap_extension, arrays in
/// walk order)`. Uses `quote::clmm::TICKS` when the pool's ticks are loaded
/// (extension present iff it exists on chain; arrays = the initialised ones the
/// program walks); falls back to [current, ±1, ±2] and no extension otherwise.
pub fn clmm_swap_tick_arrays(program: &Pubkey, pool: &Pubkey, tick_current: i32, tick_spacing: i32, a_to_b: bool) -> (Option<Pubkey>, Vec<Pubkey>) {
    use crate::quote::clmm::{TickLayout, TICKS};
    let layout = TickLayout::Raydium;
    if let Some(td) = TICKS.get(pool) {
        let starts = td.arrays_for_swap(layout, tick_current, tick_spacing, a_to_b, 3);
        if !starts.is_empty() {
            return (td.bitmap_extension, starts.iter().map(|s| layout.array_pda(program, pool, *s)).collect());
        }
    }
    let dir = if a_to_b { -1 } else { 1 };
    (None, (0..3).map(|k| derive_tick_array(program, pool, tick_current, tick_spacing, k * dir)).collect())
}

/// Append the tick-array accounts in the order the program reads them.
/// `swap` (v1) names the FIRST tick array as an account of the instruction and
/// takes the extension + further arrays as remaining accounts; `swap_v2` has
/// no named tick array — everything is remaining, extension first. Getting
/// this wrong is `AccountDiscriminatorMismatch` on `tick_array`.
pub fn push_tick_array_accounts(accounts: &mut Vec<AccountMeta>, v2: bool, ext: Option<Pubkey>, arrays: &[Pubkey]) {
    if v2 {
        if let Some(e) = ext {
            accounts.push(AccountMeta::new(e, false));
        }
        for ta in arrays {
            accounts.push(AccountMeta::new(*ta, false));
        }
    } else {
        if let Some(first) = arrays.first() {
            accounts.push(AccountMeta::new(*first, false));
        }
        if let Some(e) = ext {
            accounts.push(AccountMeta::new(e, false));
        }
        for ta in arrays.iter().skip(1) {
            accounts.push(AccountMeta::new(*ta, false));
        }
    }
}

/// Seeds: ["tick_array", pool, start_tick_index.to_be_bytes()]
fn derive_tick_array(
    program_id: &Pubkey,
    pool: &Pubkey,
    tick_current: i32,
    tick_spacing: i32,
    offset: i32,
) -> Pubkey {
    let ticks_per_array = 60 * tick_spacing; // Raydium CLMM: 60 ticks per array
    let start_index = if ticks_per_array == 0 {
        0
    } else {
        let array_idx = tick_current.div_euclid(ticks_per_array) + offset;
        array_idx * ticks_per_array
    };
    let start_bytes = start_index.to_be_bytes();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start_bytes],
        program_id,
    );
    pda
}

impl AmmExecutor for RaydiumClmmExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (
            pool, amm_config, observation,
            token_vault_0, token_vault_1,
            token_mint_0, _token_mint_1,
            tick_current, tick_spacing,
        ) = match pool_state {
            PoolState::RaydiumClmm {
                pool, amm_config, observation,
                token_vault_0, token_vault_1,
                token_mint_0, token_mint_1,
                tick_current, tick_spacing,
                // tick_array_0/1/2 from pool fetch are ignored --
                // we re-derive based on swap direction below.
                ..
            } => (
                pool, amm_config, observation,
                token_vault_0, token_vault_1,
                token_mint_0, token_mint_1,
                *tick_current, *tick_spacing,
            ),
            _ => return Err(TradeError::Execution("expected RaydiumClmm pool state".into())),
        };

        // Determine swap direction: a_to_b or b_to_a
        let a_to_b = order.input_mint == *token_mint_0;
        let (input_vault, output_vault) = if a_to_b {
            (token_vault_0, token_vault_1)
        } else {
            (token_vault_1, token_vault_0)
        };

        // Tick arrays. With the pool's ticks in memory (loaded by the quoter's
        // cold path / revalidation) the program gets exactly what it checks:
        // the bitmap extension first, then the first INITIALISED array at or
        // beyond the current tick in the swap direction, then the next ones.
        // Passing the current array when it holds no initialised tick is what
        // produced `InvalidFirstTickArrayAccount`; passing fewer arrays than the
        // swap crosses is `NotEnoughTickArrayAccount`. Without tick data fall
        // back to the current array and the next two (pruned of missing
        // accounts by the API layer).
        let (bitmap_ext, tick_arrays) = clmm_swap_tick_arrays(&RAYDIUM_CL_PROG_ID, pool, tick_current, tick_spacing, a_to_b);

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

        // WSOL handling
        let mut cleanup = Vec::new();
        if order.input_mint == SOL_NATIVE_MINT {
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
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_output_ata, &order.user, &order.user, &[],
            ).unwrap());
        }

        // sqrt_price_limit: use MIN+1 / MAX-1 to accept any price
        let sqrt_price_limit: u128 = if a_to_b {
            4295048017 // MIN_SQRT_PRICE_X64 (4295048016) + 1
        } else {
            79226673515401279992447579054 // MAX_SQRT_PRICE_X64 (79226673515401279992447579055) - 1
        };

        // Build instruction data: disc + amount(u64) + other_amount_threshold(u64)
        // + sqrt_price_limit(u128) + is_base_input(bool)
        let needs_token_2022 = order.input_token_program == TOKEN_2022_PROGRAM_ID
            || order.output_token_program == TOKEN_2022_PROGRAM_ID;

        let disc = if needs_token_2022 {
            DISC_SWAP_V2
        } else {
            DISC_SWAP
        };
        let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
        data.push(1u8); // is_base_input = true

        let mut accounts = vec![
            AccountMeta::new_readonly(order.user, true),        // 0: payer
            AccountMeta::new_readonly(*amm_config, false),      // 1: amm_config
            AccountMeta::new(*pool, false),                     // 2: pool_state
            AccountMeta::new(user_input_ata, false),            // 3: input_token_account
            AccountMeta::new(user_output_ata, false),           // 4: output_token_account
            AccountMeta::new(*input_vault, false),              // 5: input_vault
            AccountMeta::new(*output_vault, false),             // 6: output_vault
            AccountMeta::new(*observation, false),              // 7: observation_state
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false), // 8: token_program
        ];

        if needs_token_2022 {
            // swap_v2: additional accounts for Token-2022 support
            accounts.push(AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false)); // 9: token_program_2022
            accounts.push(AccountMeta::new_readonly(MEMO_PROGRAM_ID, false));       // 10: memo_program
            accounts.push(AccountMeta::new_readonly(order.input_mint, false));      // 11: input_vault_mint
            accounts.push(AccountMeta::new_readonly(order.output_mint, false));     // 12: output_vault_mint
        }

        // v1: named tick_array, then remaining [ext, arrays…]; v2: remaining [ext, arrays…]
        push_tick_array_accounts(&mut accounts, needs_token_2022, bitmap_ext, &tick_arrays);

        let swap_ix = Instruction {
            program_id: RAYDIUM_CL_PROG_ID,
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
mod tick_array_order_tests {
    use super::*;

    #[test]
    fn v1_names_the_first_array_and_v2_puts_the_extension_first() {
        let (ext, a, b, c) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut v1 = Vec::new();
        push_tick_array_accounts(&mut v1, false, Some(ext), &[a, b, c]);
        assert_eq!(v1.iter().map(|m| m.pubkey).collect::<Vec<_>>(), vec![a, ext, b, c]);
        let mut v2 = Vec::new();
        push_tick_array_accounts(&mut v2, true, Some(ext), &[a, b, c]);
        assert_eq!(v2.iter().map(|m| m.pubkey).collect::<Vec<_>>(), vec![ext, a, b, c]);
        let mut none = Vec::new();
        push_tick_array_accounts(&mut none, false, None, &[a, b]);
        assert_eq!(none.iter().map(|m| m.pubkey).collect::<Vec<_>>(), vec![a, b]);
        assert!(v1.iter().all(|m| m.is_writable));
    }

    #[test]
    fn without_tick_data_falls_back_to_three_derived_arrays() {
        let pool = Pubkey::new_unique();
        let (ext, arrays) = clmm_swap_tick_arrays(&RAYDIUM_CL_PROG_ID, &pool, 1234, 10, true);
        assert!(ext.is_none());
        assert_eq!(arrays.len(), 3);
        assert_eq!(arrays[0], derive_tick_array(&RAYDIUM_CL_PROG_ID, &pool, 1234, 10, 0));
        assert_eq!(arrays[1], derive_tick_array(&RAYDIUM_CL_PROG_ID, &pool, 1234, 10, -1));
    }
}
