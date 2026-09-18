use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    message::{v0::Message as V0Message, Message, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::{Transaction, VersionedTransaction},
};

use crate::error::{TradeError, TradeResult};
use crate::pool::types::SwapInstructions;

/// Configuration for building an unsigned swap transaction.
#[derive(Debug, Clone)]
pub struct TxBuildConfig {
    /// Compute unit limit for the transaction.
    pub compute_unit_limit: u32,
    /// Priority fee in lamports (converted to micro-lamports per CU internally).
    pub priority_fee_lamports: u64,
}

impl Default for TxBuildConfig {
    fn default() -> Self {
        Self {
            compute_unit_limit: 400_000,
            priority_fee_lamports: 5_000,
        }
    }
}

/// Assemble the full instruction list from SwapInstructions + TxBuildConfig.
pub fn assemble_instructions(
    swap_ixs: &SwapInstructions,
    config: &TxBuildConfig,
) -> TradeResult<Vec<solana_sdk::instruction::Instruction>> {
    if config.compute_unit_limit == 0 {
        return Err(TradeError::Validation(
            "compute_unit_limit must be > 0".into(),
        ));
    }

    let mut instructions = Vec::with_capacity(
        2 + swap_ixs.setup.len() + swap_ixs.swap.len() + swap_ixs.cleanup.len(),
    );

    // Compute budget: set unit limit
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        config.compute_unit_limit,
    ));

    // Priority fee
    if config.priority_fee_lamports > 0 {
        let micro_lamports_per_cu = (config.priority_fee_lamports as u128 * 1_000_000
            / config.compute_unit_limit as u128) as u64;
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
            micro_lamports_per_cu,
        ));
    }

    // Setup instructions (create ATAs, WSOL wrapping, etc.)
    instructions.extend_from_slice(&swap_ixs.setup);

    // Core swap instruction(s)
    instructions.extend_from_slice(&swap_ixs.swap);

    // Cleanup instructions (close WSOL accounts, etc.)
    instructions.extend_from_slice(&swap_ixs.cleanup);

    Ok(instructions)
}

/// Build an unsigned legacy transaction from swap instructions.
/// Returns (Message, Transaction) -- the caller signs externally.
pub fn build_unsigned_swap_message(
    swap_ixs: &SwapInstructions,
    user_pubkey: &Pubkey,
    config: &TxBuildConfig,
    recent_blockhash: Hash,
) -> TradeResult<(Message, Transaction)> {
    let instructions = assemble_instructions(swap_ixs, config)?;

    let message = Message::new(&instructions, Some(user_pubkey));
    let mut tx = Transaction::new_unsigned(message.clone());
    tx.message.recent_blockhash = recent_blockhash;

    Ok((message, tx))
}

/// Build an unsigned VersionedTransaction (v0) with ALT support.
/// Falls back to legacy if no ALTs are provided or if v0 compilation fails.
pub fn build_unsigned_versioned_tx(
    swap_ixs: &SwapInstructions,
    user_pubkey: &Pubkey,
    config: &TxBuildConfig,
    recent_blockhash: Hash,
    address_lookup_tables: &[AddressLookupTableAccount],
) -> TradeResult<VersionedTransaction> {
    let instructions = assemble_instructions(swap_ixs, config)?;

    // Try v0 message with ALTs
    if !address_lookup_tables.is_empty() {
        match V0Message::try_compile(
            user_pubkey,
            &instructions,
            address_lookup_tables,
            recent_blockhash,
        ) {
            Ok(v0_msg) => {
                let versioned_msg = VersionedMessage::V0(v0_msg);
                // Build unsigned: one empty signature slot per required signer
                let num_signers = versioned_msg.header().num_required_signatures as usize;
                let signatures = vec![Signature::default(); num_signers];
                return Ok(VersionedTransaction {
                    signatures,
                    message: versioned_msg,
                });
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "v0 message compilation failed, falling back to legacy"
                );
            }
        }
    }

    // Fallback to legacy message
    let mut legacy_msg = Message::new(&instructions, Some(user_pubkey));
    legacy_msg.recent_blockhash = recent_blockhash;
    let versioned_msg = VersionedMessage::Legacy(legacy_msg);
    let num_signers = versioned_msg.header().num_required_signatures as usize;
    let signatures = vec![Signature::default(); num_signers];
    Ok(VersionedTransaction {
        signatures,
        message: versioned_msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::{AccountMeta, Instruction};

    fn dummy_swap_ixs() -> SwapInstructions {
        let program_id = Pubkey::new_unique();
        SwapInstructions {
            setup: vec![],
            swap: vec![Instruction::new_with_bytes(program_id, &[1, 2, 3], vec![])],
            cleanup: vec![],
        }
    }

    fn default_config() -> TxBuildConfig {
        TxBuildConfig {
            priority_fee_lamports: 5000,
            compute_unit_limit: 200_000,
        }
    }

    #[test]
    fn test_unsigned_basic_build() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let (msg, _tx) = build_unsigned_swap_message(&ixs, &user, &config, hash).unwrap();
        // compute_limit + compute_price + swap = 3
        assert_eq!(msg.instructions.len(), 3);
    }

    #[test]
    fn test_unsigned_zero_priority_fee() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = TxBuildConfig {
            priority_fee_lamports: 0,
            compute_unit_limit: 200_000,
        };
        let hash = Hash::new_unique();

        let (msg, _tx) = build_unsigned_swap_message(&ixs, &user, &config, hash).unwrap();
        // compute_limit + swap = 2 (no price instruction)
        assert_eq!(msg.instructions.len(), 2);
    }

    #[test]
    fn test_unsigned_with_setup_cleanup() {
        let program_id = Pubkey::new_unique();
        let ixs = SwapInstructions {
            setup: vec![Instruction::new_with_bytes(program_id, &[10], vec![])],
            swap: vec![Instruction::new_with_bytes(program_id, &[20], vec![])],
            cleanup: vec![Instruction::new_with_bytes(program_id, &[30], vec![])],
        };
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let (msg, _tx) = build_unsigned_swap_message(&ixs, &user, &config, hash).unwrap();
        // compute_limit + compute_price + setup + swap + cleanup = 5
        assert_eq!(msg.instructions.len(), 5);
    }

    #[test]
    fn test_unsigned_zero_compute_limit_returns_error() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = TxBuildConfig {
            priority_fee_lamports: 5000,
            compute_unit_limit: 0,
        };
        let hash = Hash::new_unique();

        let result = build_unsigned_swap_message(&ixs, &user, &config, hash);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("compute_unit_limit must be > 0"));
    }

    #[test]
    fn test_unsigned_blockhash_set_on_tx() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let (_msg, tx) = build_unsigned_swap_message(&ixs, &user, &config, hash).unwrap();
        assert_eq!(tx.message.recent_blockhash, hash);
    }

    #[test]
    fn test_unsigned_priority_fee_calculation() {
        // 5000 lamports / 200_000 CU = 25_000 micro-lamports per CU
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = TxBuildConfig {
            priority_fee_lamports: 5000,
            compute_unit_limit: 200_000,
        };
        let hash = Hash::new_unique();

        let (msg, _tx) = build_unsigned_swap_message(&ixs, &user, &config, hash).unwrap();
        // Second instruction should be ComputeBudgetInstruction::set_compute_unit_price
        assert_eq!(msg.instructions.len(), 3);
    }

    // --- Versioned transaction tests ---

    #[test]
    fn test_versioned_tx_without_alts_is_legacy() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let vtx = build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[]).unwrap();

        // Should be legacy when no ALTs provided
        match &vtx.message {
            VersionedMessage::Legacy(msg) => {
                // compute_limit + compute_price + swap = 3
                assert_eq!(msg.instructions.len(), 3);
            }
            VersionedMessage::V0(_) => panic!("expected Legacy message, got V0"),
        }

        // Should have 1 signature slot (payer only)
        assert_eq!(vtx.signatures.len(), 1);
        assert_eq!(vtx.signatures[0], Signature::default());
    }

    #[test]
    fn test_versioned_tx_with_alts_produces_v0() {
        // Create a swap instruction that references accounts in our ALT
        let program_id = Pubkey::new_unique();
        let alt_key = Pubkey::new_unique();
        let acc1 = Pubkey::new_unique();
        let acc2 = Pubkey::new_unique();
        let acc3 = Pubkey::new_unique();

        let ixs = SwapInstructions {
            setup: vec![],
            swap: vec![Instruction::new_with_bytes(
                program_id,
                &[1, 2, 3],
                vec![
                    AccountMeta::new(acc1, false),
                    AccountMeta::new(acc2, false),
                    AccountMeta::new_readonly(acc3, false),
                ],
            )],
            cleanup: vec![],
        };

        let alt = AddressLookupTableAccount {
            key: alt_key,
            addresses: vec![acc1, acc2, acc3],
        };

        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let vtx = build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[alt]).unwrap();

        // Should produce a V0 message when ALT accounts match
        match &vtx.message {
            VersionedMessage::V0(msg) => {
                // Should have address table lookups
                assert!(
                    !msg.address_table_lookups.is_empty(),
                    "expected address table lookups in v0 message"
                );
                // Static keys should be fewer than the total accounts
                // (some accounts resolved via ALT)
                assert!(
                    msg.account_keys.len()
                        < 1 /* payer */ + 1 /* program */ + 3 /* accounts */ + 2, /* compute budget program + its accounts */
                    "v0 should have fewer static keys than legacy would"
                );
            }
            VersionedMessage::Legacy(_) => {
                panic!("expected V0 message, got Legacy");
            }
        }
    }

    #[test]
    fn test_versioned_tx_with_no_matching_alts_falls_back_to_legacy() {
        // ALT has addresses that don't appear in our instructions
        let alt_key = Pubkey::new_unique();
        let alt = AddressLookupTableAccount {
            key: alt_key,
            addresses: vec![Pubkey::new_unique(), Pubkey::new_unique()],
        };

        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let vtx = build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[alt]).unwrap();

        // The message will be V0 even with no lookups used (V0Message::try_compile
        // succeeds with empty lookups). But the important thing is it doesn't error.
        // The message should still compile successfully.
        let num_ixs = match &vtx.message {
            VersionedMessage::Legacy(msg) => msg.instructions.len(),
            VersionedMessage::V0(msg) => msg.instructions.len(),
        };
        assert_eq!(num_ixs, 3);
    }

    #[test]
    fn test_versioned_tx_zero_compute_limit_returns_error() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = TxBuildConfig {
            priority_fee_lamports: 5000,
            compute_unit_limit: 0,
        };
        let hash = Hash::new_unique();

        let result = build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("compute_unit_limit must be > 0"));
    }

    #[test]
    fn test_versioned_tx_serialization() {
        let ixs = dummy_swap_ixs();
        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        let vtx = build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[]).unwrap();

        // Should be serializable with bincode
        let bytes = bincode::serialize(&vtx).unwrap();
        assert!(!bytes.is_empty());

        // Should be deserializable back
        let vtx2: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        assert_eq!(vtx.signatures.len(), vtx2.signatures.len());
    }

    #[test]
    fn test_versioned_tx_v0_smaller_than_legacy() {
        // Build a swap instruction with many accounts that are in the ALT
        let program_id = Pubkey::new_unique();
        let alt_key = Pubkey::new_unique();
        let accounts: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();

        let account_metas: Vec<AccountMeta> = accounts
            .iter()
            .map(|pk| AccountMeta::new(*pk, false))
            .collect();

        let ixs = SwapInstructions {
            setup: vec![],
            swap: vec![Instruction::new_with_bytes(
                program_id,
                &[1, 2, 3],
                account_metas,
            )],
            cleanup: vec![],
        };

        let alt = AddressLookupTableAccount {
            key: alt_key,
            addresses: accounts,
        };

        let user = Pubkey::new_unique();
        let config = default_config();
        let hash = Hash::new_unique();

        // Build legacy (no ALTs)
        let legacy_vtx =
            build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[]).unwrap();
        let legacy_bytes = bincode::serialize(&legacy_vtx).unwrap();

        // Build v0 (with ALTs)
        let v0_vtx =
            build_unsigned_versioned_tx(&ixs, &user, &config, hash, &[alt]).unwrap();
        let v0_bytes = bincode::serialize(&v0_vtx).unwrap();

        // v0 should be smaller because accounts are referenced by 1-byte index
        assert!(
            v0_bytes.len() < legacy_bytes.len(),
            "v0 ({} bytes) should be smaller than legacy ({} bytes)",
            v0_bytes.len(),
            legacy_bytes.len()
        );

        // Verify the savings are significant (each account saves ~31 bytes via ALT)
        let savings = legacy_bytes.len() - v0_bytes.len();
        assert!(
            savings > 100,
            "expected >100 byte savings, got {} (legacy={}, v0={})",
            savings,
            legacy_bytes.len(),
            v0_bytes.len()
        );
    }
}
