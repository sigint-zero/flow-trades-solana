//! Client-side helper for building flow-router program instructions.
//!
//! Wraps DEX swap instructions in a flow-router CPI for on-chain fee collection.
//! Supports generic N-hop routing — 1, 2, 3, or more sequential swaps.
//!
//! Fee rate and protocol fee account are enforced by the on-chain config PDA.
//! The client only needs to provide the config PDA address and optionally a
//! referral token account.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use crate::error::{TradeError, TradeResult};

/// Discriminator matching flow-router program.
const SWAP_DISC: u8 = 0;

/// PDA seed for the config account (must match on-chain CONFIG_SEED).
const CONFIG_SEED: &[u8] = b"config";

/// Configuration for the router wrapper.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub program_id: Pubkey,
    pub treasury_wallet: Pubkey,
    pub referral_wallet: Option<Pubkey>,
    /// `fee_bps` from the on-chain config PDA. 0 = the router collects nothing
    /// and never reads the fee-account slots.
    pub fee_bps: u16,
}

/// Account layout the deployed router expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterLayout {
    /// The immutable `FLoW…` deployment: fee paid with plain `Transfer` (fails
    /// with `0x1f` on Token-2022 mints that carry extensions).
    Legacy,
    /// `transfer_checked` router: an `output_mint` account follows the token
    /// program (index N+6); DEX accounts start at N+7.
    TransferChecked,
}

impl RouterConfig {
    /// Any program id other than the legacy deployment speaks the new layout.
    pub fn layout(&self) -> RouterLayout {
        if self.program_id == crate::constants::FLOW_ROUTER_PROGRAM_ID { RouterLayout::Legacy } else { RouterLayout::TransferChecked }
    }

    pub fn fee_account_for_mint(&self, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &self.treasury_wallet, mint, token_program,
        )
    }

    pub fn referral_account_for_mint(&self, mint: &Pubkey, token_program: &Pubkey) -> Option<Pubkey> {
        self.referral_wallet.map(|w| {
            spl_associated_token_account::get_associated_token_address_with_program_id(
                &w, mint, token_program,
            )
        })
    }
}

/// Derive the config PDA address for a given router program ID.
pub fn config_pda(program_id: &Pubkey) -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[CONFIG_SEED], program_id);
    pda
}

/// Wrap N DEX swap instructions in a single flow-router CPI.
///
/// Account layout built by this function:
///   [0]            payer              (signer)
///   [1..N+2]       token_accounts     [input, inter_1..N-1, output]
///   [N+2]          config_pda         (read-only)
///   [N+3]          protocol_fee_acct  (writable)
///   [N+4]          referral_acct      (writable, or program_id to skip)
///   [N+5]          token_program      (read-only)
///   [N+6]          output_mint        (read-only; `RouterLayout::TransferChecked` only)
///   [N+6..] / [N+7..]  all DEX accounts (concatenated from all hops)
///
/// `token_accounts` must have N+1 entries: [input, intermediate_1..N-1, output]
/// `dex_swap_ixs` must have N entries: one per hop
/// `output_mint` is the mint of the last token account; ignored by the legacy layout.
#[allow(clippy::too_many_arguments)]
pub fn wrap_swap(
    config: &RouterConfig,
    payer: &Pubkey,
    token_accounts: &[Pubkey],
    protocol_fee_token_account: &Pubkey,
    referral_token_account: Option<&Pubkey>,
    dex_swap_ixs: &[Instruction],
    amount_in: u64,
    min_amount_out: u64,
    token_program_id: &Pubkey,
    output_mint: &Pubkey,
) -> TradeResult<Instruction> {
    let num_hops = dex_swap_ixs.len();
    if num_hops == 0 {
        return Err(TradeError::Validation("no swap instructions provided".into()));
    }
    if token_accounts.len() != num_hops + 1 {
        return Err(TradeError::Validation(format!(
            "expected {} token accounts for {} hops, got {}",
            num_hops + 1, num_hops, token_accounts.len()
        )));
    }

    let referral_account = referral_token_account.copied().unwrap_or(config.program_id);
    let config_pda = config_pda(&config.program_id);

    // Serialize instruction data manually to match on-chain parser:
    // [SWAP_DISC] + num_hops(u8) + [hop × N] + amount_in(u64) + min_amount_out(u64)
    // Each hop: dex_program(32) + dex_data_len(u32) + dex_data(bytes) + dex_account_count(u8)
    let mut data = vec![SWAP_DISC];
    data.push(num_hops as u8);
    for ix in dex_swap_ixs {
        data.extend_from_slice(&ix.program_id.to_bytes());
        data.extend_from_slice(&(ix.data.len() as u32).to_le_bytes());
        data.extend_from_slice(&ix.data);
        data.push(ix.accounts.len() as u8);
    }
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    // Build account list
    let mut accounts = Vec::new();

    // [0] payer
    accounts.push(AccountMeta::new_readonly(*payer, true));

    // [1..N+2] token accounts (input, intermediates, output)
    for ta in token_accounts {
        accounts.push(AccountMeta::new(*ta, false));
    }

    // Fixed accounts: config, protocol_fee, referral, token_program
    accounts.push(AccountMeta::new_readonly(config_pda, false));
    accounts.push(AccountMeta::new(*protocol_fee_token_account, false));
    accounts.push(AccountMeta::new(referral_account, false));
    accounts.push(AccountMeta::new_readonly(*token_program_id, false));
    if config.layout() == RouterLayout::TransferChecked {
        accounts.push(AccountMeta::new_readonly(*output_mint, false));
    }

    // All DEX accounts from all hops (concatenated)
    for ix in dex_swap_ixs {
        accounts.extend(ix.accounts.iter().cloned());
    }

    // DEX program IDs (needed for CPI — deduplicated)
    let mut seen_programs = Vec::new();
    for ix in dex_swap_ixs {
        if !seen_programs.contains(&ix.program_id) {
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
            seen_programs.push(ix.program_id);
        }
    }

    // Integrator PDA for whitelist validation
    if let Some(ref wallet) = config.referral_wallet {
        if referral_token_account.is_some() {
            let (integrator_pda, _) = Pubkey::find_program_address(
                &[b"integrator", wallet.as_ref()],
                &config.program_id,
            );
            accounts.push(AccountMeta::new_readonly(integrator_pda, false));
        }
    }

    Ok(Instruction {
        program_id: config.program_id,
        accounts,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    const TOKEN_PROGRAM_ID: Pubkey =
        solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

    fn dummy_dex_ix(n_accounts: usize) -> Instruction {
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: (0..n_accounts)
                .map(|_| AccountMeta::new(Pubkey::new_unique(), false))
                .collect(),
            data: vec![0x09, 1, 0, 0, 0, 0, 0, 0, 0],
        }
    }

    /// The legacy layout (the deployed `FLoW…` program); the new layout has its own test.
    fn test_config() -> RouterConfig {
        RouterConfig {
            program_id: crate::constants::FLOW_ROUTER_PROGRAM_ID,
            treasury_wallet: Pubkey::new_unique(),
            referral_wallet: None,
            fee_bps: 50,
        }
    }

    #[test]
    fn test_single_hop_account_count() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let input = Pubkey::new_unique();
        let output = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(17);

        let wrapped = wrap_swap(
            &config, &payer, &[input, output], &protocol_fee, None,
            &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // 1 payer + 2 tokens + 4 fixed + 17 DEX + 1 DEX program = 25
        assert_eq!(wrapped.accounts.len(), 1 + 2 + 4 + 17 + 1);
        assert_eq!(wrapped.program_id, config.program_id);
        assert_eq!(wrapped.data[0], SWAP_DISC);
    }

    #[test]
    fn test_two_hop_account_count() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let hop1 = dummy_dex_ix(10);
        let hop2 = dummy_dex_ix(8);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, None, &[hop1, hop2], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // 1 payer + 3 tokens + 4 fixed + 10 + 8 DEX + 2 DEX programs = 28
        assert_eq!(wrapped.accounts.len(), 1 + 3 + 4 + 10 + 8 + 2);
    }

    #[test]
    fn test_three_hop_account_count() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let hop1 = dummy_dex_ix(5);
        let hop2 = dummy_dex_ix(5);
        let hop3 = dummy_dex_ix(5);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, None, &[hop1, hop2, hop3], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // 1 payer + 4 tokens + 4 fixed + 15 DEX + 3 DEX programs = 27
        assert_eq!(wrapped.accounts.len(), 1 + 4 + 4 + 15 + 3);
    }

    #[test]
    fn test_payer_is_signer() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(3);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, None, &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        assert!(wrapped.accounts[0].is_signer);
        assert_eq!(wrapped.accounts[0].pubkey, payer);
    }

    #[test]
    fn test_config_pda_is_readonly() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(3);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, None, &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // config_pda is at index 1 (payer) + 2 (tokens) = 3
        assert!(!wrapped.accounts[3].is_writable);
        assert_eq!(wrapped.accounts[3].pubkey, config_pda(&config.program_id));
    }

    #[test]
    fn test_with_referral() {
        let referral_wallet = Pubkey::new_unique();
        let config = RouterConfig {
            program_id: Pubkey::new_unique(),
            treasury_wallet: Pubkey::new_unique(),
            referral_wallet: Some(referral_wallet),
            fee_bps: 50,
        };
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(5);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, Some(&referral_wallet),
            &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // referral at index 5 (payer + 2 tokens + config + fee = 5)
        assert_eq!(wrapped.accounts[5].pubkey, referral_wallet);
    }

    #[test]
    fn legacy_program_id_keeps_the_old_layout_and_any_other_adds_output_mint() {
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let out_mint = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(5);
        let tas = [Pubkey::new_unique(), Pubkey::new_unique()];

        let legacy = RouterConfig { program_id: crate::constants::FLOW_ROUTER_PROGRAM_ID, treasury_wallet: Pubkey::new_unique(), referral_wallet: None, fee_bps: 50 };
        assert_eq!(legacy.layout(), RouterLayout::Legacy);
        let w = wrap_swap(&legacy, &payer, &tas, &protocol_fee, None, &[dex_ix.clone()], 1000, 500, &TOKEN_PROGRAM_ID, &out_mint).unwrap();
        // N=1: [payer, in, out, config, fee, referral, token_program, dex×5, dex_program]
        assert_eq!(w.accounts.len(), 7 + 5 + 1);
        assert_eq!(w.accounts[6].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(w.accounts[7].pubkey, dex_ix.accounts[0].pubkey);

        let new = RouterConfig { program_id: Pubkey::new_unique(), treasury_wallet: Pubkey::new_unique(), referral_wallet: None, fee_bps: 50 };
        assert_eq!(new.layout(), RouterLayout::TransferChecked);
        let w = wrap_swap(&new, &payer, &tas, &protocol_fee, None, &[dex_ix.clone()], 1000, 500, &TOKEN_PROGRAM_ID, &out_mint).unwrap();
        assert_eq!(w.accounts.len(), 8 + 5 + 1);
        assert_eq!(w.accounts[6].pubkey, TOKEN_PROGRAM_ID);
        assert_eq!(w.accounts[7].pubkey, out_mint, "output_mint at N+6");
        assert!(!w.accounts[7].is_writable && !w.accounts[7].is_signer);
        assert_eq!(w.accounts[8].pubkey, dex_ix.accounts[0].pubkey, "DEX accounts start at N+7");
        // instruction data is byte-identical between layouts
        let w0 = wrap_swap(&legacy, &payer, &tas, &protocol_fee, None, &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &out_mint).unwrap();
        assert_eq!(w.data, w0.data);
    }

    #[test]
    fn test_no_referral_uses_program_id() {
        let config = test_config();
        let payer = Pubkey::new_unique();
        let protocol_fee = Pubkey::new_unique();
        let dex_ix = dummy_dex_ix(5);

        let wrapped = wrap_swap(
            &config, &payer,
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            &protocol_fee, None, &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        assert_eq!(wrapped.accounts[5].pubkey, config.program_id);
    }

    #[test]
    fn test_zero_hops_rejected() {
        let config = test_config();
        let result = wrap_swap(
            &config, &Pubkey::new_unique(),
            &[Pubkey::new_unique()],
            &Pubkey::new_unique(), None, &[], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_wrong_token_account_count_rejected() {
        let config = test_config();
        let dex_ix = dummy_dex_ix(3);
        // 1 hop needs 2 token accounts, passing 3
        let result = wrap_swap(
            &config, &Pubkey::new_unique(),
            &[Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
            &Pubkey::new_unique(), None, &[dex_ix], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_min_amount_out_in_data() {
        let config = test_config();
        let dex_ix = dummy_dex_ix(5);
        let min_out: u64 = 12_345_678;

        let wrapped = wrap_swap(
            &config, &Pubkey::new_unique(),
            &[Pubkey::new_unique(), Pubkey::new_unique()],
            &Pubkey::new_unique(), None, &[dex_ix], 1_000_000, min_out, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        let target = min_out.to_le_bytes();
        assert!(wrapped.data.windows(8).any(|w| w == target),
            "min_amount_out not found in instruction data");
    }

    #[test]
    fn test_dedup_same_dex_program() {
        let config = test_config();
        let program = Pubkey::new_unique();
        let hop1 = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(Pubkey::new_unique(), false)],
            data: vec![1],
        };
        let hop2 = Instruction {
            program_id: program, // same program
            accounts: vec![AccountMeta::new(Pubkey::new_unique(), false)],
            data: vec![2],
        };

        let wrapped = wrap_swap(
            &config, &Pubkey::new_unique(),
            &[Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()],
            &Pubkey::new_unique(), None, &[hop1, hop2], 1000, 500, &TOKEN_PROGRAM_ID, &Pubkey::default() /* output mint: ignored by the legacy layout */,
        ).unwrap();

        // 1 payer + 3 tokens + 4 fixed + 2 DEX accounts + 1 DEX program (deduped) = 11
        assert_eq!(wrapped.accounts.len(), 1 + 3 + 4 + 2 + 1);
    }
}
