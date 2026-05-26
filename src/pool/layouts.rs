// Typed account layout structs for each AMM.
//
// Every `*Layout` struct uses `#[derive(ZeroPod)]` for zero-copy, alignment-1
// parsing. The struct definition IS the byte-offset specification — field
// order and sizes encode all layout knowledge.

use solana_sdk::pubkey::Pubkey;
use zeropod::{ZeroPod, ZeroPodFixed};

use crate::error::{TradeError, TradeResult};

// Slice `data` to exactly `size_of::<W>()` bytes, then zero-copy cast.
// Returns Err with the provided message if `data` is too short.
macro_rules! parse_layout {
    ($Layout:ty, $data:expr, $err:literal) => {{
        let n = ::std::mem::size_of::<$Layout>();
        match ($data).get(..n) {
            Some(s) => <$Layout>::from_bytes(s)
                .map_err(|_| TradeError::Execution($err.into())),
            None => Err(TradeError::Execution($err.into())),
        }
    }};
}

// Raydium V4
//
// AmmInfo layout (non-Anchor, no discriminator):
//   _head         (336 @   0) — status, nonce, fees, limits, etc.
//   coin_vault    ( 32 @ 336)
//   pc_vault      ( 32 @ 368)
//   _lp_vault     ( 32 @ 400) — skipped
//   open_orders   ( 32 @ 432)
//   serum_market  ( 32 @ 464)
//   serum_program ( 32 @ 496)
//   target_orders ( 32 @ 528)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct RaydiumV4Layout {
    _head:         [u8; 336],
    coin_vault:    [u8; 32],
    pc_vault:      [u8; 32],
    _lp_vault:     [u8; 32],
    open_orders:   [u8; 32],
    serum_market:  [u8; 32],
    serum_program: [u8; 32],
    target_orders: [u8; 32],
}

pub struct RaydiumV4Pool {
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub open_orders: Pubkey,
    pub serum_market: Pubkey,
    pub serum_program: Pubkey,
    pub target_orders: Pubkey,
}

impl RaydiumV4Pool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(RaydiumV4Layout, data, "raydium v4 account too small")?;
        Ok(Self {
            coin_vault:    Pubkey::from(w.coin_vault),
            pc_vault:      Pubkey::from(w.pc_vault),
            open_orders:   Pubkey::from(w.open_orders),
            serum_market:  Pubkey::from(w.serum_market),
            serum_program: Pubkey::from(w.serum_program),
            target_orders: Pubkey::from(w.target_orders),
        })
    }
}

// Serum market accounts. `parse` falls back to `market_key` as a placeholder
// for every field when the account is too small (reclaimed post-shutdown).
//
// Layout (relevant fields only):
//   _head        (104 @   0)
//   bids         ( 32 @ 104)
//   asks         ( 32 @ 136)
//   event_queue  ( 32 @ 168)
//   coin_vault   ( 32 @ 200)
//   pc_vault     ( 32 @ 232)
//   nonce        (  8 @ 264) — u64 used to derive vault_signer

#[allow(dead_code)]
#[derive(ZeroPod)]
struct SerumMarketLayout {
    _head:       [u8; 104],
    bids:        [u8; 32],
    asks:        [u8; 32],
    event_queue: [u8; 32],
    coin_vault:  [u8; 32],
    pc_vault:    [u8; 32],
    nonce:       [u8; 8],
}

pub struct SerumMarketAccounts {
    pub bids: Pubkey,
    pub asks: Pubkey,
    pub event_queue: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub vault_signer: Pubkey,
}

impl SerumMarketAccounts {
    pub fn parse(data: &[u8], market_key: Pubkey, owner: &Pubkey) -> TradeResult<Self> {
        match parse_layout!(SerumMarketLayout, data, "") {
            Ok(w) => {
                let nonce = u64::from_le_bytes(w.nonce);
                let vault_signer = Pubkey::create_program_address(
                    &[market_key.as_ref(), &nonce.to_le_bytes()],
                    owner,
                ).map_err(|_| TradeError::Execution("failed to derive serum vault signer".into()))?;
                Ok(Self {
                    bids:        Pubkey::from(w.bids),
                    asks:        Pubkey::from(w.asks),
                    event_queue: Pubkey::from(w.event_queue),
                    coin_vault:  Pubkey::from(w.coin_vault),
                    pc_vault:    Pubkey::from(w.pc_vault),
                    vault_signer,
                })
            }
            Err(_) => Ok(Self {
                // Serum market reclaimed — use placeholder for all fields.
                bids:         market_key,
                asks:         market_key,
                event_queue:  market_key,
                coin_vault:   market_key,
                pc_vault:     market_key,
                vault_signer: market_key,
            }),
        }
    }
}

// Raydium CPMM
//
// Pool state layout (Anchor, 8-byte disc):
//   _disc           (  8 @   0)
//   amm_config      ( 32 @   8)
//   _pool_creator   ( 32 @  40) — skipped
//   token_0_vault   ( 32 @  72)
//   token_1_vault   ( 32 @ 104)
//   _lp_mint        ( 32 @ 136) — skipped
//   token_0_mint    ( 32 @ 168)
//   token_1_mint    ( 32 @ 200)
//   _token_0_program( 32 @ 232) — skipped
//   _token_1_program( 32 @ 264) — skipped
//   observation_key ( 32 @ 296)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct RaydiumCpmmLayout {
    _disc:             [u8;  8],
    amm_config:        [u8; 32],
    _pool_creator:     [u8; 32],
    token_0_vault:     [u8; 32],
    token_1_vault:     [u8; 32],
    _lp_mint:          [u8; 32],
    token_0_mint:      [u8; 32],
    token_1_mint:      [u8; 32],
    _token_0_program:  [u8; 32],
    _token_1_program:  [u8; 32],
    observation_key:   [u8; 32],
}

pub struct RaydiumCpmmPool {
    pub amm_config: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub observation_key: Pubkey,
}

impl RaydiumCpmmPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(RaydiumCpmmLayout, data, "raydium cpmm account too small")?;
        Ok(Self {
            amm_config:      Pubkey::from(w.amm_config),
            token_0_vault:   Pubkey::from(w.token_0_vault),
            token_1_vault:   Pubkey::from(w.token_1_vault),
            token_0_mint:    Pubkey::from(w.token_0_mint),
            token_1_mint:    Pubkey::from(w.token_1_mint),
            observation_key: Pubkey::from(w.observation_key),
        })
    }
}

// Raydium CLMM
//
// Pool state layout (Anchor, 8-byte disc):
//   _disc           (  8 @   0)
//   _bump           (  1 @   8)
//   amm_config      ( 32 @   9)
//   _owner          ( 32 @  41) — skipped
//   token_mint_0    ( 32 @  73)
//   token_mint_1    ( 32 @ 105)
//   token_vault_0   ( 32 @ 137)
//   token_vault_1   ( 32 @ 169)
//   observation     ( 32 @ 201)
//   _pad            (  2 @ 233)
//   tick_spacing    (  2 @ 235) — u16
//   liquidity       ( 16 @ 237) — u128
//   sqrt_price_x64  ( 16 @ 253) — u128
//   tick_current    (  4 @ 269) — i32

#[allow(dead_code)]
#[derive(ZeroPod)]
struct RaydiumClmmLayout {
    _disc:          [u8;  8],
    _bump:          [u8;  1],
    amm_config:     [u8; 32],
    _owner:         [u8; 32],
    token_mint_0:   [u8; 32],
    token_mint_1:   [u8; 32],
    token_vault_0:  [u8; 32],
    token_vault_1:  [u8; 32],
    observation:    [u8; 32],
    _pad:           [u8;  2],
    tick_spacing:   [u8;  2],
    liquidity:      [u8; 16],
    sqrt_price_x64: [u8; 16],
    tick_current:   [u8;  4],
}

pub struct RaydiumClmmPool {
    pub amm_config: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub observation: Pubkey,
    pub tick_spacing: i32,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

impl RaydiumClmmPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(RaydiumClmmLayout, data, "raydium clmm account too small")?;
        Ok(Self {
            amm_config:     Pubkey::from(w.amm_config),
            token_mint_0:   Pubkey::from(w.token_mint_0),
            token_mint_1:   Pubkey::from(w.token_mint_1),
            token_vault_0:  Pubkey::from(w.token_vault_0),
            token_vault_1:  Pubkey::from(w.token_vault_1),
            observation:    Pubkey::from(w.observation),
            tick_spacing:   u16::from_le_bytes(w.tick_spacing) as i32,
            liquidity:      u128::from_le_bytes(w.liquidity),
            sqrt_price_x64: u128::from_le_bytes(w.sqrt_price_x64),
            tick_current:   i32::from_le_bytes(w.tick_current),
        })
    }
}

// Raydium LaunchPad (LP)
//
// Pool account layout (Anchor, 429 bytes):
//   _head       (141 @   0) — disc(8) + params(133)
//   config_id   ( 32 @ 141)
//   platform_id ( 32 @ 173)
//   base_mint   ( 32 @ 205)
//   quote_mint  ( 32 @ 237)
//   base_vault  ( 32 @ 269)
//   quote_vault ( 32 @ 301)
//   creator     ( 32 @ 333)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct RaydiumLpLayout {
    _head:       [u8; 141],
    config_id:   [u8;  32],
    platform_id: [u8;  32],
    base_mint:   [u8;  32],
    quote_mint:  [u8;  32],
    base_vault:  [u8;  32],
    quote_vault: [u8;  32],
    creator:     [u8;  32],
}

pub struct RaydiumLpPool {
    pub config_id: Pubkey,
    pub platform_id: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub creator: Pubkey,
}

impl RaydiumLpPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(RaydiumLpLayout, data, "raydium lp account too small")?;
        Ok(Self {
            config_id:   Pubkey::from(w.config_id),
            platform_id: Pubkey::from(w.platform_id),
            base_mint:   Pubkey::from(w.base_mint),
            quote_mint:  Pubkey::from(w.quote_mint),
            base_vault:  Pubkey::from(w.base_vault),
            quote_vault: Pubkey::from(w.quote_vault),
            creator:     Pubkey::from(w.creator),
        })
    }
}

// PumpFun Bonding Curve
//
// BondingCurve layout V2 (Anchor):
//   _disc      (  8 @  0)
//   _reserves  ( 40 @  8) — 5×u64
//   _complete  (  1 @ 48) — bool
//   creator    ( 32 @ 49)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpFunBondingLayout {
    _disc:     [u8;  8],
    _reserves: [u8; 40],
    _complete: [u8;  1],
    creator:   [u8; 32],
}

pub struct PumpFunBondingCurve {
    pub creator: Pubkey,
}

impl PumpFunBondingCurve {
    pub fn try_from_bytes(data: &[u8]) -> Self {
        match parse_layout!(PumpFunBondingLayout, data, "") {
            Ok(w) => Self { creator: Pubkey::from(w.creator) },
            Err(_) => Self { creator: Pubkey::default() },
        }
    }
}

// PumpFun Global config (Anchor):
//   _disc          (  8 @  0)
//   _initialized   (  1 @  8)
//   _authority     ( 32 @  9)
//   fee_recipient  ( 32 @ 41)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpFunGlobalLayout {
    _disc:        [u8;  8],
    _initialized: [u8;  1],
    _authority:   [u8; 32],
    fee_recipient:[u8; 32],
}

pub struct PumpFunGlobal {
    pub fee_recipient: Pubkey,
}

impl PumpFunGlobal {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PumpFunGlobalLayout, data, "pumpfun global account too small")?;
        Ok(Self { fee_recipient: Pubkey::from(w.fee_recipient) })
    }
}

// PumpFun AMM
//
// Pool account layout (Anchor, 243 bytes):
//   _disc            (  8 @   0)
//   _pool_bump       (  1 @   8)
//   _index           (  2 @   9) — u16
//   _creator         ( 32 @  11)
//   base_mint        ( 32 @  43)
//   quote_mint       ( 32 @  75)
//   _lp_mint         ( 32 @ 107) — skipped
//   pool_base_vault  ( 32 @ 139)
//   pool_quote_vault ( 32 @ 171)
//   _lp_supply       (  8 @ 203) — u64, skipped
//   coin_creator     ( 32 @ 211)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpFunAmmLayout {
    _disc:            [u8;  8],
    _pool_bump:       [u8;  1],
    _index:           [u8;  2],
    _creator:         [u8; 32],
    base_mint:        [u8; 32],
    quote_mint:       [u8; 32],
    _lp_mint:         [u8; 32],
    pool_base_vault:  [u8; 32],
    pool_quote_vault: [u8; 32],
    _lp_supply:       [u8;  8],
    coin_creator:     [u8; 32],
}

pub struct PumpFunAmmPool {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_vault: Pubkey,
    pub pool_quote_vault: Pubkey,
    pub coin_creator: Pubkey,
}

impl PumpFunAmmPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PumpFunAmmLayout, data, "pumpfun amm account too small")?;
        Ok(Self {
            base_mint:        Pubkey::from(w.base_mint),
            quote_mint:       Pubkey::from(w.quote_mint),
            pool_base_vault:  Pubkey::from(w.pool_base_vault),
            pool_quote_vault: Pubkey::from(w.pool_quote_vault),
            coin_creator:     Pubkey::from(w.coin_creator),
        })
    }
}

// Meteora Standard (Dynamic AMM)
//
// Pool account layout (Anchor):
//   _disc              (  8 @   0)
//   _lp_mint           ( 32 @   8) — skipped
//   token_a_mint       ( 32 @  40)
//   token_b_mint       ( 32 @  72)
//   a_vault            ( 32 @ 104)
//   b_vault            ( 32 @ 136)
//   a_vault_lp         ( 32 @ 168)
//   b_vault_lp         ( 32 @ 200)
//   _bumps_enabled     (  2 @ 232) — a_vault_lp_bump(1) + enabled(1), skipped
//   admin_token_a_fee  ( 32 @ 234)
//   admin_token_b_fee  ( 32 @ 266)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraPoolLayout {
    _disc:             [u8;  8],
    _lp_mint:          [u8; 32],
    token_a_mint:      [u8; 32],
    token_b_mint:      [u8; 32],
    a_vault:           [u8; 32],
    b_vault:           [u8; 32],
    a_vault_lp:        [u8; 32],
    b_vault_lp:        [u8; 32],
    _bumps_enabled:    [u8;  2],
    admin_token_a_fee: [u8; 32],
    admin_token_b_fee: [u8; 32],
}

pub struct MeteoraPool {
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub a_vault: Pubkey,
    pub b_vault: Pubkey,
    pub a_vault_lp: Pubkey,
    pub b_vault_lp: Pubkey,
    pub admin_token_a_fee: Pubkey,
    pub admin_token_b_fee: Pubkey,
}

impl MeteoraPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraPoolLayout, data, "meteora pool account too small")?;
        Ok(Self {
            token_a_mint:      Pubkey::from(w.token_a_mint),
            token_b_mint:      Pubkey::from(w.token_b_mint),
            a_vault:           Pubkey::from(w.a_vault),
            b_vault:           Pubkey::from(w.b_vault),
            a_vault_lp:        Pubkey::from(w.a_vault_lp),
            b_vault_lp:        Pubkey::from(w.b_vault_lp),
            admin_token_a_fee: Pubkey::from(w.admin_token_a_fee),
            admin_token_b_fee: Pubkey::from(w.admin_token_b_fee),
        })
    }
}

// Meteora Vault layout (Anchor):
//   _disc        (  8 @   0)
//   _enabled     (  1 @   8)
//   _bump        (  1 @   9)
//   _flag        (  1 @  10)
//   _total_amt   (  8 @  11) — u64, skipped
//   token_vault  ( 32 @  19)
//   _fee_vault   ( 32 @  51) — skipped
//   _token_mint  ( 32 @  83) — skipped
//   lp_mint      ( 32 @ 115)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraVaultLayout {
    _disc:       [u8;  8],
    _misc:       [u8;  3],
    _total_amt:  [u8;  8],
    token_vault: [u8; 32],
    _fee_vault:  [u8; 32],
    _token_mint: [u8; 32],
    lp_mint:     [u8; 32],
}

pub struct MeteoraVault {
    pub token_vault: Pubkey,
    pub lp_mint: Pubkey,
}

impl MeteoraVault {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraVaultLayout, data, "meteora vault account too small")?;
        Ok(Self {
            token_vault: Pubkey::from(w.token_vault),
            lp_mint:     Pubkey::from(w.lp_mint),
        })
    }
}

// Meteora DLMM
//
// LbPair layout (Anchor):
//   _disc        (  8 @   0)
//   _parameters  ( 32 @   8) — skipped
//   _v_parameters( 32 @  40) — skipped
//   _misc        (  4 @  72) — bump(1)+bin_step_seed(2)+pair_type(1)
//   active_id    (  4 @  76) — i32
//   _skip_a      (  8 @  80) — bin_step(2) + 6 unknown bytes
//   token_x_mint ( 32 @  88)
//   token_y_mint ( 32 @ 120)
//   reserve_x    ( 32 @ 152)
//   reserve_y    ( 32 @ 184)
//   _skip_b      (336 @ 216) — protocol_fee(16)+padding(32)+reward_infos(288)
//   oracle       ( 32 @ 552)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraDlmmLayout {
    _disc:        [u8;   8],
    _parameters:  [u8;  32],
    _v_parameters:[u8;  32],
    _misc:        [u8;   4],
    active_id:    [u8;   4],
    _skip_a:      [u8;   8],
    token_x_mint: [u8;  32],
    token_y_mint: [u8;  32],
    reserve_x:    [u8;  32],
    reserve_y:    [u8;  32],
    _skip_b:      [u8; 336],
    oracle:       [u8;  32],
}

pub struct MeteoraDlmmLbPair {
    pub active_id: i32,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub oracle: Pubkey,
}

impl MeteoraDlmmLbPair {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraDlmmLayout, data, "meteora dlmm account too small")?;
        Ok(Self {
            active_id:    i32::from_le_bytes(w.active_id),
            token_x_mint: Pubkey::from(w.token_x_mint),
            token_y_mint: Pubkey::from(w.token_y_mint),
            reserve_x:    Pubkey::from(w.reserve_x),
            reserve_y:    Pubkey::from(w.reserve_y),
            oracle:       Pubkey::from(w.oracle),
        })
    }
}

// Meteora DAMM
//
// Pool account layout (Anchor, 1112 bytes):
//   _disc         (  8 @   0)
//   _pool_fees    (160 @   8) — skipped
//   token_a_mint  ( 32 @ 168)
//   token_b_mint  ( 32 @ 200)
//   token_a_vault ( 32 @ 232)
//   token_b_vault ( 32 @ 264)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraDammLayout {
    _disc:        [u8;   8],
    _pool_fees:   [u8; 160],
    token_a_mint: [u8;  32],
    token_b_mint: [u8;  32],
    token_a_vault:[u8;  32],
    token_b_vault:[u8;  32],
}

pub struct MeteoraDammPool {
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
}

impl MeteoraDammPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraDammLayout, data, "meteora damm account too small")?;
        Ok(Self {
            token_a_mint:  Pubkey::from(w.token_a_mint),
            token_b_mint:  Pubkey::from(w.token_b_mint),
            token_a_vault: Pubkey::from(w.token_a_vault),
            token_b_vault: Pubkey::from(w.token_b_vault),
        })
    }
}

// Meteora DBC
//
// VirtualPool layout (Anchor):
//   _head       ( 72 @   0)
//   config      ( 32 @  72)
//   _gap        ( 32 @ 104) — skipped field
//   base_mint   ( 32 @ 136)
//   base_vault  ( 32 @ 168)
//   quote_vault ( 32 @ 200)
//
// PoolConfig layout (Anchor):
//   _disc       (  8 @  0)
//   quote_mint  ( 32 @  8)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraDbcVirtualLayout {
    _head:      [u8; 72],
    config:     [u8; 32],
    _gap:       [u8; 32],
    base_mint:  [u8; 32],
    base_vault: [u8; 32],
    quote_vault:[u8; 32],
}

#[allow(dead_code)]
#[derive(ZeroPod)]
struct MeteoraDbcConfigLayout {
    _disc:      [u8;  8],
    quote_mint: [u8; 32],
}

pub struct MeteoraDbcVirtualPool {
    pub config: Pubkey,
    pub base_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

pub struct MeteoraDbcConfig {
    pub quote_mint: Pubkey,
}

impl MeteoraDbcVirtualPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraDbcVirtualLayout, data, "meteora dbc pool too small")?;
        Ok(Self {
            config:      Pubkey::from(w.config),
            base_mint:   Pubkey::from(w.base_mint),
            base_vault:  Pubkey::from(w.base_vault),
            quote_vault: Pubkey::from(w.quote_vault),
        })
    }
}

impl MeteoraDbcConfig {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(MeteoraDbcConfigLayout, data, "meteora dbc config too small")?;
        Ok(Self { quote_mint: Pubkey::from(w.quote_mint) })
    }
}

// Orca Whirlpool (and Byreal fork — identical layout)
//
// Layout (Anchor):
//   _disc              (  8 @   0)
//   _config            ( 32 @   8) — skipped
//   _bump              (  1 @  40) — skipped
//   tick_spacing       (  2 @  41) — u16, cast to i32
//   _tick_seed         (  2 @  43) — skipped
//   fee_rate           (  2 @  45) — u16
//   _protocol_fee_rate (  2 @  47) — skipped
//   liquidity          ( 16 @  49) — u128
//   sqrt_price_x64     ( 16 @  65) — u128
//   tick_current       (  4 @  81) — i32
//   _fees_owed         ( 16 @  85) — protocol_fee_owed_a(8)+b(8), skipped
//   token_mint_a       ( 32 @ 101)
//   token_vault_a      ( 32 @ 133)
//   _fee_growth_a      ( 16 @ 165) — u128, skipped
//   token_mint_b       ( 32 @ 181)
//   token_vault_b      ( 32 @ 213)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct OrcaWhirlpoolLayout {
    _disc:             [u8;  8],
    _config:           [u8; 32],
    _bump:             [u8;  1],
    tick_spacing:      [u8;  2],
    _tick_seed:        [u8;  2],
    fee_rate:          [u8;  2],
    _protocol_fee:     [u8;  2],
    liquidity:         [u8; 16],
    sqrt_price_x64:    [u8; 16],
    tick_current:      [u8;  4],
    _fees_owed:        [u8; 16],
    token_mint_a:      [u8; 32],
    token_vault_a:     [u8; 32],
    _fee_growth_a:     [u8; 16],
    token_mint_b:      [u8; 32],
    token_vault_b:     [u8; 32],
}

pub struct OrcaWhirlpool {
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub tick_spacing: i32,
    pub fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

impl OrcaWhirlpool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(OrcaWhirlpoolLayout, data, "orca whirlpool account too small")?;
        Ok(Self {
            tick_spacing:   u16::from_le_bytes(w.tick_spacing) as i32,
            fee_rate:       u16::from_le_bytes(w.fee_rate),
            liquidity:      u128::from_le_bytes(w.liquidity),
            sqrt_price_x64: u128::from_le_bytes(w.sqrt_price_x64),
            tick_current:   i32::from_le_bytes(w.tick_current),
            token_mint_a:   Pubkey::from(w.token_mint_a),
            token_vault_a:  Pubkey::from(w.token_vault_a),
            token_mint_b:   Pubkey::from(w.token_mint_b),
            token_vault_b:  Pubkey::from(w.token_vault_b),
        })
    }
}

// SPL Token Swap (shared by FluxBeam, Saros, Dooar)
//
// Layout (non-Anchor):
//   _version      (  1 @   0)
//   _is_init      (  1 @   1)
//   _bump         (  1 @   2)
//   token_program ( 32 @   3)
//   token_a_vault ( 32 @  35)
//   token_b_vault ( 32 @  67)
//   pool_mint     ( 32 @  99)
//   token_a_mint  ( 32 @ 131)
//   token_b_mint  ( 32 @ 163)
//   fee_account   ( 32 @ 195)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct SplTokenSwapLayout {
    _head:         [u8;  3],
    token_program: [u8; 32],
    token_a_vault: [u8; 32],
    token_b_vault: [u8; 32],
    pool_mint:     [u8; 32],
    token_a_mint:  [u8; 32],
    token_b_mint:  [u8; 32],
    fee_account:   [u8; 32],
}

pub struct SplTokenSwapPool {
    pub token_program: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
    pub pool_mint: Pubkey,
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub fee_account: Pubkey,
}

impl SplTokenSwapPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(SplTokenSwapLayout, data, "spl token swap pool too small")?;
        Ok(Self {
            token_program: Pubkey::from(w.token_program),
            token_a_vault: Pubkey::from(w.token_a_vault),
            token_b_vault: Pubkey::from(w.token_b_vault),
            pool_mint:     Pubkey::from(w.pool_mint),
            token_a_mint:  Pubkey::from(w.token_a_mint),
            token_b_mint:  Pubkey::from(w.token_b_mint),
            fee_account:   Pubkey::from(w.fee_account),
        })
    }
}

// FlashTrade
//
// Layout (Anchor):
//   _disc      (  8 @  0)
//   oracle     ( 32 @  8)
//   custody    ( 32 @ 40)
//   token_mint ( 32 @ 72)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct FlashTradeLayout {
    _disc:      [u8;  8],
    oracle:     [u8; 32],
    custody:    [u8; 32],
    token_mint: [u8; 32],
}

pub struct FlashTradePool {
    pub oracle: Pubkey,
    pub custody: Pubkey,
    pub token_mint: Pubkey,
}

impl FlashTradePool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(FlashTradeLayout, data, "flash trade pool too small")?;
        Ok(Self {
            oracle:     Pubkey::from(w.oracle),
            custody:    Pubkey::from(w.custody),
            token_mint: Pubkey::from(w.token_mint),
        })
    }
}

// DefiTuna Fusion
//
// Layout (Anchor, 423 bytes):
//   _disc              (  8 @   0)
//   _misc              (  3 @   8) — bump(1)+version(2)
//   token_mint_a       ( 32 @  11)
//   token_mint_b       ( 32 @  43)
//   token_vault_a      ( 32 @  75)
//   token_vault_b      ( 32 @ 107)
//   tick_spacing       (  2 @ 139) — u16
//   _tick_seed         (  2 @ 141) — skipped
//   fee_rate           (  2 @ 143) — u16
//   _skip              (  6 @ 145) — protocol_fee_rate(2)+unused0(4)
//   liquidity          ( 16 @ 151) — u128
//   sqrt_price         ( 16 @ 167) — u128
//   tick_current_index (  4 @ 183) — i32

#[allow(dead_code)]
#[derive(ZeroPod)]
struct DefiTunaFusionLayout {
    _disc:             [u8;  8],
    _misc:             [u8;  3],
    token_mint_a:      [u8; 32],
    token_mint_b:      [u8; 32],
    token_vault_a:     [u8; 32],
    token_vault_b:     [u8; 32],
    tick_spacing:      [u8;  2],
    _tick_seed:        [u8;  2],
    fee_rate:          [u8;  2],
    _skip:             [u8;  6],
    liquidity:         [u8; 16],
    sqrt_price_x64:    [u8; 16],
    tick_current_index:[u8;  4],
}

pub struct DefiTunaFusionPool {
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub tick_spacing: u16,
    pub fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current_index: i32,
}

impl DefiTunaFusionPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(DefiTunaFusionLayout, data, "defituna fusion pool too small")?;
        Ok(Self {
            token_mint_a:       Pubkey::from(w.token_mint_a),
            token_mint_b:       Pubkey::from(w.token_mint_b),
            token_vault_a:      Pubkey::from(w.token_vault_a),
            token_vault_b:      Pubkey::from(w.token_vault_b),
            tick_spacing:       u16::from_le_bytes(w.tick_spacing),
            fee_rate:           u16::from_le_bytes(w.fee_rate),
            liquidity:          u128::from_le_bytes(w.liquidity),
            sqrt_price_x64:     u128::from_le_bytes(w.sqrt_price_x64),
            tick_current_index: i32::from_le_bytes(w.tick_current_index),
        })
    }
}

// DefiTuna Pools
//
// Layout (Anchor):
//   _disc         (  8 @  0)
//   token_mint_a  ( 32 @  8)
//   token_mint_b  ( 32 @ 40)
//   token_vault_a ( 32 @ 72)
//   token_vault_b ( 32 @ 104)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct DefiTunaPoolsLayout {
    _disc:        [u8;  8],
    token_mint_a: [u8; 32],
    token_mint_b: [u8; 32],
    token_vault_a:[u8; 32],
    token_vault_b:[u8; 32],
}

pub struct DefiTunaPoolsPool {
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
}

impl DefiTunaPoolsPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(DefiTunaPoolsLayout, data, "defituna pools too small")?;
        Ok(Self {
            token_mint_a:  Pubkey::from(w.token_mint_a),
            token_mint_b:  Pubkey::from(w.token_mint_b),
            token_vault_a: Pubkey::from(w.token_vault_a),
            token_vault_b: Pubkey::from(w.token_vault_b),
        })
    }
}

// PancakeSwap CLMM
//
// Raydium CLMM fork (Anchor, 1544-byte pool account):
//   _disc         (  8 @   0)
//   _bump         (  1 @   8)
//   amm_config    ( 32 @   9)
//   _owner        ( 32 @  41) — skipped
//   token_mint_a  ( 32 @  73)
//   token_mint_b  ( 32 @ 105)
//   token_vault_a ( 32 @ 137)
//   token_vault_b ( 32 @ 169)
//   observation   ( 32 @ 201)
//   _decimals     (  2 @ 233) — decimals_a(1)+decimals_b(1), skipped
//   tick_spacing  (  2 @ 235) — u16, cast to i32
//   liquidity     ( 16 @ 237) — u128
//   sqrt_price    ( 16 @ 253) — u128
//   tick_current  (  4 @ 269) — i32

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PancakeSwapLayout {
    _disc:          [u8;  8],
    _bump:          [u8;  1],
    amm_config:     [u8; 32],
    _owner:         [u8; 32],
    token_mint_a:   [u8; 32],
    token_mint_b:   [u8; 32],
    token_vault_a:  [u8; 32],
    token_vault_b:  [u8; 32],
    observation:    [u8; 32],
    _decimals:      [u8;  2],
    tick_spacing:   [u8;  2],
    liquidity:      [u8; 16],
    sqrt_price_x64: [u8; 16],
    tick_current:   [u8;  4],
}

pub struct PancakeSwapPool {
    pub amm_config: Pubkey,
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub observation: Pubkey,
    pub tick_spacing: i32,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
}

impl PancakeSwapPool {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PancakeSwapLayout, data, "pancakeswap pool too small")?;
        Ok(Self {
            amm_config:     Pubkey::from(w.amm_config),
            token_mint_a:   Pubkey::from(w.token_mint_a),
            token_mint_b:   Pubkey::from(w.token_mint_b),
            token_vault_a:  Pubkey::from(w.token_vault_a),
            token_vault_b:  Pubkey::from(w.token_vault_b),
            observation:    Pubkey::from(w.observation),
            tick_spacing:   u16::from_le_bytes(w.tick_spacing) as i32,
            liquidity:      u128::from_le_bytes(w.liquidity),
            sqrt_price_x64: u128::from_le_bytes(w.sqrt_price_x64),
            tick_current:   i32::from_le_bytes(w.tick_current),
        })
    }
}

// Pumpup Pool (post-graduation AMM)
//
// Layout (Anchor):
//   _disc           (  8 @   0)
//   token_a_mint    ( 32 @   8)
//   token_b_mint    ( 32 @  40)
//   token_a_vault   ( 32 @  72)
//   token_b_vault   ( 32 @ 104)
//   _lp_mint        ( 32 @ 136) — skipped
//   fee_recipient   ( 32 @ 168)
//   token_a_reserve (  8 @ 200) — u64
//   token_b_reserve (  8 @ 208) — u64
//   _lp_supply      (  8 @ 216) — u64, skipped
//   _fee_rate       (  2 @ 224) — u16, skipped
//   _bump           (  1 @ 226) — skipped
//   fee_recipient2  ( 32 @ 227)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpupLayout {
    _disc:           [u8;  8],
    token_a_mint:    [u8; 32],
    token_b_mint:    [u8; 32],
    token_a_vault:   [u8; 32],
    token_b_vault:   [u8; 32],
    _lp_mint:        [u8; 32],
    fee_recipient:   [u8; 32],
    token_a_reserve: [u8;  8],
    token_b_reserve: [u8;  8],
    _lp_supply:      [u8;  8],
    _fee_rate:       [u8;  2],
    _bump:           [u8;  1],
    fee_recipient2:  [u8; 32],
}

pub struct PumpupPool {
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
    pub fee_recipient: Pubkey,
    pub fee_recipient2: Pubkey,
    pub token_a_reserve: u64,
    pub token_b_reserve: u64,
}

impl PumpupPool {
    pub const MIN_SIZE: usize = std::mem::size_of::<PumpupLayout>();

    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PumpupLayout, data, "pumpup pool too small")?;
        Ok(Self {
            token_a_mint:    Pubkey::from(w.token_a_mint),
            token_b_mint:    Pubkey::from(w.token_b_mint),
            token_a_vault:   Pubkey::from(w.token_a_vault),
            token_b_vault:   Pubkey::from(w.token_b_vault),
            fee_recipient:   Pubkey::from(w.fee_recipient),
            fee_recipient2:  Pubkey::from(w.fee_recipient2),
            token_a_reserve: u64::from_le_bytes(w.token_a_reserve),
            token_b_reserve: u64::from_le_bytes(w.token_b_reserve),
        })
    }
}

// Pumpup BondingCurve
//
// Stored inside pool_sol_account (PDA ["pumpup.pool", mint]).
// Layout (Anchor):
//   _disc                (  8 @  0)
//   launch_token_surplus (  8 @  8) — u64
//   virtual_sol          (  8 @ 16) — u64
//   real_sol             (  8 @ 24) — u64
//   pool_sol_reserves    (  8 @ 32) — u64
//   pool_token_reserves  (  8 @ 40) — u64

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpupBondingLayout {
    _disc:                [u8; 8],
    launch_token_surplus: [u8; 8],
    virtual_sol:          [u8; 8],
    real_sol:             [u8; 8],
    pool_sol_reserves:    [u8; 8],
    pool_token_reserves:  [u8; 8],
}

pub struct PumpupBondingCurve {
    pub launch_token_surplus: u64,
    pub virtual_sol: u64,
    pub real_sol: u64,
    pub pool_sol_reserves: u64,
    pub pool_token_reserves: u64,
}

impl PumpupBondingCurve {
    pub const MIN_SIZE: usize = std::mem::size_of::<PumpupBondingLayout>();

    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PumpupBondingLayout, data, "pumpup bonding curve too small")?;
        Ok(Self {
            launch_token_surplus: u64::from_le_bytes(w.launch_token_surplus),
            virtual_sol:          u64::from_le_bytes(w.virtual_sol),
            real_sol:             u64::from_le_bytes(w.real_sol),
            pool_sol_reserves:    u64::from_le_bytes(w.pool_sol_reserves),
            pool_token_reserves:  u64::from_le_bytes(w.pool_token_reserves),
        })
    }
}

// Pumpup Configuration singleton
//
// Layout (Anchor):
//   _disc              (  8 @  0)
//   _bump              (  1 @  8)
//   _fee_rate          (  8 @  9) — u64, skipped
//   _authority_address ( 32 @ 17) — skipped
//   fee_address        ( 32 @ 49)

#[allow(dead_code)]
#[derive(ZeroPod)]
struct PumpupConfigLayout {
    _disc:      [u8;  8],
    _bump:      [u8;  1],
    _fee_rate:  [u8;  8],
    _authority: [u8; 32],
    fee_address:[u8; 32],
}

pub struct PumpupConfig {
    pub fee_address: Pubkey,
}

impl PumpupConfig {
    pub fn try_from_bytes(data: &[u8]) -> TradeResult<Self> {
        let w = parse_layout!(PumpupConfigLayout, data, "pumpup config too small")?;
        Ok(Self { fee_address: Pubkey::from(w.fee_address) })
    }
}


#[cfg(test)]
pub(super) fn read_pubkey(data: &[u8], offset: usize) -> TradeResult<Pubkey> {
    data.get(offset..offset + 32)
        .and_then(|s| s.try_into().ok())
        .map(Pubkey::new_from_array)
        .ok_or_else(|| TradeError::Execution(format!(
            "account data too short at offset {offset}: need {}, have {}",
            offset + 32,
            data.len()
        )))
}
