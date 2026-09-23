use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::quote::dlmm::{arrays_for_swap, bin_array_pda, liquid_arrays, DlmmPair, BINS, SWAP_ARRAYS};
use crate::constants::*;
use super::{AmmExecutor, DISC_SWAP};

/// `swap2` (Anchor "global:swap2"): the Token-2022-capable swap. `swap` pins
/// both token programs to SPL Token.
pub const DISC_SWAP2: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];

pub struct MeteoraDlmmExecutor;

/// Bin arrays for a swap: `(bitmap extension, arrays in walk order)`.
///
/// The program walks the arrays that the pair's liquidity bitmap marks, in
/// the swap direction from the active bin, reading them from the remaining
/// accounts. With the pair's bins in memory (`quote::dlmm::BINS`, loaded by
/// the quote) the arrays come from the newest view of the pair — the bins
/// snapshot or `state`, whichever saw the later swap — and the extension is
/// passed exactly when it exists. Without it: the state's own bitmap and no
/// extension; without a parsed state: the arrays derived at parse time.
pub fn dlmm_swap_bin_arrays(lb_pair: &Pubkey, pair: &DlmmPair, swap_for_y: bool, fallback: &[Pubkey]) -> (Option<Pubkey>, Vec<Pubkey>) {
    if let Some(b) = BINS.get(lb_pair) {
        let state_newer = pair.is_parsed() && pair.last_update_timestamp > b.pair.last_update_timestamp;
        let idx = if state_newer {
            arrays_for_swap(&liquid_arrays(&pair.bitmap, b.ext_bitmap.as_ref()), pair.active_id, b.extension.is_some(), swap_for_y, SWAP_ARRAYS)
        } else {
            b.arrays_for_swap(swap_for_y, SWAP_ARRAYS)
        };
        if !idx.is_empty() {
            let keys = idx
                .iter()
                .map(|i| b.arrays.iter().find(|a| a.index == *i as i64).map(|a| a.key).unwrap_or_else(|| bin_array_pda(lb_pair, *i as i64)))
                .collect();
            return (b.extension, keys);
        }
    }
    if pair.is_parsed() {
        let idx = arrays_for_swap(&liquid_arrays(&pair.bitmap, None), pair.active_id, false, swap_for_y, SWAP_ARRAYS);
        if !idx.is_empty() {
            return (None, idx.iter().map(|i| bin_array_pda(lb_pair, *i as i64)).collect());
        }
    }
    (None, fallback.to_vec())
}

impl AmmExecutor for MeteoraDlmmExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (lb_pair, reserve_x, reserve_y, token_x_mint, token_y_mint, oracle, host_fee_in,
             event_authority, bin_arrays, pair) =
            match pool_state {
                PoolState::MeteoraDlmm {
                    lb_pair, reserve_x, reserve_y, token_x_mint, token_y_mint, oracle, host_fee_in,
                    event_authority, bin_arrays, pair, ..
                } => (lb_pair, reserve_x, reserve_y, token_x_mint, token_y_mint, oracle, host_fee_in,
                      event_authority, bin_arrays, pair),
                _ => return Err(TradeError::Execution("expected MeteoraDlmm pool state".into())),
            };

        // Determine token programs for X and Y
        let (x_prog, y_prog) = if order.input_mint == *token_x_mint {
            (order.input_token_program, order.output_token_program)
        } else {
            (order.output_token_program, order.input_token_program)
        };
        let user_token_x = get_associated_token_address_with_program_id(&order.user, token_x_mint, &x_prog);
        let user_token_y = get_associated_token_address_with_program_id(&order.user, token_y_mint, &y_prog);

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

        // `swap` requires SPL Token on both sides; a Token-2022 side needs
        // `swap2` (+ memo program, + RemainingAccountsInfo: no transfer-hook
        // slices → an empty vec).
        let v2 = x_prog == TOKEN_2022_PROGRAM_ID || y_prog == TOKEN_2022_PROGRAM_ID;
        // Data: discriminator + amount_in(u64) + min_amount_out(u64) [+ slices vec len u32]
        let mut data = Vec::with_capacity(28);
        data.extend_from_slice(if v2 { &DISC_SWAP2 } else { &DISC_SWAP });
        data.extend_from_slice(&order.amount_in.to_le_bytes());
        data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        if v2 {
            data.extend_from_slice(&0u32.to_le_bytes());
        }

        let swap_x_to_y = order.input_mint == *token_x_mint;
        let (user_token_in, user_token_out) = if swap_x_to_y {
            (user_token_x, user_token_y)
        } else {
            (user_token_y, user_token_x)
        };

        let (bitmap_ext, arrays) = dlmm_swap_bin_arrays(lb_pair, pair, swap_x_to_y, bin_arrays);
        // optional account: the program id stands for "none"
        let bitmap_ext_meta = match bitmap_ext {
            Some(e) => AccountMeta::new(e, false),
            None => AccountMeta::new_readonly(METEORA_DLMM_PROG_ID, false),
        };

        let mut accounts = vec![
            AccountMeta::new(*lb_pair, false),
            bitmap_ext_meta,
            AccountMeta::new(*reserve_x, false),
            AccountMeta::new(*reserve_y, false),
            AccountMeta::new(user_token_in, false),
            AccountMeta::new(user_token_out, false),
            AccountMeta::new_readonly(*token_x_mint, false),
            AccountMeta::new_readonly(*token_y_mint, false),
            AccountMeta::new(*oracle, false),
            AccountMeta::new_readonly(*host_fee_in, false),
            AccountMeta::new(order.user, true),
            AccountMeta::new_readonly(x_prog, false),
            AccountMeta::new_readonly(y_prog, false),
        ];
        if v2 {
            accounts.push(AccountMeta::new_readonly(MEMO_PROGRAM_ID, false));
        }
        accounts.push(AccountMeta::new_readonly(*event_authority, false));
        accounts.push(AccountMeta::new_readonly(METEORA_DLMM_PROG_ID, false));

        // Remaining accounts: bin arrays in walk order
        for ba in &arrays {
            accounts.push(AccountMeta::new(*ba, false));
        }

        let swap_ix = Instruction {
            program_id: METEORA_DLMM_PROG_ID,
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
    use crate::quote::dlmm::{Bin, BinArray, DlmmBins};

    #[test]
    fn swap2_discriminator() {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(b"global:swap2");
        assert_eq!(DISC_SWAP2[..], h[..8]);
    }

    /// Pair with liquid arrays −2, −1, 0, 1, 3 and the active bin in array 0.
    fn pair_state() -> (PoolState, Pubkey) {
        let lb_pair = Pubkey::new_unique();
        let mut pair = DlmmPair { bin_step: 10, active_id: 5, last_update_timestamp: 100, ..Default::default() };
        for idx in [-2i32, -1, 0, 1, 3] {
            let b = (idx + 512) as usize;
            pair.bitmap[b / 64] |= 1 << (b % 64);
        }
        let state = PoolState::MeteoraDlmm {
            lb_pair,
            bin_array_bitmap_extension: METEORA_DLMM_PROG_ID,
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            token_x_mint: Pubkey::new_unique(),
            token_y_mint: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            host_fee_in: METEORA_DLMM_PROG_ID,
            event_authority: Pubkey::new_unique(),
            bin_arrays: vec![],
            pair,
        };
        (state, lb_pair)
    }

    fn order(state: &PoolState, x_to_y: bool, input_token_program: Pubkey) -> SwapOrder {
        let PoolState::MeteoraDlmm { lb_pair, token_x_mint, token_y_mint, .. } = state else { unreachable!() };
        let (i, o) = if x_to_y { (*token_x_mint, *token_y_mint) } else { (*token_y_mint, *token_x_mint) };
        SwapOrder {
            pool_address: *lb_pair,
            pool_type: PoolType::MeteoraDlmm,
            input_mint: i,
            output_mint: o,
            amount_in: 1_000,
            min_amount_out: 1,
            user: Pubkey::new_unique(),
            input_token_program,
            output_token_program: TOKEN_PROGRAM_ID,
        }
    }

    #[test]
    fn bin_arrays_in_walk_order_without_bins_in_memory() {
        let (state, lb_pair) = pair_state();
        let ix = &MeteoraDlmmExecutor.build_swap_ix(&order(&state, true, TOKEN_PROGRAM_ID), &state).unwrap().swap[0];
        assert_eq!(ix.data[..8], DISC_SWAP);
        assert_eq!(ix.accounts.len(), 15 + 3);
        assert_eq!(ix.accounts[1].pubkey, METEORA_DLMM_PROG_ID, "no extension known → none");
        let tail: Vec<Pubkey> = ix.accounts[15..].iter().map(|a| a.pubkey).collect();
        assert_eq!(tail, [0i64, -1, -2].map(|i| bin_array_pda(&lb_pair, i)));
        assert!(ix.accounts[15..].iter().all(|a| a.is_writable));
        let ix = &MeteoraDlmmExecutor.build_swap_ix(&order(&state, false, TOKEN_PROGRAM_ID), &state).unwrap().swap[0];
        let tail: Vec<Pubkey> = ix.accounts[15..].iter().map(|a| a.pubkey).collect();
        assert_eq!(tail, [0i64, 1, 3].map(|i| bin_array_pda(&lb_pair, i)), "empty array 2 skipped");
    }

    #[test]
    fn bins_snapshot_supplies_extension_and_array_keys() {
        let (state, lb_pair) = pair_state();
        let PoolState::MeteoraDlmm { pair, .. } = &state else { unreachable!() };
        let keys: Vec<(i64, Pubkey)> = [-1i64, 0, 1].iter().map(|i| (*i, Pubkey::new_unique())).collect();
        let arrays = keys.iter().map(|(i, k)| BinArray { index: *i, key: *k, bins: vec![Bin::default(); 70] }).collect();
        let ext_key = Pubkey::new_unique();
        let ext = crate::quote::dlmm::ExtBitmap { positive: [[0; 8]; 12], negative: [[0; 8]; 12] };
        BINS.insert(lb_pair, std::sync::Arc::new(DlmmBins::new(*pair, arrays, ext_key, Some(ext), 1, 0)));
        let ix = &MeteoraDlmmExecutor.build_swap_ix(&order(&state, false, TOKEN_PROGRAM_ID), &state).unwrap().swap[0];
        assert_eq!(ix.accounts[1].pubkey, ext_key);
        assert!(ix.accounts[1].is_writable);
        let tail: Vec<Pubkey> = ix.accounts[15..].iter().map(|a| a.pubkey).collect();
        assert_eq!(tail, vec![keys[1].1, keys[2].1, bin_array_pda(&lb_pair, 3)]);
        BINS.remove(&lb_pair);
    }

    #[test]
    fn token_2022_side_uses_swap2_with_memo() {
        let (state, _) = pair_state();
        let ix = &MeteoraDlmmExecutor.build_swap_ix(&order(&state, true, TOKEN_2022_PROGRAM_ID), &state).unwrap().swap[0];
        assert_eq!(ix.data[..8], DISC_SWAP2);
        assert_eq!(ix.data.len(), 8 + 8 + 8 + 4, "empty RemainingAccountsInfo");
        assert_eq!(ix.accounts[11].pubkey, TOKEN_2022_PROGRAM_ID, "token_x_program");
        assert_eq!(ix.accounts[12].pubkey, TOKEN_PROGRAM_ID, "token_y_program");
        assert_eq!(ix.accounts[13].pubkey, MEMO_PROGRAM_ID);
        assert_eq!(ix.accounts[15].pubkey, METEORA_DLMM_PROG_ID);
        assert_eq!(ix.accounts.len(), 16 + 3);
    }
}
