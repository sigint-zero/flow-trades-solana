//! PumpFun AMM (PumpSwap, `pAMMBay6…`) executor + the venue's fee model.
//!
//! **Accounts** follow the PumpSwap IDL plus the pump_fees buyback accounts:
//! every swap must carry the buyback fee-recipient accounts as trailing
//! *remaining accounts*.
//! Those are per-pool/creator (buyback vault(s) + ATAs, 2–3+ accounts, some
//! Token-2022) and not derivable PDAs, so they are copied verbatim from a recent
//! on-chain swap (`pool::fetcher::resolve_pamm_fee_accounts`) and carried in
//! `PoolState::PumpFunAmm`. Without them the program errors with 6058; with an
//! incomplete set, 6023.
//!
//! **Buys are exact-INPUT** (`buy_exact_quote_in`): the user spends exactly
//! `amount_in` quote and the program enforces `min_base_amount_out`. The
//! exact-output `buy` would need a haircut on `base_amount_out` and reverts with
//! `ExceededSlippage` (6004) whenever reserves move between fetch and execution.
//!
//! **Fees are dynamic per pool**: the fee program's `fee_config` holds 25 tiers
//! keyed by market cap (`quote_reserve × base_supply / base_reserve`, lamports),
//! from 1.25% total at launch down to 0.30% above ~98k SOL. Both the quote
//! engine and the router floor use [`pamm_total_fee_bps`]; the table is read
//! from chain at startup ([`load_fee_tiers`]) with a decoded copy as the
//! built-in default.

use std::sync::RwLock;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;

use crate::error::{TradeError, TradeResult};
use crate::pool::types::{PoolState, SwapInstructions, SwapOrder};
use crate::constants::*;
use super::AmmExecutor;

// Anchor discriminators: SHA256("global:<name>")[0..8]
/// `sha256("global:buy")[..8]` — also used by the block scanners to tell a
/// swap apart from the program's other instructions (event self-CPI, create,
/// deposit, ...).
pub const BUY_DISC: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
/// `sha256("global:sell")[..8]`.
pub const SELL_DISC: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
/// `buy_exact_quote_in` — the exact-INPUT buy pump.fun added in 2026 (≈half
/// of all mainnet pAMM buys by Sept 2026, disc `c62e1552b4d9e870`). Not
/// built by this executor yet; recognised so the scanners count it as a swap.
pub const BUY_EXACT_QUOTE_IN_DISC: [u8; 8] = [0xc6, 0x2e, 0x15, 0x52, 0xb4, 0xd9, 0xe8, 0x70];

// Constant addresses from official PumpSwap IDL / mainnet
const GLOBAL_CONFIG: Pubkey = Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
/// One of the protocol fee recipients. Only a FALLBACK: pump.fun rotates the
/// valid recipient, so the executor prefers the one resolved from a recent
/// on-chain swap (`PoolState::PumpFunAmm::protocol_fee_recipient`).
const PROTOCOL_FEE_RECIPIENT: Pubkey = Pubkey::from_str_const("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");
const EVENT_AUTHORITY: Pubkey = Pubkey::from_str_const("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
/// Fee program (dynamic fee tiers + the buyback config).
pub const FEE_PROGRAM: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const ASSOC_TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

// ── Fee model ─────────────────────────────────────────────────────────────

/// One market-cap tier: applies to pools with `market_cap_lamports >= threshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    pub market_cap_lamports: u128,
    pub lp_bps: u16,
    pub protocol_bps: u16,
    pub creator_bps: u16,
}

impl FeeTier {
    pub fn total_bps(&self) -> u16 {
        self.lp_bps + self.protocol_bps + self.creator_bps
    }
}

/// The table in `fee_config` (`5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx`):
/// (market-cap threshold in SOL, lp, protocol, creator bps).
/// Refreshed from chain by [`load_fee_tiers`]; kept as the default so quoting
/// works before/without that read (and in tests).
const DEFAULT_TIERS: [(u64, u16, u16, u16); 25] = [
    (0, 2, 93, 30),
    (420, 20, 5, 95),
    (1_470, 20, 5, 90),
    (2_460, 20, 5, 85),
    (3_440, 20, 5, 80),
    (4_420, 20, 5, 75),
    (9_820, 20, 5, 70),
    (14_740, 20, 5, 65),
    (19_650, 20, 5, 60),
    (24_560, 20, 5, 55),
    (29_470, 20, 5, 50),
    (34_380, 20, 5, 45),
    (39_300, 20, 5, 40),
    (44_210, 20, 5, 35),
    (49_120, 20, 5, 30),
    (54_030, 20, 5, 28),
    (58_940, 20, 5, 25),
    (63_860, 20, 5, 23),
    (68_770, 20, 5, 20),
    (73_681, 20, 5, 18),
    (78_590, 20, 5, 15),
    (83_500, 20, 5, 13),
    (88_400, 20, 5, 10),
    (93_330, 20, 5, 8),
    (98_240, 20, 5, 5),
];

fn default_tiers() -> Vec<FeeTier> {
    DEFAULT_TIERS
        .iter()
        .map(|&(sol, lp, pr, cr)| FeeTier {
            market_cap_lamports: sol as u128 * 1_000_000_000,
            lp_bps: lp,
            protocol_bps: pr,
            creator_bps: cr,
        })
        .collect()
}

static FEE_TIERS: RwLock<Option<Vec<FeeTier>>> = RwLock::new(None);

/// `["fee_config", pAMM program]` under the fee program.
pub fn fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"fee_config", PUMP_FUN_AMM_PROG_ID.as_ref()], &FEE_PROGRAM).0
}

/// Decode the fee program's `FeeConfig` account: `disc(8) + bump(1) + admin(32)
/// + flat_fees(3×u64) + Vec<FeeTier>` where a tier is
/// `market_cap_lamports_threshold u128 + lp u64 + protocol u64 + creator u64`.
/// Tiers are returned sorted by threshold ascending.
pub fn parse_fee_config(data: &[u8]) -> TradeResult<Vec<FeeTier>> {
    let err = |m: &str| TradeError::Execution(format!("pamm fee_config: {m}"));
    let mut off = 8 + 1 + 32 + 24;
    let n_bytes: [u8; 4] = data.get(off..off + 4).ok_or_else(|| err("short"))?.try_into().unwrap();
    let n = u32::from_le_bytes(n_bytes) as usize;
    off += 4;
    if n == 0 || n > 256 {
        return Err(err("implausible tier count"));
    }
    let mut tiers = Vec::with_capacity(n);
    for _ in 0..n {
        let t = data.get(off..off + 40).ok_or_else(|| err("truncated tier"))?;
        let mcap = u128::from_le_bytes(t[..16].try_into().unwrap());
        let f = |i: usize| -> TradeResult<u16> {
            let v = u64::from_le_bytes(t[16 + i * 8..24 + i * 8].try_into().unwrap());
            u16::try_from(v).ok().filter(|b| *b <= 10_000).ok_or_else(|| err("fee bps out of range"))
        };
        tiers.push(FeeTier { market_cap_lamports: mcap, lp_bps: f(0)?, protocol_bps: f(1)?, creator_bps: f(2)? });
        off += 40;
    }
    tiers.sort_by_key(|t| t.market_cap_lamports);
    if tiers[0].market_cap_lamports != 0 {
        return Err(err("no base tier at market cap 0"));
    }
    Ok(tiers)
}

/// Read the live tier table from chain and install it. On any failure the
/// caller keeps the default — never trades on a half-read table.
pub async fn load_fee_tiers(rpc: &RpcClient) -> TradeResult<usize> {
    let acct = rpc
        .get_account(&fee_config_pda())
        .await
        .map_err(|e| TradeError::Rpc(format!("pamm fee_config fetch: {e}")))?;
    let tiers = parse_fee_config(&acct.data)?;
    let n = tiers.len();
    *FEE_TIERS.write().unwrap_or_else(|p| p.into_inner()) = Some(tiers);
    Ok(n)
}

/// Install a tier table directly (tests).
pub fn set_fee_tiers(tiers: Vec<FeeTier>) {
    *FEE_TIERS.write().unwrap_or_else(|p| p.into_inner()) = Some(tiers);
}

static DEFAULT_TIER_VEC: std::sync::LazyLock<Vec<FeeTier>> = std::sync::LazyLock::new(default_tiers);

fn with_tiers<R>(f: impl FnOnce(&[FeeTier]) -> R) -> R {
    let guard = FEE_TIERS.read().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(t) => f(t),
        // no per-call allocation: this sits on the quote hot path
        None => f(&DEFAULT_TIER_VEC),
    }
}

/// Total swap fee (bps) for a pool at `market_cap_lamports`.
pub fn fee_bps_for_market_cap(market_cap_lamports: u128) -> u16 {
    with_tiers(|tiers| {
        tiers
            .iter()
            .rev()
            .find(|t| market_cap_lamports >= t.market_cap_lamports)
            .or(tiers.first())
            .map(|t| t.total_bps())
            .unwrap_or(125)
    })
}

/// Market cap in lamports as the fee program sees it: `price × supply` with
/// price = quote per base from the EFFECTIVE reserves (vault + virtual quote).
/// `None` when the pool is not SOL-quoted (the program converts other quotes;
/// we don't) or a value is 0.
pub fn pamm_market_cap_lamports(state: &PoolState) -> Option<u128> {
    let PoolState::PumpFunAmm { quote_mint, base_reserve, quote_reserve, base_supply, virtual_quote_reserve, .. } = state else {
        return None;
    };
    if *quote_mint != SOL_NATIVE_MINT || *base_reserve == 0 || *base_supply == 0 {
        return None;
    }
    Some((*quote_reserve as u128 + *virtual_quote_reserve as u128).checked_mul(*base_supply as u128)? / (*base_reserve as u128))
}

/// The fee tier this pool is in right now, or the most expensive tier when the
/// market cap cannot be computed (unknown supply, non-SOL quote). Too HIGH an
/// assumed fee under-quotes and weakens the floor; too LOW over-quotes and
/// makes the router revert good trades, so the fallback errs high.
pub fn pamm_fee_tier(state: &PoolState) -> FeeTier {
    with_tiers(|tiers| {
        let pick = match pamm_market_cap_lamports(state) {
            Some(mcap) => tiers.iter().rev().find(|t| mcap >= t.market_cap_lamports).or(tiers.first()),
            None => tiers.iter().max_by_key(|t| t.total_bps()),
        };
        pick.copied().unwrap_or(FeeTier { market_cap_lamports: 0, lp_bps: 20, protocol_bps: 5, creator_bps: 100 })
    })
}

/// Total swap fee (bps) this pool charges right now.
pub fn pamm_total_fee_bps(state: &PoolState) -> u16 {
    pamm_fee_tier(state).total_bps()
}

#[inline]
fn ceil_bps(amount: u64, bps: u16) -> u64 {
    ((amount as u128 * bps as u128).div_ceil(10_000)) as u64
}

/// lp + protocol + creator fee on `amount`, each rounded up separately (the
/// program's arithmetic). Creator fee is 0 when the pool has no coin creator.
pub fn pamm_fees(tier: &FeeTier, has_creator: bool, amount: u64) -> u64 {
    ceil_bps(amount, tier.lp_bps) + ceil_bps(amount, tier.protocol_bps) + if has_creator { ceil_bps(amount, tier.creator_bps) } else { 0 }
}

/// Exact-input pAMM quote, the way the program settles it (matches on-chain
/// `BuyEvent`/`SellEvent`s to the atom):
///
/// * curve: x·y=k on `base_reserve` and `quote_reserve + virtual_quote_reserve`
/// * SELL (base→quote): `out = floor(b·Rq'/(Rb+b))`, user receives
///   `out − Σceil(out·fee_i)`
/// * BUY  (quote→base, `buy_exact_quote_in`): the largest `x` with
///   `x + Σceil(x·fee_i) ≤ quote_in` is the fee base, and the program puts
///   `x − 1` lamports into the curve: `base = floor((x−1)·Rb/(Rq'+x−1))`
///   (without the `−1` a quote is one unit-price of base too high)
///
/// Returns `(amount_out, fee_in_quote_units)`.
pub fn pamm_quote_exact_in(state: &PoolState, input_mint: &Pubkey, amount_in: u64) -> Option<(u64, u64)> {
    let PoolState::PumpFunAmm { base_mint, quote_mint, base_reserve, quote_reserve, virtual_quote_reserve, coin_creator, .. } = state else {
        return None;
    };
    if amount_in == 0 || *base_reserve == 0 {
        return None;
    }
    let tier = pamm_fee_tier(state);
    let has_creator = *coin_creator != Pubkey::default();
    let rb = *base_reserve as u128;
    let rq = *quote_reserve as u128 + *virtual_quote_reserve as u128;
    if input_mint == base_mint {
        let out = (amount_in as u128).checked_mul(rq)? / (rb + amount_in as u128);
        let out = u64::try_from(out).ok()?;
        let fee = pamm_fees(&tier, has_creator, out);
        let net = out.checked_sub(fee)?;
        if net == 0 || out as u128 >= rq {
            return None;
        }
        Some((net, fee))
    } else if input_mint == quote_mint {
        let total_bps = tier.lp_bps + tier.protocol_bps + if has_creator { tier.creator_bps } else { 0 };
        let mut x = ((amount_in as u128) * 10_000 / (10_000 + total_bps as u128)) as u64;
        while x > 0 && x.checked_add(pamm_fees(&tier, has_creator, x))? > amount_in {
            x -= 1;
        }
        if x <= 1 {
            return None;
        }
        let xc = (x - 1) as u128;
        let base = xc.checked_mul(rb)? / (rq + xc);
        let base = u64::try_from(base).ok()?;
        if base == 0 || base as u128 >= rb {
            return None;
        }
        Some((base, amount_in - x))
    } else {
        None
    }
}

/// Legacy shape kept for callers that want `(out, fee_bps)`.
pub fn pamm_quote_out(state: &PoolState, input_mint: &Pubkey, amount_in: u64) -> Option<(u64, u16)> {
    let (out, _) = pamm_quote_exact_in(state, input_mint, amount_in)?;
    Some((out, pamm_total_fee_bps(state)))
}

// ── Executor ──────────────────────────────────────────────────────────────

pub struct PumpFunAmmExecutor;

impl AmmExecutor for PumpFunAmmExecutor {
    fn build_swap_ix(
        &self,
        order: &SwapOrder,
        pool_state: &PoolState,
    ) -> TradeResult<SwapInstructions> {
        let (pool, base_mint, quote_mint,
             pool_base_vault, pool_quote_vault, coin_creator,
             base_reserve, quote_reserve,
             resolved_fee_recipient, buyback_accounts) =
            match pool_state {
                PoolState::PumpFunAmm {
                    pool, base_mint, quote_mint,
                    pool_base_vault, pool_quote_vault, coin_creator,
                    base_reserve, quote_reserve,
                    protocol_fee_recipient, buyback_accounts, ..
                } => (pool, base_mint, quote_mint,
                      pool_base_vault, pool_quote_vault, coin_creator,
                      *base_reserve, *quote_reserve,
                      *protocol_fee_recipient, buyback_accounts),
                _ => return Err(TradeError::Execution("expected PumpFunAmm pool state".into())),
            };

        // No buyback accounts = no valid swap (error 6058 on-chain). They are
        // resolved lazily from a recent swap on the pool; a brand-new pool with
        // no swap yet cannot be traded through this executor.
        if buyback_accounts.is_empty() {
            return Err(TradeError::Execution(
                "PumpFun AMM: buyback remaining-accounts unresolved (no recent on-chain swap to read them from)".into()
            ));
        }
        let protocol_fee_recipient = if resolved_fee_recipient != Pubkey::default() {
            resolved_fee_recipient
        } else {
            PROTOCOL_FEE_RECIPIENT
        };

        if base_reserve == 0 || quote_reserve == 0 {
            return Err(TradeError::Execution(
                "PumpFun AMM pool reserves unavailable -- cannot size the swap".into()
            ));
        }

        // PumpSwap IDL:
        //   BuyExactQuoteIn = spend exactly quote_amount_in, receive ≥ min_base_amount_out base
        //   Sell            = send base_amount_in, receive ≥ min_quote_amount_out quote
        let use_buy_ix = order.output_mint == *base_mint;

        // Token program for each pool-side mint
        let (base_prog, quote_prog) = if order.input_mint == *base_mint {
            (order.input_token_program, order.output_token_program)
        } else {
            (order.output_token_program, order.input_token_program)
        };
        let user_base_ata = get_associated_token_address_with_program_id(&order.user, base_mint, &base_prog);
        let user_quote_ata = get_associated_token_address_with_program_id(&order.user, quote_mint, &quote_prog);

        let mut setup = vec![
            create_associated_token_account_idempotent(
                &order.user, &order.user, base_mint, &base_prog,
            ),
            create_associated_token_account_idempotent(
                &order.user, &order.user, quote_mint, &quote_prog,
            ),
        ];
        let mut cleanup = Vec::new();

        // WSOL wrapping when spending SOL
        if order.input_mint == SOL_NATIVE_MINT {
            let user_wsol = get_associated_token_address_with_program_id(
                &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            );
            setup.push(solana_sdk::system_instruction::transfer(
                &order.user, &user_wsol, order.amount_in,
            ));
            setup.push(spl_token::instruction::sync_native(&TOKEN_PROGRAM_ID, &user_wsol).unwrap());
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_wsol, &order.user, &order.user, &[],
            ).unwrap());
        }
        // WSOL unwrapping when receiving SOL
        if order.output_mint == SOL_NATIVE_MINT {
            let user_wsol = get_associated_token_address_with_program_id(
                &order.user, &SOL_NATIVE_MINT, &TOKEN_PROGRAM_ID,
            );
            cleanup.push(spl_token::instruction::close_account(
                &TOKEN_PROGRAM_ID, &user_wsol, &order.user, &order.user, &[],
            ).unwrap());
        }

        // Derive PDAs
        let protocol_fee_recipient_ta = get_associated_token_address_with_program_id(
            &protocol_fee_recipient, quote_mint, &quote_prog,
        );

        let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
            &[b"creator_vault", coin_creator.as_ref()],
            &PUMP_FUN_AMM_PROG_ID,
        );

        let coin_creator_vault_ata = get_associated_token_address_with_program_id(
            &coin_creator_vault_authority, quote_mint, &quote_prog,
        );

        let fee_config = fee_config_pda();

        // Instruction data
        //   BuyExactQuoteIn: disc(8) + quote_amount_in(u64) + min_base_amount_out(u64) + track_volume(1)
        //   Sell:            disc(8) + base_amount_in(u64)  + min_quote_amount_out(u64)
        // Both are exact-input, so `amount_in` / `min_amount_out` map 1:1. The
        // program enforces the floor as well as the router.
        let mut data = Vec::with_capacity(25);
        if use_buy_ix {
            data.extend_from_slice(&BUY_EXACT_QUOTE_IN_DISC);
            data.extend_from_slice(&order.amount_in.to_le_bytes());
            data.extend_from_slice(&order.min_amount_out.to_le_bytes());
            // track_volume: OptionBool (IDL struct{bool}) — required, false.
            data.push(0u8);
        } else {
            data.extend_from_slice(&SELL_DISC);
            data.extend_from_slice(&order.amount_in.to_le_bytes());
            data.extend_from_slice(&order.min_amount_out.to_le_bytes());
        }

        // Account layout: PumpSwap IDL + pump_fees buyback update (mid-2026)
        //
        // Buy  (25+): [0]-[18] common, [19] global_volume_accumulator,
        //   [20] user_volume_accumulator, [21] fee_config, [22] fee_program,
        //   [23..] buyback remaining accounts
        // Sell (23+): [0]-[18] common, [19] fee_config, [20] fee_program,
        //   [21..] buyback remaining accounts
        let mut accounts = vec![
            AccountMeta::new(*pool, false),                                    // [0]
            AccountMeta::new(order.user, true),                                // [1]
            AccountMeta::new_readonly(GLOBAL_CONFIG, false),                   // [2]
            AccountMeta::new_readonly(*base_mint, false),                      // [3]
            AccountMeta::new_readonly(*quote_mint, false),                     // [4]
            AccountMeta::new(user_base_ata, false),                            // [5]
            AccountMeta::new(user_quote_ata, false),                           // [6]
            AccountMeta::new(*pool_base_vault, false),                         // [7]
            AccountMeta::new(*pool_quote_vault, false),                        // [8]
            AccountMeta::new_readonly(protocol_fee_recipient, false),          // [9]
            AccountMeta::new(protocol_fee_recipient_ta, false),                // [10]
            AccountMeta::new_readonly(base_prog, false),                       // [11]
            AccountMeta::new_readonly(quote_prog, false),                      // [12]
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),                  // [13]
            AccountMeta::new_readonly(ASSOC_TOKEN_PROGRAM, false),             // [14]
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),                 // [15]
            AccountMeta::new_readonly(PUMP_FUN_AMM_PROG_ID, false),            // [16]
            AccountMeta::new(coin_creator_vault_ata, false),                   // [17]
            AccountMeta::new_readonly(coin_creator_vault_authority, false),     // [18]
        ];

        if use_buy_ix {
            let (global_vol, _) = Pubkey::find_program_address(
                &[b"global_volume_accumulator"],
                &PUMP_FUN_AMM_PROG_ID,
            );
            let (user_vol, _) = Pubkey::find_program_address(
                &[b"user_volume_accumulator", order.user.as_ref()],
                &PUMP_FUN_AMM_PROG_ID,
            );
            accounts.push(AccountMeta::new_readonly(global_vol, false));       // [19]
            accounts.push(AccountMeta::new(user_vol, false));                  // [20]
        }

        // Common tail: fee_config + fee_program + the buyback remaining accounts
        // (verbatim, with their on-chain writability).
        accounts.push(AccountMeta::new_readonly(fee_config, false));
        accounts.push(AccountMeta::new_readonly(FEE_PROGRAM, false));
        for (pk, writable) in buyback_accounts {
            accounts.push(if *writable {
                AccountMeta::new(*pk, false)
            } else {
                AccountMeta::new_readonly(*pk, false)
            });
        }

        let swap_ix = Instruction {
            program_id: PUMP_FUN_AMM_PROG_ID,
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

    fn state(buyback: Vec<(Pubkey, bool)>, recipient: Pubkey) -> (PoolState, Pubkey, Pubkey) {
        let base_mint = Pubkey::new_unique();
        let quote_mint = SOL_NATIVE_MINT;
        let s = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 10_000_000,
            quote_reserve: 5_000_000,
            protocol_fee_recipient: recipient,
            buyback_accounts: buyback,
            base_supply: 1_000_000_000_000_000,
            virtual_quote_reserve: 0,
        };
        (s, base_mint, quote_mint)
    }

    fn order(input: Pubkey, output: Pubkey) -> SwapOrder {
        SwapOrder {
            pool_address: Pubkey::new_unique(),
            pool_type: PoolType::PumpFunAmm,
            input_mint: input,
            output_mint: output,
            amount_in: 1_000_000,
            min_amount_out: 777,
            user: Pubkey::new_unique(),
            input_token_program: TOKEN_PROGRAM_ID,
            output_token_program: TOKEN_PROGRAM_ID,
        }
    }

    #[test]
    fn refuses_to_build_without_buyback_accounts() {
        let (s, base, quote) = state(Vec::new(), Pubkey::default());
        let err = PumpFunAmmExecutor.build_swap_ix(&order(quote, base), &s).unwrap_err();
        assert!(err.to_string().contains("buyback"), "{err}");
    }

    #[test]
    fn buy_is_exact_input_with_the_floor_in_the_instruction() {
        let bb = vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), false), (Pubkey::new_unique(), true)];
        let recipient = Pubkey::new_unique();
        let (s, base, quote) = state(bb.clone(), recipient);
        let o = order(quote, base);
        let ixs = PumpFunAmmExecutor.build_swap_ix(&o, &s).unwrap();
        let ix = &ixs.swap[0];
        assert_eq!(ix.accounts.len(), 19 + 2 + 2 + bb.len());
        assert_eq!(ix.accounts[9].pubkey, recipient, "resolved recipient wins over the constant");
        assert_eq!(ix.accounts[21].pubkey, fee_config_pda());
        assert_eq!(ix.accounts[22].pubkey, FEE_PROGRAM);
        for (i, (pk, w)) in bb.iter().enumerate() {
            assert_eq!(ix.accounts[23 + i].pubkey, *pk);
            assert_eq!(ix.accounts[23 + i].is_writable, *w, "writability copied verbatim");
        }
        assert_eq!(ix.data.len(), 25);
        assert_eq!(ix.data[..8], BUY_EXACT_QUOTE_IN_DISC);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), o.amount_in, "spend exactly amount_in");
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), o.min_amount_out, "floor passed through");
        assert_eq!(ix.data[24], 0, "track_volume=false");
        let (pool_v2, _) = Pubkey::find_program_address(&[b"pool-v2", base.as_ref()], &PUMP_FUN_AMM_PROG_ID);
        assert!(!ix.accounts.iter().any(|a| a.pubkey == pool_v2), "the pre-buyback pool_v2 account must be gone");
    }

    #[test]
    fn sell_layout_has_no_volume_accumulators() {
        let bb = vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), true)];
        let (s, base, quote) = state(bb.clone(), Pubkey::default());
        let o = order(base, quote);
        let ixs = PumpFunAmmExecutor.build_swap_ix(&o, &s).unwrap();
        let ix = &ixs.swap[0];
        assert_eq!(ix.accounts.len(), 19 + 2 + bb.len());
        assert_eq!(ix.accounts[9].pubkey, PROTOCOL_FEE_RECIPIENT, "unresolved recipient → constant");
        assert_eq!(ix.accounts[20].pubkey, FEE_PROGRAM);
        assert_eq!(ix.accounts[21].pubkey, bb[0].0);
        assert_eq!(ix.data.len(), 24);
        assert_eq!(ix.data[..8], SELL_DISC);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), o.min_amount_out);
    }

    #[test]
    fn fee_tiers_default_table_is_monotonic_and_matches_pump_fun_schedule() {
        let t = default_tiers();
        assert_eq!(t.len(), 25);
        assert_eq!(t[0].total_bps(), 125, "1.25% at launch");
        assert_eq!(t[1].total_bps(), 120);
        assert_eq!(t.last().unwrap().total_bps(), 30, "0.30% at the top tier");
        for w in t.windows(2) {
            assert!(w[0].market_cap_lamports < w[1].market_cap_lamports);
            assert!(w[0].total_bps() >= w[1].total_bps(), "fees fall as market cap rises");
        }
        assert_eq!(fee_bps_for_market_cap(0), 125);
        assert_eq!(fee_bps_for_market_cap(419_999_999_999), 125);
        assert_eq!(fee_bps_for_market_cap(420_000_000_000), 120);
        assert_eq!(fee_bps_for_market_cap(u128::MAX), 30);
    }

    #[test]
    fn fee_config_roundtrip_parses_the_on_chain_layout() {
        let mut d = vec![0u8; 8 + 1 + 32 + 24];
        let tiers = default_tiers();
        d.extend_from_slice(&(tiers.len() as u32).to_le_bytes());
        for t in tiers.iter().rev() {
            // reverse order on purpose: the parser must sort
            d.extend_from_slice(&t.market_cap_lamports.to_le_bytes());
            d.extend_from_slice(&(t.lp_bps as u64).to_le_bytes());
            d.extend_from_slice(&(t.protocol_bps as u64).to_le_bytes());
            d.extend_from_slice(&(t.creator_bps as u64).to_le_bytes());
        }
        assert_eq!(parse_fee_config(&d).unwrap(), tiers);
        assert!(parse_fee_config(&d[..60]).is_err(), "truncated");
    }

    #[test]
    fn market_cap_and_fee_from_state() {
        let (mut s, _, _) = state(vec![(Pubkey::new_unique(), true)], Pubkey::default());
        if let PoolState::PumpFunAmm { base_reserve, quote_reserve, base_supply, .. } = &mut s {
            *base_reserve = 200_000_000_000_000; // 200M tokens (6 dp)
            *quote_reserve = 100_000_000_000; // 100 SOL
            *base_supply = 1_000_000_000_000_000; // 1B tokens
        }
        // mcap = 100 SOL × 1B / 200M = 500 SOL → tier 1 (≥420) = 120 bps
        assert_eq!(pamm_market_cap_lamports(&s), Some(500_000_000_000));
        assert_eq!(pamm_total_fee_bps(&s), 120);
        if let PoolState::PumpFunAmm { base_supply, .. } = &mut s {
            *base_supply = 0;
        }
        assert_eq!(pamm_total_fee_bps(&s), 125, "unknown supply → most expensive tier");
    }

    /// Mainnet pool 5aww2ejqUtWXmTFj5VaerYrjva9HoV4AQ8S5ynkvUjPx, two consecutive
    /// swaps decoded from their Buy/Sell events. Tier = 20/5/75.
    fn live_state(base_reserve: u64, quote_reserve: u64) -> (PoolState, Pubkey, Pubkey) {
        let (mut s, base, quote) = state(vec![(Pubkey::new_unique(), true)], Pubkey::default());
        if let PoolState::PumpFunAmm { base_reserve: b, quote_reserve: q, base_supply, virtual_quote_reserve, coin_creator, quote_mint, .. } = &mut s {
            *b = base_reserve;
            *q = quote_reserve;
            *base_supply = 998_900_000_000_029;
            *virtual_quote_reserve = 17_584_489_928;
            *coin_creator = Pubkey::new_unique();
            *quote_mint = SOL_NATIVE_MINT;
        }
        (s, base, quote)
    }

    #[test]
    fn sell_reproduces_the_live_sell_event_exactly() {
        let (s, base, _) = live_state(55_506_675_665_904, 301_093_969_018);
        assert_eq!(pamm_fee_tier(&s).total_bps(), 100, "mcap ≈ 5.7k SOL → 20/5/75 tier");
        let (net, fee) = pamm_quote_exact_in(&s, &base, 24_970_275).unwrap();
        // event: quote_amount_out 143_360; lp 287 + protocol 72 + creator 1_076; user got 141_925
        assert_eq!(fee, 287 + 72 + 1_076);
        assert_eq!(net, 141_925);
    }

    #[test]
    fn buy_reproduces_live_buy_exact_quote_in_events_exactly() {
        // pool FG41siTfAwA9FRCMRsBj3Gj3u4G9h31vg12jmqTkXBRE (tier 0: 2/93/30 bps), three
        // mainnet `buy_exact_quote_in` events: (T, x, base_out, Rb, Rq, V)
        for (t, x, base_out, rb, rq, v) in [
            (18_589_437u64, 18_359_937u64, 584_032_039_195u64, 771_187_064_222_101u64, 6_640_573_789u64, 17_584_505_321u64),
            (990_000, 977_776, 26_832_939_742, 716_031_330_524_085, 8_506_236_295, 17_584_505_321),
            (14_864_805, 14_681_288, 388_316_710_828, 703_135_963_411_567, 8_984_631_680, 17_584_505_321),
        ] {
            let (mut s, _, quote) = live_state(rb, rq);
            if let PoolState::PumpFunAmm { base_supply, virtual_quote_reserve, .. } = &mut s {
                *base_supply = 947_094_348_075_472; // mcap ≈ 34 SOL → tier 0
                *virtual_quote_reserve = v;
            }
            let tier = pamm_fee_tier(&s);
            assert_eq!((tier.lp_bps, tier.protocol_bps, tier.creator_bps), (2, 93, 30));
            let (got, fee) = pamm_quote_exact_in(&s, &quote, t).unwrap();
            assert_eq!(fee, t - x, "fee base x for T={t}");
            assert_eq!(got, base_out, "base out for T={t}");
        }
        // a plain `buy` (exact base out) event: the program charged 258_827 + fees for
        // 45_081_714 base; exact-in with the same total must yield at least that base
        let (s, _, quote) = live_state(55_506_700_636_179, 301_093_825_945);
        let (base_out, fee) = pamm_quote_exact_in(&s, &quote, 261_417).unwrap();
        assert_eq!(fee, 518 + 130 + 1_942);
        assert!(base_out >= 45_081_714 && base_out < 45_081_714 + 400, "{base_out}");
    }

    #[test]
    fn quote_applies_the_tier_fee_on_the_input_side() {
        let (mut s, base, quote) = state(vec![(Pubkey::new_unique(), true)], Pubkey::default());
        if let PoolState::PumpFunAmm { base_reserve, quote_reserve, base_supply, .. } = &mut s {
            *base_reserve = 200_000_000_000_000;
            *quote_reserve = 100_000_000_000;
            *base_supply = 1_000_000_000_000_000;
        }
        let (out, fee) = pamm_quote_out(&s, &quote, 1_000_000_000).unwrap();
        assert_eq!(fee, 120);
        // exact model ≈ fee-on-input constant product (within the −1 lamport + per-component ceil rounding)
        let approx = crate::quote::math::compute_constant_product_out(100_000_000_000, 200_000_000_000_000, 1_000_000_000, 120).unwrap();
        assert!((out as i128 - approx as i128).abs() < approx as i128 / 1_000, "{out} vs {approx}"); // fee/(1+fee) vs fee: ~1.4e-4
        let (sell_out, _) = pamm_quote_out(&s, &base, 1_000_000).unwrap();
        assert!(sell_out > 0);
        assert!(pamm_quote_out(&s, &Pubkey::new_unique(), 1).is_none(), "unrelated mint");
    }
}
