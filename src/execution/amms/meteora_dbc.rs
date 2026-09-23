use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::AmmExecutor;

/// DBC swap2 discriminator: SHA256("global:swap2")[0..8]
const DBC_DISC_SWAP2: [u8; 8] = [0x41, 0x4b, 0x3f, 0x4c, 0xeb, 0x5b, 0x5b, 0x88];

pub struct MeteoraDbcExecutor;

impl AmmExecutor for MeteoraDbcExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, config, pool_authority, base_vault, quote_vault, base_mint, quote_mint) =
            match pool_state {
                PoolState::MeteoraDbc {
                    pool,
                    config,
                    pool_authority,
                    base_vault,
                    quote_vault,
                    base_mint,
                    quote_mint,
                    ..
                } => (pool, config, pool_authority, base_vault, quote_vault, base_mint, quote_mint),
                _ => return Err(TradeError::Execution("expected MeteoraDbc pool state".into())),
            };

        let input_prog = order.input_token_program;
        let output_prog = order.output_token_program;

        let user_input_ata =
            get_associated_token_address_with_program_id(&order.user, &order.input_mint, &input_prog);
        let user_output_ata =
            get_associated_token_address_with_program_id(&order.user, &order.output_mint, &output_prog);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, &order.output_mint, &output_prog,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL handling (DBC uses WSOL, not native SOL)
        if order.input_mint == SOL_NATIVE_MINT {
            setup.insert(
                0,
                create_associated_token_account_idempotent(
                    &order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
                ),
            );
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_input_ata, order.amount_in,
            ));
            setup.push(
                spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_input_ata).unwrap(),
            );
            cleanup.push(
                spl_token::instruction::close_account(
                    &TOKEN_PROGRAM_ID, &user_input_ata, &order.user, &order.user, &[],
                )
                .unwrap(),
            );
        }
        if order.output_mint == SOL_NATIVE_MINT {
            let output_ata = get_associated_token_address_with_program_id(
                &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            );
            cleanup.push(
                spl_token::instruction::close_account(
                    &TOKEN_PROGRAM_ID, &output_ata, &order.user, &order.user, &[],
                )
                .unwrap(),
            );
        }

        // Token programs in canonical (base, quote) order
        let (token_base_program, token_quote_program) = if order.input_mint == *base_mint {
            (input_prog, output_prog)
        } else {
            (output_prog, input_prog)
        };

        // Event authority PDA: seeds = ["__event_authority"]
        let (event_authority, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &METEORA_DBC_PROG_ID,
        );

        // Swap2 instruction data: disc(8) + amount_in(8) + min_amount_out(8) + swap_mode(1) = 25 bytes
        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&DBC_DISC_SWAP2);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        data.push(0); // ExactIn

        // Referral = program ID (no referral)
        let referral = METEORA_DBC_PROG_ID;

        // DBC swap2 accounts (15):
        //  [0]  pool_authority           (readonly, PDA seeds=["pool_authority"])
        //  [1]  config                   (readonly, PoolConfig)
        //  [2]  pool                     (writable, VirtualPool)
        //  [3]  input_token_account      (writable, user's input ATA)
        //  [4]  output_token_account     (writable, user's output ATA)
        //  [5]  base_vault               (writable)
        //  [6]  quote_vault              (writable)
        //  [7]  base_mint                (readonly)
        //  [8]  quote_mint               (readonly)
        //  [9]  payer                    (signer)
        //  [10] token_base_program       (readonly)
        //  [11] token_quote_program      (readonly)
        //  [12] referral_token_account   (readonly, program ID = no referral)
        //  [13] event_authority          (readonly)
        //  [14] program                  (readonly, DBC program itself)
        let accounts = vec![
            AccountMeta::new_readonly(*pool_authority, false),          // [0]
            AccountMeta::new_readonly(*config, false),                  // [1]
            AccountMeta::new(*pool, false),                             // [2]
            AccountMeta::new(user_input_ata, false),                    // [3]
            AccountMeta::new(user_output_ata, false),                   // [4]
            AccountMeta::new(*base_vault, false),                       // [5]
            AccountMeta::new(*quote_vault, false),                      // [6]
            AccountMeta::new_readonly(*base_mint, false),               // [7]
            AccountMeta::new_readonly(*quote_mint, false),              // [8]
            AccountMeta::new(order.user, true),                         // [9]
            AccountMeta::new_readonly(token_base_program, false),       // [10]
            AccountMeta::new_readonly(token_quote_program, false),      // [11]
            AccountMeta::new_readonly(referral, false),                 // [12]
            AccountMeta::new_readonly(event_authority, false),          // [13]
            AccountMeta::new_readonly(METEORA_DBC_PROG_ID, false),      // [14]
        ];

        let swap_ix = Instruction {
            program_id: METEORA_DBC_PROG_ID,
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
