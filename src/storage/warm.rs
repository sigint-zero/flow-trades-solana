use std::sync::Arc;

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

use crate::error::{TradeError, TradeResult};
use crate::pool::cache::PoolCache;
use crate::pool::registry::{PoolEntry, PoolRegistry};
use crate::pool::types::PoolState;

/// Binary snapshot for fast warm-restart (L2 persistence).
#[derive(Serialize, Deserialize)]
struct WarmSnapshot {
    version: u8,
    entries: Vec<PoolEntry>,
    states: Vec<(Pubkey, PoolState)>,
    saved_at: i64,
}

const SNAPSHOT_VERSION: u8 = 1;

/// Save a warm snapshot (bincode) of the registry and cache to disk.
/// Uses write-to-tmp-then-rename for atomicity.
pub fn save(path: &str, registry: &PoolRegistry, cache: &PoolCache) -> TradeResult<()> {
    let snapshot = WarmSnapshot {
        version: SNAPSHOT_VERSION,
        entries: registry.entries(),
        states: cache.all_entries(),
        saved_at: chrono::Utc::now().timestamp(),
    };

    let bytes = bincode::serialize(&snapshot)
        .map_err(|e| TradeError::Internal(format!("warm snapshot serialize: {e}")))?;

    let tmp_path = format!("{path}.tmp");
    std::fs::write(&tmp_path, &bytes)
        .map_err(|e| TradeError::Internal(format!("warm snapshot write {tmp_path}: {e}")))?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| TradeError::Internal(format!("warm snapshot rename to {path}: {e}")))?;

    debug!(
        path,
        entries = snapshot.entries.len(),
        states = snapshot.states.len(),
        bytes = bytes.len(),
        "warm snapshot saved"
    );

    Ok(())
}

/// Load a warm snapshot from disk. Returns registry entries and cache states.
pub fn load(path: &str) -> TradeResult<(Vec<PoolEntry>, Vec<(Pubkey, PoolState)>)> {
    let bytes = std::fs::read(path)
        .map_err(|e| TradeError::Internal(format!("warm snapshot read {path}: {e}")))?;

    let snapshot: WarmSnapshot = bincode::deserialize(&bytes)
        .map_err(|e| TradeError::Internal(format!("warm snapshot deserialize: {e}")))?;

    if snapshot.version != SNAPSHOT_VERSION {
        return Err(TradeError::Internal(format!(
            "warm snapshot version mismatch: expected {SNAPSHOT_VERSION}, got {}",
            snapshot.version
        )));
    }

    info!(
        path,
        entries = snapshot.entries.len(),
        states = snapshot.states.len(),
        saved_at = snapshot.saved_at,
        "warm snapshot loaded"
    );

    Ok((snapshot.entries, snapshot.states))
}

/// Spawn a background task that periodically saves a warm snapshot.
pub fn spawn_periodic_save(
    registry: Arc<PoolRegistry>,
    cache: Arc<PoolCache>,
    path: String,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            if let Err(e) = save(&path, &registry, &cache) {
                error!(error = %e, "periodic warm snapshot save failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_entry() -> PoolEntry {
        PoolEntry {
            address: Pubkey::new_unique(),
            pool_type: crate::pool::types::PoolType::Orca,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
        }
    }

    fn make_test_state() -> (Pubkey, PoolState) {
        (
            Pubkey::new_unique(),
            PoolState::MeteoraDamm {
                pool: Pubkey::new_unique(),
                token_a_vault: Pubkey::new_unique(),
                token_b_vault: Pubkey::new_unique(),
                token_a_mint: Pubkey::new_unique(),
                token_b_mint: Pubkey::new_unique(),
                liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
            },
        )
    }

    #[test]
    fn test_warm_snapshot_roundtrip() {
        let registry = PoolRegistry::new();
        let entry = make_test_entry();
        let entry_addr = entry.address;
        registry.add(entry);

        let cache = PoolCache::new(60_000);
        let (addr, state) = make_test_state();
        cache.insert(addr, state);

        let path = "/tmp/flow_trades_test_warm.bin";
        save(path, &registry, &cache).unwrap();

        let (entries, states) = load(path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, entry_addr);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].0, addr);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_warm_snapshot_empty() {
        let registry = PoolRegistry::new();
        let cache = PoolCache::new(60_000);

        let path = "/tmp/flow_trades_test_warm_empty.bin";
        save(path, &registry, &cache).unwrap();

        let (entries, states) = load(path).unwrap();
        assert!(entries.is_empty());
        assert!(states.is_empty());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_warm_snapshot_corrupt_data() {
        let path = "/tmp/flow_trades_test_warm_corrupt.bin";
        std::fs::write(path, b"not valid bincode data").unwrap();

        let result = load(path);
        assert!(result.is_err());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_warm_snapshot_nonexistent_file() {
        let result = load("/tmp/flow_trades_nonexistent_warm.bin");
        assert!(result.is_err());
    }

    #[test]
    fn test_warm_snapshot_multiple_entries() {
        let registry = PoolRegistry::new();
        let cache = PoolCache::new(60_000);

        for _ in 0..5 {
            registry.add(make_test_entry());
            let (addr, state) = make_test_state();
            cache.insert(addr, state);
        }

        let path = "/tmp/flow_trades_test_warm_multi.bin";
        save(path, &registry, &cache).unwrap();

        let (entries, states) = load(path).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(states.len(), 5);

        std::fs::remove_file(path).ok();
    }
}
