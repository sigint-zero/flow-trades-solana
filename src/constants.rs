use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

// ── Quote Mints ──

pub const SOL_NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
pub const PYUSD_MINT: Pubkey = pubkey!("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo");

/// Bridge mints for multi-hop routing — high-liquidity intermediaries.
/// Includes PYUSD (Token-2022) for broader intermediate routing coverage.
pub const BRIDGE_MINTS: [Pubkey; 4] = [SOL_NATIVE_MINT, USDC_MINT, USDT_MINT, PYUSD_MINT];

/// Token-2022 bridge mints — need Token-2022 program for ATA creation.
pub const TOKEN_2022_BRIDGE_MINTS: [Pubkey; 1] = [PYUSD_MINT];

// ── DEX Program IDs ──

pub const RAYDIUM_V4_PROG_ID: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_CPMM_PROG_ID: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_CL_PROG_ID: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const RAYDIUM_LP_PROG_ID: Pubkey = pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
pub const ORCA_PROG_ID: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const METEORA_PROG_ID: Pubkey = pubkey!("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB");
pub const METEORA_DLMM_PROG_ID: Pubkey = pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
pub const METEORA_DAMM_PROG_ID: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
pub const PUMP_FUN_PROG_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const PUMP_FUN_AMM_PROG_ID: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const FLUXBEAM_PROG_ID: Pubkey = pubkey!("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSrm1X");
pub const FLASH_TRADE_PROG_ID: Pubkey = pubkey!("FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9");
pub const BYREAL_PROG_ID: Pubkey = pubkey!("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");
pub const DEFITUNA_FUSION_PROG_ID: Pubkey = pubkey!("fUSioN9YKKSa3CUC2YUc4tPkHJ5Y6XW1yz8y6F7qWz9");
pub const DEFITUNA_POOLS_PROG_ID: Pubkey = pubkey!("tuna4uSQZncNeeiAMKbstuxA9CUkHH6HmC64wgmnogD");
pub const SAROS_PROG_ID: Pubkey = pubkey!("SSwapUtytfBdBn1b9NUGG6foMVPtcWgpRU32HToDUZr");
pub const PANCAKESWAP_PROG_ID: Pubkey = pubkey!("HpNfyc2Saw7RKkQd8nEL4khUcuPhQ7WwY1B2qjx8jxFq");
pub const DOOAR_PROG_ID: Pubkey = pubkey!("Dooar9JkhdZ7J3LHN3A7YCuoGRUggXhQaG4kijfLGU2j");
pub const METEORA_DBC_PROG_ID: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const PUMPUP_PROG_ID: Pubkey = pubkey!("PdMDrKEMaX8q7CCJb7NvUCxerBCcsFUa4LjBEynTtEd");
/// OnChain Labs DEX V2 — aggregator that routes into 80+ DEXes including
/// private MM venues (Goonfi, Solfi V2, Tessera, AlphaQ, Numeraire, Humidifi,
/// ZeroFi, Heaven, MoonIt). We do **not** quote or execute against this
/// program — it's added to the Geyser/block-scanner filters only so that the
/// pool-discovery pipeline picks up the underlying pool addresses it routes
/// through. Net effect: our pool registry grows automatically as OnChain Labs
/// surfaces pools we don't yet know about.
pub const ONCHAIN_LABS_DEX_V2_PROG_ID: Pubkey = pubkey!("proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u");

// ── Flow Router Program (hardcoded — cannot be bypassed) ──

pub const FLOW_ROUTER_PROGRAM_ID: Pubkey = pubkey!("FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm");
// Fee rate is enforced by the on-chain config PDA, not client-side.

// ── Token Programs ──

pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const MEMO_PROGRAM_ID: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// Check if a mint uses the Token-2022 program (for correct ATA creation in multi-hop).
pub fn is_token_2022_bridge_mint(mint: &Pubkey) -> bool {
    TOKEN_2022_BRIDGE_MINTS.contains(mint)
}

/// Get the token program ID for a bridge mint.
/// Returns Token-2022 program for known Token-2022 mints, standard Token program otherwise.
pub fn token_program_for_bridge_mint(mint: &Pubkey) -> Pubkey {
    if is_token_2022_bridge_mint(mint) {
        TOKEN_2022_PROGRAM_ID
    } else {
        TOKEN_PROGRAM_ID
    }
}

// Platform fee is always taken from the output token (post-swap).

/// Map of DEX program ID → human label. Used by `GET /program-id-to-label`.
pub fn program_id_to_label() -> Vec<(Pubkey, &'static str)> {
    vec![
        (RAYDIUM_V4_PROG_ID, "Raydium V4"),
        (RAYDIUM_CPMM_PROG_ID, "Raydium CPMM"),
        (RAYDIUM_CL_PROG_ID, "Raydium CLMM"),
        (RAYDIUM_LP_PROG_ID, "Raydium LP"),
        (ORCA_PROG_ID, "Orca"),
        (METEORA_PROG_ID, "Meteora"),
        (METEORA_DLMM_PROG_ID, "Meteora DLMM"),
        (METEORA_DAMM_PROG_ID, "Meteora DAMM"),
        (PUMP_FUN_PROG_ID, "PumpFun"),
        (PUMP_FUN_AMM_PROG_ID, "PumpFun AMM"),
        (FLUXBEAM_PROG_ID, "FluxBeam"),
        (FLASH_TRADE_PROG_ID, "FlashTrade"),
        (BYREAL_PROG_ID, "Byreal"),
        (DEFITUNA_FUSION_PROG_ID, "DefiTuna Fusion"),
        (DEFITUNA_POOLS_PROG_ID, "DefiTuna Pools"),
        (SAROS_PROG_ID, "Saros"),
        (PANCAKESWAP_PROG_ID, "PancakeSwap"),
        (DOOAR_PROG_ID, "Dooar"),
        (METEORA_DBC_PROG_ID, "Meteora DBC"),
        (PUMPUP_PROG_ID, "Pumpup"),
        (ONCHAIN_LABS_DEX_V2_PROG_ID, "OnChain Labs DEX V2 (discovery only)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_program_id_to_label_has_all_dexes() {
        let labels = program_id_to_label();
        // 19 quotable/executable + Pumpup + OnChain Labs DEX V2 (discovery only)
        assert_eq!(labels.len(), 21);
    }

    #[test]
    fn test_sol_mint_is_correct() {
        assert_eq!(
            SOL_NATIVE_MINT.to_string(),
            "So11111111111111111111111111111111111111112"
        );
    }

    #[test]
    fn test_bridge_mints_count_and_contents() {
        assert_eq!(BRIDGE_MINTS.len(), 4);
        assert_eq!(BRIDGE_MINTS[0], SOL_NATIVE_MINT);
        assert_eq!(BRIDGE_MINTS[1], USDC_MINT);
        assert_eq!(BRIDGE_MINTS[2], USDT_MINT);
        assert_eq!(BRIDGE_MINTS[3], PYUSD_MINT);
    }

    #[test]
    fn test_pyusd_mint_is_correct() {
        assert_eq!(
            PYUSD_MINT.to_string(),
            "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo"
        );
    }

    #[test]
    fn test_token_2022_bridge_mints() {
        assert_eq!(TOKEN_2022_BRIDGE_MINTS.len(), 1);
        assert_eq!(TOKEN_2022_BRIDGE_MINTS[0], PYUSD_MINT);
    }

    #[test]
    fn test_is_token_2022_bridge_mint() {
        assert!(is_token_2022_bridge_mint(&PYUSD_MINT));
        assert!(!is_token_2022_bridge_mint(&SOL_NATIVE_MINT));
        assert!(!is_token_2022_bridge_mint(&USDC_MINT));
        assert!(!is_token_2022_bridge_mint(&USDT_MINT));
    }

    #[test]
    fn test_token_program_for_bridge_mint() {
        assert_eq!(token_program_for_bridge_mint(&SOL_NATIVE_MINT), TOKEN_PROGRAM_ID);
        assert_eq!(token_program_for_bridge_mint(&USDC_MINT), TOKEN_PROGRAM_ID);
        assert_eq!(token_program_for_bridge_mint(&PYUSD_MINT), TOKEN_2022_PROGRAM_ID);
    }

    #[test]
    fn test_token_program_id_is_correct() {
        assert_eq!(
            TOKEN_PROGRAM_ID.to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
    }
}
