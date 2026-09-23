use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_response::{Response, RpcBlockhash};
use solana_sdk::hash::Hash;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Cached recent blockhash with staleness tracking.
/// Used to avoid fetching a fresh blockhash on every /swap request.
pub struct BlockhashCache {
    inner: RwLock<Option<(Hash, u64, Instant)>>, // (hash, last_valid_block_height, fetched_at)
    max_age: Duration,
}

impl BlockhashCache {
    /// Create a new blockhash cache with the given max age in milliseconds.
    pub fn new(max_age_ms: u64) -> Self {
        Self {
            inner: RwLock::new(None),
            max_age: Duration::from_millis(max_age_ms),
        }
    }

    /// Get the cached blockhash if it is fresh (not older than max_age).
    /// Returns None if the cache is empty or stale.
    pub async fn get(&self) -> Option<(Hash, u64)> {
        let guard = self.inner.read().await;
        match *guard {
            Some((hash, height, fetched_at)) => {
                if fetched_at.elapsed() <= self.max_age {
                    Some((hash, height))
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Update the cached blockhash.
    pub async fn set(&self, hash: Hash, last_valid_block_height: u64) {
        let mut guard = self.inner.write().await;
        *guard = Some((hash, last_valid_block_height, Instant::now()));
    }

    /// Get the age of the cached blockhash in milliseconds, or None if cache is empty.
    pub async fn age_ms(&self) -> Option<u64> {
        let guard = self.inner.read().await;
        guard.map(|(_, _, fetched_at)| fetched_at.elapsed().as_millis() as u64)
    }

    /// Spawn a background task that periodically refreshes the blockhash via RPC.
    pub fn spawn_refresh(
        self: Arc<Self>,
        rpc: Arc<RpcClient>,
        interval: Duration,
    ) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                // The raw call (not `get_latest_blockhash_with_commitment`) so the
                // response context's slot is kept: without Geyser or the block
                // scanner it is the only slot source, and slot-activated fee
                // schedules (DAMM v2) price at their cliff fee on slot 0.
                let resp: Result<Response<RpcBlockhash>, _> = rpc
                    .send(RpcRequest::GetLatestBlockhash, json!([{ "commitment": "confirmed" }]))
                    .await;
                match resp.map(|r| (r.context.slot, Hash::from_str(&r.value.blockhash), r.value.last_valid_block_height)) {
                    Ok((slot, Ok(hash), height)) => {
                        crate::stream::note_slot(slot);
                        self.set(hash, height).await;
                        debug!(hash = %hash, height, slot, "blockhash cache refreshed");
                    }
                    Ok((_, Err(e), _)) => warn!(error = %e, "blockhash refresh: unparseable blockhash"),
                    Err(e) => {
                        warn!(error = %e, "blockhash refresh failed");
                        // Keep using stale value; get() will return None once max_age expires
                    }
                }
            }
        });
    }

    /// Spawn a background task that refreshes the blockhash via Geyser gRPC.
    /// Uses the Yellowstone `get_latest_blockhash()` method — zero RPC.
    pub fn spawn_geyser_refresh(
        self: Arc<Self>,
        endpoint: String,
        x_token: Option<String>,
        interval: Duration,
    ) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            let mut client: Option<yellowstone_grpc_client::GeyserGrpcClient> = None;

            loop {
                ticker.tick().await;

                // Connect lazily / reconnect on failure
                if client.is_none() {
                    match yellowstone_grpc_client::GeyserGrpcClient::build_from_shared(
                        endpoint.clone(),
                    )
                    .and_then(|b| b.x_token(x_token.as_deref()))
                    {
                        Ok(builder) => match builder.connect().await {
                            Ok(c) => {
                                info!("blockhash: Geyser gRPC connected");
                                client = Some(c);
                            }
                            Err(e) => {
                                warn!(error = %e, "blockhash: Geyser connect failed, retrying");
                                continue;
                            }
                        },
                        Err(e) => {
                            warn!(error = %e, "blockhash: Geyser build failed");
                            continue;
                        }
                    }
                }

                if let Some(ref mut c) = client {
                    match c
                        .get_latest_blockhash(Some(
                            yellowstone_grpc_proto::prelude::CommitmentLevel::Confirmed,
                        ))
                        .await
                    {
                        Ok(resp) => {
                            match resp.blockhash.parse::<Hash>() {
                                Ok(hash) => {
                                    self.set(hash, resp.last_valid_block_height).await;
                                    debug!(
                                        hash = %hash,
                                        height = resp.last_valid_block_height,
                                        "blockhash refreshed via Geyser"
                                    );
                                }
                                Err(e) => {
                                    warn!(error = %e, "blockhash: failed to parse hash from Geyser");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "blockhash: Geyser get_latest_blockhash failed");
                            // Drop client to force reconnect next tick
                            client = None;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_blockhash_cache_default_is_none() {
        let cache = BlockhashCache::new(5000);
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn test_blockhash_cache_set_and_get() {
        let cache = BlockhashCache::new(5000);
        let hash = Hash::new_unique();
        cache.set(hash, 12345).await;

        let result = cache.get().await;
        assert!(result.is_some());
        let (h, height) = result.unwrap();
        assert_eq!(h, hash);
        assert_eq!(height, 12345);
    }

    #[tokio::test]
    async fn test_blockhash_cache_staleness() {
        let cache = BlockhashCache::new(0); // 0ms max age = immediately stale
        let hash = Hash::new_unique();
        cache.set(hash, 100).await;

        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(cache.get().await.is_none(), "should be stale");
    }

    #[tokio::test]
    async fn test_blockhash_cache_overwrite() {
        let cache = BlockhashCache::new(5000);
        let hash1 = Hash::new_unique();
        let hash2 = Hash::new_unique();

        cache.set(hash1, 100).await;
        cache.set(hash2, 200).await;

        let (h, height) = cache.get().await.unwrap();
        assert_eq!(h, hash2);
        assert_eq!(height, 200);
    }

    #[tokio::test]
    async fn test_blockhash_cache_age_ms() {
        let cache = BlockhashCache::new(5000);
        assert!(cache.age_ms().await.is_none());

        cache.set(Hash::new_unique(), 100).await;
        let age = cache.age_ms().await.unwrap();
        // Should be very small (< 100ms)
        assert!(age < 100, "age_ms={age} should be < 100");
    }

    #[tokio::test]
    async fn test_blockhash_cache_fresh_within_max_age() {
        let cache = BlockhashCache::new(1000); // 1s max age
        let hash = Hash::new_unique();
        cache.set(hash, 500).await;

        // Should still be fresh
        let result = cache.get().await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, hash);
    }
}
