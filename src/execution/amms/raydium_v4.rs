use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::{RAYDIUM_V4_PROG_ID, SOL_NATIVE_MINT, TOKEN_PROGRAM_ID};

use super::AmmExecutor;

/// Raydium V4 (legacy AMM) swap instruction builder.
pub struct RaydiumV4Executor;

/// SPL Token `SyncNative` instruction index.
const SPL_SYNC_NATIVE_IX: u8 = 17;

/// SPL Token `CloseAccount` instruction index.
const SPL_CLOSE_ACCOUNT_IX: u8 = 9;

/// `AmmInstruction::SwapBaseInV2`: `swap_base_in` without the OpenBook
/// accounts (token program, amm, authority, coin vault, pc vault, user
/// source, user destination, user) — same arithmetic, 8 accounts instead of 17.
const SWAP_BASE_IN_V2_TAG: u8 = 16;

/// `AmmStatus` values that allow a swap: Initialized (1), SwapOnly (6), and
/// WaitingTrade (7) once `pool_open_time` has passed.
pub fn can_swap(status: u64, pool_open_time: u64, now_unix: u64) -> bool {
    match status {
        1 | 6 => true,
        7 => now_unix >= pool_open_time,
        _ => false,
    }
}

/// `swap_base_in` as the program computes it on its curve reserves
/// (`vault − need_take_pnl`): fee = ⌈amount·num/den⌉ off the input, then
/// out = ⌊reserve_out·(amount − fee) / (reserve_in + amount − fee)⌋.
/// Returns `(amount_out, fee)`, or `None` where the program would fail
/// (zero output, output ≥ the out reserve).
pub fn swap_base_in_out(reserve_in: u128, reserve_out: u128, amount_in: u64, fee_numerator: u64, fee_denominator: u64) -> Option<(u64, u64)> {
    if fee_denominator == 0 || amount_in == 0 {
        return None;
    }
    let fee = (amount_in as u128 * fee_numerator as u128).div_ceil(fee_denominator as u128);
    let in_less_fee = (amount_in as u128).checked_sub(fee)?;
    let out = reserve_out.checked_mul(in_less_fee)? / reserve_in.checked_add(in_less_fee)?;
    if out == 0 || out >= reserve_out {
        return None;
    }
    Some((u64::try_from(out).ok()?, fee as u64))
}

impl AmmExecutor for RaydiumV4Executor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let PoolState::RaydiumV4 {
            amm_id,
            authority,
            coin_vault,
            pc_vault,
            coin_mint,
            pc_mint,
            ..
        } = pool_state
        else {
            return Err(TradeError::Execution(
                "RaydiumV4Executor requires PoolState::RaydiumV4".into(),
            ));
        };
        let pair_ok = (order.input_mint == *coin_mint && order.output_mint == *pc_mint)
            || (order.input_mint == *pc_mint && order.output_mint == *coin_mint);
        if !pair_ok {
            return Err(TradeError::Execution(format!(
                "raydium v4 pool {amm_id} does not trade {} -> {}",
                order.input_mint, order.output_mint
            )));
        }

        let user = order.user;
        let input_is_sol = order.input_mint == SOL_NATIVE_MINT;
        let output_is_sol = order.output_mint == SOL_NATIVE_MINT;

        let user_source_ata = get_associated_token_address_with_program_id(&user, &order.input_mint, &order.input_token_program);
        let user_dest_ata = get_associated_token_address_with_program_id(&user, &order.output_mint, &order.output_token_program);

        // -- Setup: create ATAs + WSOL wrapping --
        let mut setup = Vec::new();

        // Always ensure the source ATA exists (idempotent).
        setup.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &order.input_mint,
            &order.input_token_program,
        ));

        // Always ensure the destination ATA exists (idempotent).
        setup.push(create_associated_token_account_idempotent(
            &user,
            &user,
            &order.output_mint,
            &order.output_token_program,
        ));

        // If swapping native SOL, fund the WSOL ATA and sync its balance.
        if input_is_sol {
            // Transfer lamports into the WSOL ATA.
            setup.push(system_instruction::transfer(
                &user,
                &user_source_ata,
                order.amount_in,
            ));

            // SyncNative: tells the token program to update the token balance
            // to match the account's lamport balance.
            setup.push(Instruction {
                program_id: TOKEN_PROGRAM_ID,
                accounts: vec![AccountMeta::new(user_source_ata, false)],
                data: vec![SPL_SYNC_NATIVE_IX],
            });
        }

        // -- Swap instruction --
        // Instruction data: tag 16 (swap_base_in_v2) ++ amount_in (u64 LE) ++ min_amount_out (u64 LE)
        let mut ix_data = Vec::with_capacity(1 + 8 + 8);
        ix_data.push(SWAP_BASE_IN_V2_TAG);
        ix_data.extend_from_slice(&order.amount_in.to_le_bytes());
        ix_data.extend_from_slice(&order.min_amount_out.to_le_bytes());

        let swap_ix = Instruction {
            program_id: RAYDIUM_V4_PROG_ID,
            accounts: vec![
                // 0. token program (V4 is SPL Token only)
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                // 1. amm (writable: recent_epoch / status)
                AccountMeta::new(*amm_id, false),
                // 2. authority PDA
                AccountMeta::new_readonly(*authority, false),
                // 3. coin vault
                AccountMeta::new(*coin_vault, false),
                // 4. pc vault
                AccountMeta::new(*pc_vault, false),
                // 5. user source
                AccountMeta::new(user_source_ata, false),
                // 6. user destination
                AccountMeta::new(user_dest_ata, false),
                // 7. user (signer)
                AccountMeta::new_readonly(user, true),
            ],
            data: ix_data,
        };

        // -- Cleanup: close WSOL account(s) to reclaim lamports --
        let mut cleanup = Vec::new();

        if input_is_sol {
            cleanup.push(build_close_account_ix(
                &user_source_ata,
                &user,
                &user,
            ));
        }

        if output_is_sol {
            cleanup.push(build_close_account_ix(
                &user_dest_ata,
                &user,
                &user,
            ));
        }

        Ok(SwapInstructions {
            setup,
            swap: vec![swap_ix],
            cleanup,
        })
    }
}

/// Build an SPL Token `CloseAccount` instruction.
fn build_close_account_ix(
    account: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data: vec![SPL_CLOSE_ACCOUNT_IX],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::types::PoolType;
    use solana_sdk::pubkey::Pubkey;

    fn dummy_pubkey(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    const COIN: u8 = 20;
    const PC: u8 = 21;

    fn make_pool_state_with(coin_mint: Pubkey, pc_mint: Pubkey) -> PoolState {
        PoolState::RaydiumV4 {
            amm_id: dummy_pubkey(1),
            authority: dummy_pubkey(2),
            coin_vault: dummy_pubkey(5),
            pc_vault: dummy_pubkey(6),
            coin_mint,
            pc_mint,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            need_take_pnl_coin: 0,
            need_take_pnl_pc: 0,
            status: 6,
            pool_open_time: 0,
        }
    }

    fn make_pool_state() -> PoolState {
        make_pool_state_with(dummy_pubkey(COIN), dummy_pubkey(PC))
    }

    fn make_order(input_mint: Pubkey, output_mint: Pubkey) -> SwapOrder {
        SwapOrder {
            pool_address: dummy_pubkey(1),
            pool_type: PoolType::RaydiumV4,
            input_mint,
            output_mint,
            amount_in: 1_000_000_000,
            min_amount_out: 500_000,
            user: dummy_pubkey(99),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_PROGRAM_ID,
        }
    }

    /// `ray_log` SwapBaseIn events of mainnet swaps (log_type 3):
    /// (amount_in, direction 1 = pc→coin / 2 = coin→pc, pool_coin, pool_pc, out_amount).
    /// pool_coin / pool_pc are the program's curve reserves (vault − need_take_pnl).
    const RAY_LOGS: [(u64, u64, u128, u128, u64); 8] = [
        // 58oQChx4… SOL/USDC
        (33_157_350, 2, 178_470_849_632_230, 21_208_899_055_335, 3_930_460),
        (250_000_000, 1, 178_424_525_760_365, 21_214_248_880_172, 2_097_368_298),
        (11_379, 2, 178_415_986_774_840, 21_215_268_837_276, 1_349),
        (50_000, 1, 178_423_053_732_838, 21_214_847_550_620, 419_463),
        // 65RWo5Lx…, 41ruBoo2…, DSUvc5qf…, 9Tb2ohu5…
        (531_481, 2, 447_945_423, 128_509_185_491, 151_913_293),
        (474_938_485, 1, 247_312_034, 600_477_283_087, 194_964),
        (67_618_000_000, 2, 8_449_465_215_744_309, 79_382_301_572_533, 633_674_488),
        (5_500_008, 2, 7_844_832_417_309, 44_945_945_576_058, 31_432_772),
    ];

    #[test]
    fn swap_base_in_reproduces_mainnet_ray_logs_exactly() {
        for (amount_in, direction, pool_coin, pool_pc, out) in RAY_LOGS {
            let (r_in, r_out) = if direction == 2 { (pool_coin, pool_pc) } else { (pool_pc, pool_coin) };
            let (got, fee) = swap_base_in_out(r_in, r_out, amount_in, 25, 10_000).unwrap();
            assert_eq!(got, out, "amount_in {amount_in}");
            assert_eq!(fee, (amount_in as u128 * 25).div_ceil(10_000) as u64);
        }
    }

    #[test]
    fn swap_base_in_refuses_what_the_program_refuses() {
        assert!(swap_base_in_out(1_000, 1_000, 0, 25, 10_000).is_none(), "zero input");
        assert!(swap_base_in_out(1_000_000, 1, 1_000, 25, 10_000).is_none(), "zero output");
        assert!(swap_base_in_out(1_000, 1_000, 1_000, 25, 0).is_none(), "no fee denominator");
    }

    #[test]
    fn only_swap_statuses_are_tradable() {
        assert!(can_swap(6, 0, 100));
        assert!(can_swap(1, 0, 100));
        assert!(can_swap(7, 100, 100));
        assert!(!can_swap(7, 101, 100), "WaitingTrade before open time");
        for status in [0, 2, 3, 4, 5, 8] {
            assert!(!can_swap(status, 0, 100), "status {status}");
        }
    }

    #[test]
    fn test_token_to_token_swap() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state();
        let order = make_order(dummy_pubkey(COIN), dummy_pubkey(PC));

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA-idempotent (no WSOL wrapping)
        assert_eq!(ixs.setup.len(), 2);
        // Swap: exactly 1 instruction
        assert_eq!(ixs.swap.len(), 1);
        // Cleanup: none (no WSOL)
        assert_eq!(ixs.cleanup.len(), 0);

        // swap_base_in_v2: tag 16, amount_in, min_amount_out
        let swap_data = &ixs.swap[0].data;
        assert_eq!(swap_data.len(), 17);
        assert_eq!(swap_data[0], 16);
        let amount_in = u64::from_le_bytes(swap_data[1..9].try_into().unwrap());
        let min_out = u64::from_le_bytes(swap_data[9..17].try_into().unwrap());
        assert_eq!(amount_in, 1_000_000_000);
        assert_eq!(min_out, 500_000);

        // 8 accounts, no OpenBook market
        let accs = &ixs.swap[0].accounts;
        assert_eq!(accs.len(), 8);
        assert_eq!(accs[0].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(accs[1].pubkey, dummy_pubkey(1));
        assert!(accs[1].is_writable);
        assert_eq!(accs[2].pubkey, dummy_pubkey(2));
        assert_eq!((accs[3].pubkey, accs[4].pubkey), (dummy_pubkey(5), dummy_pubkey(6)));
        let user = dummy_pubkey(99);
        assert_eq!(accs[5].pubkey, get_associated_token_address_with_program_id(&user, &dummy_pubkey(COIN), &TOKEN_PROGRAM_ID));
        assert_eq!(accs[6].pubkey, get_associated_token_address_with_program_id(&user, &dummy_pubkey(PC), &TOKEN_PROGRAM_ID));
        assert!(accs[7].is_signer && accs[7].pubkey == user);
    }

    #[test]
    fn rejects_a_pair_the_pool_does_not_trade() {
        let pool = make_pool_state();
        let order = make_order(dummy_pubkey(COIN), dummy_pubkey(77));
        assert!(RaydiumV4Executor.build_swap_ix(&order, &pool).is_err());
    }

    #[test]
    fn test_sol_input_wrapping() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state_with(SOL_NATIVE_MINT, dummy_pubkey(PC));
        let order = make_order(SOL_NATIVE_MINT, dummy_pubkey(PC));

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA + 1 system transfer + 1 SyncNative = 4
        assert_eq!(ixs.setup.len(), 4);
        // Cleanup: 1 close WSOL source ATA
        assert_eq!(ixs.cleanup.len(), 1);
    }

    #[test]
    fn test_sol_output_unwrapping() {
        let executor = RaydiumV4Executor;
        let pool = make_pool_state_with(dummy_pubkey(COIN), SOL_NATIVE_MINT);
        let order = make_order(dummy_pubkey(COIN), SOL_NATIVE_MINT);

        let ixs = executor.build_swap_ix(&order, &pool).unwrap();

        // Setup: 2 create-ATA (no wrapping needed for output)
        assert_eq!(ixs.setup.len(), 2);
        // Cleanup: 1 close WSOL dest ATA
        assert_eq!(ixs.cleanup.len(), 1);
    }

    #[test]
    fn test_wrong_pool_state_variant() {
        let executor = RaydiumV4Executor;
        let wrong_pool = PoolState::MeteoraDamm {
            pool: dummy_pubkey(1),
            token_a_vault: dummy_pubkey(2),
            token_b_vault: dummy_pubkey(3),
            token_a_mint: dummy_pubkey(4),
            token_b_mint: dummy_pubkey(5),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let order = make_order(dummy_pubkey(20), dummy_pubkey(21));

        let result = executor.build_swap_ix(&order, &wrong_pool);
        assert!(result.is_err());
    }
}
