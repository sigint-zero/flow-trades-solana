//! pump.fun bonding-curve executor (`6EF8rr…`).
//!
//! **Buys are exact-INPUT** (`buy_exact_sol_in`): the program spends at most
//! `amount_in` lamports (fees included) and enforces `min_tokens_out`; the
//! quote engine (`quote::pump_bonding`) reproduces its token amount to the
//! atom. **Sells** use `sell(amount, min_sol_output)`, the floor being on the
//! SOL the user receives after fees.
//!
//! Account lists follow the IDL plus two trailing accounts every buy and sell
//! has carried since mid-2026: the `["bonding-curve-v2", mint]` PDA
//! (read-only) and one of the Global's buyback fee recipients (writable).
//!
//! **Native SOL.** The curve takes and pays lamports, not WSOL. A buy simply
//! spends the payer's SOL. The flow-router measures a route's output on a
//! token account, so a sell is followed, inside the router, by a transfer of
//! `min_amount_out` lamports into the payer's WSOL account and a `SyncNative`:
//! the router checks (and takes its fee on) that amount, `min_sol_output`
//! guarantees the curve paid at least as much, and anything above it stays in
//! the wallet as SOL. Cleanup unwraps the WSOL account.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::AmmExecutor;

pub struct PumpFunExecutor;

// PumpFun instruction discriminators
/// `buy_exact_sol_in(spendable_sol_in, min_tokens_out, track_volume)`
pub const BUY_EXACT_SOL_IN_DISC: [u8; 8] = [0x38, 0xfc, 0x74, 0x08, 0x9e, 0xdf, 0xcd, 0x5f];
const SELL_DISC: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

const GLOBAL_VOLUME_ACCUMULATOR: Pubkey = Pubkey::from_str_const("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y");
/// `["fee_config", pump program]` under the fee program.
const FEE_CONFIG: Pubkey = crate::quote::pump_bonding::FEE_CONFIG;
const FEE_PROGRAM: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

impl AmmExecutor for PumpFunExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (global, fee_account, mint, bonding_curve, associated_bonding_curve, event_authority, creator, buyback_fee_recipient) =
            match pool_state {
                PoolState::PumpFun {
                    global, fee_account, mint, bonding_curve,
                    associated_bonding_curve, event_authority, creator, buyback_fee_recipient, ..
                } => (global, fee_account, mint, bonding_curve, associated_bonding_curve, event_authority, creator, buyback_fee_recipient),
                _ => return Err(TradeError::Execution("expected PumpFun pool state".into())),
            };
        if *buyback_fee_recipient == Pubkey::default() {
            return Err(TradeError::Execution("pumpfun: buyback fee recipient unresolved (re-fetch the pool)".into()));
        }

        // Direction: buy = SOL -> token, sell = token -> SOL
        let is_buy = order.input_mint == SOL_NATIVE_MINT;
        if !is_buy && order.output_mint != SOL_NATIVE_MINT {
            return Err(TradeError::Execution(format!("pumpfun bonding {bonding_curve} only trades SOL <-> {mint}")));
        }
        let token_prog = if is_buy { order.output_token_program } else { order.input_token_program };
        let user_token_ata = get_associated_token_address_with_program_id(&order.user, mint, &token_prog);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, mint, &token_prog,
            ),
        ];
        let mut cleanup = Vec::new();

        // Build instruction data
        let data = if is_buy {
            // buy_exact_sol_in(spendable_sol_in, min_tokens_out, track_volume: OptionBool)
            let mut buf = Vec::with_capacity(25);
            buf.extend_from_slice(&BUY_EXACT_SOL_IN_DISC);
            buf.extend_from_slice(&order.amount_in.to_le_bytes());      // SOL to spend
            // min tokens out; the program rejects 0 (BuyZeroAmount, 6020), which
            // is what a later hop of a routed swap gets (the router holds the floor)
            buf.extend_from_slice(&order.min_amount_out.max(1).to_le_bytes());
            buf.push(0x00); // track_volume = false
            buf
        } else {
            // sell(amount, min_sol_output)
            let mut buf = Vec::with_capacity(24);
            buf.extend_from_slice(&SELL_DISC);
            buf.extend_from_slice(&order.amount_in.to_le_bytes());      // token amount
            buf.extend_from_slice(&order.min_amount_out.to_le_bytes()); // min SOL output
            buf
        };

        // Creator vault PDA: seeds = ["creator-vault", bonding_curve_data.creator]
        let (creator_vault, _) = Pubkey::find_program_address(
            &[b"creator-vault", creator.as_ref()],
            &PUMP_FUN_PROG_ID,
        );
        let (bonding_curve_v2, _) = Pubkey::find_program_address(
            &[b"bonding-curve-v2", mint.as_ref()],
            &PUMP_FUN_PROG_ID,
        );

        let mut accounts = if is_buy {
            // User volume accumulator PDA: per-user trade tracking
            let (user_volume_accumulator, _) = Pubkey::find_program_address(
                &[b"user_volume_accumulator", order.user.as_ref()],
                &PUMP_FUN_PROG_ID,
            );
            // BUY (16 accounts):
            vec![
                AccountMeta::new_readonly(*global, false),                             // 0
                AccountMeta::new(*fee_account, false),                                 // 1
                AccountMeta::new_readonly(*mint, false),                               // 2
                AccountMeta::new(*bonding_curve, false),                               // 3
                AccountMeta::new(*associated_bonding_curve, false),                    // 4
                AccountMeta::new(user_token_ata, false),                               // 5
                AccountMeta::new(order.user, true),                                    // 6
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),      // 7
                AccountMeta::new_readonly(token_prog, false),                          // 8
                AccountMeta::new(creator_vault, false),                                // 9
                AccountMeta::new_readonly(*event_authority, false),                    // 10
                AccountMeta::new_readonly(PUMP_FUN_PROG_ID, false),                    // 11
                AccountMeta::new_readonly(GLOBAL_VOLUME_ACCUMULATOR, false),           // 12
                AccountMeta::new(user_volume_accumulator, false),                      // 13
                AccountMeta::new_readonly(FEE_CONFIG, false),                          // 14
                AccountMeta::new_readonly(FEE_PROGRAM, false),                         // 15
            ]
        } else {
            // SELL (14 accounts):
            vec![
                AccountMeta::new_readonly(*global, false),                             // 0
                AccountMeta::new(*fee_account, false),                                 // 1
                AccountMeta::new_readonly(*mint, false),                               // 2
                AccountMeta::new(*bonding_curve, false),                               // 3
                AccountMeta::new(*associated_bonding_curve, false),                    // 4
                AccountMeta::new(user_token_ata, false),                               // 5
                AccountMeta::new(order.user, true),                                    // 6
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),      // 7
                AccountMeta::new(creator_vault, false),                                // 8
                AccountMeta::new_readonly(token_prog, false),                          // 9
                AccountMeta::new_readonly(*event_authority, false),                    // 10
                AccountMeta::new_readonly(PUMP_FUN_PROG_ID, false),                    // 11
                AccountMeta::new_readonly(FEE_CONFIG, false),                          // 12
                AccountMeta::new_readonly(FEE_PROGRAM, false),                         // 13
            ]
        };
        // remaining accounts (both directions)
        accounts.push(AccountMeta::new_readonly(bonding_curve_v2, false));
        accounts.push(AccountMeta::new(*buyback_fee_recipient, false));

        let mut swap = vec![Instruction {
            program_id: PUMP_FUN_PROG_ID,
            accounts,
            data,
        }];

        if !is_buy {
            // The router measures WSOL: wrap the guaranteed SOL inside it.
            let wsol_ata = get_associated_token_address_with_program_id(&order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
            setup.push(create_associated_token_account_idempotent(&order.user, &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID));
            swap.push(solana_sdk::system_instruction::transfer(&order.user, &wsol_ata, order.min_amount_out));
            swap.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &wsol_ata).map_err(|e| TradeError::Execution(format!("sync_native: {e}")))?);
            cleanup.push(
                spl_token::instruction::close_account(&TOKEN_PROGRAM_ID, &wsol_ata, &order.user, &order.user, &[])
                    .map_err(|e| TradeError::Execution(format!("close wsol: {e}")))?,
            );
        }

        Ok(SwapInstructions {
            setup,
            swap,
            cleanup,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::types::PoolType;

    fn state() -> (PoolState, Pubkey) {
        let mint = Pubkey::new_unique();
        let bonding_curve = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_FUN_PROG_ID).0;
        (
            PoolState::PumpFun {
                global: Pubkey::new_unique(),
                fee_account: Pubkey::new_unique(),
                mint,
                bonding_curve,
                associated_bonding_curve: Pubkey::new_unique(),
                event_authority: Pubkey::new_unique(),
                creator: Pubkey::new_unique(),
                curve: Default::default(),
                buyback_fee_recipient: Pubkey::new_unique(),
            },
            mint,
        )
    }

    fn order(input: Pubkey, output: Pubkey, amount_in: u64, min_out: u64) -> SwapOrder {
        SwapOrder {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::PumpFun,
            input_mint: input,
            output_mint: output,
            amount_in,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_2022_PROGRAM_ID,
        }
    }

    #[test]
    fn buy_is_exact_sol_in_with_the_trailing_accounts() {
        let (st, mint) = state();
        let o = order(SOL_NATIVE_MINT, mint, 100_000_000, 1_234);
        let ixs = PumpFunExecutor.build_swap_ix(&o, &st).unwrap();
        assert_eq!(ixs.swap.len(), 1, "a buy spends lamports directly");
        let ix = &ixs.swap[0];
        assert_eq!(&ix.data[..8], &BUY_EXACT_SOL_IN_DISC);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 100_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 1_234);
        assert_eq!(ix.data.len(), 25);
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(ix.accounts[8].pubkey, TOKEN_2022_PROGRAM_ID, "the mint's token program");
        let v2 = Pubkey::find_program_address(&[b"bonding-curve-v2", mint.as_ref()], &PUMP_FUN_PROG_ID).0;
        assert_eq!((ix.accounts[16].pubkey, ix.accounts[16].is_writable), (v2, false));
        assert!(ix.accounts[17].is_writable);
        assert!(ixs.cleanup.is_empty());
    }

    #[test]
    fn sell_wraps_the_guaranteed_sol_for_the_router() {
        let (st, mint) = state();
        let mut o = order(mint, SOL_NATIVE_MINT, 5_000_000, 777);
        o.input_token_program = TOKEN_2022_PROGRAM_ID;
        o.output_token_program = TOKEN_PROGRAM_ID;
        let ixs = PumpFunExecutor.build_swap_ix(&o, &st).unwrap();
        assert_eq!(ixs.swap.len(), 3);
        let sell = &ixs.swap[0];
        assert_eq!(&sell.data[..8], &SELL_DISC);
        assert_eq!(sell.accounts.len(), 16);
        assert_eq!(sell.accounts[9].pubkey, TOKEN_2022_PROGRAM_ID);
        let wsol = get_associated_token_address_with_program_id(&o.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID);
        let transfer = &ixs.swap[1];
        assert_eq!(transfer.program_id, solana_sdk::system_program::ID);
        assert_eq!(transfer.accounts[1].pubkey, wsol);
        assert_eq!(u64::from_le_bytes(transfer.data[4..12].try_into().unwrap()), 777);
        assert_eq!((ixs.swap[2].program_id, ixs.swap[2].data.as_slice()), (TOKEN_PROGRAM_ID, [17u8].as_slice()));
        assert_eq!(ixs.cleanup.len(), 1, "WSOL unwrapped after the router");
    }

    #[test]
    fn refuses_foreign_pairs_and_unresolved_recipients() {
        let (st, mint) = state();
        assert!(PumpFunExecutor.build_swap_ix(&order(mint, Pubkey::new_unique(), 1, 0), &st).is_err());
        let PoolState::PumpFun { global, fee_account, mint, bonding_curve, associated_bonding_curve, event_authority, creator, curve, .. } = st else { unreachable!() };
        let bare = PoolState::PumpFun { global, fee_account, mint, bonding_curve, associated_bonding_curve, event_authority, creator, curve, buyback_fee_recipient: Pubkey::default() };
        assert!(PumpFunExecutor.build_swap_ix(&order(SOL_NATIVE_MINT, mint, 1, 0), &bare).is_err());
    }
}
