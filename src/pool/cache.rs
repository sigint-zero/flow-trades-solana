use std::time::Instant;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;

use super::types::PoolState;

/// Cached pool state with timestamp for TTL-based eviction.
#[derive(Debug, Clone)]
pub struct CachedPool {
    pub state: PoolState,
    pub fetched_at: Instant,
}

/// Number of consecutive failures before a pool is considered dormant.
const DORMANT_THRESHOLD: u32 = 10;

/// Thread-safe pool state cache with TTL-based expiration and failure tracking.
pub struct PoolCache {
    inner: DashMap<Pubkey, CachedPool>,
    ttl: std::time::Duration,
    /// Consecutive fetch failure count per pool address.
    failures: DashMap<Pubkey, u32>,
}

impl PoolCache {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            inner: DashMap::new(),
            ttl: std::time::Duration::from_millis(ttl_ms),
            failures: DashMap::new(),
        }
    }

    pub fn ttl(&self) -> std::time::Duration {
        self.ttl
    }

    /// Get a cached pool state. Returns None if not present or expired.
    /// TTL ensures stale reserves are re-fetched on the next quote.
    pub fn get(&self, address: &Pubkey) -> Option<PoolState> {
        let entry = self.inner.get(address)?;
        if entry.fetched_at.elapsed() > self.ttl {
            None // expired — caller will re-fetch from RPC
        } else {
            Some(entry.state.clone())
        }
    }

    /// Run `f` against the cached state WITHOUT cloning it (the hot quote path
    /// evaluates hundreds of pools per request; a `PoolState` clone allocates
    /// for every variant that carries a `Vec`). The map shard stays read-locked
    /// for the duration of `f`, so `f` must be short and must not touch the cache.
    pub fn with_state<R>(&self, address: &Pubkey, f: impl FnOnce(&PoolState, std::time::Duration) -> R) -> Option<R> {
        let entry = self.inner.get(address)?;
        let age = entry.fetched_at.elapsed();
        if age > self.ttl {
            return None;
        }
        Some(f(&entry.state, age))
    }

    /// Get the age of a cached entry (time since last fetch). Returns None if not cached.
    pub fn get_with_age(&self, address: &Pubkey) -> Option<std::time::Duration> {
        self.inner.get(address).map(|entry| entry.fetched_at.elapsed())
    }

    /// Insert a pool state into the cache with the current timestamp.
    pub fn insert(&self, address: Pubkey, state: PoolState) {
        self.inner.insert(address, CachedPool {
            state,
            fetched_at: Instant::now(),
        });
    }

    /// Record a fetch failure for a pool. After DORMANT_THRESHOLD consecutive failures,
    /// the pool is considered dormant and will be skipped during quote evaluation.
    pub fn record_failure(&self, address: &Pubkey) {
        self.failures
            .entry(*address)
            .and_modify(|c| *c += 1)
            .or_insert(1);
    }

    /// Reset the failure counter for a pool (called on successful fetch).
    pub fn reset_failure(&self, address: &Pubkey) {
        self.failures.remove(address);
    }

    /// Check if a pool is dormant (>= DORMANT_THRESHOLD consecutive failures).
    pub fn is_dormant(&self, address: &Pubkey) -> bool {
        self.failures
            .get(address)
            .map_or(false, |c| *c >= DORMANT_THRESHOLD)
    }

    /// Get the number of currently dormant pools.
    pub fn dormant_count(&self) -> usize {
        self.failures
            .iter()
            .filter(|e| *e.value() >= DORMANT_THRESHOLD)
            .count()
    }

    /// Get all dormant pool addresses (for periodic retry).
    pub fn dormant_addresses(&self) -> Vec<Pubkey> {
        self.failures
            .iter()
            .filter(|e| *e.value() >= DORMANT_THRESHOLD)
            .map(|e| *e.key())
            .collect()
    }

    /// Remove an entry from the cache and its failure tracking.
    pub fn remove(&self, address: &Pubkey) {
        self.inner.remove(address);
        self.failures.remove(address);
    }

    /// Collect all cached entries as (address, state) pairs (for persistence).
    pub fn all_entries(&self) -> Vec<(Pubkey, PoolState)> {
        self.inner
            .iter()
            .map(|r| (*r.key(), r.value().state.clone()))
            .collect()
    }

    /// Collect all cached pool addresses.
    pub fn all_addresses(&self) -> Vec<Pubkey> {
        self.inner.iter().map(|r| *r.key()).collect()
    }

    /// Number of entries currently in the cache (including expired ones not yet evicted).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_pool_state() -> PoolState {
        PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        }
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = PoolCache::new(5000); // 5 second TTL
        let addr = Pubkey::new_unique();
        let state = make_test_pool_state();

        assert!(cache.get(&addr).is_none());
        cache.insert(addr, state);
        assert!(cache.get(&addr).is_some());
    }

    #[test]
    fn test_cache_ttl_expiration() {
        let cache = PoolCache::new(0); // 0ms TTL = immediate expiry
        let addr = Pubkey::new_unique();
        cache.insert(addr, make_test_pool_state());
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(cache.get(&addr).is_none()); // expired
    }

    #[test]
    fn test_cache_len() {
        let cache = PoolCache::new(5000);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());

        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();

        cache.insert(addr1, make_test_pool_state());
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        cache.insert(addr2, make_test_pool_state());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = PoolCache::new(5000);
        let addr = Pubkey::new_unique();

        let state1 = PoolState::MeteoraDamm {
            pool: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            liquidity: 0, sqrt_price: 0, sqrt_min_price: 0, sqrt_max_price: 0, token_a_amount: 0, token_b_amount: 0, fees: Default::default(), activation_point: 0, activation_type: 0, collect_fee_mode: 0, pool_status: 0,
        };
        let expected_pool = match &state1 {
            PoolState::MeteoraDamm { pool, .. } => *pool,
            _ => unreachable!(),
        };

        cache.insert(addr, make_test_pool_state());
        cache.insert(addr, state1);

        let retrieved = cache.get(&addr).unwrap();
        match retrieved {
            PoolState::MeteoraDamm { pool, .. } => {
                assert_eq!(pool, expected_pool);
            }
            _ => panic!("expected MeteoraDamm"),
        }
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_different_keys_independent() {
        let cache = PoolCache::new(5000);
        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();

        cache.insert(addr1, make_test_pool_state());

        assert!(cache.get(&addr1).is_some());
        assert!(cache.get(&addr2).is_none());
    }

    #[test]
    fn test_cache_entry_within_ttl() {
        let cache = PoolCache::new(5000); // 5s TTL
        let addr = Pubkey::new_unique();
        cache.insert(addr, make_test_pool_state());
        // Within TTL — should still be there
        assert!(cache.get(&addr).is_some());
    }

    #[test]
    fn test_cache_all_entries() {
        let cache = PoolCache::new(5000);
        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();

        cache.insert(addr1, make_test_pool_state());
        cache.insert(addr2, make_test_pool_state());

        let entries = cache.all_entries();
        assert_eq!(entries.len(), 2);

        let addresses: std::collections::HashSet<Pubkey> =
            entries.iter().map(|(a, _)| *a).collect();
        assert!(addresses.contains(&addr1));
        assert!(addresses.contains(&addr2));
    }

    #[test]
    fn test_cache_all_entries_empty() {
        let cache = PoolCache::new(5000);
        assert!(cache.all_entries().is_empty());
    }

    #[test]
    fn test_cache_all_addresses() {
        let cache = PoolCache::new(5000);
        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();
        let addr3 = Pubkey::new_unique();

        cache.insert(addr1, make_test_pool_state());
        cache.insert(addr2, make_test_pool_state());
        cache.insert(addr3, make_test_pool_state());

        let addresses = cache.all_addresses();
        assert_eq!(addresses.len(), 3);

        let set: std::collections::HashSet<Pubkey> = addresses.into_iter().collect();
        assert!(set.contains(&addr1));
        assert!(set.contains(&addr2));
        assert!(set.contains(&addr3));
    }

    #[test]
    fn test_cache_all_addresses_empty() {
        let cache = PoolCache::new(5000);
        assert!(cache.all_addresses().is_empty());
    }

    #[test]
    fn test_cache_remove() {
        let cache = PoolCache::new(5000);
        let addr = Pubkey::new_unique();
        cache.insert(addr, make_test_pool_state());
        assert_eq!(cache.len(), 1);

        cache.remove(&addr);
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&addr).is_none());
    }

    #[test]
    fn test_cache_remove_nonexistent() {
        let cache = PoolCache::new(5000);
        cache.remove(&Pubkey::new_unique()); // should not panic
    }

    #[test]
    fn test_cache_failure_tracking_basic() {
        let cache = PoolCache::new(5000);
        let addr = Pubkey::new_unique();

        assert!(!cache.is_dormant(&addr));
        assert_eq!(cache.dormant_count(), 0);

        // Record failures below threshold
        for _ in 0..9 {
            cache.record_failure(&addr);
        }
        assert!(!cache.is_dormant(&addr));

        // 10th failure crosses threshold
        cache.record_failure(&addr);
        assert!(cache.is_dormant(&addr));
        assert_eq!(cache.dormant_count(), 1);
    }

    #[test]
    fn test_cache_failure_reset() {
        let cache = PoolCache::new(5000);
        let addr = Pubkey::new_unique();

        for _ in 0..10 {
            cache.record_failure(&addr);
        }
        assert!(cache.is_dormant(&addr));

        cache.reset_failure(&addr);
        assert!(!cache.is_dormant(&addr));
        assert_eq!(cache.dormant_count(), 0);
    }

    #[test]
    fn test_cache_dormant_addresses() {
        let cache = PoolCache::new(5000);
        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();
        let addr3 = Pubkey::new_unique();

        // Make addr1 and addr3 dormant
        for _ in 0..10 {
            cache.record_failure(&addr1);
            cache.record_failure(&addr3);
        }
        // addr2 has only 5 failures
        for _ in 0..5 {
            cache.record_failure(&addr2);
        }

        let dormant = cache.dormant_addresses();
        assert_eq!(dormant.len(), 2);
        let set: std::collections::HashSet<Pubkey> = dormant.into_iter().collect();
        assert!(set.contains(&addr1));
        assert!(set.contains(&addr3));
        assert!(!set.contains(&addr2));
    }

    #[test]
    fn test_cache_remove_clears_failures() {
        let cache = PoolCache::new(5000);
        let addr = Pubkey::new_unique();

        for _ in 0..10 {
            cache.record_failure(&addr);
        }
        assert!(cache.is_dormant(&addr));

        cache.remove(&addr);
        assert!(!cache.is_dormant(&addr));
    }
}
