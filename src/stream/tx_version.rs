//! Transaction-version negotiation with the RPC (Solana tx format v1, SIMD-0385).
//!
//! **Why this module exists.** An RPC asked for a `maxSupportedTransactionVersion`
//! below a transaction's version does not skip that transaction: `getBlock`
//! fails WHOLE (JSON-RPC `-32015`), `getTransaction` fails, and `blockSubscribe`
//! delivers `block: null`. An OMITTED version is worse — Agave then refuses
//! every versioned transaction, v0 included, i.e. nearly every mainnet block.
//! So one stale or missing field silently blinds that ingest lane.
//!
//! The Geyser path is unaffected: Yellowstone hands us decoded `Message`
//! protos (account keys inline, loaded addresses in meta) for every version.
//! Only the RPC paths negotiate a version, and every one of them builds its
//! request through this module so the number lives in exactly one place.
//! `no_stray_version_literals` scans the crate to keep it that way.

use solana_client::rpc_config::{RpcBlockConfig, RpcBlockSubscribeConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_transaction_status_client_types::{TransactionDetails, UiTransactionEncoding};

/// The highest transaction version this crate accepts from the RPC, and
/// therefore the version every request asks for. v1 (SIMD-0385) is delivered
/// by the RPC in the same JSON shape as v0 (`UiRawMessage`, no address-table
/// lookups, `version: 1`), which is all the RPC paths here consume.
pub const MAX_SUPPORTED_TX_VERSION: u8 = 1;

/// Agave's `JSON_RPC_SERVER_ERROR_UNSUPPORTED_TRANSACTION_VERSION`.
pub const JSON_RPC_UNSUPPORTED_TX_VERSION: i64 = -32015;

/// `blockSubscribe` config for the RPC fallback block scanner.
pub fn block_subscribe_config(encoding: UiTransactionEncoding) -> RpcBlockSubscribeConfig {
    RpcBlockSubscribeConfig {
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: Some(encoding),
        transaction_details: Some(TransactionDetails::Full),
        show_rewards: Some(false),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TX_VERSION),
    }
}

/// `getBlock` config with full transaction detail and no rewards.
pub fn block_config(encoding: UiTransactionEncoding, commitment: CommitmentConfig) -> RpcBlockConfig {
    RpcBlockConfig {
        encoding: Some(encoding),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(false),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TX_VERSION),
    }
}

/// `getTransaction` config.
pub fn transaction_config(
    encoding: UiTransactionEncoding,
    commitment: CommitmentConfig,
) -> RpcTransactionConfig {
    RpcTransactionConfig {
        encoding: Some(encoding),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TX_VERSION),
    }
}

/// True when an RPC error is the version refusal, so callers can log the fix
/// (raise `MAX_SUPPORTED_TX_VERSION` together with a decoder) instead of a
/// generic failure.
pub fn is_version_refusal(err: &solana_client::client_error::ClientError) -> bool {
    use solana_client::client_error::ClientErrorKind;
    use solana_client::rpc_request::RpcError;
    matches!(
        err.kind(),
        ClientErrorKind::RpcError(RpcError::RpcResponseError { code, .. })
            if *code == JSON_RPC_UNSUPPORTED_TX_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_declares_the_current_version() {
        let b = block_subscribe_config(UiTransactionEncoding::Json);
        assert_eq!(b.max_supported_transaction_version, Some(MAX_SUPPORTED_TX_VERSION));
        assert_eq!(b.transaction_details, Some(TransactionDetails::Full));
        let t = transaction_config(UiTransactionEncoding::JsonParsed, CommitmentConfig::confirmed());
        assert_eq!(t.max_supported_transaction_version, Some(MAX_SUPPORTED_TX_VERSION));
        let g = block_config(UiTransactionEncoding::Json, CommitmentConfig::confirmed());
        assert_eq!(g.max_supported_transaction_version, Some(MAX_SUPPORTED_TX_VERSION));
        assert_eq!(g.rewards, Some(false));
        assert_eq!(MAX_SUPPORTED_TX_VERSION, 1, "v1 = SIMD-0385");
    }

    /// The version number must not be re-declared anywhere else in the crate:
    /// a stray `Some(0)` blinds that call site the moment a v1 transaction
    /// lands in a block. Scans `src/` and `tests/`.
    #[test]
    fn no_stray_version_literals() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        walk(&root.join("tests"), &mut files);
        let me = root.join("src/stream/tx_version.rs");
        let mut offenders = Vec::new();
        for f in files {
            if f == me {
                continue;
            }
            let src = std::fs::read_to_string(&f).unwrap();
            for (i, line) in src.lines().enumerate() {
                let l = line.trim();
                if l.starts_with("//") {
                    continue;
                }
                if l.contains("max_supported_transaction_version") && !l.contains("MAX_SUPPORTED_TX_VERSION") {
                    offenders.push(format!("{}:{}: {}", f.display(), i + 1, l));
                }
            }
        }
        assert!(offenders.is_empty(), "version declared outside tx_version.rs:\n{}", offenders.join("\n"));
    }
}
