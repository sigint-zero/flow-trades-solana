use solana_sdk::instruction::{AccountMeta, Instruction};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP, DISC_SWAP_V2};

/// Byreal CLMM — a Raydium CLMM fork: same `swap` / `swap_v2` instructions,
/// account order and tick-array PDAs, under Byreal's program id. Pools with
/// Byreal's dynamic fee accept only `swap_v3_dyn` (the `swap_v2` accounts,
/// plus the two Pyth price accounts as the LAST remaining accounts).
pub struct ByrealExecutor;

/// Anchor discriminator of Byreal's `swap_v3_dyn`.
pub const DISC_SWAP_V3_DYN: [u8; 8] = [0xe5, 0x2e, 0xd5, 0x84, 0x69, 0x28, 0x28, 0xe4];

impl AmmExecutor for ByrealExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, amm_config, token_vault_a, token_vault_b, observation,
             token_mint_a, token_mint_b, tick_current, tick_spacing, fee) =
            match pool_state {
                PoolState::Byreal {
                    pool, amm_config, token_vault_a, token_vault_b, observation,
                    token_mint_a, token_mint_b, tick_current, tick_spacing, fee, ..
                } => (pool, amm_config, token_vault_a, token_vault_b, observation,
                      token_mint_a, token_mint_b, *tick_current, *tick_spacing, fee),
                _ => return Err(TradeError::Execution("expected Byreal pool state".into())),
            };
        let dynamic = fee.is_dynamic();

        let a_to_b = order.input_mint == *token_mint_a;
        if !a_to_b && order.input_mint != *token_mint_b {
            return Err(TradeError::Execution(format!("byreal pool {pool} does not trade {}", order.input_mint)));
        }

        // Same tick-array rules as Raydium CLMM (bitmap extension first, then
        // the initialised arrays in walk order).
        let (bitmap_ext, tick_arrays) = super::raydium_clmm::clmm_swap_tick_arrays(&BYREAL_PROG_ID, pool, tick_current, tick_spacing, a_to_b);
        if dynamic && !crate::quote::clmm::TICKS.get(pool).is_some_and(|t| !t.initialized_arrays.is_empty()) {
            // the oracle accounts go last, where tick-array pruning would look
            return Err(TradeError::Execution(format!("byreal pool {pool}: tick arrays not loaded")));
        }

        let user_input_ata = get_associated_token_address_with_program_id(&order.user, &order.input_mint, &order.input_token_program);
        let user_output_ata = get_associated_token_address_with_program_id(&order.user, &order.output_mint, &order.output_token_program);

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
            setup.push(solana_sdk::system_instruction::transfer(&order.user, &user_input_ata, order.amount_in));
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

        // sqrt_price_limit: MIN+1 / MAX-1 (accept any price)
        let sqrt_price_limit: u128 = if a_to_b { 4295048017 } else { 79226673515401279992447579054 };

        // Token-2022 on either side needs `swap_v2` (2022 program, memo program
        // and both mints after the token program); `swap_v3_dyn` has the v2 accounts.
        let needs_token_2022 = order.input_token_program == TOKEN_2022_PROGRAM_ID
            || order.output_token_program == TOKEN_2022_PROGRAM_ID;
        let v2_accounts = needs_token_2022 || dynamic;
        let disc = if dynamic { DISC_SWAP_V3_DYN } else if needs_token_2022 { DISC_SWAP_V2 } else { DISC_SWAP };
        let mut data = Vec::with_capacity(41);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
        data.push(1u8); // is_base_input = true

        let (input_vault, output_vault) = if a_to_b {
            (*token_vault_a, *token_vault_b)
        } else {
            (*token_vault_b, *token_vault_a)
        };

        let mut accounts = vec![
            AccountMeta::new(order.user, true),                     // [0] payer
            AccountMeta::new_readonly(*amm_config, false),          // [1] amm_config
            AccountMeta::new(*pool, false),                         // [2] pool_state
            AccountMeta::new(user_input_ata, false),                // [3] input_token_account
            AccountMeta::new(user_output_ata, false),               // [4] output_token_account
            AccountMeta::new(input_vault, false),                   // [5] input_vault
            AccountMeta::new(output_vault, false),                  // [6] output_vault
            AccountMeta::new(*observation, false),                  // [7] observation_state
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),     // [8] token_program
        ];
        if v2_accounts {
            accounts.push(AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false)); // token_program_2022
            accounts.push(AccountMeta::new_readonly(MEMO_PROGRAM_ID, false));       // memo_program
            accounts.push(AccountMeta::new_readonly(order.input_mint, false));      // input_vault_mint
            accounts.push(AccountMeta::new_readonly(order.output_mint, false));     // output_vault_mint
        }
        // v1: named tick_array, then remaining [ext, arrays…]; v2: remaining [ext, arrays…]
        super::raydium_clmm::push_tick_array_accounts(&mut accounts, v2_accounts, bitmap_ext, &tick_arrays);
        if dynamic {
            // swap_v3_dyn reads the token-0 and token-1 Pyth prices from the last two
            accounts.push(AccountMeta::new_readonly(fee.oracle_0, false));
            accounts.push(AccountMeta::new_readonly(fee.oracle_1, false));
        }

        let swap_ix = Instruction {
            program_id: BYREAL_PROG_ID,
            accounts,
            data,
        };

        Ok(SwapInstructions { setup, swap: vec![swap_ix], cleanup })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::types::PoolType;
    use solana_sdk::pubkey::Pubkey;

    fn state(mint_a: Pubkey, mint_b: Pubkey) -> PoolState {
        PoolState::Byreal {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_current: -21301,
            tick_spacing: 1,
            sqrt_price_x64: 1 << 64,
            liquidity: 1,
            fee_rate: 400,
            fee: Default::default(),
        }
    }

    fn order(input: Pubkey, output: Pubkey, input_program: Pubkey, output_program: Pubkey) -> SwapOrder {
        SwapOrder {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::Byreal,
            input_mint: input,
            output_mint: output,
            amount_in: 1_000_000,
            min_amount_out: 990,
            user: Pubkey::new_unique(),
            input_token_program: input_program,
            output_token_program: output_program,
        }
    }

    #[test]
    fn builds_the_raydium_swap_layout_under_the_byreal_program() {
        let st = state(SOL_NATIVE_MINT, USDC_MINT);
        let PoolState::Byreal { pool, amm_config, token_vault_a, token_vault_b, observation, .. } = &st else { unreachable!() };
        let o = order(USDC_MINT, SOL_NATIVE_MINT, TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID);
        let ixs = ByrealExecutor.build_swap_ix(&o, &st).unwrap();
        let ix = &ixs.swap[0];
        assert_eq!(ix.program_id, BYREAL_PROG_ID);
        assert_eq!(ix.data[..8], DISC_SWAP);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 990);
        assert_eq!(*ix.data.last().unwrap(), 1, "is_base_input");
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|a| a.pubkey).collect();
        assert_eq!(keys[0], o.user);
        assert_eq!(keys[1], *amm_config);
        assert_eq!(keys[2], *pool, "pool_state at [2]");
        // b → a: input vault is vault b
        assert_eq!((keys[5], keys[6]), (*token_vault_b, *token_vault_a));
        assert_eq!(keys[7], *observation);
        assert_eq!(keys[8], TOKEN_PROGRAM_ID);
        // v1: the first tick array is a named account, derived with Byreal's program id
        let first = crate::quote::clmm::TickLayout::Raydium.array_pda(&BYREAL_PROG_ID, pool, crate::quote::clmm::TickLayout::Raydium.array_start(-21301, 1));
        assert_eq!(keys[9], first);
        assert_eq!(ixs.cleanup.len(), 1, "unwrap the WSOL output");
    }

    #[test]
    fn token_2022_side_uses_swap_v2() {
        let t22 = Pubkey::new_unique();
        let st = state(t22, USDC_MINT);
        let o = order(t22, USDC_MINT, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID);
        let ix = &ByrealExecutor.build_swap_ix(&o, &st).unwrap().swap[0];
        assert_eq!(ix.data[..8], DISC_SWAP_V2);
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|a| a.pubkey).collect();
        assert_eq!(&keys[9..13], &[TOKEN_2022_PROGRAM_ID, MEMO_PROGRAM_ID, t22, USDC_MINT]);
    }

    #[test]
    fn dynamic_fee_pools_use_swap_v3_dyn_with_the_oracles_last() {
        let mut st = state(SOL_NATIVE_MINT, USDC_MINT);
        let (o0, o1) = (Pubkey::new_unique(), Pubkey::new_unique());
        let PoolState::Byreal { pool, fee, .. } = &mut st else { unreachable!() };
        fee.flags = 0b1_1000;
        fee.oracle_0 = o0;
        fee.oracle_1 = o1;
        let pool = *pool;
        let o = order(SOL_NATIVE_MINT, USDC_MINT, TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID);
        // refuses without loaded tick arrays
        assert!(ByrealExecutor.build_swap_ix(&o, &st).is_err());
        crate::quote::clmm::TICKS.insert(pool, std::sync::Arc::new(crate::quote::clmm::TickData {
            ticks: vec![(-21_360, 5)], covered_lo: -21_480, covered_hi: -21_000, initialized_arrays: vec![-21_360],
            bitmap_extension: None, limit_orders: vec![], fetched_at: std::time::Instant::now(),
        }));
        let ix = &ByrealExecutor.build_swap_ix(&o, &st).unwrap().swap[0];
        assert_eq!(ix.data[..8], DISC_SWAP_V3_DYN);
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|a| a.pubkey).collect();
        assert_eq!(&keys[9..11], &[TOKEN_2022_PROGRAM_ID, MEMO_PROGRAM_ID], "v2 account layout");
        let array = crate::quote::clmm::TickLayout::Raydium.array_pda(&BYREAL_PROG_ID, &pool, -21_360);
        assert_eq!(&keys[13..], &[array, o0, o1]);
    }

    #[test]
    fn rejects_a_mint_the_pool_does_not_hold() {
        let st = state(SOL_NATIVE_MINT, USDC_MINT);
        let o = order(Pubkey::new_unique(), USDC_MINT, TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID);
        assert!(ByrealExecutor.build_swap_ix(&o, &st).is_err());
    }
}
