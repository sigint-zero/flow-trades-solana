use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::fmt;

/// Supported DEX pool types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PoolType {
    #[default]
    Unknown,
    RaydiumV4,
    RaydiumCpmm,
    RaydiumCl,
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
    /// Pumpup pre-graduation bonding curve (native SOL pair). Pool address =
    /// `pool_sol_account` PDA (`["pumpup.pool", mint]`). State stored inline
    /// as the BondingCurve struct.
    PumpupBonding,
}

impl PoolType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PoolType::Unknown => "UNKNOWN",
            PoolType::RaydiumV4 => "RAYDIUM_V4",
            PoolType::RaydiumCpmm => "RAYDIUM_CPMM",
            PoolType::RaydiumCl => "RAYDIUM_CL",
            PoolType::RaydiumLp => "RAYDIUM_LP",
            PoolType::PumpFun => "PUMP_FUN",
            PoolType::PumpFunAmm => "PUMP_FUN_AMM",
            PoolType::Meteora => "METEORA",
            PoolType::MeteoraDlmm => "METEORA_DLMM",
            PoolType::MeteoraDamm => "METEORA_DAMM",
            PoolType::MeteoraDbc => "METEORA_DBC",
            PoolType::Orca => "ORCA",

            PoolType::FluxBeam => "FLUXBEAM",
            PoolType::FlashTrade => "FLASH_TRADE",
            PoolType::Byreal => "BYREAL",
            PoolType::DefiTunaFusion => "DEFITUNA_FUSION",
            PoolType::DefiTunaPools => "DEFITUNA_POOLS",
            PoolType::Saros => "SAROS",
            PoolType::PancakeSwap => "PANCAKESWAP",
            PoolType::Dooar => "DOOAR",
            PoolType::Pumpup => "PUMPUP",
            PoolType::PumpupBonding => "PUMPUP_BONDING",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "UNKNOWN" => Some(PoolType::Unknown),
            "RAYDIUM_V4" => Some(PoolType::RaydiumV4),
            "RAYDIUM_CPMM" => Some(PoolType::RaydiumCpmm),
            "RAYDIUM_CL" => Some(PoolType::RaydiumCl),
            "RAYDIUM_LP" => Some(PoolType::RaydiumLp),
            "PUMP_FUN" => Some(PoolType::PumpFun),
            "PUMP_FUN_AMM" => Some(PoolType::PumpFunAmm),
            "METEORA" => Some(PoolType::Meteora),
            "METEORA_DLMM" => Some(PoolType::MeteoraDlmm),
            "METEORA_DAMM" => Some(PoolType::MeteoraDamm),
            "METEORA_DBC" => Some(PoolType::MeteoraDbc),
            "ORCA" => Some(PoolType::Orca),

            "FLUXBEAM" => Some(PoolType::FluxBeam),
            "FLASH_TRADE" => Some(PoolType::FlashTrade),
            "BYREAL" => Some(PoolType::Byreal),
            "DEFITUNA_FUSION" => Some(PoolType::DefiTunaFusion),
            "DEFITUNA_POOLS" => Some(PoolType::DefiTunaPools),
            "SAROS" => Some(PoolType::Saros),
            "PANCAKESWAP" => Some(PoolType::PancakeSwap),
            "DOOAR" => Some(PoolType::Dooar),
            "PUMPUP" => Some(PoolType::Pumpup),
            "PUMPUP_BONDING" => Some(PoolType::PumpupBonding),
            _ => None,
        }
    }
}

impl fmt::Display for PoolType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for PoolType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str_opt(s).ok_or_else(|| format!("Unknown pool type: {s}"))
    }
}

impl Serialize for PoolType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PoolType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        PoolType::from_str_opt(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("Unknown pool type: {s}")))
    }
}

/// On-chain pool state fetched before building instructions.
/// One variant per AMM -- contains all account addresses needed for the swap instruction.
/// SPL token-swap fee schedule (Dooar, Saros, FluxBeam forks): `trade` and
/// `owner_trade` fees are both taken from the input (floor, minimum 1 when the
/// numerator is non-zero), then x·y=k. `curve_type` 0 = constant product; other
/// curves (offset, stable) are not quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SplSwapFees {
    pub trade_num: u64,
    pub trade_den: u64,
    pub owner_num: u64,
    pub owner_den: u64,
    pub curve_type: u8,
}

impl SplSwapFees {
    /// Layout: fees at 227 (8×u64), curve_type at 291.
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 292 {
            return None;
        }
        let rd = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        Some(Self { trade_num: rd(227), trade_den: rd(235), owner_num: rd(243), owner_den: rd(251), curve_type: d[291] })
    }

    fn fee(amount: u64, num: u64, den: u64) -> Option<u64> {
        if num == 0 || den == 0 {
            return Some(0);
        }
        let f = u64::try_from((amount as u128 * num as u128) / den as u128).ok()?;
        Some(f.max(1))
    }

    /// Total fee taken from `amount_in`, or None if the schedule is unknown.
    pub fn total_fee(&self, amount_in: u64) -> Option<u64> {
        if self.trade_den == 0 && self.owner_den == 0 {
            return None;
        }
        Some(Self::fee(amount_in, self.trade_num, self.trade_den)? + Self::fee(amount_in, self.owner_num, self.owner_den)?)
    }
}

/// The pump.fun AMM pool facts its fee program keys on (`Pool` account:
/// `creator` @11, `is_mayhem_mode` @243, `is_cashback_coin` @244,
/// `creator_fee_bps` @261). The default is a canonical pool with no overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PammFlags {
    /// `pool.creator` is not the pump program's `["pool-authority", base_mint]`
    /// PDA, i.e. the pool was not created by a pump.fun graduation: it pays the
    /// flat schedule, not the market-cap tiers.
    pub non_canonical: bool,
    /// Mayhem-mode pool: market cap is taken on a fixed 1e15 supply.
    pub mayhem: bool,
    /// Cashback coin: the creator fee is credited to the trader's volume accumulator.
    pub cashback: bool,
    /// Per-pool creator fee rate; replaces the schedule's creator rate when
    /// non-zero and the global config allows it.
    pub creator_fee_bps: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PoolState {
    /// Raydium AMM v4 (`AmmInfo`, 752 bytes). The program prices swaps on
    /// `vault − need_take_pnl` without the OpenBook market, so the
    /// state is parsed from the pool account alone and swaps use
    /// `swap_base_in_v2`, which takes no market accounts.
    RaydiumV4 {
        amm_id: Pubkey,
        authority: Pubkey,
        coin_vault: Pubkey,
        pc_vault: Pubkey,
        coin_mint: Pubkey,
        pc_mint: Pubkey,
        /// `fees.swap_fee_numerator / swap_fee_denominator` (ceil, off the input).
        swap_fee_numerator: u64,
        swap_fee_denominator: u64,
        /// `state_data.need_take_pnl_{coin,pc}`: accrued protocol PnL sitting in
        /// the vaults but outside the curve.
        need_take_pnl_coin: u64,
        need_take_pnl_pc: u64,
        /// `AmmStatus` (1 Initialized, 6 SwapOnly, 7 WaitingTrade can swap).
        status: u64,
        pool_open_time: u64,
    },
    RaydiumCpmm {
        pool: Pubkey,
        authority: Pubkey,
        config: Pubkey,
        token_0_vault: Pubkey,
        token_1_vault: Pubkey,
        token_0_mint: Pubkey,
        token_1_mint: Pubkey,
        observation: Pubkey,
        /// Trade fee from the pool's `AmmConfig` (`trade_fee_rate` / 100; the
        /// program's tiers are 25, 30, 40, 50, 100, 400 bps). 0 = not read yet →
        /// the quoter falls back to an observed fee or the 25 bps default.
        trade_fee_bps: u16,
        /// Accrued protocol/fund fees sitting in the vaults but outside the
        /// curve (`PoolState` offsets 341/349/357/365).
        protocol_fees_0: u64,
        protocol_fees_1: u64,
        fund_fees_0: u64,
        fund_fees_1: u64,
        /// `AmmConfig.creator_fee_rate` (u64 @108, /1e6) — charged on top of the
        /// trade fee when `enable_creator_fee` (pool byte 389) is set.
        #[serde(default)]
        creator_fee_ppm: u32,
        #[serde(default)]
        enable_creator_fee: bool,
        /// 0 = fee taken from the INPUT token; 1 = only token 0; 2 = only token 1
        /// (input side when that token is the input, output side otherwise).
        #[serde(default)]
        creator_fee_on: u8,
    },
    RaydiumClmm {
        pool: Pubkey,
        amm_config: Pubkey,
        observation: Pubkey,
        token_vault_0: Pubkey,
        token_vault_1: Pubkey,
        tick_array_0: Pubkey,
        tick_array_1: Pubkey,
        tick_array_2: Pubkey,
        token_mint_0: Pubkey,
        token_mint_1: Pubkey,
        tick_current: i32,
        tick_spacing: i32,
        /// Current sqrt price as Q64.64 fixed-point (u128)
        sqrt_price_x64: u128,
        /// Current tick range liquidity (u128)
        liquidity: u128,
        /// Fee rate in hundredths of a basis point (e.g., 2500 = 25 bps)
        fee_rate: u16,
        /// Fee side (`fee_on`) and dynamic fee of newer pools.
        #[serde(default)]
        fee_ext: crate::quote::clmm::RaydiumFeeExt,
    },
    RaydiumLp {
        pool_state: Pubkey,
        authority: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        config_id: Pubkey,
        platform_id: Pubkey,
        creator: Pubkey,
        /// Bonding curve + fee rates (GlobalConfig / PlatformConfig).
        #[serde(default)]
        curve: crate::quote::launchlab::LaunchLabCurve,
    },
    PumpFun {
        global: Pubkey,
        fee_account: Pubkey,
        mint: Pubkey,
        bonding_curve: Pubkey,
        associated_bonding_curve: Pubkey,
        event_authority: Pubkey,
        /// Creator pubkey from bonding curve data (offset +49). Used for creator_vault PDA.
        creator: Pubkey,
        /// Reserves and flags of the curve (`quote::pump_bonding`), re-read with
        /// the account.
        #[serde(default)]
        curve: crate::quote::pump_bonding::PumpCurve,
        /// One of `Global.buyback_fee_recipients`: a writable remaining account
        /// every buy/sell must carry (after the `bonding-curve-v2` PDA).
        #[serde(default)]
        buyback_fee_recipient: Pubkey,
    },
    PumpFunAmm {
        pool: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        pool_base_vault: Pubkey,
        pool_quote_vault: Pubkey,
        coin_creator: Pubkey,
        /// Token balance in pool_base_vault (for swap amount computation)
        base_reserve: u64,
        /// Token balance in pool_quote_vault (for swap amount computation)
        quote_reserve: u64,
        /// Currently valid `protocol_fee_recipient` (account[9]; pump.fun rotates
        /// among several). Resolved from a recent on-chain swap on this pool.
        /// `Pubkey::default()` = unresolved → the executor falls back to the
        /// static constant.
        protocol_fee_recipient: Pubkey,
        /// The pump_fees "buyback" remaining accounts (mid-2026 update): every
        /// account AFTER the fee program in a recent swap on this pool, with its
        /// on-chain writability. Per-pool/creator buyback vault(s) + ATAs, count
        /// varies (2–3+), some Token-2022 — NOT derivable PDAs, so they are copied
        /// verbatim from chain (`fetcher::resolve_pamm_fee_accounts`). The program
        /// rejects a swap without them (error 6058) or with an incomplete set
        /// (6023). Empty = unresolved; the executor refuses to build.
        ///
        /// The Geyser account stream re-parses this pool from raw bytes on every
        /// update, which cannot see these — `carry_over_pamm_fee_accounts` keeps
        /// the resolved set across those refreshes.
        buyback_accounts: Vec<(Pubkey, bool)>,
        /// Total supply of `base_mint` (atoms), read once per pool. pump.fun's
        /// swap fee is a market-cap tier (`quote_reserve × supply / base_reserve`),
        /// so the quote engine needs it; 0 = unknown → the most expensive tier is
        /// assumed (a conservative quote, never a spurious revert).
        base_supply: u64,
        /// Virtual quote reserve (i128 at pool offset 245, mid-2026 update): the
        /// curve prices on `quote_reserve + virtual_quote_reserve`, not on the
        /// vault balance alone (verified byte-exact on live Buy/Sell events).
        #[serde(default)]
        virtual_quote_reserve: u64,
        /// Pool facts that select the fee schedule (see `PammFlags`).
        #[serde(default)]
        pamm_flags: PammFlags,
    },
    Meteora {
        pool: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        a_vault: Pubkey,
        b_vault: Pubkey,
        a_token_vault: Pubkey,
        b_token_vault: Pubkey,
        a_vault_lp_mint: Pubkey,
        b_vault_lp_mint: Pubkey,
        a_vault_lp: Pubkey,
        b_vault_lp: Pubkey,
        admin_token_a_fee: Pubkey,
        admin_token_b_fee: Pubkey,
        vault_program: Pubkey,
        /// Pool's share of each dynamic vault + fee (see `quote::meteora_std`).
        #[serde(default)]
        reserves: crate::quote::meteora_std::MeteoraStdReserves,
    },
    MeteoraDlmm {
        lb_pair: Pubkey,
        bin_array_bitmap_extension: Pubkey,
        reserve_x: Pubkey,
        reserve_y: Pubkey,
        token_x_mint: Pubkey,
        token_y_mint: Pubkey,
        oracle: Pubkey,
        host_fee_in: Pubkey,
        event_authority: Pubkey,
        /// Bin array PDAs derived from active_id
        bin_arrays: Vec<Pubkey>,
        /// Fee parameters, active bin and liquidity bitmap (see `quote::dlmm`).
        #[serde(default)]
        pair: crate::quote::dlmm::DlmmPair,
    },
    MeteoraDamm {
        pool: Pubkey,
        token_a_vault: Pubkey,
        token_b_vault: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        /// Curve: liquidity is stored ×2^64; prices are Q64.64 sqrt prices.
        #[serde(default)]
        liquidity: u128,
        #[serde(default)]
        sqrt_price: u128,
        #[serde(default)]
        sqrt_min_price: u128,
        #[serde(default)]
        sqrt_max_price: u128,
        /// Reserves the pool tracks itself (layout v1); the curve of a
        /// compounding pool (`collect_fee_mode` 2).
        #[serde(default)]
        token_a_amount: u64,
        #[serde(default)]
        token_b_amount: u64,
        #[serde(default)]
        fees: crate::quote::damm_v2::DammFees,
        #[serde(default)]
        activation_point: u64,
        /// 0 = slot, 1 = unix timestamp
        #[serde(default)]
        activation_type: u8,
        #[serde(default)]
        collect_fee_mode: u8,
        #[serde(default)]
        pool_status: u8,
    },
    MeteoraDbc {
        pool: Pubkey,
        config: Pubkey,
        pool_authority: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        /// Price, reserves and the config's curve + fees (`quote::dbc`).
        #[serde(default)]
        curve: crate::quote::dbc::DbcCurve,
    },
    Orca {
        whirlpool: Pubkey,
        token_vault_a: Pubkey,
        token_vault_b: Pubkey,
        oracle: Pubkey,
        token_mint_a: Pubkey,
        token_mint_b: Pubkey,
        tick_current: i32,
        tick_spacing: i32,
        /// Current sqrt price as Q64.64 fixed-point (u128)
        sqrt_price_x64: u128,
        /// Current tick range liquidity (u128)
        liquidity: u128,
        /// Fee rate in hundredths of a basis point (e.g., 2500 = 25 bps)
        fee_rate: u16,
    },
    FluxBeam {
        pool: Pubkey,
        authority: Pubkey,
        token_a_vault: Pubkey,
        token_b_vault: Pubkey,
        pool_mint: Pubkey,
        fee_account: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        pool_token_program: Pubkey,
        #[serde(default)]
        fees: SplSwapFees,
    },
    FlashTrade {
        pool: Pubkey,
        oracle: Pubkey,
        custody: Pubkey,
        token_mint: Pubkey,
    },
    /// Byreal CLMM: a Raydium CLMM fork (same pool / AmmConfig / tick-array
    /// layouts and PDAs, own program id).
    Byreal {
        pool: Pubkey,
        amm_config: Pubkey,
        token_vault_a: Pubkey,
        token_vault_b: Pubkey,
        observation: Pubkey,
        token_mint_a: Pubkey,
        token_mint_b: Pubkey,
        tick_current: i32,
        tick_spacing: i32,
        /// Current sqrt price as Q64.64 fixed-point (u128)
        sqrt_price_x64: u128,
        /// Current tick range liquidity (u128)
        liquidity: u128,
        /// `AmmConfig.trade_fee_rate`, hundredths of a basis point
        fee_rate: u16,
        /// Byreal's pool-level fee fields (rate override, launch decay fee,
        /// dynamic-fee flag) — see `quote::byreal_fee`.
        #[serde(default)]
        fee: crate::quote::byreal_fee::ByrealFee,
    },
    DefiTunaFusion {
        pool: Pubkey,
        token_vault_a: Pubkey,
        token_vault_b: Pubkey,
        token_mint_a: Pubkey,
        token_mint_b: Pubkey,
        tick_spacing: u16,
        tick_current_index: i32,
        /// Current sqrt price as Q64.64 fixed-point (u128)
        sqrt_price_x64: u128,
        /// Current tick range liquidity (u128)
        liquidity: u128,
        /// Fee rate in hundredths of a basis point
        fee_rate: u16,
    },
    DefiTunaPools {
        pool: Pubkey,
        token_vault_a: Pubkey,
        token_vault_b: Pubkey,
        token_mint_a: Pubkey,
        token_mint_b: Pubkey,
    },
    Saros {
        pool: Pubkey,
        authority: Pubkey,
        token_a_vault: Pubkey,
        token_b_vault: Pubkey,
        pool_mint: Pubkey,
        fee_account: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        #[serde(default)]
        fees: SplSwapFees,
    },
    PancakeSwap {
        pool: Pubkey,
        amm_config: Pubkey,
        token_vault_a: Pubkey,
        token_vault_b: Pubkey,
        observation: Pubkey,
        token_mint_a: Pubkey,
        token_mint_b: Pubkey,
        tick_current: i32,
        tick_spacing: i32,
        /// Current sqrt price as Q64.64 fixed-point (u128)
        sqrt_price_x64: u128,
        /// Current tick range liquidity (u128)
        liquidity: u128,
        /// Fee rate in hundredths of a basis point
        fee_rate: u16,
    },
    Dooar {
        pool: Pubkey,
        authority: Pubkey,
        token_a_vault: Pubkey,
        token_b_vault: Pubkey,
        pool_mint: Pubkey,
        fee_account: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        #[serde(default)]
        fees: SplSwapFees,
    },
    /// Pumpup post-graduation AMM pool. Layout sourced from on-chain Anchor IDL
    /// at `BzBmXJiz9H88PAZomvWn8UvmdmeucWZg7N1cygN5po61`. Reserves are stored
    /// inline in the Pool account — no vault RPC fetch required for quoting.
    Pumpup {
        pool: Pubkey,
        token_a_mint: Pubkey,
        token_b_mint: Pubkey,
        token_a_vault: Pubkey,
        token_b_vault: Pubkey,
        fee_recipient: Pubkey,
        fee_recipient2: Pubkey,
        token_a_reserve: u64,
        token_b_reserve: u64,
    },
    /// Pumpup pre-graduation bonding curve. The BondingCurve struct lives
    /// inside `pool_sol_account` (PDA `["pumpup.pool", mint]`) — that account
    /// holds both lamports (real SOL) and the curve data. `pool` is therefore
    /// `pool_sol_account` itself. Native SOL pair (no WSOL).
    ///
    /// Quoting uses constant-product math against `(virtual_sol + real_sol,
    /// pool_token_reserves)` — matches the PumpFun bonding pattern.
    PumpupBonding {
        /// `pool_sol_account` — also the pool address.
        pool: Pubkey,
        /// Token mint paired against SOL.
        mint: Pubkey,
        /// ATA(pool_sol_account, mint, TOKEN_PROGRAM_ID) — token vault.
        pool_token_account: Pubkey,
        /// pumpup_fee recipient — read from PumpupConfiguration singleton.
        pumpup_fee: Pubkey,
        /// Virtual SOL reserve for constant-product math.
        virtual_sol: u64,
        /// Real SOL collected (subset of pool_sol_reserves).
        real_sol: u64,
        /// Live SOL reserve in pool_sol_account.
        pool_sol_reserves: u64,
        /// Live token reserve in pool_token_account.
        pool_token_reserves: u64,
    },
}

/// What the caller submits to execute a swap.
#[derive(Debug, Clone)]
pub struct SwapOrder {
    pub pool_address: Pubkey,
    pub pool_type: PoolType,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub user: Pubkey,
    /// Token program owning input_mint (Token or Token-2022).
    pub input_token_program: Pubkey,
    /// Token program owning output_mint (Token or Token-2022).
    pub output_token_program: Pubkey,
}

/// Instructions built by an AMM executor, ready for transaction assembly.
#[derive(Debug)]
pub struct SwapInstructions {
    pub setup: Vec<Instruction>,
    pub swap: Vec<Instruction>,
    pub cleanup: Vec<Instruction>,
}


impl PoolState {
    /// Preserve the pump.fun AMM fee/buyback accounts resolved on `prev` when
    /// `self` is a fresh re-parse of the same pool from raw account bytes (which
    /// carry no such information). No-op for every other variant, and when
    /// `self` already has a resolved set.
    pub fn carry_over_pamm_fee_accounts(&mut self, prev: &PoolState) {
        if let (
            PoolState::PumpFunAmm {
                protocol_fee_recipient,
                buyback_accounts,
                base_supply,
                ..
            },
            PoolState::PumpFunAmm {
                protocol_fee_recipient: prev_recipient,
                buyback_accounts: prev_buyback,
                base_supply: prev_supply,
                ..
            },
        ) = (self, prev)
        {
            if buyback_accounts.is_empty() && !prev_buyback.is_empty() {
                *buyback_accounts = prev_buyback.clone();
            }
            if *protocol_fee_recipient == Pubkey::default() {
                *protocol_fee_recipient = *prev_recipient;
            }
            if *base_supply == 0 {
                *base_supply = *prev_supply;
            }
        }
    }

    /// True for a pump.fun AMM pool whose buyback remaining-accounts have not
    /// been resolved yet (a swap cannot be built until they are).
    /// Copy of a pAMM state with fresher vault balances (mirror) for pricing.
    /// Clones the small buyback-account Vec (2–4 entries) — the exact fee
    /// model needs the whole state (tier from supply/creator/virtual reserve).
    pub fn clone_shallow_pamm(&self, base_reserve: u64, quote_reserve: u64) -> PoolState {
        let mut st = self.clone();
        if let PoolState::PumpFunAmm { base_reserve: b, quote_reserve: q, .. } = &mut st {
            *b = base_reserve;
            *q = quote_reserve;
        }
        st
    }

    pub fn needs_pamm_fee_accounts(&self) -> bool {
        matches!(self, PoolState::PumpFunAmm { buyback_accounts, .. } if buyback_accounts.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_type_display() {
        assert_eq!(PoolType::RaydiumV4.as_str(), "RAYDIUM_V4");
        assert_eq!(PoolType::PumpFun.as_str(), "PUMP_FUN");
        assert_eq!(PoolType::MeteoraDlmm.as_str(), "METEORA_DLMM");
        assert_eq!(PoolType::Unknown.as_str(), "UNKNOWN");
    }

    #[test]
    fn test_pool_type_default() {
        let pt: PoolType = Default::default();
        assert_eq!(pt, PoolType::Unknown);
    }

    #[test]
    fn test_pool_type_from_str_roundtrip() {
        let variants = [
            PoolType::Unknown,
            PoolType::RaydiumV4,
            PoolType::RaydiumCpmm,
            PoolType::RaydiumCl,
            PoolType::RaydiumLp,
            PoolType::PumpFun,
            PoolType::PumpFunAmm,
            PoolType::Meteora,
            PoolType::MeteoraDlmm,
            PoolType::MeteoraDamm,
            PoolType::MeteoraDbc,
            PoolType::Orca,

            PoolType::FluxBeam,
            PoolType::FlashTrade,
            PoolType::Byreal,
            PoolType::DefiTunaFusion,
            PoolType::DefiTunaPools,
            PoolType::Saros,
            PoolType::PancakeSwap,
            PoolType::Dooar,
            PoolType::Pumpup,
            PoolType::PumpupBonding,
        ];
        for variant in &variants {
            let s = variant.as_str();
            let parsed: PoolType = s.parse().unwrap();
            assert_eq!(*variant, parsed, "round-trip failed for {s}");
        }
    }

    #[test]
    fn test_pool_type_from_str_invalid() {
        let result: Result<PoolType, _> = "INVALID_POOL".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_pool_type_serialize_roundtrip() {
        let variants = [
            PoolType::RaydiumV4,
            PoolType::PumpFun,
            PoolType::Orca,
            PoolType::MeteoraDlmm,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: PoolType = serde_json::from_str(&json).unwrap();
            assert_eq!(*variant, parsed);
        }
    }

    #[test]
    fn test_pool_type_deserialize_invalid() {
        let result: Result<PoolType, _> = serde_json::from_str("\"NOT_A_POOL\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_pool_type_all_variants_count() {
        // 22 variants total (including Unknown, Pumpup, PumpupBonding)
        let variants = [
            PoolType::Unknown,
            PoolType::RaydiumV4,
            PoolType::RaydiumCpmm,
            PoolType::RaydiumCl,
            PoolType::RaydiumLp,
            PoolType::PumpFun,
            PoolType::PumpFunAmm,
            PoolType::Meteora,
            PoolType::MeteoraDlmm,
            PoolType::MeteoraDamm,
            PoolType::MeteoraDbc,
            PoolType::Orca,

            PoolType::FluxBeam,
            PoolType::FlashTrade,
            PoolType::Byreal,
            PoolType::DefiTunaFusion,
            PoolType::DefiTunaPools,
            PoolType::Saros,
            PoolType::PancakeSwap,
            PoolType::Dooar,
            PoolType::Pumpup,
            PoolType::PumpupBonding,
        ];
        assert_eq!(variants.len(), 22);
        // Verify all have unique string representations
        let strs: std::collections::HashSet<&str> = variants.iter().map(|v| v.as_str()).collect();
        assert_eq!(strs.len(), 22);
    }

    #[test]
    fn test_pool_type_display_matches_as_str() {
        let variants = [
            PoolType::RaydiumCpmm,
            PoolType::Orca,
            PoolType::PumpFunAmm,
        ];
        for v in &variants {
            assert_eq!(format!("{v}"), v.as_str());
        }
    }

    #[test]
    fn test_pool_state_json_roundtrip() {
        // Test a simple variant
        let state = PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: PoolState = serde_json::from_str(&json).unwrap();
        // Compare by re-serializing (PoolState doesn't derive PartialEq)
        let json2 = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn test_pool_state_json_roundtrip_with_vec() {
        // Test a variant with Vec<Pubkey> (MeteoraDlmm.bin_arrays)
        let state = PoolState::MeteoraDlmm {
            lb_pair: Pubkey::new_unique(),
            bin_array_bitmap_extension: Pubkey::new_unique(),
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            token_x_mint: Pubkey::new_unique(),
            token_y_mint: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            host_fee_in: Pubkey::new_unique(),
            event_authority: Pubkey::new_unique(),
            bin_arrays: vec![Pubkey::new_unique(), Pubkey::new_unique()],
            pair: Default::default(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: PoolState = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn test_pool_state_json_roundtrip_with_numerics() {
        // Test a variant with u64, i32, u16 fields
        let state = PoolState::RaydiumClmm {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            tick_array_0: Pubkey::new_unique(),
            tick_array_1: Pubkey::new_unique(),
            tick_array_2: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            tick_current: -42,
            tick_spacing: 10,
            sqrt_price_x64: 1u128 << 64,
            liquidity: 1_000_000,
            fee_rate: 25,
            fee_ext: Default::default(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: PoolState = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, json2);
    }
}
