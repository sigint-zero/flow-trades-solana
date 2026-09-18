//! Mint facts the quoter needs and no pool account carries: the token program
//! and, for Token-2022 mints, the transfer fee. A transfer-fee mint delivers
//! `out − fee(out)` to the user's account, which is what the router's
//! slippage check measures — quoting the gross DEX output over-quotes by the
//! fee and the venue reverts on its own slippage check.
//!
//! Filled off the quote path (`ensure_mint_info`, batched); the hot path reads
//! [`transfer_fee`] and treats an unknown mint as fee-free until known.

use std::sync::LazyLock;

use dashmap::{DashMap, DashSet};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use crate::constants::TOKEN_2022_PROGRAM_ID;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferFee {
    pub basis_points: u16,
    pub maximum_fee: u64,
}

impl TransferFee {
    /// Fee withheld from a transfer of `amount` (ceil, capped at `maximum_fee`).
    pub fn fee(&self, amount: u64) -> u64 {
        if self.basis_points == 0 || amount == 0 {
            return 0;
        }
        let raw = (amount as u128 * self.basis_points as u128).div_ceil(10_000) as u64;
        raw.min(self.maximum_fee)
    }
}

/// Mints already looked up (fee-free ones are here too).
static KNOWN: LazyLock<DashSet<Pubkey>> = LazyLock::new(DashSet::new);
static FEES: LazyLock<DashMap<Pubkey, TransferFee>> = LazyLock::new(DashMap::new);
static PROGRAMS: LazyLock<DashMap<Pubkey, Pubkey>> = LazyLock::new(DashMap::new);

pub fn transfer_fee(mint: &Pubkey) -> Option<TransferFee> {
    FEES.get(mint).map(|f| *f)
}

pub fn is_known(mint: &Pubkey) -> bool {
    KNOWN.contains(mint)
}

pub fn token_program(mint: &Pubkey) -> Option<Pubkey> {
    PROGRAMS.get(mint).map(|p| *p)
}

/// Net amount a recipient receives when `amount` of `mint` is transferred.
pub fn net_of_transfer_fee(mint: &Pubkey, amount: u64) -> u64 {
    match transfer_fee(mint) {
        Some(f) => amount.saturating_sub(f.fee(amount)),
        None => amount,
    }
}

/// Parse a Token-2022 mint's `TransferFeeConfig` extension (type 1). Returns
/// the HIGHER of the older/newer schedule — conservative for a quote.
pub fn parse_transfer_fee(data: &[u8]) -> Option<TransferFee> {
    // base mint 82 B, padded to 165, then account type byte (1 = Mint), then TLV
    if data.len() < 166 || data[165] != 1 {
        return None;
    }
    let mut o = 166;
    while o + 4 <= data.len() {
        let typ = u16::from_le_bytes(data[o..o + 2].try_into().unwrap());
        let len = u16::from_le_bytes(data[o + 2..o + 4].try_into().unwrap()) as usize;
        let body = data.get(o + 4..o + 4 + len)?;
        if typ == 1 && len >= 108 {
            // authority 32 | withdraw authority 32 | withheld u64 | older {epoch u64, max u64, bps u16} | newer {…}
            let rd64 = |p: usize| u64::from_le_bytes(body[p..p + 8].try_into().unwrap());
            let rd16 = |p: usize| u16::from_le_bytes(body[p..p + 2].try_into().unwrap());
            let older = TransferFee { maximum_fee: rd64(72 + 8), basis_points: rd16(72 + 16) };
            let newer = TransferFee { maximum_fee: rd64(90 + 8), basis_points: rd16(90 + 16) };
            return Some(if newer.basis_points >= older.basis_points { newer } else { older });
        }
        if typ == 0 {
            break;
        }
        o += 4 + len;
    }
    None
}

pub fn record(mint: Pubkey, owner: Pubkey, data: &[u8]) {
    PROGRAMS.insert(mint, owner);
    if owner == TOKEN_2022_PROGRAM_ID {
        if let Some(f) = parse_transfer_fee(data) {
            if f.basis_points > 0 {
                FEES.insert(mint, f);
            }
        }
    }
    KNOWN.insert(mint);
}

/// Look up every not-yet-known mint in `mints` (one `getMultipleAccounts` per 100).
pub async fn ensure_mint_info(rpc: &RpcClient, mints: &[Pubkey]) -> usize {
    let todo: Vec<Pubkey> = {
        let mut v: Vec<Pubkey> = mints.iter().copied().filter(|m| !KNOWN.contains(m)).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    if todo.is_empty() {
        return 0;
    }
    let mut n = 0;
    for chunk in todo.chunks(100) {
        if let Ok(accts) = rpc.get_multiple_accounts(chunk).await {
            for (m, a) in chunk.iter().zip(accts) {
                if let Some(a) = a {
                    record(*m, a.owner, &a.data);
                    n += 1;
                }
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_is_ceil_and_capped() {
        let f = TransferFee { basis_points: 100, maximum_fee: 5_000 };
        assert_eq!(f.fee(1_000_000), 5_000);
        assert_eq!(f.fee(100_000), 1_000);
        assert_eq!(f.fee(1), 1, "ceil");
        assert_eq!(TransferFee::default().fee(1_000), 0);
    }

    #[test]
    fn parses_transfer_fee_config_tlv() {
        let mut d = vec![0u8; 165 + 1 + 4 + 108];
        d[165] = 1;
        d[166..168].copy_from_slice(&1u16.to_le_bytes());
        d[168..170].copy_from_slice(&108u16.to_le_bytes());
        let b = 170;
        d[b + 80..b + 88].copy_from_slice(&u64::MAX.to_le_bytes()); // older max
        d[b + 88..b + 90].copy_from_slice(&50u16.to_le_bytes()); // older 50 bps
        d[b + 98..b + 106].copy_from_slice(&1_000_000u64.to_le_bytes()); // newer max
        d[b + 106..b + 108].copy_from_slice(&100u16.to_le_bytes()); // newer 100 bps
        assert_eq!(parse_transfer_fee(&d), Some(TransferFee { basis_points: 100, maximum_fee: 1_000_000 }));
        // plain SPL mint: no extension
        assert_eq!(parse_transfer_fee(&vec![0u8; 82]), None);
        // Token-2022 mint with another extension only
        let mut e = vec![0u8; 166 + 4 + 8];
        e[165] = 1;
        e[166..168].copy_from_slice(&3u16.to_le_bytes()); // MintCloseAuthority-ish
        e[168..170].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(parse_transfer_fee(&e), None);
        let m = Pubkey::new_unique();
        record(m, TOKEN_2022_PROGRAM_ID, &d);
        assert_eq!(net_of_transfer_fee(&m, 1_000_000), 990_000);
        assert!(is_known(&m));
    }
}
