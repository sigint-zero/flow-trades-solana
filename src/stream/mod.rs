pub mod account_mirror;
pub mod block_refresh;
pub mod block_scanner;
pub mod blockhash;
pub mod geyser;
pub mod observed_fees;
pub mod swap_stream;
pub mod tx_version;
pub mod types;

use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;

use crate::pool::cache::PoolCache;
use crate::pool::registry::PoolRegistry;

pub use types::{StreamConfig, StreamStats};

/// Highest slot seen on any stream (Geyser block/account updates or the RPC
/// blockSubscribe fallback). Read by slot-activated fee schedules (DAMM v2).
pub static LATEST_SLOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_slot(slot: u64) {
    LATEST_SLOT.fetch_max(slot, std::sync::atomic::Ordering::Relaxed);
}

pub fn latest_slot() -> u64 {
    LATEST_SLOT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Optional swap-stream context. When present on `StreamManager`, every
/// block update fed by Yellowstone is also passed through the swap parser
/// and resulting swaps go into the broadcast channel. Lossy by design —
/// slow consumers drop, never block.
#[derive(Clone)]
pub struct SwapStreamCtx {
    pub tx: tokio::sync::broadcast::Sender<std::sync::Arc<swap_stream::Swap>>,
    pub oracle: Arc<crate::enrichment::PriceOracle>,
}

/// Central manager for Geyser pool state streaming.
///
/// Owns references to the cache, registry, and RPC client.
/// Streams real-time pool state and vault balance updates via Geyser gRPC.
pub struct StreamManager {
    pub cache: Arc<PoolCache>,
    pub registry: Arc<PoolRegistry>,
    pub rpc: Arc<RpcClient>,
    pub config: StreamConfig,
    pub stats: Arc<StreamStats>,
    pub pool_db: Option<Arc<crate::storage::sqlite::PoolDb>>,
    pub mirror: Arc<account_mirror::AccountMirror>,
    pub swap_stream: Option<SwapStreamCtx>,
}

impl StreamManager {
    pub fn new(
        cache: Arc<PoolCache>,
        registry: Arc<PoolRegistry>,
        rpc: Arc<RpcClient>,
        config: StreamConfig,
        stats: Arc<StreamStats>,
        pool_db: Option<Arc<crate::storage::sqlite::PoolDb>>,
        mirror: Arc<account_mirror::AccountMirror>,
    ) -> Self {
        Self {
            cache,
            registry,
            rpc,
            config,
            stats,
            pool_db,
            mirror,
            swap_stream: None,
        }
    }

    /// Attach an active swap-stream context — enables per-block swap parsing.
    pub fn with_swap_stream(mut self, ctx: SwapStreamCtx) -> Self {
        self.swap_stream = Some(ctx);
        self
    }

    /// Start Geyser gRPC streaming. Runs forever (or until cancelled).
    pub async fn spawn(self: Arc<Self>) {
        geyser::run(Arc::clone(&self)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::commitment_config::CommitmentConfig;

    fn make_test_stream_manager() -> Arc<StreamManager> {
        let rpc = Arc::new(RpcClient::new_with_commitment(
            "http://localhost:8899".to_string(),
            CommitmentConfig::confirmed(),
        ));
        let cache = Arc::new(PoolCache::new(2000));
        let registry = Arc::new(PoolRegistry::new());
        let stats = Arc::new(StreamStats::new());
        let config = StreamConfig {
            geyser_endpoint: None,
            geyser_token: None,
        };
        let mirror = Arc::new(account_mirror::AccountMirror::new());
        Arc::new(StreamManager::new(cache, registry, rpc, config, stats, None, mirror))
    }

    #[test]
    fn test_stream_manager_creation() {
        let mgr = make_test_stream_manager();
        assert!(mgr.registry.is_empty());
        assert!(mgr.cache.is_empty());
    }

    #[test]
    fn test_stream_manager_with_geyser_config() {
        let rpc = Arc::new(RpcClient::new_with_commitment(
            "http://localhost:8899".to_string(),
            CommitmentConfig::confirmed(),
        ));
        let config = StreamConfig {
            geyser_endpoint: Some("http://geyser.example.com:10000".to_string()),
            geyser_token: Some("secret-token".to_string()),
        };
        let mgr = StreamManager::new(
            Arc::new(PoolCache::new(2000)),
            Arc::new(PoolRegistry::new()),
            rpc,
            config,
            Arc::new(StreamStats::new()),
            None,
            Arc::new(account_mirror::AccountMirror::new()),
        );
        assert!(mgr.config.geyser_endpoint.is_some());
        assert!(mgr.config.geyser_token.is_some());
    }
}
