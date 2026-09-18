//! Pumpup post-graduation AMM executor.
//!
//! Builds the `swap` instruction for the Pumpup AMM (program
//! `PdMDrKEMaX8q7CCJb7NvUCxerBCcsFUa4LjBEynTtEd`). Source-of-truth: on-chain
//! Anchor IDL fetched from `BzBmXJiz9H88PAZomvWn8UvmdmeucWZg7N1cygN5po61`.
//!
//! # Instruction layout (per IDL)
//! - Discriminator: `[248, 198, 158, 145, 225, 117, 135, 200]` = `f8c69e91e17587c8`
//!   (= sha256("global:swap")[0..8])
//! - Args: `amount_in: u64`, `minimum_amount_out: u64`
//! - Accounts (11, IDL order):
//!   [0] pool                    [writable]
//!   [1] token_a_vault           [writable]
//!   [2] token_b_vault           [writable]
//!   [3] user_token_in           [writable]
//!   [4] user_token_out          [writable]
//!   [5] fee_recipient           [writable]
//!   [6] fee_recipient2          [writable]
//!   [7] user                    [writable, signer]
//!   [8] token_program           [readonly]
//!   [9] event_authority         [readonly] PDA(["__event_authority"], program)
//!   [10] program                [readonly] = Pumpup program itself
//!
//! # Notes
//! - This is the **AMM phase** swap. Pre-graduation bonding-curve trades
//!   would use separate `buy`/`sell` instructions (with native SOL handling)
//!   which we don't currently support.
//! - All observed pools so far trade against USDT, not SOL — no WSOL
//!   wrap/unwrap path is included. If a Pumpup pool ever pairs against SOL
//!   we'd need to add it (matches the Dooar/FluxBeam pattern).

use std::sync::LazyLock;

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::constants::*;
use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use super::AmmExecutor;

/// `sha256("global:swap")[0..8]` — verified against the on-chain IDL.
const PUMPUP_SWAP_DISC: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Cached event authority PDA — same for all Pumpup pools.
static PUMPUP_EVENT_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"__event_authority"], &PUMPUP_PROG_ID).0
});

pub struct PumpupExecutor;

impl AmmExecutor for PumpupExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, token_a_mint, token_b_mint, token_a_vault, token_b_vault, fee_recipient, fee_recipient2) =
            match pool_state {
                PoolState::Pumpup {
                    pool, token_a_mint, token_b_mint, token_a_vault, token_b_vault,
                    fee_recipient, fee_recipient2, ..
                } => (pool, token_a_mint, token_b_mint, token_a_vault, token_b_vault, fee_recipient, fee_recipient2),
                _ => return Err(TradeError::Execution("expected Pumpup pool state".into())),
            };

        // Validate the order's input mint matches one of the pool's two mints.
        // The Pumpup program reads user_token_in's mint and matches it against
        // the pool's token_a/b vaults internally; if our user_token_in points
        // to a mint that isn't in the pool, the program rejects.
        if order.input_mint != *token_a_mint && order.input_mint != *token_b_mint {
            return Err(TradeError::Execution(format!(
                "Pumpup pool {pool} doesn't trade input mint {} (has {} / {})",
                order.input_mint, token_a_mint, token_b_mint
            )));
        }

        let user_token_in = get_associated_token_address_with_program_id(
            &order.user, &order.input_mint, &order.input_token_program,
        );
        let user_token_out = get_associated_token_address_with_program_id(
            &order.user, &order.output_mint, &order.output_token_program,
        );

        // Setup: ensure user has output ATA. Idempotent — no-op if already exists.
        let setup = vec![create_associated_token_account_idempotent(
            &order.user,
            &order.user,
            &order.output_mint,
            &order.output_token_program,
        )];
        let cleanup = Vec::new();

        let mut data = Vec::with_capacity(8 + 16);
        data.extend_from_slice(&PUMPUP_SWAP_DISC);
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        let accounts = vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new(*token_a_vault, false),
            AccountMeta::new(*token_b_vault, false),
            AccountMeta::new(user_token_in, false),
            AccountMeta::new(user_token_out, false),
            AccountMeta::new(*fee_recipient, false),
            AccountMeta::new(*fee_recipient2, false),
            AccountMeta::new(order.user, true),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(*PUMPUP_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMPUP_PROG_ID, false),
        ];

        let swap_ix = Instruction {
            program_id: PUMPUP_PROG_ID,
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

    fn make_order(input_mint: Pubkey, output_mint: Pubkey, pool: Pubkey) -> SwapOrder {
        SwapOrder {
            pool_address: pool,
            pool_type: PoolType::Pumpup,
            input_mint,
            output_mint,
            amount_in: 1_000_000,
            min_amount_out: 950_000,
            user: Pubkey::new_unique(),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_PROGRAM_ID,
        }
    }

    fn make_pool(token_a_mint: Pubkey, token_b_mint: Pubkey, pool: Pubkey) -> PoolState {
        PoolState::Pumpup {
            pool,
            token_a_mint,
            token_b_mint,
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            fee_recipient: Pubkey::new_unique(),
            fee_recipient2: Pubkey::new_unique(),
            token_a_reserve: 1_000_000_000,
            token_b_reserve: 1_000_000_000,
        }
    }

    #[test]
    fn test_builds_swap_with_correct_disc_and_args() {
        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let order = make_order(token_a, token_b, pool);
        let pool_state = make_pool(token_a, token_b, pool);

        let result = PumpupExecutor.build_swap_ix(&order, &pool_state).unwrap();
        assert_eq!(result.swap.len(), 1);
        let ix = &result.swap[0];
        assert_eq!(ix.program_id, PUMPUP_PROG_ID);
        assert_eq!(&ix.data[..8], &PUMPUP_SWAP_DISC);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 950_000);
        assert_eq!(ix.accounts.len(), 11);
    }

    #[test]
    fn test_account_ordering_per_idl() {
        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let order = make_order(token_a, token_b, pool);
        let pool_state = make_pool(token_a, token_b, pool);
        let (token_a_vault, token_b_vault, fee_recipient, fee_recipient2) =
            if let PoolState::Pumpup {
                token_a_vault, token_b_vault, fee_recipient, fee_recipient2, ..
            } = &pool_state
            {
                (*token_a_vault, *token_b_vault, *fee_recipient, *fee_recipient2)
            } else {
                unreachable!()
            };

        let result = PumpupExecutor.build_swap_ix(&order, &pool_state).unwrap();
        let ix = &result.swap[0];
        assert_eq!(ix.accounts[0].pubkey, pool, "[0] pool");
        assert_eq!(ix.accounts[1].pubkey, token_a_vault, "[1] token_a_vault");
        assert_eq!(ix.accounts[2].pubkey, token_b_vault, "[2] token_b_vault");
        assert_eq!(ix.accounts[5].pubkey, fee_recipient, "[5] fee_recipient");
        assert_eq!(ix.accounts[6].pubkey, fee_recipient2, "[6] fee_recipient2");
        assert_eq!(ix.accounts[7].pubkey, order.user, "[7] user");
        assert!(ix.accounts[7].is_signer, "[7] user must be signer");
        assert_eq!(ix.accounts[8].pubkey, TOKEN_PROGRAM_ID, "[8] token_program");
        assert_eq!(ix.accounts[9].pubkey, *PUMPUP_EVENT_AUTHORITY, "[9] event_authority");
        assert_eq!(ix.accounts[10].pubkey, PUMPUP_PROG_ID, "[10] program (self)");
    }

    #[test]
    fn test_writability_flags_per_idl() {
        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let order = make_order(token_a, token_b, pool);
        let pool_state = make_pool(token_a, token_b, pool);

        let result = PumpupExecutor.build_swap_ix(&order, &pool_state).unwrap();
        let ix = &result.swap[0];
        for i in 0..8 {
            assert!(ix.accounts[i].is_writable, "[{i}] should be writable");
        }
        for i in 8..11 {
            assert!(!ix.accounts[i].is_writable, "[{i}] should be readonly");
        }
    }

    #[test]
    fn test_rejects_wrong_pool_type() {
        let pool = Pubkey::new_unique();
        let order = make_order(Pubkey::new_unique(), Pubkey::new_unique(), pool);
        let bad_state = PoolState::Dooar {
            pool,
            authority: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            pool_mint: Pubkey::new_unique(),
            fee_account: Pubkey::new_unique(),
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            fees: Default::default(),
        };
        assert!(PumpupExecutor.build_swap_ix(&order, &bad_state).is_err());
    }

    #[test]
    fn test_rejects_mint_not_in_pool() {
        let token_a = Pubkey::new_unique();
        let token_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let unrelated_mint = Pubkey::new_unique();
        let order = make_order(unrelated_mint, token_b, pool);
        let pool_state = make_pool(token_a, token_b, pool);
        assert!(PumpupExecutor.build_swap_ix(&order, &pool_state).is_err());
    }
}
