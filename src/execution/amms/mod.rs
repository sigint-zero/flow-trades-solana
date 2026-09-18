pub mod raydium_v4;
pub mod raydium_cpmm;
pub mod raydium_clmm;
pub mod raydium_lp;
pub mod pumpfun;
pub mod pumpfun_amm;
pub mod meteora;
pub mod meteora_dlmm;
pub mod meteora_damm;
pub mod meteora_dbc;
pub mod orca;
pub mod fluxbeam;
pub mod flash_trade;
pub mod byreal;
pub mod defituna;
pub mod saros;
pub mod pancakeswap;
pub mod dooar;
pub mod pumpup;
pub mod pumpup_bonding;

use crate::error::TradeResult;
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};

// Pre-computed Anchor discriminators (first 8 bytes of SHA256("global:<method>")).
// Eliminates runtime SHA256 computation in each AMM executor.
pub const DISC_SWAP: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
pub const DISC_SWAP_V2: [u8; 8] = [0x2b, 0x04, 0xed, 0x0b, 0x1a, 0xc9, 0x1e, 0x62];
pub const DISC_SWAP_BASE_INPUT: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
pub const DISC_BUY_EXACT_IN: [u8; 8] = [0xfa, 0xea, 0x0d, 0x7b, 0xd5, 0x9c, 0x13, 0xec];
pub const DISC_SELL_EXACT_IN: [u8; 8] = [0x95, 0x27, 0xde, 0x9b, 0xd3, 0x7c, 0x98, 0x1a];

/// Trait implemented by each AMM's swap instruction builder.
pub trait AmmExecutor: Send + Sync {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions>;
}

/// Zero-allocation enum dispatch for AMM executors (mirrors AmmParserType).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmmExecutorType {
    RaydiumV4,
    RaydiumCpmm,
    RaydiumClmm,
    RaydiumLp,
    PumpFun,
    PumpFunAmm,
    Meteora,
    MeteoraDlmm,
    MeteoraDamm,
    MeteoraDbc,
    Orca,
    FluxBeam,
    FlashTrade,
    Byreal,
    DefiTunaFusion,
    DefiTunaPools,
    Saros,
    PancakeSwap,
    Dooar,
    Pumpup,
    PumpupBonding,
}

impl AmmExecutorType {
    #[inline]
    pub fn build_swap_ix(
        self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        match self {
            Self::RaydiumV4 => raydium_v4::RaydiumV4Executor.build_swap_ix(order, pool_state),
            Self::RaydiumCpmm => raydium_cpmm::RaydiumCpmmExecutor.build_swap_ix(order, pool_state),
            Self::RaydiumClmm => raydium_clmm::RaydiumClmmExecutor.build_swap_ix(order, pool_state),
            Self::RaydiumLp => raydium_lp::RaydiumLpExecutor.build_swap_ix(order, pool_state),
            Self::PumpFun => pumpfun::PumpFunExecutor.build_swap_ix(order, pool_state),
            Self::PumpFunAmm => pumpfun_amm::PumpFunAmmExecutor.build_swap_ix(order, pool_state),
            Self::Meteora => meteora::MeteoraExecutor.build_swap_ix(order, pool_state),
            Self::MeteoraDlmm => meteora_dlmm::MeteoraDlmmExecutor.build_swap_ix(order, pool_state),
            Self::MeteoraDamm => meteora_damm::MeteoraDammExecutor.build_swap_ix(order, pool_state),
            Self::MeteoraDbc => meteora_dbc::MeteoraDbcExecutor.build_swap_ix(order, pool_state),
            Self::Orca => orca::OrcaExecutor.build_swap_ix(order, pool_state),
            Self::FluxBeam => fluxbeam::FluxBeamExecutor.build_swap_ix(order, pool_state),
            Self::FlashTrade => flash_trade::FlashTradeExecutor.build_swap_ix(order, pool_state),
            Self::Byreal => byreal::ByrealExecutor.build_swap_ix(order, pool_state),
            Self::DefiTunaFusion => defituna::DefiTunaFusionExecutor.build_swap_ix(order, pool_state),
            Self::DefiTunaPools => defituna::DefiTunaPoolsExecutor.build_swap_ix(order, pool_state),
            Self::Saros => saros::SarosExecutor.build_swap_ix(order, pool_state),
            Self::PancakeSwap => pancakeswap::PancakeSwapExecutor.build_swap_ix(order, pool_state),
            Self::Dooar => dooar::DooarExecutor.build_swap_ix(order, pool_state),
            Self::Pumpup => pumpup::PumpupExecutor.build_swap_ix(order, pool_state),
            Self::PumpupBonding => pumpup_bonding::PumpupBondingExecutor.build_swap_ix(order, pool_state),
        }
    }

    /// Map from PoolType to executor.
    pub fn from_pool_type(pool_type: crate::pool::types::PoolType) -> TradeResult<Self> {
        use crate::pool::types::PoolType;
        match pool_type {
            PoolType::RaydiumV4 => Ok(Self::RaydiumV4),
            PoolType::RaydiumCpmm => Ok(Self::RaydiumCpmm),
            PoolType::RaydiumCl => Ok(Self::RaydiumClmm),
            PoolType::RaydiumLp => Ok(Self::RaydiumLp),
            PoolType::PumpFun => Ok(Self::PumpFun),
            PoolType::PumpFunAmm => Ok(Self::PumpFunAmm),
            PoolType::Meteora => Ok(Self::Meteora),
            PoolType::MeteoraDlmm => Ok(Self::MeteoraDlmm),
            PoolType::MeteoraDamm => Ok(Self::MeteoraDamm),
            PoolType::MeteoraDbc => Ok(Self::MeteoraDbc),
            PoolType::Orca => Ok(Self::Orca),
            PoolType::FluxBeam => Ok(Self::FluxBeam),
            PoolType::FlashTrade => Ok(Self::FlashTrade),
            PoolType::Byreal => Ok(Self::Byreal),
            PoolType::DefiTunaFusion => Ok(Self::DefiTunaFusion),
            PoolType::DefiTunaPools => Ok(Self::DefiTunaPools),
            PoolType::Saros => Ok(Self::Saros),
            PoolType::PancakeSwap => Ok(Self::PancakeSwap),
            PoolType::Dooar => Ok(Self::Dooar),
            PoolType::Pumpup => Ok(Self::Pumpup),
            PoolType::PumpupBonding => Ok(Self::PumpupBonding),
            PoolType::Unknown => {
                Err(crate::error::TradeError::Execution(
                    format!("no direct executor for pool type {:?}", pool_type),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn compute_disc(label: &[u8]) -> [u8; 8] {
        let mut h = Sha256::new();
        h.update(label);
        h.finalize()[..8].try_into().unwrap()
    }

    #[test]
    fn test_precomputed_discriminators_match_sha256() {
        assert_eq!(DISC_SWAP, compute_disc(b"global:swap"));
        assert_eq!(DISC_SWAP_V2, compute_disc(b"global:swap_v2"));
        assert_eq!(DISC_SWAP_BASE_INPUT, compute_disc(b"global:swap_base_input"));
        assert_eq!(DISC_BUY_EXACT_IN, compute_disc(b"global:buy_exact_in"));
        assert_eq!(DISC_SELL_EXACT_IN, compute_disc(b"global:sell_exact_in"));
    }

    #[test]
    fn test_from_pool_type_all_supported() {
        use crate::pool::types::PoolType;
        let supported = [
            PoolType::RaydiumV4, PoolType::RaydiumCpmm, PoolType::RaydiumCl,
            PoolType::RaydiumLp, PoolType::PumpFun, PoolType::PumpFunAmm,
            PoolType::Meteora, PoolType::MeteoraDlmm, PoolType::MeteoraDamm, PoolType::MeteoraDbc,
            PoolType::Orca, PoolType::FluxBeam, PoolType::FlashTrade,
            PoolType::Byreal, PoolType::DefiTunaFusion, PoolType::DefiTunaPools,
            PoolType::Saros, PoolType::PancakeSwap, PoolType::Dooar,
            PoolType::Pumpup, PoolType::PumpupBonding,
        ];
        for pt in &supported {
            assert!(AmmExecutorType::from_pool_type(*pt).is_ok(), "failed for {:?}", pt);
        }
    }

    #[test]
    fn test_from_pool_type_unsupported() {
        use crate::pool::types::PoolType;
        assert!(AmmExecutorType::from_pool_type(PoolType::Unknown).is_err());
        assert!(AmmExecutorType::from_pool_type(PoolType::Unknown).is_err());
    }

    // ── Slippage enforcement: min_amount_out encoding in AMM instruction data ──

    /// Scan instruction data for a u64 LE value at any byte offset.
    fn find_u64_le(data: &[u8], value: u64) -> bool {
        let target = value.to_le_bytes();
        data.windows(8).any(|w| w == target)
    }

    /// Verify min_amount_out is encoded at a specific byte offset.
    fn check_u64_le_at(data: &[u8], offset: usize, value: u64) -> bool {
        if data.len() < offset + 8 {
            return false;
        }
        data[offset..offset + 8] == value.to_le_bytes()
    }

    #[test]
    fn test_raydium_cpmm_encodes_min_amount_out() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let min_out: u64 = 12_345_678;

        let state = PoolState::RaydiumCpmm {
            pool,
            authority: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            token_0_vault: Pubkey::new_unique(),
            token_1_vault: Pubkey::new_unique(),
            token_0_mint: mint_a,
            token_1_mint: mint_b,
            observation: Pubkey::new_unique(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        };

        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::RaydiumCpmm,
            input_mint: mint_a,
            output_mint: mint_b,
            amount_in: 1_000_000,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::RaydiumCpmm;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        // Swap instruction data layout: disc(8) + amount_in(8) + min_amount_out(8)
        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;
        assert!(data.len() >= 24);

        // min_amount_out at bytes [16..24]
        assert!(check_u64_le_at(data, 16, min_out),
            "min_amount_out {} not found at offset 16 in RaydiumCpmm instruction data", min_out);
        // Also verify amount_in at bytes [8..16]
        assert!(check_u64_le_at(data, 8, 1_000_000));
    }

    #[test]
    fn test_pumpfun_amm_sell_encodes_min_amount_out() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        // For a SELL, input_mint = base_mint, output_mint = quote_mint
        // Data layout: sell_disc(8) + amount_in(8) + min_quote_amount_out(8)
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let min_out: u64 = 12_345_678;

        let state = PoolState::PumpFunAmm {
            pool,
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000,
            quote_reserve: 5_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), true)],
            base_supply: 0,
            virtual_quote_reserve: 0,
        };

        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::PumpFunAmm,
            input_mint: base_mint,   // selling base → receiving quote
            output_mint: quote_mint,
            amount_in: 500_000,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::PumpFunAmm;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;

        // Sell layout: disc(8) + amount_in(8) + min_quote_amount_out(8)
        assert!(check_u64_le_at(data, 16, min_out),
            "min_amount_out {} not found at offset 16 in PumpFunAmm sell instruction data", min_out);
        assert!(check_u64_le_at(data, 8, 500_000));
    }

    #[test]
    fn test_pumpfun_amm_buy_is_exact_input_with_floor() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        // For a BUY, input_mint = quote_mint, output_mint = base_mint
        // Data layout: buy_disc(8) + base_amount_out(8) + max_quote_amount_in(8)
        // The max_quote_amount_in IS the amount_in (not min_amount_out)
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let amount_in: u64 = 1_000_000;

        let state = PoolState::PumpFunAmm {
            pool,
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000,
            quote_reserve: 5_000_000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), true)],
            base_supply: 0,
            virtual_quote_reserve: 0,
        };

        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::PumpFunAmm,
            input_mint: quote_mint,  // buying base with quote
            output_mint: base_mint,
            amount_in,
            min_amount_out: 900_000,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::PumpFunAmm;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;

        // Buy layout: disc(8) + base_amount_out(8) + max_quote_amount_in(8)
        // max_quote_amount_in = amount_in at bytes [16..24]
        // BuyExactQuoteIn: disc(8) + quote_amount_in(8) + min_base_amount_out(8) + track_volume(1)
        assert!(check_u64_le_at(data, 8, amount_in),
            "amount_in {} not found at offset 8 (BuyExactQuoteIn quote_amount_in)", amount_in);
        assert!(check_u64_le_at(data, 16, 900_000), "min_amount_out at offset 16");
    }

    #[test]
    fn test_meteora_damm_encodes_min_amount_out() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let min_out: u64 = 12_345_678;

        let state = PoolState::MeteoraDamm {
            pool,
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: mint_a,
            token_b_mint: mint_b,
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };

        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::MeteoraDamm,
            input_mint: mint_a,
            output_mint: mint_b,
            amount_in: 1_000_000,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::MeteoraDamm;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;
        assert!(data.len() >= 24);

        // Data: disc(8) + amount_in(8) + min_amount_out(8)
        assert!(check_u64_le_at(data, 16, min_out),
            "min_amount_out {} not found at offset 16 in MeteoraDamm instruction data", min_out);
        assert!(check_u64_le_at(data, 8, 1_000_000));
    }

    #[test]
    fn test_raydium_lp_buy_encodes_min_amount_out() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let min_out: u64 = 12_345_678;

        let state = PoolState::RaydiumLp {
            pool_state: pool,
            authority: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            config_id: Pubkey::new_unique(),
            platform_id: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
        };

        // Buy: input=quote, output=base
        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::RaydiumLp,
            input_mint: quote_mint,
            output_mint: base_mint,
            amount_in: 1_000_000,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::RaydiumLp;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;
        // Layout: disc(8) + amount_in(8) + min_amount_out(8) + share_fee_rate(8)
        assert!(data.len() >= 32);

        assert!(check_u64_le_at(data, 16, min_out),
            "min_amount_out {} not found at offset 16 in RaydiumLp buy instruction data", min_out);
        assert!(check_u64_le_at(data, 8, 1_000_000));
        // share_fee_rate should be 0
        assert!(check_u64_le_at(data, 24, 0));
    }

    #[test]
    fn test_raydium_lp_sell_encodes_min_amount_out() {
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let min_out: u64 = 87_654_321;

        let state = PoolState::RaydiumLp {
            pool_state: pool,
            authority: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            config_id: Pubkey::new_unique(),
            platform_id: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            curve: Default::default(),
        };

        // Sell: input=base, output=quote
        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::RaydiumLp,
            input_mint: base_mint,
            output_mint: quote_mint,
            amount_in: 2_000_000,
            min_amount_out: min_out,
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::RaydiumLp;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;

        // disc(8) + amount_in(8) + min_amount_out(8) + share_fee_rate(8)
        assert!(check_u64_le_at(data, 16, min_out),
            "min_amount_out {} not found at offset 16 in RaydiumLp sell instruction data", min_out);
        assert!(check_u64_le_at(data, 8, 2_000_000));
    }

    #[test]
    fn test_min_amount_out_zero_is_valid() {
        // Zero min_amount_out is valid (used for intermediate hops in multi-hop swaps)
        use crate::pool::types::{PoolState, PoolType, SwapOrder};
        use solana_sdk::pubkey::Pubkey;

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let pool = Pubkey::new_unique();

        let state = PoolState::RaydiumCpmm {
            pool,
            authority: Pubkey::new_unique(),
            config: Pubkey::new_unique(),
            token_0_vault: Pubkey::new_unique(),
            token_1_vault: Pubkey::new_unique(),
            token_0_mint: mint_a,
            token_1_mint: mint_b,
            observation: Pubkey::new_unique(),
            trade_fee_bps: 0,
            protocol_fees_0: 0,
            protocol_fees_1: 0,
            fund_fees_0: 0,
            fund_fees_1: 0,
            creator_fee_ppm: 0, enable_creator_fee: false, creator_fee_on: 0,
        };

        let order = SwapOrder {
            pool_address: pool,
            pool_type: PoolType::RaydiumCpmm,
            input_mint: mint_a,
            output_mint: mint_b,
            amount_in: 1_000_000,
            min_amount_out: 0, // intermediate hop — no slippage enforcement
            user: Pubkey::new_unique(),
            input_token_program: crate::constants::TOKEN_PROGRAM_ID,
            output_token_program: crate::constants::TOKEN_PROGRAM_ID,
        };

        let executor = AmmExecutorType::RaydiumCpmm;
        let ixs = executor.build_swap_ix(&order, &state).unwrap();

        assert_eq!(ixs.swap.len(), 1);
        let data = &ixs.swap[0].data;
        assert!(check_u64_le_at(data, 16, 0),
            "min_amount_out=0 not found at offset 16 in RaydiumCpmm instruction data");
    }

    #[test]
    fn test_find_u64_le_helper() {
        let data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
                        0x4E, 0x61, 0xBC, 0x00, 0x00, 0x00, 0x00, 0x00]; // 12345678 as u64 LE
        assert!(find_u64_le(&data, 12_345_678));
        assert!(!find_u64_le(&data, 12_345_679));
    }
}
