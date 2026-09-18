//! Encoder for Solana **transaction format v1** (SIMD-0385, with the 4096-byte
//! limit of SIMD-0296) — active on mainnet since September 2026.
//!
//! Why: a v0 transaction is capped at 1,232 bytes and depends on Address
//! Lookup Tables to fit. A router-wrapped multi-hop swap (pump.fun AMM buyback
//! accounts + CLMM tick arrays + the router's own accounts) can exceed that
//! even with lookup tables, so it cannot be sent as v0 at all.
//! v1 carries every account inline (no lookup tables, max 64 addresses) in up
//! to 4,096 bytes, and moves the compute budget into the message header.
//!
//! solana-sdk 2.x has no v1 message type, so this module writes the wire
//! format directly. Layout (SIMD-0385, checked against a reference decoder):
//! version byte `0x81`,
//! legacy header (3×u8), config mask (u32 LE), lifetime specifier (32 = recent
//! blockhash), num instructions (u8), num addresses (u8), addresses (32 each),
//! config values in bit order (priority fee u64 — a TOTAL in lamports —, CU
//! limit u32), instruction headers (program index u8, num accounts u8, data
//! len u16 LE), instruction payloads (account indexes then data), then
//! `num_required_signatures` × 64-byte signatures with no length prefix.
//!
//! **Compatibility:** the signer/wallet must understand v1 (`0x81` first byte
//! instead of a signature count), which is why `/swap` only emits it when the
//! caller asks (`tx_version: 1`). Compute-budget *instructions* are dropped: v1
//! ignores them for configuration and they would only cost bytes.

use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;

use crate::error::{TradeError, TradeResult};

/// `MESSAGE_VERSION_PREFIX (0x80) | 1`.
pub const V1_PREFIX: u8 = 0x81;
/// SIMD-0296 packet limit for a v1 transaction.
pub const MAX_V1_TX_BYTES: usize = 4096;
const MAX_ADDRESSES: usize = 64;
const MAX_INSTRUCTIONS: usize = 64;
const MASK_PRIORITY_FEE: u32 = 0b11;
const MASK_COMPUTE_UNIT_LIMIT: u32 = 0b100;
const MASK_LOADED_ACCOUNTS_DATA_SIZE: u32 = 0b1000;
/// v1 has NO implicit loaded-accounts budget (legacy/v0 default to 64 MiB):
/// without this bit a route touching a few large accounts fails with
/// `MaxLoadedAccountsDataSizeExceeded`. 64 MiB is the runtime maximum.
const LOADED_ACCOUNTS_DATA_SIZE: u32 = 64 * 1024 * 1024;
const COMPUTE_BUDGET_PROGRAM: Pubkey = Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// Compute budget carried in the v1 header instead of instructions.
#[derive(Debug, Clone, Copy)]
pub struct V1Budget {
    pub compute_unit_limit: u32,
    /// TOTAL priority fee in lamports (not micro-lamports per CU).
    pub priority_fee_lamports: u64,
}

/// Build an UNSIGNED v1 transaction: `num_required_signatures` zeroed
/// signature slots follow the message, exactly where a signer writes them.
///
/// Account ordering and instruction compilation reuse the legacy `Message`
/// compiler (writable signers, readonly signers, writable non-signers,
/// readonly non-signers — v1 keeps that ordering), so any instruction set the
/// legacy/v0 builders accept encodes identically here, minus lookup tables.
pub fn encode_unsigned_v1(
    instructions: &[Instruction],
    payer: &Pubkey,
    recent_blockhash: Hash,
    budget: V1Budget,
) -> TradeResult<Vec<u8>> {
    // Drop ComputeBudget instructions — the budget lives in the header.
    let ixs: Vec<Instruction> = instructions
        .iter()
        .filter(|ix| ix.program_id != COMPUTE_BUDGET_PROGRAM)
        .cloned()
        .collect();
    if ixs.is_empty() {
        return Err(TradeError::Validation("v1: no instructions".into()));
    }
    if ixs.len() > MAX_INSTRUCTIONS {
        return Err(TradeError::Validation(format!("v1: {} instructions exceeds {MAX_INSTRUCTIONS}", ixs.len())));
    }
    if budget.compute_unit_limit == 0 {
        return Err(TradeError::Validation("v1: compute_unit_limit must be > 0".into()));
    }
    let msg = Message::new(&ixs, Some(payer));
    if msg.account_keys.len() > MAX_ADDRESSES {
        return Err(TradeError::Validation(format!(
            "v1: {} addresses exceeds the {MAX_ADDRESSES}-address limit",
            msg.account_keys.len()
        )));
    }

    let mut mask = MASK_COMPUTE_UNIT_LIMIT | MASK_LOADED_ACCOUNTS_DATA_SIZE;
    if budget.priority_fee_lamports > 0 {
        mask |= MASK_PRIORITY_FEE;
    }

    let mut b = Vec::with_capacity(1024);
    b.push(V1_PREFIX);
    b.push(msg.header.num_required_signatures);
    b.push(msg.header.num_readonly_signed_accounts);
    b.push(msg.header.num_readonly_unsigned_accounts);
    b.extend_from_slice(&mask.to_le_bytes());
    b.extend_from_slice(recent_blockhash.as_ref());
    b.push(ixs.len() as u8);
    b.push(msg.account_keys.len() as u8);
    for k in &msg.account_keys {
        b.extend_from_slice(k.as_ref());
    }
    // config values in bit order: priority fee (bits 0-1), then CU limit (bit 2)
    if budget.priority_fee_lamports > 0 {
        b.extend_from_slice(&budget.priority_fee_lamports.to_le_bytes());
    }
    b.extend_from_slice(&budget.compute_unit_limit.to_le_bytes());
    b.extend_from_slice(&LOADED_ACCOUNTS_DATA_SIZE.to_le_bytes());
    for ci in &msg.instructions {
        let data_len = u16::try_from(ci.data.len())
            .map_err(|_| TradeError::Validation("v1: instruction data over 65535 bytes".into()))?;
        let n_acc = u8::try_from(ci.accounts.len())
            .map_err(|_| TradeError::Validation("v1: instruction has over 255 accounts".into()))?;
        b.push(ci.program_id_index);
        b.push(n_acc);
        b.extend_from_slice(&data_len.to_le_bytes());
    }
    for ci in &msg.instructions {
        b.extend_from_slice(&ci.accounts);
        b.extend_from_slice(&ci.data);
    }
    b.extend(std::iter::repeat(0u8).take(64 * usize::from(msg.header.num_required_signatures)));

    if b.len() > MAX_V1_TX_BYTES {
        return Err(TradeError::Validation(format!(
            "v1 transaction is {} bytes, over the {MAX_V1_TX_BYTES}-byte limit",
            b.len()
        )));
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::compute_budget::ComputeBudgetInstruction;
    use solana_sdk::instruction::AccountMeta;

    /// Minimal reader for the layout, independent of the encoder's code paths.
    struct R<'a> {
        b: &'a [u8],
        at: usize,
    }
    impl<'a> R<'a> {
        fn take(&mut self, n: usize) -> &'a [u8] {
            let s = &self.b[self.at..self.at + n];
            self.at += n;
            s
        }
        fn u8(&mut self) -> u8 {
            self.take(1)[0]
        }
    }

    #[test]
    fn encodes_the_simd_0385_layout_and_moves_the_budget_into_the_header() {
        let payer = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let (a, c) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
            ComputeBudgetInstruction::set_compute_unit_price(1_000),
            Instruction {
                program_id: prog,
                accounts: vec![AccountMeta::new(a, false), AccountMeta::new_readonly(c, false), AccountMeta::new(payer, true)],
                data: vec![9, 8, 7],
            },
            Instruction { program_id: prog, accounts: vec![AccountMeta::new_readonly(c, false)], data: vec![0xAB; 300] },
        ];
        let bh = Hash::new_from_array([0xBB; 32]);
        let bytes = encode_unsigned_v1(&ixs, &payer, bh, V1Budget { compute_unit_limit: 1_400_000, priority_fee_lamports: 7_000 }).unwrap();

        let mut r = R { b: &bytes, at: 0 };
        assert_eq!(r.u8(), V1_PREFIX);
        let [n_sig, n_ro_signed, n_ro_unsigned] = [r.u8(), r.u8(), r.u8()];
        assert_eq!((n_sig, n_ro_signed), (1, 0));
        let mask = u32::from_le_bytes(r.take(4).try_into().unwrap());
        assert_eq!(mask, MASK_PRIORITY_FEE | MASK_COMPUTE_UNIT_LIMIT | MASK_LOADED_ACCOUNTS_DATA_SIZE);
        assert_eq!(r.take(32), bh.as_ref());
        let n_ix = r.u8();
        assert_eq!(n_ix, 2, "compute-budget instructions are dropped");
        let n_addr = r.u8() as usize;
        assert_eq!(n_addr, 4, "payer, a, c, prog");
        let keys: Vec<Pubkey> = (0..n_addr).map(|_| Pubkey::new_from_array(r.take(32).try_into().unwrap())).collect();
        assert_eq!(keys[0], payer, "fee payer first");
        assert!(!keys.contains(&COMPUTE_BUDGET_PROGRAM));
        assert_eq!(u64::from_le_bytes(r.take(8).try_into().unwrap()), 7_000);
        assert_eq!(u32::from_le_bytes(r.take(4).try_into().unwrap()), 1_400_000);
        assert_eq!(u32::from_le_bytes(r.take(4).try_into().unwrap()), LOADED_ACCOUNTS_DATA_SIZE, "explicit loaded-accounts budget");
        let mut hdrs = Vec::new();
        for _ in 0..n_ix {
            let p = r.u8();
            let n = r.u8() as usize;
            let len = u16::from_le_bytes(r.take(2).try_into().unwrap()) as usize;
            hdrs.push((p, n, len));
        }
        assert_eq!(hdrs[0].1, 3);
        assert_eq!(hdrs[0].2, 3);
        assert_eq!(hdrs[1].2, 300, "u16 data length needs its high byte");
        for (p, n, len) in &hdrs {
            assert_eq!(keys[*p as usize], prog);
            let accs = r.take(*n);
            assert!(accs.iter().all(|i| (*i as usize) < n_addr));
            let _ = r.take(*len);
        }
        assert_eq!(r.take(64), &[0u8; 64], "one zeroed signature slot");
        assert_eq!(r.at, bytes.len(), "no trailing bytes");
        assert_eq!(n_ro_unsigned as usize, keys.iter().filter(|k| **k == c || **k == prog).count());
    }

    #[test]
    fn refuses_what_v1_cannot_carry() {
        let payer = Pubkey::new_unique();
        let bh = Hash::default();
        let budget = V1Budget { compute_unit_limit: 1, priority_fee_lamports: 0 };
        assert!(encode_unsigned_v1(&[], &payer, bh, budget).is_err(), "no instructions");
        let too_many_accounts: Vec<AccountMeta> = (0..70).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
        let ix = Instruction { program_id: Pubkey::new_unique(), accounts: too_many_accounts, data: vec![] };
        assert!(encode_unsigned_v1(&[ix], &payer, bh, budget).is_err(), "over 64 addresses");
        let big = Instruction { program_id: Pubkey::new_unique(), accounts: vec![], data: vec![0; 4200] };
        assert!(encode_unsigned_v1(&[big], &payer, bh, budget).is_err(), "over 4096 bytes");
    }
}
