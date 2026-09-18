use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::constants::program_id_to_label;

use super::AppState;

/// GET /health — returns server status, pool cache stats, and stream stats.
pub async fn handle_health(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let blockhash_age_ms = state.blockhash_cache.age_ms().await;
    let (stream_updates, stream_errors, last_stream_update) =
        state.stream_stats.snapshot().await;

    let last_stream_update_ms = last_stream_update
        .map(|t| t.elapsed().as_millis() as u64);

    let router_info = state.router_config.as_ref().map(|r| json!({
        "programId": r.program_id.to_string(),
        "configPda": crate::execution::router::config_pda(&r.program_id).to_string(),
        "referralAccount": r.referral_wallet.map(|a| a.to_string()),
        "feeEnforcement": "on-chain config PDA (admin-controlled)",
    }));

    Json(json!({
        "status": "ok",
        "poolCacheSize": state.cache.len(),
        "registrySize": state.registry.len(),
        "poolDbSize": state.pool_db.count(),
        "dormantPools": state.cache.dormant_count(),
        "blockhashCacheAgeMs": blockhash_age_ms,
        "streamUpdates": stream_updates,
        "streamErrors": stream_errors,
        "observedFeePools": crate::stream::observed_fees::len(),
        "lastStreamUpdateMs": last_stream_update_ms,
        "altTablesLoaded": state.alt_cache.len(),
        "altAddressesTotal": state.alt_cache.total_addresses(),
        "router": router_info,
    }))
}

/// GET /program-id-to-label — returns mapping of DEX program IDs to human labels.
pub async fn handle_labels() -> Json<Value> {
    let labels = program_id_to_label();
    let map: serde_json::Map<String, Value> = labels
        .into_iter()
        .map(|(id, label)| (id.to_string(), Value::String(label.to_string())))
        .collect();
    Json(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_program_id_to_label_has_entries() {
        let labels = program_id_to_label();
        assert!(labels.len() >= 18);
    }

    #[test]
    fn test_labels_all_unique() {
        let labels = program_id_to_label();
        let ids: std::collections::HashSet<String> = labels.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(ids.len(), labels.len(), "duplicate program IDs");
    }
}
