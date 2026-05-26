use std::sync::LazyLock;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::constants::*;
use crate::error::{TradeError, TradeResult};
use super::types::{PoolState, PoolType};
use super::layouts::{
    RaydiumV4Pool, SerumMarketAccounts,
    RaydiumCpmmPool, RaydiumClmmPool, RaydiumLpPool,
    PumpFunBondingCurve, PumpFunGlobal, PumpFunAmmPool,
    MeteoraPool, MeteoraVault, MeteoraDlmmLbPair, MeteoraDammPool,
    MeteoraDbcVirtualPool, MeteoraDbcConfig,
    OrcaWhirlpool, SplTokenSwapPool, FlashTradePool,
    DefiTunaFusionPool, DefiTunaPoolsPool, PancakeSwapPool,
    PumpupPool, PumpupBondingCurve, PumpupConfig,
};

// Pre-computed global PDAs -- `find_program_address` does elliptic curve math,
// so computing once avoids ~10-50us per pool fetch for these fixed seeds.
static RAYDIUM_V4_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"amm authority"], &RAYDIUM_V4_PROG_ID).0
});
static RAYDIUM_CPMM_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"vault_and_lp_mint_auth_seed"], &RAYDIUM_CPMM_PROG_ID).0
});
static RAYDIUM_LP_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"vault_auth_seed"], &RAYDIUM_LP_PROG_ID).0
});
static METEORA_DLMM_EVENT_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"__event_authority"], &METEORA_DLMM_PROG_ID).0
});
static METEORA_DBC_POOL_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"pool_authority"], &METEORA_DBC_PROG_ID).0
});

// Pre-parsed constant pubkeys for PumpFun / Meteora (avoids base58 decode per call).
static PUMPFUN_GLOBAL: LazyLock<Pubkey> = LazyLock::new(|| pubkey_from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf"));
static PUMPFUN_FEE_FALLBACK: LazyLock<Pubkey> = LazyLock::new(|| pubkey_from_str("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"));
static PUMPFUN_EVENT_AUTHORITY: LazyLock<Pubkey> = LazyLock::new(|| pubkey_from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"));
static METEORA_VAULT_PROGRAM: LazyLock<Pubkey> = LazyLock::new(|| pubkey_from_str("24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi"));

/// Fetch on-chain pool state for a given AMM, deserializing all accounts needed for the swap instruction.
pub async fn fetch_pool_state(
    rpc: &RpcClient,
    pool_type: PoolType,
    pool_address: &Pubkey,
) -> TradeResult<PoolState> {
    debug!(?pool_type, %pool_address, "fetching pool state");

    let pool_data = fetch_account(rpc, pool_address).await?;

    match pool_type {
        PoolType::RaydiumV4 => parse_raydium_v4(rpc, pool_address, &pool_data).await,
        PoolType::RaydiumCpmm => parse_raydium_cpmm(pool_address, &pool_data),
        PoolType::RaydiumCl => parse_raydium_clmm(pool_address, &pool_data),
        PoolType::RaydiumLp => parse_raydium_lp(pool_address, &pool_data),
        PoolType::PumpFun => parse_pumpfun(rpc, pool_address, &pool_data).await,
        PoolType::PumpFunAmm => parse_pumpfun_amm(rpc, pool_address, &pool_data).await,
        PoolType::Meteora => parse_meteora(rpc, pool_address, &pool_data).await,
        PoolType::MeteoraDlmm => parse_meteora_dlmm(pool_address, &pool_data),
        PoolType::MeteoraDamm => parse_meteora_damm(pool_address, &pool_data),
        PoolType::MeteoraDbc => parse_meteora_dbc(rpc, pool_address, &pool_data).await,
        PoolType::Orca => parse_orca(pool_address, &pool_data),
        PoolType::FluxBeam => parse_fluxbeam(pool_address, &pool_data),
        PoolType::FlashTrade => parse_flash_trade(pool_address, &pool_data),
        PoolType::Byreal => parse_byreal(pool_address, &pool_data),
        PoolType::DefiTunaFusion => parse_defituna_fusion(pool_address, &pool_data),
        PoolType::DefiTunaPools => parse_defituna_pools(pool_address, &pool_data),
        PoolType::Saros => parse_saros(pool_address, &pool_data),
        PoolType::PancakeSwap => parse_pancakeswap(pool_address, &pool_data),
        PoolType::Dooar => parse_dooar(pool_address, &pool_data),
        PoolType::Pumpup => parse_pumpup(pool_address, &pool_data),
        PoolType::PumpupBonding => parse_pumpup_bonding(rpc, pool_address, &pool_data).await,
        _ => Err(TradeError::Execution(format!(
            "unsupported pool type for fetching: {pool_type:?}"
        ))),
    }
}

/// Parse pool state directly from raw account bytes — zero RPC.
/// Returns Ok(state) for the 14 sync pool types that only need the pool account data.
/// Returns Err for the 5 async pool types that need additional RPC calls (Raydium V4,
/// PumpFun bonding, PumpFun AMM, Meteora Standard, Meteora DBC).
///
/// Used by Geyser account updates to parse inline from notification payloads.
pub fn parse_pool_state_from_bytes(
    pool_type: PoolType,
    pool_address: &Pubkey,
    data: &[u8],
    owner: &Pubkey,
) -> TradeResult<PoolState> {
    // Build a minimal Account struct from raw bytes
    let account = Account {
        lamports: 0,
        data: data.to_vec(),
        owner: *owner,
        executable: false,
        rent_epoch: 0,
    };

    match pool_type {
        // Sync parsers — work from pool data alone (zero RPC)
        PoolType::RaydiumCpmm => parse_raydium_cpmm(pool_address, &account),
        PoolType::RaydiumCl => parse_raydium_clmm(pool_address, &account),
        PoolType::RaydiumLp => parse_raydium_lp(pool_address, &account),
        PoolType::MeteoraDlmm => parse_meteora_dlmm(pool_address, &account),
        PoolType::MeteoraDamm => parse_meteora_damm(pool_address, &account),
        PoolType::Orca => parse_orca(pool_address, &account),
        PoolType::FluxBeam => parse_fluxbeam(pool_address, &account),
        PoolType::FlashTrade => parse_flash_trade(pool_address, &account),
        PoolType::Byreal => parse_byreal(pool_address, &account),
        PoolType::DefiTunaFusion => parse_defituna_fusion(pool_address, &account),
        PoolType::DefiTunaPools => parse_defituna_pools(pool_address, &account),
        PoolType::Saros => parse_saros(pool_address, &account),
        PoolType::PancakeSwap => parse_pancakeswap(pool_address, &account),
        PoolType::Dooar => parse_dooar(pool_address, &account),
        PoolType::Pumpup => parse_pumpup(pool_address, &account),
        // Async parsers — need additional RPC calls, caller must use fetch_pool_state() instead
        _ => Err(TradeError::Execution(format!(
            "pool type {:?} requires RPC (async parse)", pool_type
        ))),
    }
}

/// Returns true if this pool type can be parsed from raw bytes without RPC.
pub fn is_sync_parseable(pool_type: PoolType) -> bool {
    matches!(pool_type,
        PoolType::RaydiumCpmm | PoolType::RaydiumCl | PoolType::RaydiumLp |
        PoolType::MeteoraDlmm | PoolType::MeteoraDamm | PoolType::Orca |
        PoolType::FluxBeam | PoolType::FlashTrade | PoolType::Byreal |
        PoolType::DefiTunaFusion | PoolType::DefiTunaPools | PoolType::Saros |
        PoolType::PancakeSwap | PoolType::Dooar
    )
}

pub async fn fetch_account(rpc: &RpcClient, address: &Pubkey) -> TradeResult<Account> {
    rpc.get_account(address)
        .await
        .map_err(|e| TradeError::Execution(format!("fetch account {address}: {e}")))
}

/// Determine which token program owns a mint (Token or Token-2022).
/// Returns the mint account's owner program ID.
pub async fn get_mint_token_program(rpc: &RpcClient, mint: &Pubkey) -> TradeResult<Pubkey> {
    // SOL native mint is always Token Program
    if *mint == SOL_NATIVE_MINT {
        return Ok(TOKEN_PROGRAM_ID);
    }
    let account = fetch_account(rpc, mint).await?;
    if account.owner == TOKEN_PROGRAM_ID || account.owner == TOKEN_2022_PROGRAM_ID {
        Ok(account.owner)
    } else {
        Err(TradeError::Execution(format!(
            "mint {mint} owned by unexpected program {}",
            account.owner,
        )))
    }
}

// Thin wrapper so the test module (which does `use super::*`) can call
// read_pubkey without importing from layouts.
#[cfg(test)]
fn read_pubkey(data: &[u8], offset: usize) -> TradeResult<Pubkey> {
    super::layouts::read_pubkey(data, offset)
}

// -- Raydium V4 --
async fn parse_raydium_v4(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    let pool = RaydiumV4Pool::try_from_bytes(&pool_data.data)?;
    let market_data = fetch_account(rpc, &pool.serum_market).await?;
    let serum = SerumMarketAccounts::parse(&market_data.data, pool.serum_market, &market_data.owner)?;

    Ok(PoolState::RaydiumV4 {
        amm_id: *pool_address,
        authority: *RAYDIUM_V4_AUTHORITY,
        open_orders: pool.open_orders,
        target_orders: pool.target_orders,
        coin_vault: pool.coin_vault,
        pc_vault: pool.pc_vault,
        serum_program: pool.serum_program,
        serum_market: pool.serum_market,
        serum_bids: serum.bids,
        serum_asks: serum.asks,
        serum_event_queue: serum.event_queue,
        serum_coin_vault: serum.coin_vault,
        serum_pc_vault: serum.pc_vault,
        serum_vault_signer: serum.vault_signer,
    })
}

// -- Raydium CPMM --
fn parse_raydium_cpmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = RaydiumCpmmPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::RaydiumCpmm {
        pool: *pool_address,
        authority: *RAYDIUM_CPMM_AUTHORITY,
        config: pool.amm_config,
        token_0_vault: pool.token_0_vault,
        token_1_vault: pool.token_1_vault,
        token_0_mint: pool.token_0_mint,
        token_1_mint: pool.token_1_mint,
        observation: pool.observation_key,
    })
}

// -- Raydium CLMM --
fn parse_raydium_clmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = RaydiumClmmPool::try_from_bytes(&pool_data.data)?;

    let tick_array_0 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, pool.tick_current, pool.tick_spacing, 0);
    let tick_array_1 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, pool.tick_current, pool.tick_spacing, -1);
    let tick_array_2 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, pool.tick_current, pool.tick_spacing, 1);

    // Fee rate lives in amm_config; default 25 bps (common values: 100/2500/10000 hundredths-of-bp).
    Ok(PoolState::RaydiumClmm {
        pool: *pool_address,
        amm_config: pool.amm_config,
        observation: pool.observation,
        token_vault_0: pool.token_vault_0,
        token_vault_1: pool.token_vault_1,
        tick_array_0,
        tick_array_1,
        tick_array_2,
        token_mint_0: pool.token_mint_0,
        token_mint_1: pool.token_mint_1,
        tick_current: pool.tick_current,
        tick_spacing: pool.tick_spacing,
        sqrt_price_x64: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
        fee_rate: 25,
    })
}

/// Derive a tick array PDA for Raydium CLMM pools.
/// NOTE: Raydium CLMM uses 60 ticks per array and big-endian bytes in PDA seeds.
fn derive_tick_array(
    program_id: &Pubkey,
    pool: &Pubkey,
    tick_current: i32,
    tick_spacing: i32,
    offset: i32,
) -> Pubkey {
    let ticks_per_array = 60 * tick_spacing; // Raydium CLMM: 60 ticks per array
    let start_index = if ticks_per_array == 0 {
        0
    } else {
        let array_idx = tick_current.div_euclid(ticks_per_array) + offset;
        array_idx * ticks_per_array
    };
    let start_bytes = start_index.to_be_bytes();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start_bytes],
        program_id,
    );
    pda
}

// -- Raydium LaunchPad (LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj) --
fn parse_raydium_lp(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = RaydiumLpPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::RaydiumLp {
        pool_state: *pool_address,
        authority: *RAYDIUM_LP_AUTHORITY,
        base_vault: pool.base_vault,
        quote_vault: pool.quote_vault,
        base_mint: pool.base_mint,
        quote_mint: pool.quote_mint,
        config_id: pool.config_id,
        platform_id: pool.platform_id,
        creator: pool.creator,
    })
}

// -- PumpFun (Bonding Curve) --
// Bonding curve layout V2: discriminator(8), reserves(5xu64=40), complete(1), creator(32), ...
// NOTE: mint is NOT stored in bonding curve data. We discover it by scanning
// the bonding curve's token accounts via getTokenAccountsByOwner.
async fn parse_pumpfun(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    use solana_client::rpc_request::TokenAccountsFilter;

    // Find the bonding curve's token account(s) to discover the mint (parallel fetch)
    let (token_accounts, token_2022_accounts) = tokio::try_join!(
        async {
            rpc.get_token_accounts_by_owner(
                pool_address,
                TokenAccountsFilter::ProgramId(TOKEN_PROGRAM_ID),
            ).await.map_err(|e| TradeError::Execution(format!("pumpfun: failed to fetch token accounts: {e}")))
        },
        async {
            rpc.get_token_accounts_by_owner(
                pool_address,
                TokenAccountsFilter::ProgramId(TOKEN_2022_PROGRAM_ID),
            ).await.map_err(|e| TradeError::Execution(format!("pumpfun: failed to fetch token-2022 accounts: {e}")))
        },
    )?;

    let all_accounts: Vec<_> = token_accounts.into_iter().chain(token_2022_accounts).collect();

    if all_accounts.is_empty() {
        return Err(TradeError::Execution(
            "pumpfun: no token accounts found for bonding curve".into(),
        ));
    }

    // The first (and usually only) token account holds the bonding curve's tokens
    let ta = &all_accounts[0];
    let mint = if let solana_account_decoder::UiAccountData::Json(parsed) = &ta.account.data {
        let mint_str = parsed.parsed["info"]["mint"].as_str().unwrap_or_default();
        mint_str.parse::<Pubkey>().map_err(|e| TradeError::Execution(format!(
            "pumpfun: failed to parse mint: {e}"
        )))?
    } else {
        return Err(TradeError::Execution("pumpfun: unexpected account data format".into()));
    };

    let curve = PumpFunBondingCurve::try_from_bytes(&pool_data.data);

    // Fetch mint account (for token program) and global config in parallel
    let global = *PUMPFUN_GLOBAL;
    let (mint_account, global_data) = tokio::try_join!(
        fetch_account(rpc, &mint),
        fetch_account(rpc, &global),
    )?;
    let token_prog = mint_account.owner;
    let associated_bonding_curve = spl_associated_token_account::get_associated_token_address_with_program_id(
        pool_address,
        &mint,
        &token_prog,
    );
    let fee_account = PumpFunGlobal::try_from_bytes(&global_data.data)
        .map(|g| g.fee_recipient)
        .unwrap_or(*PUMPFUN_FEE_FALLBACK);
    let event_authority = *PUMPFUN_EVENT_AUTHORITY;

    Ok(PoolState::PumpFun {
        global,
        fee_account,
        mint,
        bonding_curve: *pool_address,
        associated_bonding_curve,
        event_authority,
        creator: curve.creator,
    })
}

/// Parse PumpFun AMM pool layout (sync, no RPC needed).
fn parse_pumpfun_amm_layout(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let p = PumpFunAmmPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::PumpFunAmm {
        pool: *pool_address,
        base_mint: p.base_mint,
        quote_mint: p.quote_mint,
        pool_base_vault: p.pool_base_vault,
        pool_quote_vault: p.pool_quote_vault,
        coin_creator: p.coin_creator,
        base_reserve: 0,
        quote_reserve: 0,
    })
}

/// Parse PumpFun AMM pool and fetch vault reserves for swap computation.
async fn parse_pumpfun_amm(rpc: &RpcClient, pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let mut state = parse_pumpfun_amm_layout(pool_address, pool_data)?;
    // Enrich with vault token balances
    if let PoolState::PumpFunAmm {
        ref pool_base_vault, ref pool_quote_vault,
        ref mut base_reserve, ref mut quote_reserve, ..
    } = state {
        if let Ok((b, q)) = tokio::try_join!(
            fetch_token_balance(rpc, pool_base_vault),
            fetch_token_balance(rpc, pool_quote_vault),
        ) {
            *base_reserve = b;
            *quote_reserve = q;
        }
    }
    Ok(state)
}

/// Fetch the raw token balance of an SPL token account.
async fn fetch_token_balance(rpc: &RpcClient, token_account: &Pubkey) -> TradeResult<u64> {
    let balance = rpc.get_token_account_balance(token_account)
        .await
        .map_err(|e| TradeError::Execution(format!("fetch token balance {token_account}: {e}")))?;
    balance.amount.parse::<u64>()
        .map_err(|e| TradeError::Execution(format!("parse token balance: {e}")))
}

// -- Meteora Standard (Dynamic AMM, vault-based) --
async fn parse_meteora(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    let pool = MeteoraPool::try_from_bytes(&pool_data.data)?;

    let (a_vault_data, b_vault_data) = tokio::try_join!(
        fetch_account(rpc, &pool.a_vault),
        fetch_account(rpc, &pool.b_vault),
    )?;

    let av = MeteoraVault::try_from_bytes(&a_vault_data.data)?;
    let bv = MeteoraVault::try_from_bytes(&b_vault_data.data)?;

    Ok(PoolState::Meteora {
        pool: *pool_address,
        token_a_mint: pool.token_a_mint,
        token_b_mint: pool.token_b_mint,
        a_vault: pool.a_vault,
        b_vault: pool.b_vault,
        a_token_vault: av.token_vault,
        b_token_vault: bv.token_vault,
        a_vault_lp_mint: av.lp_mint,
        b_vault_lp_mint: bv.lp_mint,
        a_vault_lp: pool.a_vault_lp,
        b_vault_lp: pool.b_vault_lp,
        admin_token_a_fee: pool.admin_token_a_fee,
        admin_token_b_fee: pool.admin_token_b_fee,
        vault_program: *METEORA_VAULT_PROGRAM,
    })
}

// -- Meteora DLMM --
fn parse_meteora_dlmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let lb = MeteoraDlmmLbPair::try_from_bytes(&pool_data.data)?;

    let bin_idx = lb.active_id.div_euclid(70); // MAX_BIN_PER_ARRAY = 70
    let bin_arrays: Vec<Pubkey> = [bin_idx, bin_idx - 1, bin_idx + 1]
        .iter()
        .map(|&idx| {
            let (pda, _) = Pubkey::find_program_address(
                &[b"bin_array", pool_address.as_ref(), &(idx as i64).to_le_bytes()],
                &METEORA_DLMM_PROG_ID,
            );
            pda
        })
        .collect();

    Ok(PoolState::MeteoraDlmm {
        lb_pair: *pool_address,
        bin_array_bitmap_extension: METEORA_DLMM_PROG_ID, // None → program ID placeholder
        reserve_x: lb.reserve_x,
        reserve_y: lb.reserve_y,
        token_x_mint: lb.token_x_mint,
        token_y_mint: lb.token_y_mint,
        oracle: lb.oracle,
        host_fee_in: METEORA_DLMM_PROG_ID, // None → program ID placeholder
        event_authority: *METEORA_DLMM_EVENT_AUTHORITY,
        bin_arrays,
    })
}

// -- Meteora DAMM (cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG) --
fn parse_meteora_damm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = MeteoraDammPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::MeteoraDamm {
        pool: *pool_address,
        token_a_vault: pool.token_a_vault,
        token_b_vault: pool.token_b_vault,
        token_a_mint: pool.token_a_mint,
        token_b_mint: pool.token_b_mint,
    })
}

// -- Meteora DBC (dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN) --
async fn parse_meteora_dbc(rpc: &RpcClient, pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = MeteoraDbcVirtualPool::try_from_bytes(&pool_data.data)?;
    let config_data = fetch_account(rpc, &pool.config).await?;
    let cfg = MeteoraDbcConfig::try_from_bytes(&config_data.data)?;

    Ok(PoolState::MeteoraDbc {
        pool: *pool_address,
        config: pool.config,
        pool_authority: *METEORA_DBC_POOL_AUTHORITY,
        base_vault: pool.base_vault,
        quote_vault: pool.quote_vault,
        base_mint: pool.base_mint,
        quote_mint: cfg.quote_mint,
    })
}

// -- Orca Whirlpool --
fn parse_orca(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = OrcaWhirlpool::try_from_bytes(&pool_data.data)?;
    let (oracle, _) = Pubkey::find_program_address(
        &[b"oracle", pool_address.as_ref()],
        &ORCA_PROG_ID,
    );
    Ok(PoolState::Orca {
        whirlpool: *pool_address,
        token_vault_a: pool.token_vault_a,
        token_vault_b: pool.token_vault_b,
        oracle,
        token_mint_a: pool.token_mint_a,
        token_mint_b: pool.token_mint_b,
        tick_current: pool.tick_current,
        tick_spacing: pool.tick_spacing,
        sqrt_price_x64: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
        fee_rate: pool.fee_rate,
    })
}

// -- Tier 2 parsers (simpler account layouts) --

/// Parse the shared SPL Token Swap layout used by FluxBeam, Saros, and Dooar,
/// and derive the program-specific authority PDA. Returns (layout, authority).
fn parse_spl_token_swap(
    pool_address: &Pubkey,
    pool_data: &Account,
    program_id: &Pubkey,
    name: &str,
) -> TradeResult<(SplTokenSwapPool, Pubkey)> {
    let layout = SplTokenSwapPool::try_from_bytes(&pool_data.data)
        .map_err(|_| TradeError::Execution(format!("{name} pool too small")))?;
    let (authority, _) = Pubkey::find_program_address(&[pool_address.as_ref()], program_id);
    Ok((layout, authority))
}

// -- FluxBeam (SPL Token Swap fork) --
fn parse_fluxbeam(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let (layout, authority) = parse_spl_token_swap(pool_address, pool_data, &FLUXBEAM_PROG_ID, "fluxbeam")?;
    Ok(PoolState::FluxBeam {
        pool: *pool_address,
        authority,
        token_a_vault: layout.token_a_vault,
        token_b_vault: layout.token_b_vault,
        pool_mint: layout.pool_mint,
        fee_account: layout.fee_account,
        token_a_mint: layout.token_a_mint,
        token_b_mint: layout.token_b_mint,
        pool_token_program: layout.token_program,
    })
}

fn parse_flash_trade(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = FlashTradePool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::FlashTrade {
        pool: *pool_address,
        oracle: pool.oracle,
        custody: pool.custody,
        token_mint: pool.token_mint,
    })
}

fn parse_byreal(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    // Byreal is an Orca Whirlpool fork with the identical account layout.
    let pool = OrcaWhirlpool::try_from_bytes(&pool_data.data)
        .map_err(|_| TradeError::Execution("byreal pool too small".into()))?;
    let (oracle, _) = Pubkey::find_program_address(
        &[b"oracle", pool_address.as_ref()],
        &BYREAL_PROG_ID,
    );
    Ok(PoolState::Byreal {
        pool: *pool_address,
        token_vault_a: pool.token_vault_a,
        token_vault_b: pool.token_vault_b,
        oracle,
        token_mint_a: pool.token_mint_a,
        token_mint_b: pool.token_mint_b,
        tick_current: pool.tick_current,
        tick_spacing: pool.tick_spacing,
        sqrt_price_x64: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
    })
}

// -- DefiTuna Fusion --
fn parse_defituna_fusion(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = DefiTunaFusionPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::DefiTunaFusion {
        pool: *pool_address,
        token_vault_a: pool.token_vault_a,
        token_vault_b: pool.token_vault_b,
        token_mint_a: pool.token_mint_a,
        token_mint_b: pool.token_mint_b,
        tick_spacing: pool.tick_spacing,
        tick_current_index: pool.tick_current_index,
        sqrt_price_x64: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
        fee_rate: pool.fee_rate,
    })
}

fn parse_defituna_pools(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = DefiTunaPoolsPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::DefiTunaPools {
        pool: *pool_address,
        token_vault_a: pool.token_vault_a,
        token_vault_b: pool.token_vault_b,
        token_mint_a: pool.token_mint_a,
        token_mint_b: pool.token_mint_b,
    })
}

// -- Saros (SPL Token Swap fork) --
fn parse_saros(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let (layout, authority) = parse_spl_token_swap(pool_address, pool_data, &SAROS_PROG_ID, "saros")?;
    Ok(PoolState::Saros {
        pool: *pool_address,
        authority,
        token_a_vault: layout.token_a_vault,
        token_b_vault: layout.token_b_vault,
        pool_mint: layout.pool_mint,
        fee_account: layout.fee_account,
        token_a_mint: layout.token_a_mint,
        token_b_mint: layout.token_b_mint,
    })
}

fn parse_pancakeswap(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let pool = PancakeSwapPool::try_from_bytes(&pool_data.data)?;
    Ok(PoolState::PancakeSwap {
        pool: *pool_address,
        amm_config: pool.amm_config,
        token_vault_a: pool.token_vault_a,
        token_vault_b: pool.token_vault_b,
        observation: pool.observation,
        token_mint_a: pool.token_mint_a,
        token_mint_b: pool.token_mint_b,
        tick_current: pool.tick_current,
        tick_spacing: pool.tick_spacing,
        sqrt_price_x64: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
        fee_rate: 25,
    })
}

// -- Pumpup (post-graduation AMM) --
//
// Account layout (after 8-byte Anchor disc) per on-chain Anchor IDL at
// `BzBmXJiz9H88PAZomvWn8UvmdmeucWZg7N1cygN5po61`:
//
//   offset 8   token_a_mint        (32B Pubkey)
//   offset 40  token_b_mint        (32B Pubkey)
//   offset 72  token_a_vault       (32B Pubkey)
//   offset 104 token_b_vault       (32B Pubkey)
//   offset 136 lp_mint             (32B Pubkey)
//   offset 168 fee_recipient       (32B Pubkey)
//   offset 200 token_a_reserve     (u64)
//   offset 208 token_b_reserve     (u64)
//   offset 216 lp_token_supply     (u64)
//   offset 224 fee_rate            (u16)
//   offset 226 bump                (u8)
//   offset 227 fee_recipient2      (32B Pubkey)
//   offset 259 fee_rate2           (u16)
//
// Pool account discriminator = sha256("account:Pool")[0..8]
//   = [241, 154, 109, 4, 17, 177, 109, 188] = `f19a6d0411b16dbc`.
//
// Verified against live mainnet pool 7Q9RYYbijphbAXBV527Jz2QmgY4BXdaAzfXhJ3wT8hv1.
const PUMPUP_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];

fn parse_pumpup(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    if pool_data.owner != PUMPUP_PROG_ID {
        return Err(TradeError::Execution(format!(
            "pumpup pool {pool_address} not owned by Pumpup program (owned by {})",
            pool_data.owner
        )));
    }
    let d = &pool_data.data;
    let pool = PumpupPool::try_from_bytes(d).map_err(|_| TradeError::Execution(format!(
        "pumpup pool {pool_address} data too short: {} bytes (need >= {})",
        d.len(), PumpupPool::MIN_SIZE
    )))?;
    if d[..8] != PUMPUP_POOL_DISCRIMINATOR {
        return Err(TradeError::Execution(format!(
            "pumpup pool {pool_address} has wrong discriminator: {:02x?} (expected {:02x?})",
            &d[..8],
            PUMPUP_POOL_DISCRIMINATOR
        )));
    }
    Ok(PoolState::Pumpup {
        pool: *pool_address,
        token_a_mint: pool.token_a_mint,
        token_b_mint: pool.token_b_mint,
        token_a_vault: pool.token_a_vault,
        token_b_vault: pool.token_b_vault,
        fee_recipient: pool.fee_recipient,
        fee_recipient2: pool.fee_recipient2,
        token_a_reserve: pool.token_a_reserve,
        token_b_reserve: pool.token_b_reserve,
    })
}

// -- Pumpup (pre-graduation bonding curve) --
//
// The BondingCurve struct is stored INSIDE `pool_sol_account` (PDA
// `["pumpup.pool", mint]`). That account holds both lamports (real SOL) and
// the curve data — there is no separate "BondingCurve" PDA.
//
// Account layout (after 8-byte Anchor disc) per on-chain Anchor IDL at
// `BzBmXJiz9H88PAZomvWn8UvmdmeucWZg7N1cygN5po61`:
//
//   offset 8   launch_token_surplus  (u64)
//   offset 16  virtual_sol           (u64)
//   offset 24  real_sol              (u64)
//   offset 32  pool_sol_reserves     (u64)
//   offset 40  pool_token_reserves   (u64)
//   offset 48  current_leverage_index(u8)
//   offset 49  leverage              (vec<[u64;3]>)  -- variable-length tail
//
// Account discriminator = sha256("account:BondingCurve")[0..8]
//   = [23, 183, 248, 55, 96, 216, 172, 96] = `17b7f83760d8ac60`.
const PUMPUP_BONDING_DISCRIMINATOR: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

const PUMPUP_CONFIG_SEED: &[u8] = b"pumpup.config";
const PUMPUP_POOL_SEED: &[u8] = b"pumpup.pool";

/// Cached `["pumpup.config"]` PDA — same for every bonding curve.
static PUMPUP_CONFIG_PDA: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[PUMPUP_CONFIG_SEED], &PUMPUP_PROG_ID).0
});

/// Cached pumpup_fee recipient extracted from the PumpupConfiguration singleton.
/// The singleton's `fee_address` is currently immutable; cache it process-wide
/// to avoid re-fetching for every bonding-curve cold-fetch.
static PUMPUP_FEE_CACHE: tokio::sync::OnceCell<Pubkey> =
    tokio::sync::OnceCell::const_new();

async fn fetch_pumpup_fee_recipient(rpc: &RpcClient) -> TradeResult<Pubkey> {
    PUMPUP_FEE_CACHE
        .get_or_try_init(|| async {
            let cfg = fetch_account(rpc, &*PUMPUP_CONFIG_PDA).await?;
            if cfg.owner != PUMPUP_PROG_ID {
                return Err(TradeError::Execution(format!(
                    "pumpup config {} not owned by Pumpup program (owned by {})",
                    *PUMPUP_CONFIG_PDA, cfg.owner
                )));
            }
            let config = PumpupConfig::try_from_bytes(&cfg.data)?;
            Ok(config.fee_address)
        })
        .await
        .copied()
}

/// Derive `pool_sol_account` for a given mint — the bonding curve PDA address.
pub fn derive_pumpup_pool_sol_account(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[PUMPUP_POOL_SEED, mint.as_ref()], &PUMPUP_PROG_ID).0
}

/// Parse the BondingCurve struct out of a pool_sol_account's data.
fn parse_pumpup_bonding_layout(
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<(u64, u64, u64, u64, u64)> {
    if pool_data.owner != PUMPUP_PROG_ID {
        return Err(TradeError::Execution(format!(
            "pumpup bonding curve {pool_address} not owned by Pumpup program (owned by {})",
            pool_data.owner
        )));
    }
    let d = &pool_data.data;
    let curve = PumpupBondingCurve::try_from_bytes(d).map_err(|_| {
        TradeError::Execution(format!(
            "pumpup bonding curve {pool_address} data too short: {} bytes (need >= {})",
            d.len(), PumpupBondingCurve::MIN_SIZE
        ))
    })?;
    if d[..8] != PUMPUP_BONDING_DISCRIMINATOR {
        return Err(TradeError::Execution(format!(
            "pumpup bonding curve {pool_address} has wrong discriminator: {:02x?} (expected {:02x?})",
            &d[..8],
            PUMPUP_BONDING_DISCRIMINATOR
        )));
    }
    Ok((curve.launch_token_surplus, curve.virtual_sol, curve.real_sol, curve.pool_sol_reserves, curve.pool_token_reserves))
}

/// Fetch a Pumpup bonding curve. Uses RPC to discover the mint from the
/// pool_sol_account's owned token account, then derives pool_token_account
/// and looks up the cached pumpup_fee recipient.
async fn parse_pumpup_bonding(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    use solana_client::rpc_request::TokenAccountsFilter;

    let (_lts, virtual_sol, real_sol, pool_sol_reserves, pool_token_reserves) =
        parse_pumpup_bonding_layout(pool_address, pool_data)?;

    // Discover the mint by enumerating token accounts owned by pool_sol_account.
    // Pumpup bonding curves only ever own the single SPL Token (classic) account
    // for their paired mint.
    let token_accounts = rpc
        .get_token_accounts_by_owner(pool_address, TokenAccountsFilter::ProgramId(TOKEN_PROGRAM_ID))
        .await
        .map_err(|e| TradeError::Execution(format!("pumpup bonding: failed to fetch token accounts: {e}")))?;

    if token_accounts.is_empty() {
        return Err(TradeError::Execution(format!(
            "pumpup bonding {pool_address}: no SPL token accounts found — \
             cannot determine paired mint"
        )));
    }

    let ta = &token_accounts[0];
    let mint = if let solana_account_decoder::UiAccountData::Json(parsed) = &ta.account.data {
        let mint_str = parsed.parsed["info"]["mint"].as_str().unwrap_or_default();
        mint_str
            .parse::<Pubkey>()
            .map_err(|e| TradeError::Execution(format!("pumpup bonding: failed to parse mint: {e}")))?
    } else {
        return Err(TradeError::Execution(
            "pumpup bonding: unexpected token account data format".into(),
        ));
    };

    // pool_token_account = ATA(pool_sol_account, mint, TOKEN_PROGRAM_ID)
    let pool_token_account =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            pool_address,
            &mint,
            &TOKEN_PROGRAM_ID,
        );

    // Sanity: the discovered token account should be the derived ATA.
    let discovered_ta: Pubkey = ta.pubkey.parse().map_err(|_| {
        TradeError::Execution("pumpup bonding: failed to parse discovered token account address".into())
    })?;
    if discovered_ta != pool_token_account {
        return Err(TradeError::Execution(format!(
            "pumpup bonding {pool_address}: discovered token account {discovered_ta} \
             differs from derived ATA {pool_token_account}"
        )));
    }

    let pumpup_fee = fetch_pumpup_fee_recipient(rpc).await?;

    Ok(PoolState::PumpupBonding {
        pool: *pool_address,
        mint,
        pool_token_account,
        pumpup_fee,
        virtual_sol,
        real_sol,
        pool_sol_reserves,
        pool_token_reserves,
    })
}

// -- Dooar (SPL Token Swap fork) --
fn parse_dooar(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let (layout, authority) = parse_spl_token_swap(pool_address, pool_data, &DOOAR_PROG_ID, "dooar")?;
    Ok(PoolState::Dooar {
        pool: *pool_address,
        authority,
        token_a_vault: layout.token_a_vault,
        token_b_vault: layout.token_b_vault,
        pool_mint: layout.pool_mint,
        fee_account: layout.fee_account,
        token_a_mint: layout.token_a_mint,
        token_b_mint: layout.token_b_mint,
    })
}

fn pubkey_from_str(s: &str) -> Pubkey {
    // These are compile-time known addresses; panic is acceptable here
    // as it indicates a programming error, not a runtime condition.
    s.parse::<Pubkey>().unwrap_or_else(|_| panic!("BUG: invalid hardcoded pubkey: {s}"))
}

// ──────────────────────────────────────────────────────────────────────
// Sync companion parsers — build PoolState from pool bytes + cached
// companion account data, eliminating RPC on the hot path.
// ──────────────────────────────────────────────────────────────────────

/// Extract the companion account pubkeys needed to build PoolState for async types.
/// Returns an empty Vec for sync-parseable types.
pub fn extract_companion_keys(pool_type: PoolType, data: &[u8]) -> Vec<Pubkey> {
    match pool_type {
        PoolType::RaydiumV4 => RaydiumV4Pool::try_from_bytes(data)
            .map(|p| vec![p.serum_market])
            .unwrap_or_default(),
        PoolType::Meteora => MeteoraPool::try_from_bytes(data)
            .map(|p| vec![p.a_vault, p.b_vault])
            .unwrap_or_default(),
        PoolType::MeteoraDbc => MeteoraDbcVirtualPool::try_from_bytes(data)
            .map(|p| vec![p.config])
            .unwrap_or_default(),
        // PumpFun bonding: mint must be discovered via RPC (get_token_accounts_by_owner).
        // PumpFunAmm: sync parser exists (parse_pumpfun_amm_layout), vault balances are Phase 2.
        // All other types are already sync-parseable.
        _ => vec![],
    }
}

/// Parse RaydiumV4 using cached serum market data (no RPC).
pub fn parse_raydium_v4_with_companion(
    pool_address: &Pubkey,
    pool_data: &[u8],
    serum_market_data: &[u8],
    serum_market_owner: &Pubkey,
) -> TradeResult<PoolState> {
    let pool = RaydiumV4Pool::try_from_bytes(pool_data)?;
    let serum = SerumMarketAccounts::parse(serum_market_data, pool.serum_market, serum_market_owner)?;
    Ok(PoolState::RaydiumV4 {
        amm_id: *pool_address,
        authority: *RAYDIUM_V4_AUTHORITY,
        open_orders: pool.open_orders,
        target_orders: pool.target_orders,
        coin_vault: pool.coin_vault,
        pc_vault: pool.pc_vault,
        serum_program: pool.serum_program,
        serum_market: pool.serum_market,
        serum_bids: serum.bids,
        serum_asks: serum.asks,
        serum_event_queue: serum.event_queue,
        serum_coin_vault: serum.coin_vault,
        serum_pc_vault: serum.pc_vault,
        serum_vault_signer: serum.vault_signer,
    })
}

/// Parse PumpFun bonding curve using pre-discovered companion data (no RPC).
///
/// The caller must have already discovered the mint (via one-time RPC) and
/// fetched the global config. These are cached in the AccountMirror.
pub fn parse_pumpfun_with_companion(
    pool_address: &Pubkey,
    pool_data: &[u8],
    mint: Pubkey,
    token_prog: Pubkey,
    global_data: &[u8],
) -> TradeResult<PoolState> {
    let bonding = PumpFunBondingCurve::try_from_bytes(pool_data);
    let global_cfg = PumpFunGlobal::try_from_bytes(global_data)
        .unwrap_or(PumpFunGlobal { fee_recipient: *PUMPFUN_FEE_FALLBACK });
    let associated_bonding_curve =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            pool_address,
            &mint,
            &token_prog,
        );
    Ok(PoolState::PumpFun {
        global: *PUMPFUN_GLOBAL,
        fee_account: global_cfg.fee_recipient,
        mint,
        bonding_curve: *pool_address,
        associated_bonding_curve,
        event_authority: *PUMPFUN_EVENT_AUTHORITY,
        creator: bonding.creator,
    })
}

/// Parse PumpFun AMM using cached vault balances from the AccountMirror (no RPC).
/// Falls back to zero reserves if balances aren't available yet.
pub fn parse_pumpfun_amm_with_balances(
    pool_address: &Pubkey,
    pool_data: &[u8],
    base_balance: Option<u64>,
    quote_balance: Option<u64>,
) -> TradeResult<PoolState> {
    let account = Account {
        lamports: 0,
        data: pool_data.to_vec(),
        owner: Pubkey::default(),
        executable: false,
        rent_epoch: 0,
    };
    let mut state = parse_pumpfun_amm_layout(pool_address, &account)?;
    if let PoolState::PumpFunAmm {
        ref mut base_reserve,
        ref mut quote_reserve,
        ..
    } = state
    {
        if let Some(b) = base_balance {
            *base_reserve = b;
        }
        if let Some(q) = quote_balance {
            *quote_reserve = q;
        }
    }
    Ok(state)
}

/// Parse Meteora Standard using cached vault data (no RPC).
pub fn parse_meteora_with_companion(
    pool_address: &Pubkey,
    pool_data: &[u8],
    a_vault_data: &[u8],
    b_vault_data: &[u8],
) -> TradeResult<PoolState> {
    let pool = MeteoraPool::try_from_bytes(pool_data)?;
    let a_vault = MeteoraVault::try_from_bytes(a_vault_data)?;
    let b_vault = MeteoraVault::try_from_bytes(b_vault_data)?;
    Ok(PoolState::Meteora {
        pool: *pool_address,
        token_a_mint: pool.token_a_mint,
        token_b_mint: pool.token_b_mint,
        a_vault: pool.a_vault,
        b_vault: pool.b_vault,
        a_token_vault: a_vault.token_vault,
        b_token_vault: b_vault.token_vault,
        a_vault_lp_mint: a_vault.lp_mint,
        b_vault_lp_mint: b_vault.lp_mint,
        a_vault_lp: pool.a_vault_lp,
        b_vault_lp: pool.b_vault_lp,
        admin_token_a_fee: pool.admin_token_a_fee,
        admin_token_b_fee: pool.admin_token_b_fee,
        vault_program: *METEORA_VAULT_PROGRAM,
    })
}

/// Parse MeteoraDbc using cached config data (no RPC).
pub fn parse_meteora_dbc_with_companion(
    pool_address: &Pubkey,
    pool_data: &[u8],
    config_data: &[u8],
) -> TradeResult<PoolState> {
    let pool = MeteoraDbcVirtualPool::try_from_bytes(pool_data)?;
    let cfg = MeteoraDbcConfig::try_from_bytes(config_data)?;
    Ok(PoolState::MeteoraDbc {
        pool: *pool_address,
        config: pool.config,
        pool_authority: *METEORA_DBC_POOL_AUTHORITY,
        base_vault: pool.base_vault,
        quote_vault: pool.quote_vault,
        base_mint: pool.base_mint,
        quote_mint: cfg.quote_mint,
    })
}

/// Try to parse an async pool type using companion data from the AccountMirror.
/// Returns Ok(PoolState) if companions are available, Err if not cached yet.
pub fn parse_with_mirror(
    pool_type: PoolType,
    pool_address: &Pubkey,
    data: &[u8],
    owner: &Pubkey,
    mirror: &crate::stream::account_mirror::AccountMirror,
) -> TradeResult<PoolState> {
    match pool_type {
        // Sync types — no mirror needed
        pt if is_sync_parseable(pt) => parse_pool_state_from_bytes(pt, pool_address, data, owner),

        PoolType::RaydiumV4 => {
            let pool = RaydiumV4Pool::try_from_bytes(data)?;
            let market_data = mirror.get_companion(&pool.serum_market).ok_or_else(||
                TradeError::Execution("raydium v4: serum market not in mirror".into()))?;
            // Serum is closed — market owner doesn't matter for the placeholder path.
            let market_owner = Pubkey::default();
            parse_raydium_v4_with_companion(pool_address, data, &market_data, &market_owner)
        }

        PoolType::PumpFunAmm => {
            // Sync layout parser + vault balances from mirror
            let amm = PumpFunAmmPool::try_from_bytes(data)?;
            let base_bal = mirror.get_vault_balance(&amm.pool_base_vault);
            let quote_bal = mirror.get_vault_balance(&amm.pool_quote_vault);
            parse_pumpfun_amm_with_balances(pool_address, data, base_bal, quote_bal)
        }

        PoolType::Meteora => {
            let pool = MeteoraPool::try_from_bytes(data)?;
            let a_data = mirror.get_companion(&pool.a_vault).ok_or_else(||
                TradeError::Execution("meteora: vault A not in mirror".into()))?;
            let b_data = mirror.get_companion(&pool.b_vault).ok_or_else(||
                TradeError::Execution("meteora: vault B not in mirror".into()))?;
            parse_meteora_with_companion(pool_address, data, &a_data, &b_data)
        }

        PoolType::MeteoraDbc => {
            let pool = MeteoraDbcVirtualPool::try_from_bytes(data)?;
            let config_data = mirror.get_companion(&pool.config).ok_or_else(||
                TradeError::Execution("meteora dbc: config not in mirror".into()))?;
            parse_meteora_dbc_with_companion(pool_address, data, &config_data)
        }

        PoolType::PumpFun => {
            // PumpFun bonding: mint is cached in mirror by the tx handler.
            // The tx handler stores [mint(32) + token_program(32)] keyed by pool address.
            let mint_data = mirror.get_companion(pool_address).ok_or_else(||
                TradeError::Execution("pumpfun: mint not cached (no tx seen yet)".into()))?;
            if mint_data.len() < 64 {
                return Err(TradeError::Execution("pumpfun: cached mint data too short".into()));
            }
            let mint = Pubkey::new_from_array(mint_data[..32].try_into().unwrap());
            let token_prog = Pubkey::new_from_array(mint_data[32..64].try_into().unwrap());
            // Fetch global config from mirror (singleton, cached on first encounter)
            let global_key = *PUMPFUN_GLOBAL;
            let global_data = mirror.get_companion(&global_key).unwrap_or_default();
            parse_pumpfun_with_companion(pool_address, data, mint, token_prog, &global_data)
        }

        _ => Err(TradeError::Execution(format!(
            "pool type {:?} not supported for mirror parsing", pool_type
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::account::Account;
    use solana_sdk::pubkey::Pubkey;

    /// Helper: create an Account with zeroed data of the given size.
    fn make_account(size: usize) -> Account {
        Account {
            lamports: 1_000_000,
            data: vec![0u8; size],
            owner: Pubkey::default(),
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Helper: write a pubkey's bytes into data at the given offset.
    fn write_pubkey(data: &mut [u8], offset: usize, pk: &Pubkey) {
        data[offset..offset + 32].copy_from_slice(pk.as_ref());
    }

    /// Helper: write an i32 as little-endian bytes at the given offset.
    fn write_i32(data: &mut [u8], offset: usize, val: i32) {
        data[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
    }

    /// Helper: write a u16 as little-endian bytes at the given offset.
    fn write_u16(data: &mut [u8], offset: usize, val: u16) {
        data[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
    }

    // -- Raydium CPMM --

    #[test]
    fn test_parse_raydium_cpmm_success() {
        // min size = 8 + 10*32 + 1 = 329
        let mut acct = make_account(329);
        let pool_addr = Pubkey::new_unique();

        let config = Pubkey::new_unique();
        let token_0_vault = Pubkey::new_unique();
        let token_1_vault = Pubkey::new_unique();
        let token_0_mint = Pubkey::new_unique();
        let token_1_mint = Pubkey::new_unique();
        let observation = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 8, &config);
        write_pubkey(&mut acct.data, 72, &token_0_vault);
        write_pubkey(&mut acct.data, 104, &token_1_vault);
        write_pubkey(&mut acct.data, 168, &token_0_mint);
        write_pubkey(&mut acct.data, 200, &token_1_mint);
        write_pubkey(&mut acct.data, 296, &observation);

        let result = parse_raydium_cpmm(&pool_addr, &acct).unwrap();
        match result {
            PoolState::RaydiumCpmm {
                pool,
                authority: _,
                config: cfg,
                token_0_vault: v0,
                token_1_vault: v1,
                token_0_mint: m0,
                token_1_mint: m1,
                observation: obs,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(cfg, config);
                assert_eq!(v0, token_0_vault);
                assert_eq!(v1, token_1_vault);
                assert_eq!(m0, token_0_mint);
                assert_eq!(m1, token_1_mint);
                assert_eq!(obs, observation);
            }
            _ => panic!("expected RaydiumCpmm variant"),
        }
    }

    #[test]
    fn test_parse_raydium_cpmm_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_raydium_cpmm(&pool_addr, &acct).is_err());
    }

    // -- Raydium CLMM --

    #[test]
    fn test_parse_raydium_clmm_success() {
        let mut acct = make_account(273);
        let pool_addr = Pubkey::new_unique();

        let amm_config = Pubkey::new_unique();
        let token_mint_0 = Pubkey::new_unique();
        let token_mint_1 = Pubkey::new_unique();
        let token_vault_0 = Pubkey::new_unique();
        let token_vault_1 = Pubkey::new_unique();
        let observation = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 9, &amm_config);
        write_pubkey(&mut acct.data, 73, &token_mint_0);
        write_pubkey(&mut acct.data, 105, &token_mint_1);
        write_pubkey(&mut acct.data, 137, &token_vault_0);
        write_pubkey(&mut acct.data, 169, &token_vault_1);
        write_pubkey(&mut acct.data, 201, &observation);

        write_u16(&mut acct.data, 235, 10);
        write_i32(&mut acct.data, 269, 42);

        let result = parse_raydium_clmm(&pool_addr, &acct).unwrap();
        match result {
            PoolState::RaydiumClmm {
                pool,
                amm_config: cfg,
                observation: obs,
                token_vault_0: v0,
                token_vault_1: v1,
                token_mint_0: m0,
                token_mint_1: m1,
                ..
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(cfg, amm_config);
                assert_eq!(obs, observation);
                assert_eq!(v0, token_vault_0);
                assert_eq!(v1, token_vault_1);
                assert_eq!(m0, token_mint_0);
                assert_eq!(m1, token_mint_1);
            }
            _ => panic!("expected RaydiumClmm variant"),
        }
    }

    #[test]
    fn test_parse_raydium_clmm_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_raydium_clmm(&pool_addr, &acct).is_err());
    }

    // -- Raydium LP --

    #[test]
    fn test_parse_raydium_lp_success() {
        let mut acct = make_account(365);
        let pool_addr = Pubkey::new_unique();

        let config_id = Pubkey::new_unique();
        let platform_id = Pubkey::new_unique();
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let base_vault = Pubkey::new_unique();
        let quote_vault = Pubkey::new_unique();
        let creator = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 141, &config_id);
        write_pubkey(&mut acct.data, 173, &platform_id);
        write_pubkey(&mut acct.data, 205, &base_mint);
        write_pubkey(&mut acct.data, 237, &quote_mint);
        write_pubkey(&mut acct.data, 269, &base_vault);
        write_pubkey(&mut acct.data, 301, &quote_vault);
        write_pubkey(&mut acct.data, 333, &creator);

        let result = parse_raydium_lp(&pool_addr, &acct).unwrap();
        match result {
            PoolState::RaydiumLp {
                pool_state,
                authority: _,
                base_vault: bv,
                quote_vault: qv,
                base_mint: bm,
                quote_mint: qm,
                config_id: ci,
                platform_id: pi,
                creator: cr,
            } => {
                assert_eq!(pool_state, pool_addr);
                assert_eq!(ci, config_id);
                assert_eq!(pi, platform_id);
                assert_eq!(bm, base_mint);
                assert_eq!(qm, quote_mint);
                assert_eq!(bv, base_vault);
                assert_eq!(qv, quote_vault);
                assert_eq!(cr, creator);
            }
            _ => panic!("expected RaydiumLp variant"),
        }
    }

    #[test]
    fn test_parse_raydium_lp_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_raydium_lp(&pool_addr, &acct).is_err());
    }

    // -- PumpFun AMM --

    #[test]
    fn test_parse_pumpfun_amm_success() {
        let mut acct = make_account(243);
        let pool_addr = Pubkey::new_unique();

        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool_base_vault = Pubkey::new_unique();
        let pool_quote_vault = Pubkey::new_unique();
        let coin_creator = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 43, &base_mint);
        write_pubkey(&mut acct.data, 75, &quote_mint);
        write_pubkey(&mut acct.data, 139, &pool_base_vault);
        write_pubkey(&mut acct.data, 171, &pool_quote_vault);
        write_pubkey(&mut acct.data, 211, &coin_creator);

        let result = parse_pumpfun_amm_layout(&pool_addr, &acct).unwrap();
        match result {
            PoolState::PumpFunAmm {
                pool,
                base_mint: bm,
                quote_mint: qm,
                pool_base_vault: bv,
                pool_quote_vault: qv,
                coin_creator: cc,
                ..
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(bm, base_mint);
                assert_eq!(qm, quote_mint);
                assert_eq!(bv, pool_base_vault);
                assert_eq!(qv, pool_quote_vault);
                assert_eq!(cc, coin_creator);
            }
            _ => panic!("expected PumpFunAmm variant"),
        }
    }

    #[test]
    fn test_parse_pumpfun_amm_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_pumpfun_amm_layout(&pool_addr, &acct).is_err());
    }

    // -- Meteora DLMM --

    #[test]
    fn test_parse_meteora_dlmm_success() {
        let mut acct = make_account(584);
        let pool_addr = Pubkey::new_unique();

        let token_x_mint = Pubkey::new_unique();
        let token_y_mint = Pubkey::new_unique();
        let reserve_x = Pubkey::new_unique();
        let reserve_y = Pubkey::new_unique();
        let oracle = Pubkey::new_unique();

        write_i32(&mut acct.data, 76, 100);
        write_pubkey(&mut acct.data, 88, &token_x_mint);
        write_pubkey(&mut acct.data, 120, &token_y_mint);
        write_pubkey(&mut acct.data, 152, &reserve_x);
        write_pubkey(&mut acct.data, 184, &reserve_y);
        write_pubkey(&mut acct.data, 552, &oracle);

        let result = parse_meteora_dlmm(&pool_addr, &acct).unwrap();
        match result {
            PoolState::MeteoraDlmm {
                lb_pair,
                bin_array_bitmap_extension: _,
                reserve_x: rx,
                reserve_y: ry,
                token_x_mint: mx,
                token_y_mint: my,
                oracle: orc,
                host_fee_in: _,
                event_authority: _,
                bin_arrays,
            } => {
                assert_eq!(lb_pair, pool_addr);
                assert_eq!(rx, reserve_x);
                assert_eq!(ry, reserve_y);
                assert_eq!(mx, token_x_mint);
                assert_eq!(my, token_y_mint);
                assert_eq!(orc, oracle);
                assert_eq!(bin_arrays.len(), 3);
            }
            _ => panic!("expected MeteoraDlmm variant"),
        }
    }

    #[test]
    fn test_parse_meteora_dlmm_too_short() {
        let acct = make_account(200);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_meteora_dlmm(&pool_addr, &acct).is_err());
    }

    // -- Meteora DAMM --

    #[test]
    fn test_parse_meteora_damm_success() {
        let mut acct = make_account(296);
        let pool_addr = Pubkey::new_unique();

        let token_a_mint = Pubkey::new_unique();
        let token_b_mint = Pubkey::new_unique();
        let token_a_vault = Pubkey::new_unique();
        let token_b_vault = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 168, &token_a_mint);
        write_pubkey(&mut acct.data, 200, &token_b_mint);
        write_pubkey(&mut acct.data, 232, &token_a_vault);
        write_pubkey(&mut acct.data, 264, &token_b_vault);

        let result = parse_meteora_damm(&pool_addr, &acct).unwrap();
        match result {
            PoolState::MeteoraDamm {
                pool,
                token_a_vault: va,
                token_b_vault: vb,
                token_a_mint: ma,
                token_b_mint: mb,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_a_vault);
                assert_eq!(vb, token_b_vault);
                assert_eq!(ma, token_a_mint);
                assert_eq!(mb, token_b_mint);
            }
            _ => panic!("expected MeteoraDamm variant"),
        }
    }

    #[test]
    fn test_parse_meteora_damm_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_meteora_damm(&pool_addr, &acct).is_err());
    }

    // -- Meteora DBC --

    #[test]
    fn test_parse_meteora_dbc_pool_authority_pda() {
        // Verify the pool authority PDA is deterministic
        let (authority, _) = Pubkey::find_program_address(
            &[b"pool_authority"],
            &METEORA_DBC_PROG_ID,
        );
        assert_eq!(authority, *METEORA_DBC_POOL_AUTHORITY);
    }

    #[tokio::test]
    async fn test_parse_meteora_dbc_too_short() {
        // parse_meteora_dbc rejects pools < 232 bytes before any RPC calls
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        // Create a dummy RPC client (won't be called since size check fails first)
        let rpc = RpcClient::new("http://127.0.0.1:1".to_string());
        assert!(parse_meteora_dbc(&rpc, &pool_addr, &acct).await.is_err());
    }

    // -- Orca --

    #[test]
    fn test_parse_orca_success() {
        let mut acct = make_account(296);
        let pool_addr = Pubkey::new_unique();

        let token_mint_a = Pubkey::new_unique();
        let token_vault_a = Pubkey::new_unique();
        let token_mint_b = Pubkey::new_unique();
        let token_vault_b = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 101, &token_mint_a);
        write_pubkey(&mut acct.data, 133, &token_vault_a);
        write_pubkey(&mut acct.data, 181, &token_mint_b);
        write_pubkey(&mut acct.data, 213, &token_vault_b);

        write_u16(&mut acct.data, 41, 8);
        write_i32(&mut acct.data, 81, -50);

        let result = parse_orca(&pool_addr, &acct).unwrap();
        match result {
            PoolState::Orca {
                whirlpool,
                token_vault_a: va,
                token_vault_b: vb,
                token_mint_a: ma,
                token_mint_b: mb,
                tick_current,
                tick_spacing,
                ..
            } => {
                assert_eq!(whirlpool, pool_addr);
                assert_eq!(va, token_vault_a);
                assert_eq!(vb, token_vault_b);
                assert_eq!(ma, token_mint_a);
                assert_eq!(mb, token_mint_b);
                assert_eq!(tick_current, -50);
                assert_eq!(tick_spacing, 8);
            }
            _ => panic!("expected Orca variant"),
        }
    }

    #[test]
    fn test_parse_orca_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_orca(&pool_addr, &acct).is_err());
    }

    // -- FluxBeam --

    #[test]
    fn test_parse_fluxbeam_success() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();

        let pool_token_program = Pubkey::new_unique();
        let token_a_vault = Pubkey::new_unique();
        let token_b_vault = Pubkey::new_unique();
        let pool_mint = Pubkey::new_unique();
        let token_a_mint = Pubkey::new_unique();
        let token_b_mint = Pubkey::new_unique();
        let fee_account = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 3, &pool_token_program);
        write_pubkey(&mut acct.data, 35, &token_a_vault);
        write_pubkey(&mut acct.data, 67, &token_b_vault);
        write_pubkey(&mut acct.data, 99, &pool_mint);
        write_pubkey(&mut acct.data, 131, &token_a_mint);
        write_pubkey(&mut acct.data, 163, &token_b_mint);
        write_pubkey(&mut acct.data, 195, &fee_account);

        let result = parse_fluxbeam(&pool_addr, &acct).unwrap();
        match result {
            PoolState::FluxBeam {
                pool,
                authority: _,
                token_a_vault: va,
                token_b_vault: vb,
                pool_mint: pm,
                fee_account: fa,
                token_a_mint: ma,
                token_b_mint: mb,
                pool_token_program: ptp,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_a_vault);
                assert_eq!(vb, token_b_vault);
                assert_eq!(pm, pool_mint);
                assert_eq!(fa, fee_account);
                assert_eq!(ma, token_a_mint);
                assert_eq!(mb, token_b_mint);
                assert_eq!(ptp, pool_token_program);
            }
            _ => panic!("expected FluxBeam variant"),
        }
    }

    #[test]
    fn test_parse_fluxbeam_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_fluxbeam(&pool_addr, &acct).is_err());
    }

    // -- FlashTrade --

    #[test]
    fn test_parse_flash_trade_success() {
        let mut acct = make_account(136);
        let pool_addr = Pubkey::new_unique();

        let oracle = Pubkey::new_unique();
        let custody = Pubkey::new_unique();
        let token_mint = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 8, &oracle);
        write_pubkey(&mut acct.data, 40, &custody);
        write_pubkey(&mut acct.data, 72, &token_mint);

        let result = parse_flash_trade(&pool_addr, &acct).unwrap();
        match result {
            PoolState::FlashTrade {
                pool,
                oracle: orc,
                custody: cust,
                token_mint: tm,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(orc, oracle);
                assert_eq!(cust, custody);
                assert_eq!(tm, token_mint);
            }
            _ => panic!("expected FlashTrade variant"),
        }
    }

    #[test]
    fn test_parse_flash_trade_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_flash_trade(&pool_addr, &acct).is_err());
    }

    // -- Byreal --

    #[test]
    fn test_parse_byreal_success() {
        let mut acct = make_account(296);
        let pool_addr = Pubkey::new_unique();

        let token_mint_a = Pubkey::new_unique();
        let token_vault_a = Pubkey::new_unique();
        let token_mint_b = Pubkey::new_unique();
        let token_vault_b = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 101, &token_mint_a);
        write_pubkey(&mut acct.data, 133, &token_vault_a);
        write_pubkey(&mut acct.data, 181, &token_mint_b);
        write_pubkey(&mut acct.data, 213, &token_vault_b);

        write_u16(&mut acct.data, 41, 4);
        write_i32(&mut acct.data, 81, 123);

        let result = parse_byreal(&pool_addr, &acct).unwrap();
        match result {
            PoolState::Byreal {
                pool,
                token_vault_a: va,
                token_vault_b: vb,
                token_mint_a: ma,
                token_mint_b: mb,
                tick_current,
                tick_spacing,
                ..
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_vault_a);
                assert_eq!(vb, token_vault_b);
                assert_eq!(ma, token_mint_a);
                assert_eq!(mb, token_mint_b);
                assert_eq!(tick_current, 123);
                assert_eq!(tick_spacing, 4);
            }
            _ => panic!("expected Byreal variant"),
        }
    }

    #[test]
    fn test_parse_byreal_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_byreal(&pool_addr, &acct).is_err());
    }

    // -- DefiTuna Fusion --

    #[test]
    fn test_parse_defituna_fusion_success() {
        let mut acct = make_account(187);
        let pool_addr = Pubkey::new_unique();

        let token_mint_a = Pubkey::new_unique();
        let token_mint_b = Pubkey::new_unique();
        let token_vault_a = Pubkey::new_unique();
        let token_vault_b = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 11, &token_mint_a);
        write_pubkey(&mut acct.data, 43, &token_mint_b);
        write_pubkey(&mut acct.data, 75, &token_vault_a);
        write_pubkey(&mut acct.data, 107, &token_vault_b);
        write_u16(&mut acct.data, 139, 64);
        write_i32(&mut acct.data, 183, -200);

        let result = parse_defituna_fusion(&pool_addr, &acct).unwrap();
        match result {
            PoolState::DefiTunaFusion {
                pool,
                token_vault_a: va,
                token_vault_b: vb,
                token_mint_a: ma,
                token_mint_b: mb,
                tick_spacing,
                tick_current_index,
                ..
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_vault_a);
                assert_eq!(vb, token_vault_b);
                assert_eq!(ma, token_mint_a);
                assert_eq!(mb, token_mint_b);
                assert_eq!(tick_spacing, 64);
                assert_eq!(tick_current_index, -200);
            }
            _ => panic!("expected DefiTunaFusion variant"),
        }
    }

    #[test]
    fn test_parse_defituna_fusion_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_defituna_fusion(&pool_addr, &acct).is_err());
    }

    // -- DefiTuna Pools --

    #[test]
    fn test_parse_defituna_pools_success() {
        let mut acct = make_account(168);
        let pool_addr = Pubkey::new_unique();

        let token_mint_a = Pubkey::new_unique();
        let token_mint_b = Pubkey::new_unique();
        let token_vault_a = Pubkey::new_unique();
        let token_vault_b = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 8, &token_mint_a);
        write_pubkey(&mut acct.data, 40, &token_mint_b);
        write_pubkey(&mut acct.data, 72, &token_vault_a);
        write_pubkey(&mut acct.data, 104, &token_vault_b);

        let result = parse_defituna_pools(&pool_addr, &acct).unwrap();
        match result {
            PoolState::DefiTunaPools {
                pool,
                token_vault_a: va,
                token_vault_b: vb,
                token_mint_a: ma,
                token_mint_b: mb,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_vault_a);
                assert_eq!(vb, token_vault_b);
                assert_eq!(ma, token_mint_a);
                assert_eq!(mb, token_mint_b);
            }
            _ => panic!("expected DefiTunaPools variant"),
        }
    }

    #[test]
    fn test_parse_defituna_pools_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_defituna_pools(&pool_addr, &acct).is_err());
    }

    // -- Saros --

    #[test]
    fn test_parse_saros_success() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();

        let token_a_vault = Pubkey::new_unique();
        let token_b_vault = Pubkey::new_unique();
        let pool_mint = Pubkey::new_unique();
        let token_a_mint = Pubkey::new_unique();
        let token_b_mint = Pubkey::new_unique();
        let fee_account = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 35, &token_a_vault);
        write_pubkey(&mut acct.data, 67, &token_b_vault);
        write_pubkey(&mut acct.data, 99, &pool_mint);
        write_pubkey(&mut acct.data, 131, &token_a_mint);
        write_pubkey(&mut acct.data, 163, &token_b_mint);
        write_pubkey(&mut acct.data, 195, &fee_account);

        let result = parse_saros(&pool_addr, &acct).unwrap();
        match result {
            PoolState::Saros {
                pool,
                authority: _,
                token_a_vault: va,
                token_b_vault: vb,
                pool_mint: pm,
                fee_account: fa,
                token_a_mint: ma,
                token_b_mint: mb,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_a_vault);
                assert_eq!(vb, token_b_vault);
                assert_eq!(pm, pool_mint);
                assert_eq!(fa, fee_account);
                assert_eq!(ma, token_a_mint);
                assert_eq!(mb, token_b_mint);
            }
            _ => panic!("expected Saros variant"),
        }
    }

    #[test]
    fn test_parse_saros_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_saros(&pool_addr, &acct).is_err());
    }

    // -- PancakeSwap --

    #[test]
    fn test_parse_pancakeswap_success() {
        let mut acct = make_account(273);
        let pool_addr = Pubkey::new_unique();

        let amm_config = Pubkey::new_unique();
        let token_mint_a = Pubkey::new_unique();
        let token_mint_b = Pubkey::new_unique();
        let token_vault_a = Pubkey::new_unique();
        let token_vault_b = Pubkey::new_unique();
        let observation = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 9, &amm_config);
        write_pubkey(&mut acct.data, 73, &token_mint_a);
        write_pubkey(&mut acct.data, 105, &token_mint_b);
        write_pubkey(&mut acct.data, 137, &token_vault_a);
        write_pubkey(&mut acct.data, 169, &token_vault_b);
        write_pubkey(&mut acct.data, 201, &observation);
        write_u16(&mut acct.data, 235, 16);
        write_i32(&mut acct.data, 269, -999);

        let result = parse_pancakeswap(&pool_addr, &acct).unwrap();
        match result {
            PoolState::PancakeSwap {
                pool,
                amm_config: cfg,
                token_vault_a: va,
                token_vault_b: vb,
                observation: obs,
                token_mint_a: ma,
                token_mint_b: mb,
                tick_current,
                tick_spacing,
                ..
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(cfg, amm_config);
                assert_eq!(va, token_vault_a);
                assert_eq!(vb, token_vault_b);
                assert_eq!(obs, observation);
                assert_eq!(ma, token_mint_a);
                assert_eq!(mb, token_mint_b);
                assert_eq!(tick_current, -999);
                assert_eq!(tick_spacing, 16);
            }
            _ => panic!("expected PancakeSwap variant"),
        }
    }

    #[test]
    fn test_parse_pancakeswap_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_pancakeswap(&pool_addr, &acct).is_err());
    }

    // -- Dooar --

    #[test]
    fn test_parse_dooar_success() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();

        let token_a_vault = Pubkey::new_unique();
        let token_b_vault = Pubkey::new_unique();
        let pool_mint = Pubkey::new_unique();
        let token_a_mint = Pubkey::new_unique();
        let token_b_mint = Pubkey::new_unique();
        let fee_account = Pubkey::new_unique();

        write_pubkey(&mut acct.data, 35, &token_a_vault);
        write_pubkey(&mut acct.data, 67, &token_b_vault);
        write_pubkey(&mut acct.data, 99, &pool_mint);
        write_pubkey(&mut acct.data, 131, &token_a_mint);
        write_pubkey(&mut acct.data, 163, &token_b_mint);
        write_pubkey(&mut acct.data, 195, &fee_account);

        let result = parse_dooar(&pool_addr, &acct).unwrap();
        match result {
            PoolState::Dooar {
                pool,
                authority: _,
                token_a_vault: va,
                token_b_vault: vb,
                pool_mint: pm,
                fee_account: fa,
                token_a_mint: ma,
                token_b_mint: mb,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(va, token_a_vault);
                assert_eq!(vb, token_b_vault);
                assert_eq!(pm, pool_mint);
                assert_eq!(fa, fee_account);
                assert_eq!(ma, token_a_mint);
                assert_eq!(mb, token_b_mint);
            }
            _ => panic!("expected Dooar variant"),
        }
    }

    #[test]
    fn test_parse_dooar_too_short() {
        let acct = make_account(100);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_dooar(&pool_addr, &acct).is_err());
    }

    // -- read_pubkey edge cases --

    #[test]
    fn test_read_pubkey_exact_boundary() {
        let pk = Pubkey::new_unique();
        let data: Vec<u8> = pk.as_ref().to_vec();
        let result = read_pubkey(&data, 0).unwrap();
        assert_eq!(result, pk);
    }

    #[test]
    fn test_read_pubkey_off_by_one() {
        let data = vec![0u8; 32];
        assert!(read_pubkey(&data, 1).is_err());
    }

    #[test]
    fn test_read_pubkey_empty_data() {
        let data: Vec<u8> = vec![];
        assert!(read_pubkey(&data, 0).is_err());
    }

    // -- PDA authority tests --

    #[test]
    fn test_raydium_cpmm_authority_is_pda() {
        let mut acct = make_account(329);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[8, 72, 104, 168, 200, 296] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_raydium_cpmm(&pool_addr, &acct).unwrap();
        if let PoolState::RaydiumCpmm { authority, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[b"vault_and_lp_mint_auth_seed"],
                &RAYDIUM_CPMM_PROG_ID,
            );
            assert_eq!(authority, expected);
        } else {
            panic!("expected RaydiumCpmm");
        }
    }

    #[test]
    fn test_raydium_lp_authority_is_pda() {
        let mut acct = make_account(365);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[141, 173, 205, 237, 269, 301, 333] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_raydium_lp(&pool_addr, &acct).unwrap();
        if let PoolState::RaydiumLp { authority, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[b"vault_auth_seed"],
                &RAYDIUM_LP_PROG_ID,
            );
            assert_eq!(authority, expected);
        } else {
            panic!("expected RaydiumLp");
        }
    }

    #[test]
    fn test_fluxbeam_authority_is_pda() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[3, 35, 67, 99, 131, 163, 195] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_fluxbeam(&pool_addr, &acct).unwrap();
        if let PoolState::FluxBeam { authority, pool, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[pool.as_ref()],
                &FLUXBEAM_PROG_ID,
            );
            assert_eq!(authority, expected);
        } else {
            panic!("expected FluxBeam");
        }
    }

    #[test]
    fn test_saros_authority_is_pda() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[35, 67, 99, 131, 163, 195] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_saros(&pool_addr, &acct).unwrap();
        if let PoolState::Saros { authority, pool, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[pool.as_ref()],
                &SAROS_PROG_ID,
            );
            assert_eq!(authority, expected);
        } else {
            panic!("expected Saros");
        }
    }

    #[test]
    fn test_dooar_authority_is_pda() {
        let mut acct = make_account(227);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[35, 67, 99, 131, 163, 195] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_dooar(&pool_addr, &acct).unwrap();
        if let PoolState::Dooar { authority, pool, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[pool.as_ref()],
                &DOOAR_PROG_ID,
            );
            assert_eq!(authority, expected);
        } else {
            panic!("expected Dooar");
        }
    }

    #[test]
    fn test_orca_oracle_is_pda() {
        let mut acct = make_account(296);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[101, 133, 181, 213] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_orca(&pool_addr, &acct).unwrap();
        if let PoolState::Orca { oracle, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", pool_addr.as_ref()],
                &ORCA_PROG_ID,
            );
            assert_eq!(oracle, expected);
        } else {
            panic!("expected Orca");
        }
    }

    #[test]
    fn test_byreal_oracle_is_pda() {
        let mut acct = make_account(296);
        let pool_addr = Pubkey::new_unique();
        for &offset in &[101, 133, 181, 213] {
            write_pubkey(&mut acct.data, offset, &Pubkey::new_unique());
        }
        let result = parse_byreal(&pool_addr, &acct).unwrap();
        if let PoolState::Byreal { oracle, .. } = result {
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", pool_addr.as_ref()],
                &BYREAL_PROG_ID,
            );
            assert_eq!(oracle, expected);
        } else {
            panic!("expected Byreal");
        }
    }

    // -- Boundary size tests (exact minimum) --

    #[test]
    fn test_raydium_cpmm_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(328);   // RaydiumCpmmWire = 8 + 10*32 = 328
        assert!(parse_raydium_cpmm(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(327);
        assert!(parse_raydium_cpmm(&pool_addr, &acct_fail).is_err());
    }

    #[test]
    fn test_pumpfun_amm_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(243);
        assert!(parse_pumpfun_amm_layout(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(242);
        assert!(parse_pumpfun_amm_layout(&pool_addr, &acct_fail).is_err());
    }

    #[test]
    fn test_meteora_damm_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(296);
        assert!(parse_meteora_damm(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(295);
        assert!(parse_meteora_damm(&pool_addr, &acct_fail).is_err());
    }

    #[test]
    fn test_defituna_fusion_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(187);
        assert!(parse_defituna_fusion(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(186);
        assert!(parse_defituna_fusion(&pool_addr, &acct_fail).is_err());
    }

    #[test]
    fn test_defituna_pools_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(136);   // DefiTunaPoolsWire = 8 + 4*32 = 136
        assert!(parse_defituna_pools(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(135);
        assert!(parse_defituna_pools(&pool_addr, &acct_fail).is_err());
    }

    // ── Companion parser tests ──

    #[test]
    fn test_extract_companion_keys_raydium_v4() {
        let mut data = vec![0u8; 680];
        let market = Pubkey::new_unique();
        write_pubkey(&mut data, 464, &market);
        let keys = extract_companion_keys(PoolType::RaydiumV4, &data);
        assert_eq!(keys, vec![market]);
    }

    #[test]
    fn test_extract_companion_keys_meteora() {
        let mut data = vec![0u8; 298];
        let vault_a = Pubkey::new_unique();
        let vault_b = Pubkey::new_unique();
        write_pubkey(&mut data, 104, &vault_a);
        write_pubkey(&mut data, 136, &vault_b);
        let keys = extract_companion_keys(PoolType::Meteora, &data);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], vault_a);
        assert_eq!(keys[1], vault_b);
    }

    #[test]
    fn test_extract_companion_keys_meteora_dbc() {
        let mut data = vec![0u8; 232];
        let config = Pubkey::new_unique();
        write_pubkey(&mut data, 72, &config);
        let keys = extract_companion_keys(PoolType::MeteoraDbc, &data);
        assert_eq!(keys, vec![config]);
    }

    #[test]
    fn test_extract_companion_keys_sync_type_empty() {
        let data = vec![0u8; 500];
        assert!(extract_companion_keys(PoolType::RaydiumCpmm, &data).is_empty());
        assert!(extract_companion_keys(PoolType::Orca, &data).is_empty());
        assert!(extract_companion_keys(PoolType::PumpFunAmm, &data).is_empty());
    }

    #[test]
    fn test_extract_companion_keys_too_short() {
        let data = vec![0u8; 10]; // way too short
        assert!(extract_companion_keys(PoolType::RaydiumV4, &data).is_empty());
        assert!(extract_companion_keys(PoolType::Meteora, &data).is_empty());
        assert!(extract_companion_keys(PoolType::MeteoraDbc, &data).is_empty());
    }

    #[test]
    fn test_raydium_v4_with_companion_closed_market() {
        let pool_addr = Pubkey::new_unique();
        let mut pool_data = vec![0u8; 680];
        let market = Pubkey::new_unique();
        write_pubkey(&mut pool_data, 336, &Pubkey::new_unique()); // coin_vault
        write_pubkey(&mut pool_data, 368, &Pubkey::new_unique()); // pc_vault
        write_pubkey(&mut pool_data, 432, &Pubkey::new_unique()); // open_orders
        write_pubkey(&mut pool_data, 464, &market); // serum_market
        write_pubkey(&mut pool_data, 496, &Pubkey::new_unique()); // serum_program
        write_pubkey(&mut pool_data, 528, &Pubkey::new_unique()); // target_orders

        // Short market data = closed serum (< 388 bytes)
        let market_data = vec![0u8; 82];
        let market_owner = Pubkey::new_unique();

        let result = parse_raydium_v4_with_companion(&pool_addr, &pool_data, &market_data, &market_owner);
        assert!(result.is_ok());
        if let PoolState::RaydiumV4 { serum_bids, serum_asks, serum_market: sm, .. } = result.unwrap() {
            // Closed market: all serum fields = market address
            assert_eq!(serum_bids, market);
            assert_eq!(serum_asks, market);
            assert_eq!(sm, market);
        } else {
            panic!("expected RaydiumV4");
        }
    }

    #[test]
    fn test_raydium_v4_with_companion_too_small() {
        let result = parse_raydium_v4_with_companion(
            &Pubkey::new_unique(), &[0u8; 100], &[], &Pubkey::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_pumpfun_amm_with_balances() {
        let pool_addr = Pubkey::new_unique();
        let mut data = vec![0u8; 243];
        write_pubkey(&mut data, 43, &Pubkey::new_unique()); // base_mint
        write_pubkey(&mut data, 75, &Pubkey::new_unique()); // quote_mint
        write_pubkey(&mut data, 139, &Pubkey::new_unique()); // base_vault
        write_pubkey(&mut data, 171, &Pubkey::new_unique()); // quote_vault
        write_pubkey(&mut data, 211, &Pubkey::new_unique()); // coin_creator

        let result = parse_pumpfun_amm_with_balances(&pool_addr, &data, Some(1000), Some(2000));
        assert!(result.is_ok());
        if let PoolState::PumpFunAmm { base_reserve, quote_reserve, .. } = result.unwrap() {
            assert_eq!(base_reserve, 1000);
            assert_eq!(quote_reserve, 2000);
        } else {
            panic!("expected PumpFunAmm");
        }
    }

    #[test]
    fn test_pumpfun_amm_with_no_balances() {
        let pool_addr = Pubkey::new_unique();
        let mut data = vec![0u8; 243];
        write_pubkey(&mut data, 43, &Pubkey::new_unique());
        write_pubkey(&mut data, 75, &Pubkey::new_unique());
        write_pubkey(&mut data, 139, &Pubkey::new_unique());
        write_pubkey(&mut data, 171, &Pubkey::new_unique());
        write_pubkey(&mut data, 211, &Pubkey::new_unique());

        let result = parse_pumpfun_amm_with_balances(&pool_addr, &data, None, None);
        assert!(result.is_ok());
        if let PoolState::PumpFunAmm { base_reserve, quote_reserve, .. } = result.unwrap() {
            assert_eq!(base_reserve, 0);
            assert_eq!(quote_reserve, 0);
        } else {
            panic!("expected PumpFunAmm");
        }
    }

    #[test]
    fn test_meteora_with_companion() {
        let pool_addr = Pubkey::new_unique();
        let mut pool_data = vec![0u8; 298];
        let a_vault = Pubkey::new_unique();
        let b_vault = Pubkey::new_unique();
        write_pubkey(&mut pool_data, 40, &Pubkey::new_unique()); // token_a_mint
        write_pubkey(&mut pool_data, 72, &Pubkey::new_unique()); // token_b_mint
        write_pubkey(&mut pool_data, 104, &a_vault);
        write_pubkey(&mut pool_data, 136, &b_vault);
        write_pubkey(&mut pool_data, 168, &Pubkey::new_unique()); // a_vault_lp
        write_pubkey(&mut pool_data, 200, &Pubkey::new_unique()); // b_vault_lp
        write_pubkey(&mut pool_data, 234, &Pubkey::new_unique()); // admin_token_a_fee
        write_pubkey(&mut pool_data, 266, &Pubkey::new_unique()); // admin_token_b_fee

        let mut a_vault_data = vec![0u8; 147];
        let mut b_vault_data = vec![0u8; 147];
        let a_token_vault = Pubkey::new_unique();
        let b_token_vault = Pubkey::new_unique();
        write_pubkey(&mut a_vault_data, 19, &a_token_vault);
        write_pubkey(&mut a_vault_data, 115, &Pubkey::new_unique()); // a_vault_lp_mint
        write_pubkey(&mut b_vault_data, 19, &b_token_vault);
        write_pubkey(&mut b_vault_data, 115, &Pubkey::new_unique()); // b_vault_lp_mint

        let result = parse_meteora_with_companion(&pool_addr, &pool_data, &a_vault_data, &b_vault_data);
        assert!(result.is_ok());
        if let PoolState::Meteora { a_token_vault: atv, b_token_vault: btv, .. } = result.unwrap() {
            assert_eq!(atv, a_token_vault);
            assert_eq!(btv, b_token_vault);
        } else {
            panic!("expected Meteora");
        }
    }

    #[test]
    fn test_meteora_with_companion_vault_too_small() {
        let mut pool_data = vec![0u8; 298];
        for offset in [40, 72, 104, 136, 168, 200, 234, 266] {
            write_pubkey(&mut pool_data, offset, &Pubkey::new_unique());
        }
        let result = parse_meteora_with_companion(
            &Pubkey::new_unique(), &pool_data, &[0u8; 10], &[0u8; 147],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_meteora_dbc_with_companion() {
        let pool_addr = Pubkey::new_unique();
        let mut pool_data = vec![0u8; 232];
        let config_key = Pubkey::new_unique();
        let base_mint = Pubkey::new_unique();
        write_pubkey(&mut pool_data, 72, &config_key);
        write_pubkey(&mut pool_data, 136, &base_mint);
        write_pubkey(&mut pool_data, 168, &Pubkey::new_unique()); // base_vault
        write_pubkey(&mut pool_data, 200, &Pubkey::new_unique()); // quote_vault

        let mut config_data = vec![0u8; 40];
        let quote_mint = Pubkey::new_unique();
        write_pubkey(&mut config_data, 8, &quote_mint);

        let result = parse_meteora_dbc_with_companion(&pool_addr, &pool_data, &config_data);
        assert!(result.is_ok());
        if let PoolState::MeteoraDbc { base_mint: bm, quote_mint: qm, config, .. } = result.unwrap() {
            assert_eq!(bm, base_mint);
            assert_eq!(qm, quote_mint);
            assert_eq!(config, config_key);
        } else {
            panic!("expected MeteoraDbc");
        }
    }

    #[test]
    fn test_meteora_dbc_with_companion_config_too_small() {
        let mut pool_data = vec![0u8; 232];
        for offset in [72, 136, 168, 200] {
            write_pubkey(&mut pool_data, offset, &Pubkey::new_unique());
        }
        let result = parse_meteora_dbc_with_companion(
            &Pubkey::new_unique(), &pool_data, &[0u8; 10],
        );
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod pumpup_bonding_pda_tests {
    use super::*;
    use std::str::FromStr;

    /// Verifies our bonding-curve PDA derivation against a known mainnet pool.
    /// Reference pool: AQxKPt88jGP1DiwRbqweoo74Yi2o3fMTATAbDDA6BVLT
    /// Mint:           9U3FcH1Z3vZFHvN5KrkHHkuJSPKKnBBLpPQ1FkezxAai
    /// Pool token ATA: 4HDrHZexBdYyaRBhzq7d4G9cqrC4NBuerwEChnHE6Pth
    #[test]
    fn test_pumpup_pdas_match_mainnet_reference() {
        let mint = Pubkey::from_str("9U3FcH1Z3vZFHvN5KrkHHkuJSPKKnBBLpPQ1FkezxAai").unwrap();
        let expected_pool = Pubkey::from_str("AQxKPt88jGP1DiwRbqweoo74Yi2o3fMTATAbDDA6BVLT").unwrap();
        let expected_pta = Pubkey::from_str("4HDrHZexBdYyaRBhzq7d4G9cqrC4NBuerwEChnHE6Pth").unwrap();

        let pool_sol = derive_pumpup_pool_sol_account(&mint);
        assert_eq!(pool_sol, expected_pool, "pool_sol_account PDA mismatch");

        let pool_token_account =
            spl_associated_token_account::get_associated_token_address_with_program_id(
                &pool_sol, &mint, &TOKEN_PROGRAM_ID,
            );
        assert_eq!(pool_token_account, expected_pta, "pool_token_account ATA mismatch");
    }

    /// PumpupConfiguration PDA must equal the mainnet config account.
    #[test]
    fn test_pumpup_config_pda_matches_mainnet() {
        let expected = Pubkey::from_str("9yacTHL2DyPpSjaBE3yZVzUo49HWFsrwBGsdMJngqwvV").unwrap();
        assert_eq!(*PUMPUP_CONFIG_PDA, expected);
    }
}
