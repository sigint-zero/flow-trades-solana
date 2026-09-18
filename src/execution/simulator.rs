use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::transaction::VersionedTransaction;
use tracing::debug;

use crate::error::{TradeError, TradeResult};

/// Result of a transaction simulation.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    pub success: bool,
    pub amount_out: u64,
    pub slot: u64,
    pub units_consumed: u64,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

/// CU headroom multiplier: simulated CU * 1.1 + 1000 flat buffer.
/// Prevents landing failures from minor CU variance between sim and execution.
const CU_HEADROOM_PCT: u64 = 10;
const CU_HEADROOM_FLAT: u64 = 1_000;

/// Apply headroom to a simulated CU value.
pub fn cu_with_headroom(units_consumed: u64) -> u32 {
    let with_pct = units_consumed.saturating_add(units_consumed * CU_HEADROOM_PCT / 100);
    let with_flat = with_pct.saturating_add(CU_HEADROOM_FLAT);
    // Cap at 1.4M (max CU per transaction)
    with_flat.min(1_400_000) as u32
}

/// Simulate already-serialized transaction bytes (any version, including v1,
/// which `solana_sdk` cannot represent). Same semantics as [`simulate_versioned`].
pub async fn simulate_raw(rpc: &RpcClient, tx_bytes: &[u8]) -> TradeResult<SimulationResult> {
    use solana_client::rpc_request::RpcRequest;
    use solana_client::rpc_response::{Response, RpcSimulateTransactionResult};
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tx_bytes);
    let params = serde_json::json!([
        b64,
        {
            "sigVerify": false,
            "replaceRecentBlockhash": true,
            "commitment": "confirmed",
            "encoding": "base64",
        }
    ]);
    let response: Response<RpcSimulateTransactionResult> = rpc
        .send(RpcRequest::SimulateTransaction, params)
        .await
        .map_err(|e| TradeError::Rpc(format!("simulate_transaction: {e}")))?;
    let result = response.value;
    let logs = result.logs.unwrap_or_default();
    let units_consumed = result.units_consumed.unwrap_or(0);
    let amount_out = parse_amount_from_logs(&logs);
    Ok(SimulationResult {
        success: result.err.is_none(),
        amount_out,
        slot: response.context.slot,
        units_consumed,
        error: result.err.map(|e| format!("{e:?}")),
        logs,
    })
}

/// Simulate a versioned transaction via RPC.
/// Returns the simulation result regardless of success/failure (caller decides).
pub async fn simulate_versioned(
    rpc: &RpcClient,
    tx: &VersionedTransaction,
) -> TradeResult<SimulationResult> {
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    let response = rpc
        .simulate_transaction_with_config(tx, config)
        .await
        .map_err(|e| TradeError::Rpc(format!("simulate_transaction: {e}")))?;

    let result = response.value;
    let logs = result.logs.unwrap_or_default();
    let units_consumed = result.units_consumed.unwrap_or(0);
    let amount_out = parse_amount_from_logs(&logs);

    if let Some(err) = result.err {
        debug!(error = ?err, "simulation returned error");
        return Ok(SimulationResult {
            success: false,
            amount_out,
            slot: response.context.slot,
            units_consumed,
            error: Some(format!("{err:?}")),
            logs,
        });
    }

    Ok(SimulationResult {
        success: true,
        amount_out,
        slot: response.context.slot,
        units_consumed,
        error: None,
        logs,
    })
}

/// Best-effort extraction of swap output amount from program logs.
fn parse_amount_from_logs(logs: &[String]) -> u64 {
    let mut last_transfer_amount: u64 = 0;

    for log in logs {
        if let Some(rest) = log.strip_prefix("Program log: Transfer ") {
            if let Ok(amount) = rest.trim().parse::<u64>() {
                last_transfer_amount = amount;
            }
        }
    }

    last_transfer_amount
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_transfer() {
        let logs = vec!["Program log: Transfer 12345".to_string()];
        assert_eq!(parse_amount_from_logs(&logs), 12345);
    }

    #[test]
    fn test_parse_multiple_transfers_takes_last() {
        let logs = vec![
            "Program log: Transfer 100".to_string(),
            "Program log: Transfer 200".to_string(),
            "Program log: Transfer 300".to_string(),
        ];
        assert_eq!(parse_amount_from_logs(&logs), 300);
    }

    #[test]
    fn test_parse_no_transfer_logs() {
        let logs = vec![
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke".to_string(),
            "Program log: Instruction: Swap".to_string(),
        ];
        assert_eq!(parse_amount_from_logs(&logs), 0);
    }

    #[test]
    fn test_parse_empty_logs() {
        let logs: Vec<String> = vec![];
        assert_eq!(parse_amount_from_logs(&logs), 0);
    }

    #[test]
    fn test_parse_non_numeric_transfer() {
        let logs = vec!["Program log: Transfer abc".to_string()];
        assert_eq!(parse_amount_from_logs(&logs), 0);
    }

    #[test]
    fn test_parse_u64_max() {
        let logs = vec![format!("Program log: Transfer {}", u64::MAX)];
        assert_eq!(parse_amount_from_logs(&logs), u64::MAX);
    }

    #[test]
    fn test_parse_whitespace_trimming() {
        let logs = vec!["Program log: Transfer  42  ".to_string()];
        assert_eq!(parse_amount_from_logs(&logs), 42);
    }

    #[test]
    fn test_parse_mixed_valid_invalid() {
        let logs = vec![
            "Program log: Transfer xyz".to_string(),
            "Program log: Transfer 999".to_string(),
            "Program log: Transfer not_a_number".to_string(),
        ];
        assert_eq!(parse_amount_from_logs(&logs), 999);
    }

    #[test]
    fn test_cu_with_headroom_basic() {
        // 100K CU → 100K * 1.1 + 1000 = 111_000
        assert_eq!(cu_with_headroom(100_000), 111_000);
    }

    #[test]
    fn test_cu_with_headroom_zero() {
        // 0 CU → 0 + 1000 = 1000
        assert_eq!(cu_with_headroom(0), 1_000);
    }

    #[test]
    fn test_cu_with_headroom_typical_swap() {
        // 80K CU (typical single-hop) → 89_000
        assert_eq!(cu_with_headroom(80_000), 89_000);
    }

    #[test]
    fn test_cu_with_headroom_caps_at_1_4m() {
        // Very large CU should cap at 1.4M
        assert_eq!(cu_with_headroom(2_000_000), 1_400_000);
    }
}
