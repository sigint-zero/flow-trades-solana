use std::sync::LazyLock;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::constants::*;
use crate::error::{TradeError, TradeResult};
use super::types::{PoolState, PoolType};

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
        PoolType::RaydiumV4 => parse_raydium_v4(pool_address, &pool_data),
        PoolType::RaydiumCpmm => {
            let mut st = parse_raydium_cpmm(pool_address, &pool_data)?;
            if let PoolState::RaydiumCpmm { config, trade_fee_bps, creator_fee_ppm, .. } = &mut st {
                match cpmm_config_fees(rpc, config).await {
                    Ok((trade_bps, creator_ppm)) => {
                        *trade_fee_bps = trade_bps;
                        *creator_fee_ppm = creator_ppm;
                    }
                    Err(e) => debug!(pool = %pool_address, error = %e, "cpmm amm_config unreadable"),
                }
            }
            Ok(st)
        }
        PoolType::RaydiumCl => {
            let mut st = parse_raydium_clmm(pool_address, &pool_data)?;
            if let PoolState::RaydiumClmm { amm_config, tick_spacing, fee_rate, .. } = &mut st {
                *fee_rate = clmm_fee_rate_u16(rpc, amm_config, *tick_spacing, *fee_rate).await;
            }
            Ok(st)
        }
        PoolType::RaydiumLp => {
            let mut st = parse_raydium_lp(pool_address, &pool_data)?;
            if let PoolState::RaydiumLp { config_id, platform_id, curve, .. } = &mut st {
                match launchlab_fee_rates(rpc, config_id, platform_id).await {
                    Ok((ct, prot, plat, cre)) => {
                        curve.curve_type = ct;
                        curve.protocol_fee_rate = prot;
                        curve.platform_fee_rate = plat;
                        curve.creator_fee_rate = cre;
                    }
                    Err(e) => {
                        debug!(pool = %pool_address, error = %e, "launchlab fee rates unreadable; pool not quotable");
                        curve.curve_type = 255;
                    }
                }
            }
            Ok(st)
        }
        PoolType::PumpFun => parse_pumpfun(rpc, pool_address, &pool_data).await,
        PoolType::PumpFunAmm => parse_pumpfun_amm(rpc, pool_address, &pool_data).await,
        PoolType::Meteora => parse_meteora(rpc, pool_address, &pool_data).await,
        PoolType::MeteoraDlmm => parse_meteora_dlmm(pool_address, &pool_data),
        PoolType::MeteoraDamm => parse_meteora_damm(pool_address, &pool_data),
        PoolType::MeteoraDbc => parse_meteora_dbc(rpc, pool_address, &pool_data).await,
        PoolType::Orca => parse_orca(pool_address, &pool_data),
        PoolType::FluxBeam => parse_fluxbeam(pool_address, &pool_data),
        PoolType::FlashTrade => parse_flash_trade(pool_address, &pool_data),
        PoolType::Byreal => {
            let mut st = parse_byreal(pool_address, &pool_data)?;
            if let PoolState::Byreal { amm_config, tick_spacing, fee_rate, .. } = &mut st {
                *fee_rate = clmm_fee_rate_u16(rpc, amm_config, *tick_spacing, *fee_rate).await;
            }
            Ok(st)
        }
        PoolType::DefiTunaFusion => parse_defituna_fusion(pool_address, &pool_data),
        PoolType::DefiTunaPools => parse_defituna_pools(pool_address, &pool_data),
        PoolType::Saros => parse_saros(pool_address, &pool_data),
        PoolType::PancakeSwap => {
            let mut st = parse_pancakeswap(pool_address, &pool_data)?;
            if let PoolState::PancakeSwap { amm_config, tick_spacing, fee_rate, .. } = &mut st {
                *fee_rate = clmm_fee_rate_u16(rpc, amm_config, *tick_spacing, *fee_rate).await;
            }
            Ok(st)
        }
        PoolType::Dooar => parse_dooar(pool_address, &pool_data),
        PoolType::Pumpup => parse_pumpup(pool_address, &pool_data),
        PoolType::PumpupBonding => parse_pumpup_bonding(rpc, pool_address, &pool_data).await,
        _ => Err(TradeError::Execution(format!(
            "unsupported pool type for fetching: {pool_type:?}"
        ))),
    }
}

/// Parse pool state directly from raw account bytes — zero RPC.
/// Returns Ok(state) for the 15 sync pool types that only need the pool account data.
/// Returns Err for the 4 async pool types that need additional RPC calls (PumpFun
/// bonding, PumpFun AMM, Meteora Standard, Meteora DBC).
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
        PoolType::RaydiumV4 => parse_raydium_v4(pool_address, &account),
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
        PoolType::RaydiumV4 | PoolType::RaydiumCpmm | PoolType::RaydiumCl | PoolType::RaydiumLp |
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
        super::mints::record(*mint, account.owner, &account.data);
        Ok(account.owner)
    } else {
        Err(TradeError::Execution(format!(
            "mint {mint} owned by unexpected program {}",
            account.owner,
        )))
    }
}

fn read_pubkey(data: &[u8], offset: usize) -> TradeResult<Pubkey> {
    if data.len() < offset + 32 {
        return Err(TradeError::Execution(format!(
            "account data too short: need {} bytes at offset {offset}, have {}",
            offset + 32,
            data.len()
        )));
    }
    let arr: [u8; 32] = data[offset..offset + 32]
        .try_into()
        .map_err(|_| TradeError::Execution(format!("failed to read pubkey at offset {offset}")))?;
    Ok(Pubkey::new_from_array(arr))
}

// -- Raydium V4 --
// AmmInfo (752 bytes, no discriminator): status u64@0, nonce@8, … fees
// {…, swap_fee_numerator@176, swap_fee_denominator@184}, state_data
// {need_take_pnl_coin@192, need_take_pnl_pc@200, …, pool_open_time@224, …},
// coin_vault@336, pc_vault@368, coin_vault_mint@400, pc_vault_mint@432,
// lp_mint@464, open_orders@496, market@528, market_program@560,
// target_orders@592.
fn parse_raydium_v4(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if pool_data.owner != RAYDIUM_V4_PROG_ID {
        return Err(TradeError::Execution(format!("raydium v4: account owned by {}", pool_data.owner)));
    }
    if data.len() < 752 {
        return Err(TradeError::Execution("raydium v4 account too small".into()));
    }
    let rd = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    let (swap_fee_numerator, swap_fee_denominator) = (rd(176), rd(184));
    if swap_fee_denominator == 0 || swap_fee_numerator >= swap_fee_denominator {
        return Err(TradeError::Execution(format!("raydium v4 swap fee implausible: {swap_fee_numerator}/{swap_fee_denominator}")));
    }
    Ok(PoolState::RaydiumV4 {
        amm_id: *pool_address,
        authority: *RAYDIUM_V4_AUTHORITY,
        coin_vault: read_pubkey(data, 336)?,
        pc_vault: read_pubkey(data, 368)?,
        coin_mint: read_pubkey(data, 400)?,
        pc_mint: read_pubkey(data, 432)?,
        swap_fee_numerator,
        swap_fee_denominator,
        need_take_pnl_coin: rd(192),
        need_take_pnl_pc: rd(200),
        status: rd(0),
        pool_open_time: rd(224),
    })
}

// -- Raydium CPMM --
// Pool state layout (after 8-byte Anchor discriminator):
// amm_config(32), pool_creator(32), token_0_vault(32), token_1_vault(32),
// lp_mint(32), token_0_mint(32), token_1_mint(32), token_0_program(32),
// token_1_program(32), observation_key(32), auth_bump(1), ...
// NOTE: authority is NOT stored in the struct -- it's a PDA derived from seeds.
fn parse_raydium_cpmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 8 + 10 * 32 + 1 {
        return Err(TradeError::Execution("raydium cpmm account too small".into()));
    }
    let off = 8; // skip Anchor discriminator
    let config = read_pubkey(data, off)?;
    let token_0_vault = read_pubkey(data, off + 64)?;
    let token_1_vault = read_pubkey(data, off + 96)?;
    let token_0_mint = read_pubkey(data, off + 160)?;
    let token_1_mint = read_pubkey(data, off + 192)?;
    let observation = read_pubkey(data, off + 288)?;
    let u64_at = |o: usize| -> u64 {
        data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
    };
    let (protocol_fees_0, protocol_fees_1, fund_fees_0, fund_fees_1) = (u64_at(341), u64_at(349), u64_at(357), u64_at(365));
    // creator fee flags (2025 cp-swap): enable_creator_fee u8 @389, creator_fee_on u8 @390
    let enable_creator_fee = data.get(389).copied().unwrap_or(0) != 0;
    let creator_fee_on = data.get(390).copied().unwrap_or(0);

    let authority = *RAYDIUM_CPMM_AUTHORITY;

    Ok(PoolState::RaydiumCpmm {
        pool: *pool_address,
        authority,
        config,
        token_0_vault,
        token_1_vault,
        token_0_mint,
        token_1_mint,
        observation,
        trade_fee_bps: 0,
        protocol_fees_0,
        protocol_fees_1,
        fund_fees_0,
        fund_fees_1,
        creator_fee_ppm: 0,
        enable_creator_fee,
        creator_fee_on,
    })
}

// -- Raydium CLMM --
// Pool state (after 8-byte discriminator):
// bump(1), amm_config(32), owner(32), token_mint_0(32), token_mint_1(32),
// token_vault_0(32), token_vault_1(32), observation_key(32), ...
// tick_current(i32), ...
fn parse_raydium_clmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 8 + 1 + 7 * 32 {
        return Err(TradeError::Execution("raydium clmm account too small".into()));
    }
    let off = 8;
    // bump at off, skip 2 bytes (bump + padding)
    let amm_config = read_pubkey(data, off + 1)?;
    let token_mint_0 = read_pubkey(data, off + 65)?;
    let token_mint_1 = read_pubkey(data, off + 97)?;
    let token_vault_0 = read_pubkey(data, off + 129)?;
    let token_vault_1 = read_pubkey(data, off + 161)?;
    let observation = read_pubkey(data, off + 193)?;

    // tick_spacing: u16 at off+227 (after 7xPubkey + 2xu8)
    let tick_spacing = if data.len() >= off + 229 {
        u16::from_le_bytes(data[off + 227..off + 229].try_into().unwrap()) as i32
    } else {
        1
    };

    // liquidity: u128 at off+229
    let liquidity = if data.len() >= off + 245 {
        u128::from_le_bytes(data[off + 229..off + 245].try_into().unwrap())
    } else {
        0
    };

    // sqrt_price_x64: u128 at off+245
    let sqrt_price_x64 = if data.len() >= off + 261 {
        u128::from_le_bytes(data[off + 245..off + 261].try_into().unwrap())
    } else {
        0
    };

    // tick_current: i32 at off+261 (after tick_spacing + u128 liquidity + u128 sqrt_price)
    let tick_current = if data.len() >= off + 265 {
        i32::from_le_bytes(data[off + 261..off + 265].try_into().unwrap())
    } else {
        0
    };

    // Fee rate lives in the amm_config account (read by `fetch_pool_state`);
    // hundredths of a basis point, default 25 bps until read.
    let fee_rate: u16 = 2500;

    // Tick arrays are PDAs: seeds = ["tick_array", pool, start_tick_index]
    let tick_array_0 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, tick_current, tick_spacing, 0);
    let tick_array_1 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, tick_current, tick_spacing, -1);
    let tick_array_2 = derive_tick_array(&RAYDIUM_CL_PROG_ID, pool_address, tick_current, tick_spacing, 1);

    Ok(PoolState::RaydiumClmm {
        pool: *pool_address,
        amm_config,
        observation,
        token_vault_0,
        token_vault_1,
        tick_array_0,
        tick_array_1,
        tick_array_2,
        token_mint_0,
        token_mint_1,
        tick_current,
        tick_spacing,
        sqrt_price_x64,
        liquidity,
        fee_rate,
        fee_ext: crate::quote::clmm::RaydiumFeeExt::parse(data),
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
// Pool account layout (verified from mainnet, 429 bytes):
// disc(8) + params(133) + configId(32@141) + platformId(32@173)
// + mintA(32@205) + mintB(32@237) + vaultA(32@269) + vaultB(32@301)
// + creator(32@333)
fn parse_raydium_lp(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 365 {
        return Err(TradeError::Execution("raydium lp account too small".into()));
    }
    let config_id = read_pubkey(data, 141)?;
    let platform_id = read_pubkey(data, 173)?;
    let base_mint = read_pubkey(data, 205)?;
    let quote_mint = read_pubkey(data, 237)?;
    let base_vault = read_pubkey(data, 269)?;
    let quote_vault = read_pubkey(data, 301)?;
    let creator = read_pubkey(data, 333)?;

    let authority = *RAYDIUM_LP_AUTHORITY;

    // Curve (verified on mainnet, 429-byte account): status u8@17,
    // total_base_sell u64@29, virtual_base@37, virtual_quote@45, real_base@53,
    // real_quote@61, total_quote_fund_raising@69. Fee rates come from the
    // configs (`launchlab_fee_rates`).
    let rd = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    let curve = crate::quote::launchlab::LaunchLabCurve {
        status: data[17],
        curve_type: 0,
        virtual_base: rd(37),
        virtual_quote: rd(45),
        real_base: rd(53),
        real_quote: rd(61),
        total_base_sell: rd(29),
        total_quote_fund_raising: rd(69),
        protocol_fee_rate: 0,
        platform_fee_rate: 0,
        creator_fee_rate: 0,
    };

    Ok(PoolState::RaydiumLp {
        pool_state: *pool_address,
        authority,
        base_vault,
        quote_vault,
        base_mint,
        quote_mint,
        config_id,
        platform_id,
        creator,
        curve,
    })
}

/// LaunchLab GlobalConfig (371 B: curve_type u8@16, trade_fee_rate u64@27) and
/// PlatformConfig (944 B: fee_rate u64@104, creator_fee_rate u64@720), cached
/// per config account. Returns (curve_type, protocol, platform, creator).
static LAUNCHLAB_CONFIGS: LazyLock<dashmap::DashMap<Pubkey, (u8, u64, u64, u64)>> = LazyLock::new(dashmap::DashMap::new);

pub async fn launchlab_fee_rates(rpc: &RpcClient, global: &Pubkey, platform: &Pubkey) -> TradeResult<(u8, u64, u64, u64)> {
    let key = Pubkey::new_from_array({
        let mut k = [0u8; 32];
        for (i, b) in global.to_bytes().iter().zip(platform.to_bytes().iter()).enumerate() {
            k[i] = b.0 ^ b.1;
        }
        k
    });
    if let Some(v) = LAUNCHLAB_CONFIGS.get(&key) {
        return Ok(*v);
    }
    let accts = rpc.get_multiple_accounts(&[*global, *platform]).await.map_err(|e| TradeError::Rpc(format!("launchlab configs: {e}")))?;
    let g = accts[0].as_ref().ok_or_else(|| TradeError::Execution("launchlab global config missing".into()))?;
    let p = accts[1].as_ref().ok_or_else(|| TradeError::Execution("launchlab platform config missing".into()))?;
    if g.data.len() < 35 || p.data.len() < 728 {
        return Err(TradeError::Execution("launchlab config layout".into()));
    }
    let curve_type = g.data[16];
    let protocol = u64::from_le_bytes(g.data[27..35].try_into().unwrap());
    let platform_rate = u64::from_le_bytes(p.data[104..112].try_into().unwrap());
    let creator = u64::from_le_bytes(p.data[720..728].try_into().unwrap());
    if protocol > 100_000 || platform_rate > 100_000 || creator > 100_000 {
        return Err(TradeError::Execution(format!("launchlab fee rates implausible: {protocol}/{platform_rate}/{creator}")));
    }
    let v = (curve_type, protocol, platform_rate, creator);
    LAUNCHLAB_CONFIGS.insert(key, v);
    Ok(v)
}

// -- PumpFun (Bonding Curve) --
// Bonding curve layout: see `quote::pump_bonding::PumpCurve::parse`.
// NOTE: mint is NOT stored in bonding curve data. We discover it by scanning
// the bonding curve's token accounts via getTokenAccountsByOwner, once per curve.
static PUMPFUN_MINTS: LazyLock<dashmap::DashMap<Pubkey, (Pubkey, Pubkey)>> = LazyLock::new(dashmap::DashMap::new);
/// The pump.fun `Global` account (fee recipients), re-read at most every 5 min.
static PUMPFUN_GLOBAL_DATA: std::sync::RwLock<Option<(std::time::Instant, Vec<u8>)>> = std::sync::RwLock::new(None);
const PUMPFUN_GLOBAL_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(300);
/// First of `Global.buyback_fee_recipients` — used when the Global is unreadable.
static PUMPFUN_BUYBACK_FALLBACK: LazyLock<Pubkey> = LazyLock::new(|| pubkey_from_str("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"));

async fn pumpfun_global(rpc: &RpcClient) -> TradeResult<Vec<u8>> {
    if let Some((at, d)) = PUMPFUN_GLOBAL_DATA.read().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if at.elapsed() < PUMPFUN_GLOBAL_MAX_AGE {
            return Ok(d.clone());
        }
    }
    let d = fetch_account(rpc, &PUMPFUN_GLOBAL).await?.data;
    *PUMPFUN_GLOBAL_DATA.write().unwrap_or_else(|p| p.into_inner()) = Some((std::time::Instant::now(), d.clone()));
    Ok(d)
}

/// `(fee_recipient, buyback_fee_recipient)` for a curve, from the `Global`
/// account (verified on mainnet, 1087 bytes): mayhem-mode curves pay the
/// reserved recipient (@483), the others the main one (@41); the buyback
/// recipient is one of eight (@741), picked per mint to spread write locks.
fn pumpfun_recipients(global: &[u8], mayhem: bool, mint: &Pubkey) -> (Pubkey, Pubkey) {
    let fee = read_pubkey(global, if mayhem { 483 } else { 41 }).ok().filter(|k| *k != Pubkey::default()).unwrap_or(*PUMPFUN_FEE_FALLBACK);
    let buyback = read_pubkey(global, 741 + 32 * (mint.to_bytes()[0] as usize % 8))
        .ok()
        .filter(|k| *k != Pubkey::default())
        .unwrap_or(*PUMPFUN_BUYBACK_FALLBACK);
    (fee, buyback)
}

/// Build a `PumpFun` state from the curve account + its (known) mint.
fn pumpfun_state(pool_address: &Pubkey, data: &[u8], mint: Pubkey, token_prog: Pubkey, global_data: &[u8]) -> TradeResult<PoolState> {
    let curve = crate::quote::pump_bonding::PumpCurve::parse(data)
        .ok_or_else(|| TradeError::Execution("pumpfun: bonding curve account too small".into()))?;
    // Creator at offset 49 (used for the creator_vault PDA)
    let creator = if data.len() >= 81 { read_pubkey(data, 49)? } else { Pubkey::default() };
    let associated_bonding_curve = spl_associated_token_account::get_associated_token_address_with_program_id(pool_address, &mint, &token_prog);
    // no Global handed in (Geyser companion not cached yet): the last one read
    let cached;
    let global_data = if !global_data.is_empty() {
        global_data
    } else {
        cached = PUMPFUN_GLOBAL_DATA.read().unwrap_or_else(|p| p.into_inner()).as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
        &cached
    };
    let (fee_account, buyback_fee_recipient) = pumpfun_recipients(global_data, curve.is_mayhem_mode, &mint);
    Ok(PoolState::PumpFun {
        global: *PUMPFUN_GLOBAL,
        fee_account,
        mint,
        bonding_curve: *pool_address,
        associated_bonding_curve,
        event_authority: *PUMPFUN_EVENT_AUTHORITY,
        creator,
        curve,
        buyback_fee_recipient,
    })
}

async fn parse_pumpfun(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    use solana_client::rpc_request::TokenAccountsFilter;

    let known = PUMPFUN_MINTS.get(pool_address).map(|v| *v);
    let (mint, token_prog) = match known {
        Some(v) => v,
        None => {
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
            // Anyone can send tokens to the curve: the mint is the one whose
            // `["bonding-curve", mint]` PDA is this account.
            let mint = token_accounts
                .into_iter()
                .chain(token_2022_accounts)
                .filter_map(|ta| match &ta.account.data {
                    solana_account_decoder::UiAccountData::Json(parsed) => parsed.parsed["info"]["mint"].as_str().and_then(|m| m.parse::<Pubkey>().ok()),
                    _ => None,
                })
                .find(|m| Pubkey::find_program_address(&[b"bonding-curve", m.as_ref()], &PUMP_FUN_PROG_ID).0 == *pool_address)
                .ok_or_else(|| TradeError::Execution("pumpfun: no token account of the curve's mint".into()))?;
            let token_prog = fetch_account(rpc, &mint).await?.owner;
            PUMPFUN_MINTS.insert(*pool_address, (mint, token_prog));
            (mint, token_prog)
        }
    };
    let global_data = pumpfun_global(rpc).await?;
    pumpfun_state(pool_address, &pool_data.data, mint, token_prog, &global_data)
}

// -- PumpFun AMM (pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA) --
// Pool account layout (verified from mainnet):
// 0-7:   Anchor discriminator (account:Pool)
// 8:     pool_bump (1)
// 9-10:  index (u16)
// 11-42: creator (32)
// 43-74: base_mint (32)
// 75-106: quote_mint (32)
// 107-138: lp_mint (32)
// 139-170: pool_base_vault (32)
// 171-202: pool_quote_vault (32)
// 203-210: lp_supply (u64)
// 211-242: coin_creator (32)
/// Parse PumpFun AMM pool layout (sync, no RPC needed).
fn parse_pumpfun_amm_layout(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 243 {
        return Err(TradeError::Execution("pumpfun amm account too small".into()));
    }
    let coin_creator = read_pubkey(data, 211)?;
    let base_mint = read_pubkey(data, 43)?;
    let quote_mint = read_pubkey(data, 75)?;
    let pool_base_vault = read_pubkey(data, 139)?;
    let pool_quote_vault = read_pubkey(data, 171)?;

    Ok(PoolState::PumpFunAmm {
        pool: *pool_address,
        base_mint,
        quote_mint,
        pool_base_vault,
        pool_quote_vault,
        coin_creator,
        base_reserve: 0,
        quote_reserve: 0,
        protocol_fee_recipient: Pubkey::default(),
        buyback_accounts: Vec::new(),
        base_supply: 0,
            virtual_quote_reserve: if data.len() >= 253 { u64::from_le_bytes(data[245..253].try_into().unwrap()) } else { 0 },
    })
}

/// Resolve, from a RECENT pAMM swap on this pool, (a) the currently valid
/// `protocol_fee_recipient` (account[9] — pump.fun rotates it) and (b) the
/// pump_fees buyback "remaining accounts": every account AFTER the fee program
/// (`pfeeUxB…`) in that swap, with its writability. They are per-pool/creator
/// buyback vault(s) + ATAs (count varies, some Token-2022), not derivable PDAs,
/// so on-chain truth is copied verbatim. Returns `(zero, empty)` if none found.
///
/// Reads at CONFIRMED commitment: a fresh pool's recent swaps are confirmed but
/// not yet finalized, and `getTransaction` at finalized returns null for all of
/// them. Prefers a successful swap but falls back to a failed one — the
/// buyback set is valid regardless of why a swap reverted (usually slippage),
/// and a fresh pool often has only errored recent txs. Retries with backoff:
/// a transient `getSignatures`/`getTransaction` error must not leave the pool
/// unresolved.
pub async fn resolve_pamm_fee_accounts(rpc: &RpcClient, pool: &Pubkey) -> (Pubkey, Vec<(Pubkey, bool)>) {
    use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_transaction_status_client_types::{
        EncodedTransaction, UiInstruction, UiMessage, UiParsedInstruction, UiTransactionEncoding,
    };
    use std::str::FromStr;

    let commit = CommitmentConfig::confirmed();
    let pamm = PUMP_FUN_AMM_PROG_ID.to_string();
    let fee_prog = crate::execution::amms::pumpfun_amm::FEE_PROGRAM.to_string();
    let empty = (Pubkey::default(), Vec::new());

    for attempt in 0..4u32 {
        let sigs = match rpc
            .get_signatures_for_address_with_config(
                pool,
                GetConfirmedSignaturesForAddress2Config {
                    limit: Some(25),
                    commitment: Some(commit),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                continue;
            }
        };
        let mut fallback: Option<(Pubkey, Vec<(Pubkey, bool)>)> = None;
        let mut fetched = 0u32;
        for si in sigs {
            let errored = si.err.is_some();
            let sig = match solana_sdk::signature::Signature::from_str(&si.signature) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Other traders' swaps: any transaction version can turn up, so the
            // request declares the crate-wide maximum (see stream::tx_version).
            let cfg = crate::stream::tx_version::transaction_config(UiTransactionEncoding::JsonParsed, commit);
            let tx = match rpc.get_transaction_with_config(&sig, cfg).await {
                Ok(t) => t,
                Err(e) => {
                    if crate::stream::tx_version::is_version_refusal(&e) {
                        tracing::warn!(pool = %pool, signature = %sig, error = %e,
                            "getTransaction refused the transaction version — raise MAX_SUPPORTED_TX_VERSION");
                    }
                    continue;
                }
            };
            fetched += 1;
            let parsed = match tx.transaction.transaction {
                EncodedTransaction::Json(u) => match u.message {
                    UiMessage::Parsed(p) => p,
                    _ => continue,
                },
                _ => continue,
            };
            let writable: std::collections::HashMap<String, bool> = parsed
                .account_keys
                .iter()
                .map(|k| (k.pubkey.clone(), k.writable))
                .collect();
            let mut all = parsed.instructions.clone();
            if let Some(meta) = tx.transaction.meta {
                if let solana_transaction_status_client_types::option_serializer::OptionSerializer::Some(inner) =
                    meta.inner_instructions
                {
                    for ii in inner {
                        all.extend(ii.instructions);
                    }
                }
            }
            for ix in &all {
                let UiInstruction::Parsed(UiParsedInstruction::PartiallyDecoded(p)) = ix else { continue };
                if p.program_id != pamm || p.accounts.len() < 23 {
                    continue;
                }
                let Some(fp_idx) = p.accounts.iter().position(|a| *a == fee_prog) else { continue };
                if fp_idx + 1 >= p.accounts.len() {
                    continue;
                }
                let recipient = Pubkey::from_str(&p.accounts[9]).unwrap_or_default();
                let remaining: Vec<(Pubkey, bool)> = p.accounts[fp_idx + 1..]
                    .iter()
                    .filter_map(|a| Pubkey::from_str(a).ok().map(|pk| (pk, *writable.get(a).unwrap_or(&true))))
                    .collect();
                if remaining.is_empty() {
                    continue;
                }
                if !errored {
                    return (recipient, remaining);
                }
                if fallback.is_none() {
                    fallback = Some((recipient, remaining));
                }
            }
            if fallback.is_some() && fetched >= 6 {
                break; // have valid (failed-tx) accounts; cap RPC load
            }
        }
        if let Some(fb) = fallback {
            return fb;
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    }
    empty
}

/// Fill in the pump.fun AMM fee/buyback accounts on `state` if they are still
/// unresolved. Returns true when the state now carries a buyback set. No-op
/// (returns true) for every other variant and for an already-resolved pool.
pub async fn ensure_pamm_fee_accounts(rpc: &RpcClient, state: &mut PoolState) -> bool {
    if !state.needs_pamm_fee_accounts() {
        return true;
    }
    let PoolState::PumpFunAmm { pool, base_mint, protocol_fee_recipient, buyback_accounts, base_supply, .. } = state else {
        return true;
    };
    let (fee, supply) = tokio::join!(resolve_pamm_fee_accounts(rpc, pool), fetch_mint_supply(rpc, base_mint));
    if *base_supply == 0 {
        *base_supply = supply.unwrap_or(0);
    }
    let (recipient, remaining) = fee;
    if remaining.is_empty() {
        debug!(pool = %pool, "pumpfun amm: buyback accounts unresolved (no recent swap on the pool)");
        return false;
    }
    *protocol_fee_recipient = recipient;
    *buyback_accounts = remaining;
    true
}

/// Re-read a pump.fun AMM pool's vault balances into `state` (2 parallel RPC
/// calls). Everything else — mints, vaults, the resolved fee/buyback accounts —
/// is kept. Used on the swap path when the cached state is older than a few
/// seconds: without Geyser nothing refreshes reserves after discovery, and a
/// pAMM buy is exact-output, so stale reserves turn into `ExceededSlippage`
/// (6004) on a moving pool. No-op for other variants.
pub async fn refresh_pamm_reserves(rpc: &RpcClient, state: &mut PoolState) -> TradeResult<()> {
    let PoolState::PumpFunAmm { pool_base_vault, pool_quote_vault, base_reserve, quote_reserve, .. } = state else {
        return Ok(());
    };
    let (b, q) = tokio::try_join!(
        fetch_token_balance(rpc, pool_base_vault),
        fetch_token_balance(rpc, pool_quote_vault),
    )?;
    *base_reserve = b;
    *quote_reserve = q;
    Ok(())
}

/// Venues whose price lives in the pool account itself (not in vault
/// balances): a block that touches them needs the account re-read.
pub fn is_state_priced(pool_type: PoolType) -> bool {
    matches!(
        pool_type,
        PoolType::RaydiumCl | PoolType::Orca | PoolType::PancakeSwap | PoolType::Byreal | PoolType::DefiTunaFusion | PoolType::MeteoraDamm | PoolType::RaydiumLp
            | PoolType::MeteoraDlmm
            | PoolType::PumpFun | PoolType::MeteoraDbc
    )
}

/// Re-parse a state-priced pool from an account already fetched in a batch
/// (`getMultipleAccounts`). Fee rates come from the per-config cache filled by
/// the first full fetch; `prev` supplies them if the cache is cold.
pub fn reparse_pool_state(pool_type: PoolType, pool_address: &Pubkey, account: &Account, prev: Option<&PoolState>) -> TradeResult<PoolState> {
    let mut st = match pool_type {
        PoolType::RaydiumCl => parse_raydium_clmm(pool_address, account)?,
        PoolType::Orca => parse_orca(pool_address, account)?,
        PoolType::PancakeSwap => parse_pancakeswap(pool_address, account)?,
        PoolType::Byreal => parse_byreal(pool_address, account)?,
        PoolType::DefiTunaFusion => parse_defituna_fusion(pool_address, account)?,
        PoolType::MeteoraDamm => parse_meteora_damm(pool_address, account)?,
        PoolType::RaydiumLp => parse_raydium_lp(pool_address, account)?,
        PoolType::MeteoraDlmm => parse_meteora_dlmm(pool_address, account)?,
        // not state-priced (vault balances), but its PnL / status live in the pool
        PoolType::RaydiumV4 => parse_raydium_v4(pool_address, account)?,
        // Bonding curves: the account carries price + reserves; mint, config
        // and fee recipients come from the full fetch (`prev`).
        PoolType::PumpFun => {
            let Some(PoolState::PumpFun { mint, associated_bonding_curve, fee_account, buyback_fee_recipient, .. }) = prev else {
                return Err(TradeError::Execution("pumpfun: re-parse needs the fetched state".into()));
            };
            let token_prog = PUMPFUN_MINTS.get(pool_address).map(|v| v.1).unwrap_or(TOKEN_PROGRAM_ID);
            // fee recipients from the cached Global when there is one (a curve
            // can enter mayhem mode), else from the fetched state
            let mut st = pumpfun_state(pool_address, &account.data, *mint, token_prog, &[])?;
            if let PoolState::PumpFun { associated_bonding_curve: abc, fee_account: fee, buyback_fee_recipient: bb, .. } = &mut st {
                if PUMPFUN_GLOBAL_DATA.read().unwrap_or_else(|p| p.into_inner()).is_none() {
                    (*fee, *bb) = (*fee_account, *buyback_fee_recipient);
                }
                *abc = *associated_bonding_curve;
            }
            if matches!(st, PoolState::PumpFun { buyback_fee_recipient, .. } if buyback_fee_recipient == Pubkey::default()) {
                return Err(TradeError::Execution("pumpfun: state predates the buyback recipient; needs a full fetch".into()));
            }
            st
        }
        PoolType::MeteoraDbc => {
            // a state without its config (older warm file) is left to age into
            // a full re-fetch instead of being refreshed as unquotable
            let Some(PoolState::MeteoraDbc { quote_mint, curve, .. }) = prev.filter(|p| matches!(p, PoolState::MeteoraDbc { curve, .. } if !curve.config.curve.is_empty())) else {
                return Err(TradeError::Execution("meteora dbc: re-parse needs the fetched state".into()));
            };
            dbc_state(pool_address, &account.data, *quote_mint, curve.config.clone())?
        }
        other => return Err(TradeError::Execution(format!("{other:?} is not state-priced"))),
    };
    match (&mut st, prev) {
        (PoolState::RaydiumLp { curve, .. }, Some(PoolState::RaydiumLp { curve: prev_curve, .. })) => {
            curve.curve_type = prev_curve.curve_type;
            curve.protocol_fee_rate = prev_curve.protocol_fee_rate;
            curve.platform_fee_rate = prev_curve.platform_fee_rate;
            curve.creator_fee_rate = prev_curve.creator_fee_rate;
        }
        (PoolState::RaydiumClmm { fee_rate, .. }, Some(PoolState::RaydiumClmm { fee_rate: prev_fee, .. }))
        | (PoolState::PancakeSwap { fee_rate, .. }, Some(PoolState::PancakeSwap { fee_rate: prev_fee, .. }))
        | (PoolState::Byreal { fee_rate, .. }, Some(PoolState::Byreal { fee_rate: prev_fee, .. })) => *fee_rate = *prev_fee,
        _ => {}
    }
    Ok(st)
}

/// Raydium-style CLMM `AmmConfig.trade_fee_rate` as hundredths of a basis
/// point (ppm) in the pool state's `fee_rate: u16`; keeps `fallback` (also
/// ppm) if the config cannot be read.
async fn clmm_fee_rate_u16(rpc: &RpcClient, config: &Pubkey, tick_spacing: i32, fallback: u16) -> u16 {
    match super::ticks::clmm_config_fee_ppm(rpc, config, tick_spacing).await {
        Ok(ppm) => u16::try_from(ppm).unwrap_or(u16::MAX),
        Err(e) => {
            debug!(%config, error = %e, "clmm amm_config fee unreadable; using fallback");
            fallback
        }
    }
}

/// Raydium CPMM `AmmConfig.trade_fee_rate` (offset 12, u64, 1e6 denominator)
/// as bps. Configs are few (~21) and immutable in practice, so they are cached
/// for the process lifetime.
static CPMM_CONFIG_FEES: std::sync::LazyLock<dashmap::DashMap<Pubkey, (u16, u32)>> = std::sync::LazyLock::new(dashmap::DashMap::new);

/// `(trade_fee_bps, creator_fee_ppm)` from a CPMM `AmmConfig` (trade_fee_rate
/// u64 @12, creator_fee_rate u64 @108 — 1e6 denominator), cached per config.
pub async fn cpmm_config_fees(rpc: &RpcClient, config: &Pubkey) -> TradeResult<(u16, u32)> {
    if let Some(f) = CPMM_CONFIG_FEES.get(config) {
        return Ok(*f);
    }
    let acct = fetch_account(rpc, config).await?;
    let d = &acct.data;
    if d.len() < 20 {
        return Err(TradeError::Execution("cpmm amm_config too small".into()));
    }
    let rate = u64::from_le_bytes(d[12..20].try_into().unwrap());
    let bps = u16::try_from(rate / 100).map_err(|_| TradeError::Execution("cpmm fee out of range".into()))?;
    if bps == 0 || bps > 5_000 {
        return Err(TradeError::Execution(format!("cpmm trade_fee_rate implausible: {rate}")));
    }
    let creator = if d.len() >= 116 { u64::from_le_bytes(d[108..116].try_into().unwrap()) } else { 0 };
    let creator = u32::try_from(creator).ok().filter(|c| *c <= 100_000).unwrap_or(0);
    CPMM_CONFIG_FEES.insert(*config, (bps, creator));
    Ok((bps, creator))
}

pub async fn cpmm_config_fee_bps(rpc: &RpcClient, config: &Pubkey) -> TradeResult<u16> {
    cpmm_config_fees(rpc, config).await.map(|(b, _)| b)
}

/// Parse PumpFun AMM pool and fetch vault reserves for swap computation.
/// The fee/buyback accounts are resolved in parallel with the reserve fetch.
async fn parse_pumpfun_amm(rpc: &RpcClient, pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let mut state = parse_pumpfun_amm_layout(pool_address, pool_data)?;
    if let PoolState::PumpFunAmm {
        ref base_mint, ref pool_base_vault, ref pool_quote_vault,
        ref mut base_reserve, ref mut quote_reserve,
        ref mut protocol_fee_recipient, ref mut buyback_accounts, ref mut base_supply, ..
    } = state {
        let (bal, (recipient, remaining), supply) = tokio::join!(
            async {
                tokio::try_join!(
                    fetch_token_balance(rpc, pool_base_vault),
                    fetch_token_balance(rpc, pool_quote_vault),
                )
            },
            resolve_pamm_fee_accounts(rpc, pool_address),
            fetch_mint_supply(rpc, base_mint),
        );
        if let Ok((b, q)) = bal {
            *base_reserve = b;
            *quote_reserve = q;
        }
        *protocol_fee_recipient = recipient;
        *buyback_accounts = remaining;
        // The fee tier is keyed by market cap = price × supply; 0 = unknown →
        // the quoter assumes the most expensive tier (see pumpfun_amm).
        *base_supply = supply.unwrap_or_else(|e| {
            debug!(pool = %pool_address, error = %e, "pumpfun amm: base supply unavailable");
            0
        });
    }
    Ok(state)
}

/// Total supply of a mint in atoms.
pub async fn fetch_mint_supply(rpc: &RpcClient, mint: &Pubkey) -> TradeResult<u64> {
    let s = rpc
        .get_token_supply(mint)
        .await
        .map_err(|e| TradeError::Execution(format!("get_token_supply {mint}: {e}")))?;
    s.amount.parse::<u64>().map_err(|e| TradeError::Execution(format!("parse supply: {e}")))
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
// Pool layout (verified from mainnet, program Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB):
// disc(8) + lp_mint(32@8) + token_a_mint(32@40) + token_b_mint(32@72)
// + a_vault(32@104) + b_vault(32@136) + a_vault_lp(32@168) + b_vault_lp(32@200)
// + a_vault_lp_bump(1@232) + enabled(1@233) + admin_token_a_fee(32@234) + admin_token_b_fee(32@266)
//
// NOTE: a_token_vault, b_token_vault, a_vault_lp_mint, b_vault_lp_mint are stored
// inside the vault accounts (program 24Uqj9...), NOT in the pool account.
// Vault layout: disc(8) + enabled(1@8) + bump(1@9) + flag(1@10) + total_amount(u64@11)
// + token_vault(32@19) + fee_vault(32@51) + token_mint(32@83) + lp_mint(32@115)
async fn parse_meteora(
    rpc: &RpcClient,
    pool_address: &Pubkey,
    pool_data: &Account,
) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 298 {
        return Err(TradeError::Execution("meteora pool account too small".into()));
    }

    let token_a_mint = read_pubkey(data, 40)?;
    let token_b_mint = read_pubkey(data, 72)?;
    let a_vault = read_pubkey(data, 104)?;
    let b_vault = read_pubkey(data, 136)?;
    let a_vault_lp = read_pubkey(data, 168)?;
    let b_vault_lp = read_pubkey(data, 200)?;
    let admin_token_a_fee = read_pubkey(data, 234)?;
    let admin_token_b_fee = read_pubkey(data, 266)?;

    // Fetch vault accounts in parallel to get token_vaults and lp_mints
    let (a_vault_data, b_vault_data) = tokio::try_join!(
        fetch_account(rpc, &a_vault),
        fetch_account(rpc, &b_vault),
    )?;

    if a_vault_data.data.len() < 147 || b_vault_data.data.len() < 147 {
        return Err(TradeError::Execution("meteora vault account too small".into()));
    }

    let a_token_vault = read_pubkey(&a_vault_data.data, 19)?;
    let b_token_vault = read_pubkey(&b_vault_data.data, 19)?;
    let a_vault_lp_mint = read_pubkey(&a_vault_data.data, 115)?;
    let b_vault_lp_mint = read_pubkey(&b_vault_data.data, 115)?;

    let vault_program = *METEORA_VAULT_PROGRAM;

    // Reserves = the pool's LP share of each vault (2 lp mints + 2 lp accounts).
    let (trade_fee_numerator, trade_fee_denominator, constant_product) = crate::quote::meteora_std::parse_pool_fees(data).unwrap_or((0, 0, false));
    let (protocol_fee_numerator, protocol_fee_denominator) = crate::quote::meteora_std::parse_protocol_fee(data).unwrap_or((0, 0));
    let mut reserves = crate::quote::meteora_std::MeteoraStdReserves {
        trade_fee_numerator, trade_fee_denominator, constant_product, protocol_fee_numerator, protocol_fee_denominator, ..Default::default()
    };
    if constant_product {
        let extra = rpc.get_multiple_accounts(&[a_vault_lp_mint, b_vault_lp_mint, a_vault_lp, b_vault_lp]).await
            .map_err(|e| TradeError::Rpc(format!("meteora vault lp accounts: {e}")))?;
        meteora_std_update(&mut reserves, &a_vault_data.data, &b_vault_data.data, &extra);
    }

    Ok(PoolState::Meteora {
        pool: *pool_address,
        token_a_mint,
        token_b_mint,
        a_vault,
        b_vault,
        a_token_vault,
        b_token_vault,
        a_vault_lp_mint,
        b_vault_lp_mint,
        a_vault_lp,
        b_vault_lp,
        admin_token_a_fee,
        admin_token_b_fee,
        vault_program,
        reserves,
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Refresh a Meteora Standard pool's vault shares from vault a/b data and
/// `[lp_mint_a, lp_mint_b, pool_lp_a, pool_lp_b]` accounts.
fn meteora_std_update(reserves: &mut crate::quote::meteora_std::MeteoraStdReserves, vault_a: &[u8], vault_b: &[u8], extra: &[Option<Account>]) -> bool {
    if extra.len() < 4 {
        return false;
    }
    let lp = |i: usize| extra[i].as_ref().map(|a| a.data.as_slice());
    reserves.update(vault_a, vault_b, [lp(0), lp(1), lp(2), lp(3)], unix_now())
}

/// Keys to batch-read for a Meteora Standard reserve refresh, in the order
/// `refresh_meteora_std_from` expects: [a_vault, b_vault, lp_mint_a, lp_mint_b, pool_lp_a, pool_lp_b].
pub fn meteora_std_refresh_keys(state: &PoolState) -> Option<[Pubkey; 6]> {
    match state {
        PoolState::Meteora { a_vault, b_vault, a_vault_lp_mint, b_vault_lp_mint, a_vault_lp, b_vault_lp, .. } => {
            Some([*a_vault, *b_vault, *a_vault_lp_mint, *b_vault_lp_mint, *a_vault_lp, *b_vault_lp])
        }
        _ => None,
    }
}

/// Recompute a Meteora Standard pool's reserves from freshly fetched accounts
/// (same order as `meteora_std_refresh_keys`).
pub fn refresh_meteora_std_from(state: &mut PoolState, accounts: &[Option<Account>]) -> bool {
    let PoolState::Meteora { reserves, .. } = state else { return false };
    if accounts.len() < 6 {
        return false;
    }
    let (Some(va), Some(vb)) = (&accounts[0], &accounts[1]) else { return false };
    meteora_std_update(reserves, &va.data, &vb.data, &accounts[2..6])
}

// -- Meteora DLMM --
// LbPair layout (after 8-byte Anchor discriminator):
// parameters (StaticParameters, 32), v_parameters (VariableParameters, 32),
// bump_seed(1), bin_step_seed(2), pair_type(1), active_id(4), bin_step(2),
// status(1), require_base_factor_seed(1), base_factor_seed(2), activation_type(1), _pad(1),
// token_x_mint(32), token_y_mint(32), reserve_x(32), reserve_y(32),
// protocol_fee(16), _padding_1(32), reward_infos(2x144=288), oracle(32), ...
// Fee parameters, active bin and bitmap for quoting: `quote::dlmm::DlmmPair`.
fn parse_meteora_dlmm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 584 {
        return Err(TradeError::Execution("meteora dlmm account too small".into()));
    }
    let off = 8; // skip Anchor discriminator
    // active_id at off + 68 (32+32+1+2+1 = 68)
    let active_id = i32::from_le_bytes(data[off + 68..off + 72].try_into().unwrap());
    // token_x_mint at off + 80
    let token_x_mint = read_pubkey(data, off + 80)?;
    let token_y_mint = read_pubkey(data, off + 112)?;
    let reserve_x = read_pubkey(data, off + 144)?;
    let reserve_y = read_pubkey(data, off + 176)?;
    // oracle at off + 544 (176+32+16+32+288 = 544)
    let oracle = read_pubkey(data, off + 544)?;

    // bin_array_bitmap_extension: None -> use DLMM program ID as placeholder
    let bin_array_bitmap_extension = METEORA_DLMM_PROG_ID;

    // host_fee_in: None -> use DLMM program ID as placeholder
    let host_fee_in = METEORA_DLMM_PROG_ID;

    let event_authority = *METEORA_DLMM_EVENT_AUTHORITY;

    // Derive bin array PDAs from active_id
    // MAX_BIN_PER_ARRAY = 70
    let bin_idx = active_id.div_euclid(70);
    let bin_arrays: Vec<Pubkey> = [bin_idx, bin_idx - 1, bin_idx + 1]
        .iter()
        .map(|&idx| {
            let idx_bytes = (idx as i64).to_le_bytes();
            let (pda, _) = Pubkey::find_program_address(
                &[b"bin_array", pool_address.as_ref(), &idx_bytes],
                &METEORA_DLMM_PROG_ID,
            );
            pda
        })
        .collect();

    Ok(PoolState::MeteoraDlmm {
        lb_pair: *pool_address,
        bin_array_bitmap_extension,
        reserve_x,
        reserve_y,
        token_x_mint,
        token_y_mint,
        oracle,
        host_fee_in,
        event_authority,
        bin_arrays,
        pair: crate::quote::dlmm::DlmmPair::parse(data).unwrap_or_default(),
    })
}

// -- Meteora DAMM (cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG) --
// Pool account layout (verified from mainnet, 1112 bytes):
// disc(8) + pool_fees(160@8) + token_a_mint(32@168) + token_b_mint(32@200)
// + token_a_vault(32@232) + token_b_vault(32@264)
fn parse_meteora_damm(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 296 {
        return Err(TradeError::Execution("meteora damm account too small".into()));
    }
    let token_a_mint = read_pubkey(data, 168)?;
    let token_b_mint = read_pubkey(data, 200)?;
    let token_a_vault = read_pubkey(data, 232)?;
    let token_b_vault = read_pubkey(data, 264)?;
    // Curve (verified on mainnet): liquidity u128@360 (×2^64), sqrt_min@424,
    // sqrt_max@440, sqrt_price@456, activation_point u64@472, activation_type
    // u8@480, pool_status@481, collect_fee_mode@484, tracked reserves
    // token_a_amount / token_b_amount u64@680/688 (compounding pools' curve).
    let rd128 = |o: usize| if data.len() >= o + 16 { u128::from_le_bytes(data[o..o + 16].try_into().unwrap()) } else { 0 };
    let (liquidity, sqrt_min_price, sqrt_max_price, sqrt_price) = (rd128(360), rd128(424), rd128(440), rd128(456));
    let rd64 = |o: usize| if data.len() >= o + 8 { u64::from_le_bytes(data[o..o + 8].try_into().unwrap()) } else { 0 };
    let (token_a_amount, token_b_amount) = (rd64(680), rd64(688));
    let activation_point = if data.len() >= 480 { u64::from_le_bytes(data[472..480].try_into().unwrap()) } else { 0 };
    let (activation_type, pool_status, collect_fee_mode) = if data.len() >= 485 { (data[480], data[481], data[484]) } else { (0, 0, 0) };
    let fees = crate::quote::damm_v2::DammFees::parse(data).unwrap_or_default();

    Ok(PoolState::MeteoraDamm {
        pool: *pool_address,
        token_a_vault,
        token_b_vault,
        token_a_mint,
        token_b_mint,
        liquidity,
        sqrt_price,
        sqrt_min_price,
        sqrt_max_price,
        token_a_amount,
        token_b_amount,
        fees,
        activation_point,
        activation_type,
        collect_fee_mode,
        pool_status,
    })
}

// -- Meteora DBC (dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN) --
// VirtualPool layout (verified from mainnet):
//  72:  config (Pubkey)
//  136: base_mint (Pubkey)
//  168: base_vault (Pubkey)
//  200: quote_vault (Pubkey)
//  price/reserves: see `quote::dbc::DbcCurve::apply_pool`
// PoolConfig layout (1048 bytes):
//   8:  quote_mint (Pubkey); curve + fees: see `quote::dbc::DbcConfig::parse`
/// PoolConfig → (quote_mint, curve + fees). Configs are immutable.
static DBC_CONFIGS: LazyLock<dashmap::DashMap<Pubkey, (Pubkey, crate::quote::dbc::DbcConfig)>> = LazyLock::new(dashmap::DashMap::new);

fn dbc_config(config_key: &Pubkey, config_data: &[u8]) -> TradeResult<(Pubkey, crate::quote::dbc::DbcConfig)> {
    if let Some(c) = DBC_CONFIGS.get(config_key) {
        return Ok(c.clone());
    }
    if config_data.len() < 40 {
        return Err(TradeError::Execution("meteora dbc config too small".into()));
    }
    let quote_mint = read_pubkey(config_data, 8)?;
    // An unparseable config leaves the curve empty: the pool is not quoted.
    let cfg = crate::quote::dbc::DbcConfig::parse(config_data).unwrap_or_default();
    if !cfg.curve.is_empty() {
        DBC_CONFIGS.insert(*config_key, (quote_mint, cfg.clone()));
    }
    Ok((quote_mint, cfg))
}

fn dbc_state(pool_address: &Pubkey, data: &[u8], quote_mint: Pubkey, config: crate::quote::dbc::DbcConfig) -> TradeResult<PoolState> {
    if data.len() < 232 {
        return Err(TradeError::Execution("meteora dbc pool too small".into()));
    }
    let mut curve = crate::quote::dbc::DbcCurve { config, ..Default::default() };
    curve.apply_pool(data);
    Ok(PoolState::MeteoraDbc {
        pool: *pool_address,
        config: read_pubkey(data, 72)?,
        pool_authority: *METEORA_DBC_POOL_AUTHORITY,
        base_vault: read_pubkey(data, 168)?,
        quote_vault: read_pubkey(data, 200)?,
        base_mint: read_pubkey(data, 136)?,
        quote_mint,
        curve,
    })
}

async fn parse_meteora_dbc(rpc: &RpcClient, pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 232 {
        return Err(TradeError::Execution("meteora dbc pool too small".into()));
    }
    let config_key = read_pubkey(data, 72)?;
    let (quote_mint, config) = match DBC_CONFIGS.get(&config_key) {
        Some(c) => c.clone(),
        None => dbc_config(&config_key, &fetch_account(rpc, &config_key).await?.data)?,
    };
    dbc_state(pool_address, data, quote_mint, config)
}

// -- Orca Whirlpool --
fn parse_orca(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 8 + 9 * 32 {
        return Err(TradeError::Execution("orca whirlpool account too small".into()));
    }
    // Whirlpool layout (after 8-byte discriminator):
    // config(32), bump(1), tick_spacing(2), tick_spacing_seed(2),
    // fee_rate(2), protocol_fee_rate(2), liquidity(16), sqrt_price(16),
    // tick_current_index(4), protocol_fee_owed_a(8), protocol_fee_owed_b(8),
    // token_mint_a(32), token_vault_a(32), fee_growth_global_a(16),
    // token_mint_b(32), token_vault_b(32), fee_growth_global_b(16)
    let off = 8;
    let token_mint_a = read_pubkey(data, off + 93)?;
    let token_vault_a = read_pubkey(data, off + 125)?;
    let token_mint_b = read_pubkey(data, off + 173)?;
    let token_vault_b = read_pubkey(data, off + 205)?;

    // tick_spacing: u16 at off+33
    let tick_spacing = if data.len() >= off + 35 {
        u16::from_le_bytes(data[off + 33..off + 35].try_into().unwrap()) as i32
    } else {
        1
    };

    // fee_rate: u16 at off+37 (in hundredths of a basis point)
    let fee_rate = if data.len() >= off + 39 {
        u16::from_le_bytes(data[off + 37..off + 39].try_into().unwrap())
    } else {
        0
    };

    // liquidity: u128 at off+41
    let liquidity = if data.len() >= off + 57 {
        u128::from_le_bytes(data[off + 41..off + 57].try_into().unwrap())
    } else {
        0
    };

    // sqrt_price_x64: u128 at off+57
    let sqrt_price_x64 = if data.len() >= off + 73 {
        u128::from_le_bytes(data[off + 57..off + 73].try_into().unwrap())
    } else {
        0
    };

    // tick_current_index: i32 at off+73
    let tick_current = if data.len() >= off + 77 {
        i32::from_le_bytes(data[off + 73..off + 77].try_into().unwrap())
    } else {
        0
    };

    // Oracle PDA
    let (oracle, _) = Pubkey::find_program_address(
        &[b"oracle", pool_address.as_ref()],
        &ORCA_PROG_ID,
    );

    Ok(PoolState::Orca {
        whirlpool: *pool_address,
        token_vault_a,
        token_vault_b,
        oracle,
        token_mint_a,
        token_mint_b,
        tick_current,
        tick_spacing,
        sqrt_price_x64,
        liquidity,
        fee_rate,
    })
}

// -- Tier 2 parsers (simpler account layouts) --

/// Shared SPL Token Swap layout parser (used by FluxBeam, Saros, Dooar).
/// Layout: version(1) + is_init(1) + bump(1) + token_program(32@3)
/// + vault_a(32@35) + vault_b(32@67) + pool_mint(32@99)
/// + mint_a(32@131) + mint_b(32@163) + fee_account(32@195)
struct SplTokenSwapFields {
    pool_token_program: Option<Pubkey>, // Only FluxBeam uses this
    token_a_vault: Pubkey,
    token_b_vault: Pubkey,
    pool_mint: Pubkey,
    token_a_mint: Pubkey,
    token_b_mint: Pubkey,
    fee_account: Pubkey,
    authority: Pubkey,
}

fn parse_spl_token_swap(
    pool_address: &Pubkey,
    pool_data: &Account,
    program_id: &Pubkey,
    name: &str,
    read_token_program: bool,
) -> TradeResult<SplTokenSwapFields> {
    let data = &pool_data.data;
    if data.len() < 227 {
        return Err(TradeError::Execution(format!("{name} pool too small")));
    }
    let pool_token_program = if read_token_program {
        Some(read_pubkey(data, 3)?)
    } else {
        None
    };
    let token_a_vault = read_pubkey(data, 35)?;
    let token_b_vault = read_pubkey(data, 67)?;
    let pool_mint = read_pubkey(data, 99)?;
    let token_a_mint = read_pubkey(data, 131)?;
    let token_b_mint = read_pubkey(data, 163)?;
    let fee_account = read_pubkey(data, 195)?;

    let (authority, _) = Pubkey::find_program_address(
        &[pool_address.as_ref()],
        program_id,
    );

    Ok(SplTokenSwapFields {
        pool_token_program,
        token_a_vault,
        token_b_vault,
        pool_mint,
        token_a_mint,
        token_b_mint,
        fee_account,
        authority,
    })
}

// -- FluxBeam (SPL Token Swap fork) --
fn parse_fluxbeam(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let f = parse_spl_token_swap(pool_address, pool_data, &FLUXBEAM_PROG_ID, "fluxbeam", true)?;

    Ok(PoolState::FluxBeam {
        pool: *pool_address,
        authority: f.authority,
        token_a_vault: f.token_a_vault,
        token_b_vault: f.token_b_vault,
        pool_mint: f.pool_mint,
        fee_account: f.fee_account,
        fees: super::types::SplSwapFees::parse(&pool_data.data).unwrap_or_default(),
        token_a_mint: f.token_a_mint,
        token_b_mint: f.token_b_mint,
        pool_token_program: f.pool_token_program.unwrap_or(TOKEN_PROGRAM_ID),
    })
}

fn parse_flash_trade(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 8 + 4 * 32 {
        return Err(TradeError::Execution("flash trade pool too small".into()));
    }
    let off = 8;
    let oracle = read_pubkey(data, off)?;
    let custody = read_pubkey(data, off + 32)?;
    let token_mint = read_pubkey(data, off + 64)?;

    Ok(PoolState::FlashTrade {
        pool: *pool_address,
        oracle,
        custody,
        token_mint,
    })
}

// -- Byreal CLMM (Raydium CLMM fork) --
// Pool account = Raydium's 1544-byte PoolState (same offsets as PancakeSwap
// below), plus Byreal fee fields in Raydium's padding (`quote::byreal_fee`).
/// Anchor discriminator of Byreal's `PoolState` account.
const BYREAL_POOL_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

fn parse_byreal(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 1176 {
        return Err(TradeError::Execution("byreal pool too small".into()));
    }
    if data[..8] != BYREAL_POOL_DISCRIMINATOR {
        return Err(TradeError::Execution(format!("byreal: {pool_address} is not a pool state")));
    }
    let amm_config = read_pubkey(data, 9)?;
    let token_mint_a = read_pubkey(data, 73)?;
    let token_mint_b = read_pubkey(data, 105)?;
    let token_vault_a = read_pubkey(data, 137)?;
    let token_vault_b = read_pubkey(data, 169)?;
    let observation = read_pubkey(data, 201)?;
    let tick_spacing = u16::from_le_bytes(data[235..237].try_into().unwrap()) as i32;
    let liquidity = u128::from_le_bytes(data[237..253].try_into().unwrap());
    let sqrt_price_x64 = u128::from_le_bytes(data[253..269].try_into().unwrap());
    let tick_current = i32::from_le_bytes(data[269..273].try_into().unwrap());
    let fee = crate::quote::byreal_fee::ByrealFee::parse(data)
        .ok_or_else(|| TradeError::Execution("byreal pool too small".into()))?;

    Ok(PoolState::Byreal {
        pool: *pool_address,
        amm_config,
        token_vault_a,
        token_vault_b,
        observation,
        token_mint_a,
        token_mint_b,
        tick_current,
        tick_spacing,
        sqrt_price_x64,
        liquidity,
        // AmmConfig.trade_fee_rate, read by `fetch_pool_state`; 25 bps until then
        fee_rate: 2500,
        fee,
    })
}

// -- DefiTuna Fusion --
// Pool account layout (verified from fusionamm-client docs, 423 bytes):
// 0-7:   Anchor discriminator
// 8:     bump (1), 9-10: version (u16)
// 11:    token_mint_a (32)
// 43:    token_mint_b (32)
// 75:    token_vault_a (32)
// 107:   token_vault_b (32)
// 139:   tick_spacing (u16)
// 141:   tick_spacing_seed (2), 143: fee_rate (u16), 145: protocol_fee_rate (u16)
// 147:   unused0 (u32)
// 151:   liquidity (u128)
// 167:   sqrt_price (u128)
// 183:   tick_current_index (i32)
fn parse_defituna_fusion(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 187 {
        return Err(TradeError::Execution("defituna fusion pool too small".into()));
    }
    let token_mint_a = read_pubkey(data, 11)?;
    let token_mint_b = read_pubkey(data, 43)?;
    let token_vault_a = read_pubkey(data, 75)?;
    let token_vault_b = read_pubkey(data, 107)?;
    let tick_spacing = u16::from_le_bytes(data[139..141].try_into().unwrap());
    // fee_rate: u16 at 143
    let fee_rate = if data.len() >= 145 {
        u16::from_le_bytes(data[143..145].try_into().unwrap())
    } else {
        0
    };
    // liquidity: u128 at 151
    let liquidity = if data.len() >= 167 {
        u128::from_le_bytes(data[151..167].try_into().unwrap())
    } else {
        0
    };
    // sqrt_price: u128 at 167
    let sqrt_price_x64 = if data.len() >= 183 {
        u128::from_le_bytes(data[167..183].try_into().unwrap())
    } else {
        0
    };
    let tick_current_index = i32::from_le_bytes(data[183..187].try_into().unwrap());

    Ok(PoolState::DefiTunaFusion {
        pool: *pool_address,
        token_vault_a,
        token_vault_b,
        token_mint_a,
        token_mint_b,
        tick_spacing,
        tick_current_index,
        sqrt_price_x64,
        liquidity,
        fee_rate,
    })
}

fn parse_defituna_pools(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    if data.len() < 8 + 5 * 32 {
        return Err(TradeError::Execution("defituna pools too small".into()));
    }
    let off = 8;
    let token_mint_a = read_pubkey(data, off)?;
    let token_mint_b = read_pubkey(data, off + 32)?;
    let token_vault_a = read_pubkey(data, off + 64)?;
    let token_vault_b = read_pubkey(data, off + 96)?;

    Ok(PoolState::DefiTunaPools {
        pool: *pool_address,
        token_vault_a,
        token_vault_b,
        token_mint_a,
        token_mint_b,
    })
}

// -- Saros (SPL Token Swap fork) --
fn parse_saros(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let f = parse_spl_token_swap(pool_address, pool_data, &SAROS_PROG_ID, "saros", false)?;

    Ok(PoolState::Saros {
        pool: *pool_address,
        authority: f.authority,
        token_a_vault: f.token_a_vault,
        token_b_vault: f.token_b_vault,
        pool_mint: f.pool_mint,
        fee_account: f.fee_account,
        fees: super::types::SplSwapFees::parse(&pool_data.data).unwrap_or_default(),
        token_a_mint: f.token_a_mint,
        token_b_mint: f.token_b_mint,
    })
}

fn parse_pancakeswap(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    let data = &pool_data.data;
    // PancakeSwap CLMM = Raydium CLMM fork, 1544-byte pool account
    // disc(8) + bump(1@8) + amm_config(32@9) + owner(32@41) + mint_a(32@73)
    // + mint_b(32@105) + vault_a(32@137) + vault_b(32@169) + observation(32@201)
    // + decimals_a(1@233) + decimals_b(1@234) + tick_spacing(u16@235)
    // + liquidity(u128@237) + sqrt_price(u128@253) + tick_current(i32@269)
    if data.len() < 273 {
        return Err(TradeError::Execution("pancakeswap pool too small".into()));
    }
    let amm_config = read_pubkey(data, 9)?;
    let token_mint_a = read_pubkey(data, 73)?;
    let token_mint_b = read_pubkey(data, 105)?;
    let token_vault_a = read_pubkey(data, 137)?;
    let token_vault_b = read_pubkey(data, 169)?;
    let observation = read_pubkey(data, 201)?;
    let tick_spacing = u16::from_le_bytes(data[235..237].try_into().unwrap()) as i32;
    // liquidity: u128 at 237
    let liquidity = u128::from_le_bytes(data[237..253].try_into().unwrap());
    // sqrt_price_x64: u128 at 253
    let sqrt_price_x64 = u128::from_le_bytes(data[253..269].try_into().unwrap());
    let tick_current = i32::from_le_bytes(data[269..273].try_into().unwrap());
    // Fee rate lives in the amm_config account (read by `fetch_pool_state`);
    // hundredths of a basis point, default 25 bps until read.
    let fee_rate: u16 = 2500;

    Ok(PoolState::PancakeSwap {
        pool: *pool_address,
        amm_config,
        token_vault_a,
        token_vault_b,
        observation,
        token_mint_a,
        token_mint_b,
        tick_current,
        tick_spacing,
        sqrt_price_x64,
        liquidity,
        fee_rate,
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
const PUMPUP_POOL_MIN_SIZE: usize = 8 + 6 * 32 + 3 * 8 + 2 + 1 + 32 + 2; // = 261

fn parse_pumpup(pool_address: &Pubkey, pool_data: &Account) -> TradeResult<PoolState> {
    if pool_data.owner != PUMPUP_PROG_ID {
        return Err(TradeError::Execution(format!(
            "pumpup pool {pool_address} not owned by Pumpup program (owned by {})",
            pool_data.owner
        )));
    }
    let d = &pool_data.data;
    if d.len() < PUMPUP_POOL_MIN_SIZE {
        return Err(TradeError::Execution(format!(
            "pumpup pool {pool_address} data too short: {} bytes (need >= {})",
            d.len(),
            PUMPUP_POOL_MIN_SIZE
        )));
    }
    if d[..8] != PUMPUP_POOL_DISCRIMINATOR {
        return Err(TradeError::Execution(format!(
            "pumpup pool {pool_address} has wrong discriminator: {:02x?} (expected {:02x?})",
            &d[..8],
            PUMPUP_POOL_DISCRIMINATOR
        )));
    }

    let token_a_mint = Pubkey::try_from(&d[8..40])
        .map_err(|_| TradeError::Execution("pumpup token_a_mint slice".into()))?;
    let token_b_mint = Pubkey::try_from(&d[40..72])
        .map_err(|_| TradeError::Execution("pumpup token_b_mint slice".into()))?;
    let token_a_vault = Pubkey::try_from(&d[72..104])
        .map_err(|_| TradeError::Execution("pumpup token_a_vault slice".into()))?;
    let token_b_vault = Pubkey::try_from(&d[104..136])
        .map_err(|_| TradeError::Execution("pumpup token_b_vault slice".into()))?;
    let fee_recipient = Pubkey::try_from(&d[168..200])
        .map_err(|_| TradeError::Execution("pumpup fee_recipient slice".into()))?;
    let token_a_reserve = u64::from_le_bytes(d[200..208].try_into().unwrap());
    let token_b_reserve = u64::from_le_bytes(d[208..216].try_into().unwrap());
    let fee_recipient2 = Pubkey::try_from(&d[227..259])
        .map_err(|_| TradeError::Execution("pumpup fee_recipient2 slice".into()))?;

    Ok(PoolState::Pumpup {
        pool: *pool_address,
        token_a_mint,
        token_b_mint,
        token_a_vault,
        token_b_vault,
        fee_recipient,
        fee_recipient2,
        token_a_reserve,
        token_b_reserve,
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
const PUMPUP_BONDING_MIN_SIZE: usize = 8 + 5 * 8 + 1; // = 49 (vec len optional)

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

/// Fetch the global PumpupConfiguration once and extract `fee_address`.
///
/// Layout (after 8-byte Anchor disc, packed bytemuck repr):
///   offset 8   bump                (u8)
///   offset 9   fee_rate            (u64)
///   offset 17  authority_address   (Pubkey)
///   offset 49  fee_address         (Pubkey)  ← what we want
///   offset 81  migration_address   (Pubkey)
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
            if cfg.data.len() < 81 {
                return Err(TradeError::Execution(format!(
                    "pumpup config too short: {} bytes (need >= 81)",
                    cfg.data.len()
                )));
            }
            // fee_address at offset 49 (8 disc + 1 bump + 8 fee_rate + 32 authority)
            Pubkey::try_from(&cfg.data[49..81]).map_err(|_| {
                TradeError::Execution("pumpup config fee_address slice".into())
            })
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
    if d.len() < PUMPUP_BONDING_MIN_SIZE {
        return Err(TradeError::Execution(format!(
            "pumpup bonding curve {pool_address} data too short: {} bytes (need >= {})",
            d.len(),
            PUMPUP_BONDING_MIN_SIZE
        )));
    }
    if d[..8] != PUMPUP_BONDING_DISCRIMINATOR {
        return Err(TradeError::Execution(format!(
            "pumpup bonding curve {pool_address} has wrong discriminator: {:02x?} (expected {:02x?})",
            &d[..8],
            PUMPUP_BONDING_DISCRIMINATOR
        )));
    }
    let launch_token_surplus = u64::from_le_bytes(d[8..16].try_into().unwrap());
    let virtual_sol = u64::from_le_bytes(d[16..24].try_into().unwrap());
    let real_sol = u64::from_le_bytes(d[24..32].try_into().unwrap());
    let pool_sol_reserves = u64::from_le_bytes(d[32..40].try_into().unwrap());
    let pool_token_reserves = u64::from_le_bytes(d[40..48].try_into().unwrap());
    Ok((launch_token_surplus, virtual_sol, real_sol, pool_sol_reserves, pool_token_reserves))
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
    let f = parse_spl_token_swap(pool_address, pool_data, &DOOAR_PROG_ID, "dooar", false)?;

    Ok(PoolState::Dooar {
        pool: *pool_address,
        authority: f.authority,
        token_a_vault: f.token_a_vault,
        token_b_vault: f.token_b_vault,
        pool_mint: f.pool_mint,
        fee_account: f.fee_account,
        fees: super::types::SplSwapFees::parse(&pool_data.data).unwrap_or_default(),
        token_a_mint: f.token_a_mint,
        token_b_mint: f.token_b_mint,
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
        PoolType::Meteora => {
            // a_vault at 104, b_vault at 136
            if data.len() >= 168 {
                let mut keys = Vec::with_capacity(2);
                if let Ok(a) = read_pubkey(data, 104) { keys.push(a); }
                if let Ok(b) = read_pubkey(data, 136) { keys.push(b); }
                keys
            } else {
                vec![]
            }
        }
        PoolType::MeteoraDbc => {
            // config at offset 72
            if data.len() >= 104 {
                read_pubkey(data, 72).into_iter().collect()
            } else {
                vec![]
            }
        }
        // PumpFun bonding: mint must be discovered via RPC (get_token_accounts_by_owner).
        // PumpFunAmm: sync parser exists (parse_pumpfun_amm_layout), vault balances are Phase 2.
        // All other types are already sync-parseable.
        _ => vec![],
    }
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
    pumpfun_state(pool_address, pool_data, mint, token_prog, global_data)
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
    if pool_data.len() < 298 {
        return Err(TradeError::Execution("meteora pool account too small".into()));
    }

    let token_a_mint = read_pubkey(pool_data, 40)?;
    let token_b_mint = read_pubkey(pool_data, 72)?;
    let a_vault = read_pubkey(pool_data, 104)?;
    let b_vault = read_pubkey(pool_data, 136)?;
    let a_vault_lp = read_pubkey(pool_data, 168)?;
    let b_vault_lp = read_pubkey(pool_data, 200)?;
    let admin_token_a_fee = read_pubkey(pool_data, 234)?;
    let admin_token_b_fee = read_pubkey(pool_data, 266)?;

    if a_vault_data.len() < 147 || b_vault_data.len() < 147 {
        return Err(TradeError::Execution("meteora vault account too small".into()));
    }

    let a_token_vault = read_pubkey(a_vault_data, 19)?;
    let b_token_vault = read_pubkey(b_vault_data, 19)?;
    let a_vault_lp_mint = read_pubkey(a_vault_data, 115)?;
    let b_vault_lp_mint = read_pubkey(b_vault_data, 115)?;
    let vault_program = *METEORA_VAULT_PROGRAM;

    Ok(PoolState::Meteora {
        pool: *pool_address,
        token_a_mint,
        token_b_mint,
        a_vault,
        b_vault,
        a_token_vault,
        b_token_vault,
        a_vault_lp_mint,
        b_vault_lp_mint,
        a_vault_lp,
        b_vault_lp,
        admin_token_a_fee,
        admin_token_b_fee,
        vault_program,
        reserves: Default::default(),
    })
}

/// Parse MeteoraDbc using cached config data (no RPC).
pub fn parse_meteora_dbc_with_companion(
    pool_address: &Pubkey,
    pool_data: &[u8],
    config_data: &[u8],
) -> TradeResult<PoolState> {
    if pool_data.len() < 232 {
        return Err(TradeError::Execution("meteora dbc pool too small".into()));
    }
    let (quote_mint, config) = dbc_config(&read_pubkey(pool_data, 72)?, config_data)?;
    dbc_state(pool_address, pool_data, quote_mint, config)
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

        PoolType::PumpFunAmm => {
            // Sync layout parser + vault balances from mirror
            let base_vault = read_pubkey(data, 139)?;
            let quote_vault = read_pubkey(data, 171)?;
            let base_bal = mirror.get_vault_balance(&base_vault);
            let quote_bal = mirror.get_vault_balance(&quote_vault);
            parse_pumpfun_amm_with_balances(pool_address, data, base_bal, quote_bal)
        }

        PoolType::Meteora => {
            let a_vault = read_pubkey(data, 104)?;
            let b_vault = read_pubkey(data, 136)?;
            let a_data = mirror.get_companion(&a_vault).ok_or_else(||
                TradeError::Execution("meteora: vault A not in mirror".into()))?;
            let b_data = mirror.get_companion(&b_vault).ok_or_else(||
                TradeError::Execution("meteora: vault B not in mirror".into()))?;
            parse_meteora_with_companion(pool_address, data, &a_data, &b_data)
        }

        PoolType::MeteoraDbc => {
            let config_key = read_pubkey(data, 72)?;
            let config_data = mirror.get_companion(&config_key).ok_or_else(||
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
                ..
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
                ..
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
                pair: _,
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
                ..
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
                ..
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

    /// First 277 bytes of the mainnet Byreal SOL/USDC pool
    /// 9GTj99g9tbz9U6UYDsX6YeRTgUnkYG6GTnHv3qLa5aXq (1544-byte Raydium-layout PoolState).
    const BYREAL_SOL_USDC_HEAD: &str = "9+3j9dfD3kb+L+5YLsVv0o3EkYaL3WHYfedkSjbRQLqlJ34QDGMZ3i0O5g+zIL5ZpbxeXsNo7D0wbV4rc72JMFhn16FkdzmaAQabiFf+q4GE+2h/Y0YYwDXaxDncGus7VZig8AAAAAABxvp6877brTo9ZfNqq8l0MbG75MLS9uDkfKYCA0UvXWE+P+p8vsA0tRWI2pmfJWcDgCvYd1Z8DtaHbhyB+fCP8fKh9AAWLHxqPXRnLTvQYUQMIDXWKE4rJcPOdBLvnQoTJGcSSIj+UCpR/pbiCz8GHBSB03Hx1cNFGFm0ROoEwl4JBgEAO4RL5b0AAAAAAAAAAAAAAHRVmtjZOU9YAAAAAAAAAADYrP//AAAAAA==";

    fn byreal_account() -> Account {
        use base64::Engine;
        let mut acct = make_account(1544);
        let head = base64::engine::general_purpose::STANDARD.decode(BYREAL_SOL_USDC_HEAD).unwrap();
        acct.data[..head.len()].copy_from_slice(&head);
        acct
    }

    #[test]
    fn test_parse_byreal_success() {
        let pool_addr = pubkey_from_str("9GTj99g9tbz9U6UYDsX6YeRTgUnkYG6GTnHv3qLa5aXq");
        let result = parse_byreal(&pool_addr, &byreal_account()).unwrap();
        match result {
            PoolState::Byreal {
                pool,
                amm_config,
                token_vault_a,
                token_vault_b,
                observation,
                token_mint_a,
                token_mint_b,
                tick_current,
                tick_spacing,
                sqrt_price_x64,
                liquidity,
                fee_rate,
                fee,
            } => {
                assert_eq!(pool, pool_addr);
                assert_eq!(amm_config, pubkey_from_str("4E6xP73xzTs4aCvY92hbXRwWkYptNwvViPZmLcZEBUk4"));
                assert_eq!(token_mint_a, SOL_NATIVE_MINT);
                assert_eq!(token_mint_b, USDC_MINT);
                assert_eq!(token_vault_a, pubkey_from_str("5BzogZvHNEuwstR4iwTWdd7jknFBZqJQWVjxPsDfEUD6"));
                assert_eq!(token_vault_b, pubkey_from_str("HL8turx8hJEEPVH4ivxzxwfdxVA1PH3LeYbSmh3hYfzz"));
                assert_eq!(observation, pubkey_from_str("3T6qNbQqWYDfSTew1ifsNedtoeDP8LRuCegmUH27ykEZ"));
                assert_eq!(tick_spacing, 1);
                assert_eq!(tick_current, -21288);
                assert_eq!(liquidity, 815_595_750_459);
                assert_eq!(sqrt_price_x64, 6_363_368_406_302_479_732);
                assert_eq!(fee_rate, 2500, "AmmConfig fee is read by fetch_pool_state");
                assert_eq!((fee.pool_trade_fee_rate, fee.flags, fee.decimals_0, fee.decimals_1), (0, 0, 9, 6), "no pool-level fee features");
                assert!(!fee.is_dynamic());
            }
            _ => panic!("expected Byreal variant"),
        }
    }

    #[test]
    fn test_parse_byreal_reads_the_pool_fee_fields() {
        let pool_addr = Pubkey::new_unique();
        // the mainnet dynamic-fee SOL/USDC pool's fields: trade_fee_rate 1,
        // flags 24 (dynamic fee, token 1 quote), buffer 100, 10/20/40/50, USDC feed
        let mut acct = byreal_account();
        acct.data[393..397].copy_from_slice(&1u32.to_le_bytes());
        acct.data[1096] = 24;
        acct.data[1100..1102].copy_from_slice(&100u16.to_le_bytes());
        acct.data[1102..1106].copy_from_slice(&[10, 20, 40, 50]);
        acct.data[1080..1088].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        let usdc_feed: [u8; 32] = [0xea, 0xa0, 0x20, 0xc6, 0x1c, 0xc4, 0x79, 0x71, 0x28, 0x13, 0x46, 0x1c, 0xe1, 0x53, 0x89, 0x4a, 0x96, 0xa6, 0xc0, 0x0b, 0x21, 0xed, 0x0c, 0xfc, 0x27, 0x98, 0xd1, 0xf9, 0xa9, 0xe9, 0xc9, 0x4a];
        acct.data[1144..1176].copy_from_slice(&usdc_feed);
        let PoolState::Byreal { fee, .. } = parse_byreal(&pool_addr, &acct).unwrap() else { panic!() };
        assert_eq!((fee.pool_trade_fee_rate, fee.flags, fee.open_time, fee.arbitrage_fee_buffer_ppm), (1, 24, 1_700_000_000, 100));
        assert_eq!((fee.slippage_fee_base, fee.slippage_fee_threshold, fee.imbalance_fee_base, fee.imbalance_fee_x), (10, 20, 40, 50));
        assert!(fee.is_dynamic());
        assert_eq!(fee.oracle_1, pubkey_from_str("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX"), "sponsored USDC/USD feed");
        // a launch decay-fee pool (flags 7, init 2 %, −1 % / 255 s)
        let mut acct = byreal_account();
        acct.data[1096..1100].copy_from_slice(&[7, 2, 1, 255]);
        let PoolState::Byreal { fee, .. } = parse_byreal(&pool_addr, &acct).unwrap() else { panic!() };
        assert_eq!((fee.flags, fee.decay_init_rate, fee.decay_decrease_rate, fee.decay_interval), (7, 2, 1, 255));
    }

    #[test]
    fn test_parse_byreal_too_short() {
        let acct = make_account(50);
        let pool_addr = Pubkey::new_unique();
        assert!(parse_byreal(&pool_addr, &acct).is_err());
        // right size, wrong account type (e.g. a payer picked as the pool)
        assert!(parse_byreal(&pool_addr, &make_account(1544)).is_err());
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
                ..
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
                ..
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

    // -- Boundary size tests (exact minimum) --

    #[test]
    fn test_raydium_cpmm_exact_min_size() {
        let pool_addr = Pubkey::new_unique();
        let acct_ok = make_account(329);
        assert!(parse_raydium_cpmm(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(328);
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
        let acct_ok = make_account(168);
        assert!(parse_defituna_pools(&pool_addr, &acct_ok).is_ok());
        let acct_fail = make_account(167);
        assert!(parse_defituna_pools(&pool_addr, &acct_fail).is_err());
    }

    // ── Companion parser tests ──

    /// Mainnet Raydium AMM v4 SOL/USDC pool 58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2
    /// (752-byte AmmInfo, status 6 = SwapOnly, need_take_pnl coin 39,925,487 /
    /// pc 4,739,163 — the offsets its `ray_log` curve reserves were checked against).
    const RAYDIUM_V4_SOL_USDC: &str = "BgAAAAAAAAD+AAAAAAAAAAcAAAAAAAAAAwAAAAAAAAAJAAAAAAAAAAYAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAEBCDwAAAAAA9AEAAAAAAAAAAAAAAAAAAEBCDwAAAAAAQEIPAAAAAAABAAAAAAAAAADKmjsAAAAAAMqaOwAAAAAFAAAAAAAAABAnAAAAAAAAGQAAAAAAAAAQJwAAAAAAAAwAAAAAAAAAZAAAAAAAAAAZAAAAAAAAABAnAAAAAAAA7zZhAgAAAABbUEgAAAAAAE64FyutAwAALkI4/Jo0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACzqVos/TzgAAAAAAAAAAABzVLPxIFgFAAAAAAAAAAAAMWNjiHMDAACZN03ShGQFAAAAAAAAAAAAQEusDaXyOAAAAAAAAAAAAEiYZsIJJAAAuHDhLdN5iRVh0un6jyZDGDTrc28vJPwqKk3/H9XcpN/yy7m3YO3bGFcGMDBjrTPXtXKW6gLU4DNeMc6vpMxC3QabiFf+q4GE+2h/Y0YYwDXaxDncGus7VZig8AAAAAABxvp6877brTo9ZfNqq8l0MbG75MLS9uDkfKYCA0UvXWFsT5PYWOiP+v6gjENnRJfo5qkywMgxSCYqGuPMx4KexvkvOQ/5YJ6K1De7jkwfGqQ6wF0kMIzKd96FEsVQkpLTasTDzvqfGb9UyNwPXk0c7uUyfSZIKynSsTy6pDRHIY0NB1GoKC2mEwX+KZw3uZjlhHHbETUDcxD4vhBFpgr27qvkPHweIeqm+XyL01XiG9EnlnR1bByOEGxucSuhFtlwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAOW2K2XLO72m9WiI5m/ujmTcVWAZnA+IsR/ic70Fnoqh9l8Mi0iVAABO2XAAAAAAABAEAAAAAAAAAAAAAAAAAAA=";

    fn raydium_v4_account() -> Account {
        use base64::Engine;
        let mut acct = make_account(0);
        acct.data = base64::engine::general_purpose::STANDARD.decode(RAYDIUM_V4_SOL_USDC).unwrap();
        acct.owner = RAYDIUM_V4_PROG_ID;
        acct
    }

    #[test]
    fn test_parse_raydium_v4_mainnet_amm_info() {
        let pool = pubkey_from_str("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2");
        let st = parse_raydium_v4(&pool, &raydium_v4_account()).unwrap();
        let PoolState::RaydiumV4 { amm_id, authority, coin_vault, pc_vault, coin_mint, pc_mint, swap_fee_numerator, swap_fee_denominator, need_take_pnl_coin, need_take_pnl_pc, status, pool_open_time } = st else {
            panic!("expected RaydiumV4");
        };
        assert_eq!(amm_id, pool);
        assert_eq!(authority, pubkey_from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"));
        assert_eq!(coin_vault, pubkey_from_str("DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz"));
        assert_eq!(pc_vault, pubkey_from_str("HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz"));
        assert_eq!((coin_mint, pc_mint), (SOL_NATIVE_MINT, USDC_MINT));
        assert_eq!((swap_fee_numerator, swap_fee_denominator), (25, 10_000));
        assert_eq!((need_take_pnl_coin, need_take_pnl_pc), (39_925_487, 4_739_163));
        assert_eq!((status, pool_open_time), (6, 0));
        // sync: parseable from raw bytes (Geyser / block refresh)
        assert!(is_sync_parseable(PoolType::RaydiumV4));
        assert!(parse_pool_state_from_bytes(PoolType::RaydiumV4, &pool, &raydium_v4_account().data, &RAYDIUM_V4_PROG_ID).is_ok());
    }

    #[test]
    fn test_parse_raydium_v4_rejects_foreign_or_short_accounts() {
        let pool = Pubkey::new_unique();
        let mut foreign = raydium_v4_account();
        foreign.owner = Pubkey::new_unique();
        assert!(parse_raydium_v4(&pool, &foreign).is_err());
        let mut short = raydium_v4_account();
        short.data.truncate(700);
        assert!(parse_raydium_v4(&pool, &short).is_err());
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
        assert!(extract_companion_keys(PoolType::RaydiumV4, &data).is_empty());
    }

    #[test]
    fn test_extract_companion_keys_too_short() {
        let data = vec![0u8; 10]; // way too short
        assert!(extract_companion_keys(PoolType::Meteora, &data).is_empty());
        assert!(extract_companion_keys(PoolType::MeteoraDbc, &data).is_empty());
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

    fn b64(s: &str) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap()
    }

    fn key(s: &str) -> Pubkey {
        s.parse().unwrap()
    }

    /// pump.fun `Global` (4wTV1Y…, mainnet), the first 997 of its 1087 bytes:
    /// everything through the buyback fee recipients.
    const PUMP_GLOBAL_B64: &str = "p+joschscn8B07uMqzQc4FKEV/LDgX0yeEQZY9zVX+1YuiTJmd2sAqpKwvjQ3Vy8l+MonBl8tQYqVPPZVrnOblEV+WVnqlyz5gAQ2EfjzwMAAKwj/AYAAAAAeMX7UdECAACAxqR+jQMAXwAAAAAAAAAf6nQ58860xO9Lucx77kChpiYXG2hBX+3tQLeolW+E5wHB4eQAAAAAAAUAAAAAAAAAYIzMHfzpYbQ7d5wZFQWm4tO/RdWk20YYrXbILWF1RTVjg3MADqIssmTTSv9koEte+r+7dN3NBImXsZgVR9fREIOEdCkuZ1qUtDbssKmYiUIyioPdxiM4ApYSZ8XNYRfLjRgaDISfqTem80re0wge+VcAqssMm7PZCaS5FHUnpOutEeak/ClEpPqCUb74FUJuG/soxrZkZndgfGrZ9WamRteqj7Bg2CkbTE1HXa/3Yslr3A2s6zbAEurRLtOpSEFh4ATIfOuY+lzkf4A4Bv0seUXSlSSVmuwA3tl4FPOPeEYf6nQ58860xO9Lucx77kChpiYXG2hBX+3tQLeolW+E5wchXZlAeTaU4RYGbORZuBj9+bugx7QbeD+joSDKQZUyAaKLX9JqtHmmqcxsv2sLI+thiFo3HgEgrKkTvu89E4p46JMUH7GOnxV02BDheOGeMGBOMXWqLkoy38hgByfRBwkBNYRTYlYJT5EoGRJ++k5Ea0MzcheT0Th2+arb89x9C19udQGCIPlCZ3ADI3tNa0U3WbSlxpC1nDXZuxh6CQy9KjOYep67E2eZq1mSWxPl3Iswgd8AXbQnwUePpG/4w0egdOlUPz43otBGInrdy06cd0xEJYxD7fJKqKrh8AIUZlvaTDjNbbdDj1m0CLuew7TKnorR8fJGU8SZtXlsINv5sy3dnuo/ObNyEVxxhHwYRc+lNsaFB04DDkTQId4++eNcTLeA8I7i/uhL7ERqV3gl2mjUOfqKXaOwxc/1D2P0VGsBQ55lEMA9ZfrZMeidBL4Ltw1Rlx9RxBX7NEwH20GfISICI1UWqRcTTGdYjEk4IK4VXulmZVd6wbcY2kfdzyoFDuan4iBou4hkCqV/kJMIxh/vcRoBY/WnVcBwvIYNH2NnIHzs2lvMbLHq8PFtaEBFZrGNVtJIGssxcDJlbpBVHHhElkH4SVjcc6dqhdh1b1XALNrKiboZMnkMNoqxV+ktc8VLlrXJMZQeRupL4uDjESd0T8a3TPtFXv6vi9VxeSztRPwfePlKM9CQnF5rX7AhVwrY262N6P2z0g7RzZnrjk6HcBV+6+tnimVduZs39rEybHZX25DPuKh6vvjHtvLIaQ==";

    #[test]
    fn pumpfun_recipients_from_the_live_global() {
        let g = b64(PUMP_GLOBAL_B64);
        let mint = Pubkey::new_from_array([3u8; 32]); // 3 % 8 → the fourth buyback recipient
        let (fee, buyback) = pumpfun_recipients(&g, false, &mint);
        assert_eq!(fee, key("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"));
        assert_eq!(buyback, key("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"));
        let (fee, _) = pumpfun_recipients(&g, true, &mint);
        assert_eq!(fee, key("GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS"), "mayhem-mode curves pay the reserved recipient");
        assert_eq!(pumpfun_recipients(&[], false, &mint), (*PUMPFUN_FEE_FALLBACK, *PUMPFUN_BUYBACK_FALLBACK));
    }

    #[test]
    fn pumpfun_block_refresh_reparses_the_curve_and_keeps_the_rest() {
        let g = b64(PUMP_GLOBAL_B64);
        let mint = Pubkey::new_unique();
        let curve_addr = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_FUN_PROG_ID).0;
        let mut data = vec![0u8; 151];
        data[8..16].copy_from_slice(&1_073_000_000_000_000u64.to_le_bytes());
        data[16..24].copy_from_slice(&30_000_000_000u64.to_le_bytes());
        data[24..32].copy_from_slice(&793_100_000_000_000u64.to_le_bytes());
        data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
        write_pubkey(&mut data, 49, &Pubkey::new_unique());
        let prev = pumpfun_state(&curve_addr, &data, mint, TOKEN_2022_PROGRAM_ID, &g).unwrap();
        // a buy lands: the refresh re-reads only the curve account
        data[16..24].copy_from_slice(&31_000_000_000u64.to_le_bytes());
        data[32..40].copy_from_slice(&1_000_000_000u64.to_le_bytes());
        let acct = Account { data: data.clone(), ..make_account(0) };
        let fresh = reparse_pool_state(PoolType::PumpFun, &curve_addr, &acct, Some(&prev)).unwrap();
        let (PoolState::PumpFun { mint: m, associated_bonding_curve: abc, curve, buyback_fee_recipient, .. }, PoolState::PumpFun { associated_bonding_curve: prev_abc, .. }) = (&fresh, &prev) else {
            panic!("expected PumpFun");
        };
        assert_eq!((*m, *abc), (mint, *prev_abc), "mint and the Token-2022 curve ATA come from the fetch");
        assert_eq!((curve.virtual_sol_reserves, curve.real_sol_reserves), (31_000_000_000, 1_000_000_000));
        assert_ne!(*buyback_fee_recipient, Pubkey::default());
        assert!(reparse_pool_state(PoolType::PumpFun, &curve_addr, &acct, None).is_err(), "never a bare re-parse");
    }

    /// Live DBC pool CCa7ito… and its config 7bH1hBvb… (Bags: 2 % in SOL, two
    /// segments), each up to its last non-zero byte.
    const DBC_POOL_B64: &str = "1eAF0WJFd1wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAYe35TRSCCnng8FOHW+TAASCkfWEdaxdvbJO0bPpZO2GW9IxB+lQoWn0Mv1MP3ImwcJ2CW26SYv/mj11vXExSy9aTxQHbIVvSx+PM8j6FRjFlhGnxpnqyHt/NdX+zekFzACYbykOWwbZyFZVgIm22NFtR0o2h//FUAA+7/0YvAvZnegc0Y9i+7BORSteZmwPCJHIeiMSs3Nr9ivQxCM1lE+zUbyk4Qc4N6LoECQAAAAAAAAAAAAAAAP8OUgYAAAAAAAAAAAAAAACQxT0AAAAAADiVs1b5NAsAAAAAAAAAAAAmhssaAAAAAAAAAAAAAAAAAAAAAAAAAAD/DlIGAAAAAAAAAAAAAAAAsmDmGgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEAAAAAABQAAAAAAAAA";
    const DBC_CONFIG_B64: &str = "GmwOe3TmgSsGm4hX/quBhPtof2NGGMA12sQ53BrrO1WYoPAAAAAAARnY+t+uy/kVf3qIq/o+IZk+Z2NzDeTdiAQ/sYqEvcdDlvSMQfpUKFp9DL9TD9yJsHCdgltukmL/5o9db1xMUssALTEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAkAAABkAAAABgEAAAAAAAAAAAAAALACcfeMwIQLABJlyhMAAAAfItprGfZbAnsU/n9IVy4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAGSns7bgDQAAZKeztuANAgDIAAABxAkAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB+dTuTDikLAAAAAAAAAAAA5Dn3oty9FgAAAAAAAAAAAACA9gWeJVwazMlQmbvBAAAubv5/SFcuAAAAAAAAAAAAAMAwVc8efbppjQZGnHcAAA==";

    #[test]
    fn dbc_state_from_live_accounts_and_block_refresh() {
        let pool = key("CCa7itoNXuvpPDYyyRhMqJaNstq9SD6rkB29xDHWRmKR");
        let mut p = b64(DBC_POOL_B64);
        p.resize(crate::quote::dbc::POOL_LEN, 0);
        let mut c = b64(DBC_CONFIG_B64);
        c.resize(crate::quote::dbc::CONFIG_LEN, 0);
        let st = parse_meteora_dbc_with_companion(&pool, &p, &c).unwrap();
        let PoolState::MeteoraDbc { base_mint, quote_mint, config, curve, .. } = &st else { panic!("expected MeteoraDbc") };
        assert_eq!(*base_mint, key("FScwFv8SDJhfKZJjz65XPYrPkrYkCUMYmFAhdGAVBAGS"));
        assert_eq!(*quote_mint, SOL_NATIVE_MINT);
        assert_eq!(*config, key("7bH1hBvbEiJneWXwGSYRYGewto314EeK3xcFPoaaJHQL"));
        assert_eq!((curve.sqrt_price, curve.quote_reserve, curve.base_reserve), (3_154_470_249_927_992, 151_304_936, 994_804_277_164_627_180));
        assert_eq!((curve.activation_point, curve.is_migrated), (449_545_766, false));
        let cfg = &curve.config;
        assert_eq!((cfg.collect_fee_mode, cfg.activation_type, cfg.base_fee_mode, cfg.cliff_fee_numerator, cfg.dynamic_fee), (0, 0, 0, 20_000_000, false));
        assert_eq!((cfg.migration_quote_threshold, cfg.migration_sqrt_price, cfg.sqrt_start_price), (85_000_000_000, 13_043_817_825_309_819, 3_141_367_320_245_630));
        assert_eq!(cfg.curve, vec![
            (6_401_204_812_200_420, 3_929_368_168_768_468_756_200_000_000_000_000),
            (13_043_817_825_332_782, 2_425_988_008_058_820_449_100_000_000_000_000),
        ]);
        // block refresh: the pool account alone, config kept
        p[280..296].copy_from_slice(&3_200_000_000_000_000u128.to_le_bytes());
        let acct = Account { data: p, ..make_account(0) };
        let fresh = reparse_pool_state(PoolType::MeteoraDbc, &pool, &acct, Some(&st)).unwrap();
        let PoolState::MeteoraDbc { curve: fresh_curve, .. } = &fresh else { panic!("expected MeteoraDbc") };
        assert_eq!(fresh_curve.sqrt_price, 3_200_000_000_000_000);
        assert_eq!(fresh_curve.config, *cfg);
        assert!(is_state_priced(PoolType::PumpFun) && is_state_priced(PoolType::MeteoraDbc), "touched by a block → re-read");
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
