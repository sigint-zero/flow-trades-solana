use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::{AmmExecutor, DISC_BUY_EXACT_IN, DISC_SELL_EXACT_IN};

pub struct RaydiumLpExecutor;

impl AmmExecutor for RaydiumLpExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool_state_addr, authority, base_vault, quote_vault,
             base_mint, quote_mint, config_id, platform_id, creator) =
            match pool_state {
                PoolState::RaydiumLp {
                    pool_state, authority, base_vault, quote_vault,
                    base_mint, quote_mint, config_id, platform_id, creator, ..
                } => (pool_state, authority, base_vault, quote_vault,
                      base_mint, quote_mint, config_id, platform_id, creator),
                _ => return Err(TradeError::Execution("expected RaydiumLp pool state".into())),
            };

        // Direction: buy = quote(SOL) -> base(token), sell = base(token) -> quote(SOL)
        let is_buy = order.input_mint == *quote_mint;

        let base_prog = if is_buy { order.output_token_program } else { order.input_token_program };
        let quote_prog = if is_buy { order.input_token_program } else { order.output_token_program };
        let user_base_ata = get_associated_token_address_with_program_id(&order.user, base_mint, &base_prog);
        let user_quote_ata = get_associated_token_address_with_program_id(&order.user, quote_mint, &quote_prog);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, base_mint, &base_prog,
            ),
            create_associated_token_account_idempotent(
                &order.user, &order.user, quote_mint, &quote_prog,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL wrap/unwrap
        if *quote_mint == SOL_NATIVE_MINT && is_buy {
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_quote_ata, order.amount_in,
            ));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_quote_ata).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_quote_ata, &order.user, &order.user, &[],
            ).unwrap());
        }
        if *quote_mint == SOL_NATIVE_MINT && !is_buy {
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_quote_ata, &order.user, &order.user, &[],
            ).unwrap());
        }

        // Instruction data: disc(8) + amount_in(u64) + min_amount_out(u64) + share_fee_rate(u64)
        let disc = if is_buy { DISC_BUY_EXACT_IN } else { DISC_SELL_EXACT_IN };
        let mut data = Vec::with_capacity(32);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // share_fee_rate = 0

        // PDA derivations
        let platform_config = *platform_id;
        let (event_authority, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &RAYDIUM_LP_PROG_ID,
        );
        let (platform_claim_fee_vault, _) = Pubkey::find_program_address(
            &[platform_id.as_ref(), quote_mint.as_ref()],
            &RAYDIUM_LP_PROG_ID,
        );
        let (creator_claim_fee_vault, _) = Pubkey::find_program_address(
            &[creator.as_ref(), quote_mint.as_ref()],
            &RAYDIUM_LP_PROG_ID,
        );

        // 18 accounts (no share_fee_receiver)
        let accounts = vec![
            AccountMeta::new(order.user, true),                              // [0]  payer
            AccountMeta::new_readonly(*authority, false),                     // [1]  authority
            AccountMeta::new_readonly(*config_id, false),                    // [2]  global_config
            AccountMeta::new_readonly(platform_config, false),                // [3]  platform_config
            AccountMeta::new(*pool_state_addr, false),                        // [4]  pool_state
            AccountMeta::new(user_base_ata, false),                           // [5]  user_base_token
            AccountMeta::new(user_quote_ata, false),                          // [6]  user_quote_token
            AccountMeta::new(*base_vault, false),                             // [7]  base_vault
            AccountMeta::new(*quote_vault, false),                            // [8]  quote_vault
            AccountMeta::new_readonly(*base_mint, false),                     // [9]  base_token_mint
            AccountMeta::new_readonly(*quote_mint, false),                    // [10] quote_token_mint
            AccountMeta::new_readonly(base_prog, false),                      // [11] base_token_program
            AccountMeta::new_readonly(quote_prog, false),                     // [12] quote_token_program
            AccountMeta::new_readonly(event_authority, false),                // [13] event_authority
            AccountMeta::new_readonly(RAYDIUM_LP_PROG_ID, false),             // [14] program (self)
            // remaining_accounts:
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // [15] system_program
            AccountMeta::new(platform_claim_fee_vault, false),                // [16] platform_claim_fee_vault
            AccountMeta::new(creator_claim_fee_vault, false),                 // [17] creator_claim_fee_vault
        ];

        let swap_ix = Instruction {
            program_id: RAYDIUM_LP_PROG_ID,
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
