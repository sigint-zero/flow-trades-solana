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
/// Token-2022 mints with a transfer-hook program set: every transfer CPIs into
/// it and needs its extra accounts.
static HOOKED: LazyLock<DashSet<Pubkey>> = LazyLock::new(DashSet::new);

// Token-2022 extension types (spl-token-2022 `ExtensionType`, u16).
const EXT_TRANSFER_FEE_CONFIG: u16 = 1;
const EXT_TRANSFER_HOOK: u16 = 14;

pub fn transfer_fee(mint: &Pubkey) -> Option<TransferFee> {
    FEES.get(mint).map(|f| *f)
}

pub fn is_known(mint: &Pubkey) -> bool {
    KNOWN.contains(mint)
}

pub fn token_program(mint: &Pubkey) -> Option<Pubkey> {
    PROGRAMS.get(mint).map(|p| *p)
}

/// Transfers of this mint invoke a transfer-hook program.
pub fn has_transfer_hook(mint: &Pubkey) -> bool {
    HOOKED.contains(mint)
}

/// Net amount a recipient receives when `amount` of `mint` is transferred.
pub fn net_of_transfer_fee(mint: &Pubkey, amount: u64) -> u64 {
    match transfer_fee(mint) {
        Some(f) => amount.saturating_sub(f.fee(amount)),
        None => amount,
    }
}

/// The (type, body) TLV entries of a Token-2022 mint's extensions.
fn mint_extensions(data: &[u8]) -> Vec<(u16, &[u8])> {
    // base mint 82 B, padded to 165, then account type byte (1 = Mint), then TLV
    let mut out = Vec::new();
    if data.len() < 166 || data[165] != 1 {
        return out;
    }
    let mut o = 166;
    while o + 4 <= data.len() {
        let typ = u16::from_le_bytes(data[o..o + 2].try_into().unwrap());
        let len = u16::from_le_bytes(data[o + 2..o + 4].try_into().unwrap()) as usize;
        let Some(body) = data.get(o + 4..o + 4 + len) else { break };
        if typ == 0 {
            break;
        }
        out.push((typ, body));
        o += 4 + len;
    }
    out
}

/// Parse a Token-2022 mint's `TransferFeeConfig` extension (type 1). Returns
/// the HIGHER of the older/newer schedule — conservative for a quote.
pub fn parse_transfer_fee(data: &[u8]) -> Option<TransferFee> {
    let (_, body) = mint_extensions(data).into_iter().find(|(t, b)| *t == EXT_TRANSFER_FEE_CONFIG && b.len() >= 108)?;
    // authority 32 | withdraw authority 32 | withheld u64 | older {epoch u64, max u64, bps u16} | newer {…}
    let rd64 = |p: usize| u64::from_le_bytes(body[p..p + 8].try_into().unwrap());
    let rd16 = |p: usize| u16::from_le_bytes(body[p..p + 2].try_into().unwrap());
    let older = TransferFee { maximum_fee: rd64(72 + 8), basis_points: rd16(72 + 16) };
    let newer = TransferFee { maximum_fee: rd64(90 + 8), basis_points: rd16(90 + 16) };
    Some(if newer.basis_points >= older.basis_points { newer } else { older })
}

pub fn record(mint: Pubkey, owner: Pubkey, data: &[u8]) {
    PROGRAMS.insert(mint, owner);
    if owner == TOKEN_2022_PROGRAM_ID {
        if let Some(f) = parse_transfer_fee(data) {
            if f.basis_points > 0 {
                FEES.insert(mint, f);
            }
        }
        for (typ, body) in mint_extensions(data) {
            // TransferHook: authority 32 | program id 32 (all zeros = no hook)
            if typ == EXT_TRANSFER_HOOK && body.len() >= 64 && body[32..64].iter().any(|b| *b != 0) {
                HOOKED.insert(mint);
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

    fn b64(parts: &[&str]) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, parts.concat()).unwrap()
    }

    #[test]
    fn flags_only_transfer_hooks_with_a_program() {
        // mainnet XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W (xStock): transfer
        // hook with NO program set + pausable (+ permanent delegate, scaled UI…)
        let xstock = b64(&[
        "AQAAAGVqQkIv6okUBqQZ0dHeCPQqhHlBtaGulevOYZrDFyk0r8cbR6kIAAAIAQEAAAD/3+wbzSzTg5PITaoIyRzA041nf/jQ",
        "q3tdAz8A9zLMMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAARIAQABD+fHuLje4B+pFy3TmAJcivsAJKWAm5ORVi0NeJKXpxgfo3CzeeyOg10P48Sdr",
        "ZX2KnuoGlQumeo0wM8U8TN5PDAAgAEP58e4uN7gH6kXLdOYAlyK+wAkpYCbk5FWLQ14kpenGBgABAAEZADgABm9ZIlHMR3R4",
        "JaWa0UIupDVz9SjaXe4q94ErMU+ZReMkC6AiAxDwP0BtM2oAAAAA0KWYJmgX8D8aACEA/9/sG80s04OTyE2qCMkcwNONZ3/4",
        "0Kt7XQM/APcyzDAABABBAEP58e4uN7gH6kXLdOYAlyK+wAkpYCbk5FWLQ14kpenGAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAADgBAAEP58e4uN7gH6kXLdOYAlyK+wAkpYCbk5FWLQ14kpenGAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAATAKMAQ/nx7i43uAfqRct05gCXIr7ACSlgJuTkVYtDXiSl6cYH6Nws3nsjoNdD+PEna2V9ip7qBpULpnqNMDPFPEze",
        "TwwAAABTUDUwMCB4U3RvY2sEAAAAU1BZeEMAAABodHRwczovL3hzdG9ja3MtbWV0YWRhdGEuYmFja2VkLmZpL3Rva2Vucy9T",
        "b2xhbmEvU1BZeC9tZXRhZGF0YS5qc29uAAAAAA==",
        ]);
        let m = Pubkey::new_unique();
        record(m, TOKEN_2022_PROGRAM_ID, &xstock);
        assert!(!has_transfer_hook(&m), "hook program is all zeros");
        assert_eq!(transfer_fee(&m), None);

        // mainnet HjHGA7QaU44KJzemzdvH4TmxAGcksdBKAW7yfBSahQpq: transfer-fee config
        let tfc = b64(&[
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIDGpH6NAwAGAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAARIAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAPiPxYQKBSNJ5gO0ssGP",
        "k0rzT9ntUg5z1uuDLE90DBiCAQBsAEAtXK96l8U0l2WOVhsmI9Djj1NA8git1QpO9FQLCssuQC1cr3qXxTSXZY5WGyYj0OOP",
        "U0DyCK3VCk70VAsKyy4AAAAAAAAAABAEAAAAAAAAAIDGpH6NAwAsARAEAAAAAAAAAIDGpH6NAwAsARMAhAAHg6gVJiEPlKDo",
        "6E0ghwA7OkJg0pznnaz+IaTqdx+mKPiPxYQKBSNJ5gO0ssGPk0rzT9ntUg5z1uuDLE90DBiCDAAAAEZpbmFuY2UgQnJvcwQA",
        "AABCUk9TJAAAAGh0dHBzOi8vbS5yYXBpZGxhdW5jaC5pby9tL0lJeUxJMU5UNgAAAAA=",
        ]);
        let f = Pubkey::new_unique();
        record(f, TOKEN_2022_PROGRAM_ID, &tfc);
        assert!(!has_transfer_hook(&f));
        assert_eq!(transfer_fee(&f).map(|t| t.basis_points), Some(300));

        // a hook with a program id set
        let mut hooked = vec![0u8; 166 + 4 + 64];
        hooked[165] = 1;
        hooked[166..168].copy_from_slice(&EXT_TRANSFER_HOOK.to_le_bytes());
        hooked[168..170].copy_from_slice(&64u16.to_le_bytes());
        hooked[170 + 40] = 7;
        let h = Pubkey::new_unique();
        record(h, TOKEN_2022_PROGRAM_ID, &hooked);
        assert!(has_transfer_hook(&h));

        // plain SPL mints are never flagged
        let spl = Pubkey::new_unique();
        record(spl, crate::constants::TOKEN_PROGRAM_ID, &vec![0u8; 82]);
        assert!(!has_transfer_hook(&spl));
    }
}
