use clap::Parser;

/// flow-trades — Self-hosted Solana swap API.
#[derive(Parser, Debug, Clone)]
#[command(name = "flow-trades", version, about)]
pub struct Config {
    /// Path to TOML config file.
    #[arg(long, default_value = "./config.toml")]
    pub config: String,

    /// Solana RPC endpoint URL.
    #[arg(long, env = "RPC_URL", default_value = "")]
    pub rpc_url: String,

    /// API listen address.
    #[arg(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:8080")]
    pub listen: String,

    /// Pool state cache TTL in milliseconds.
    #[arg(long, env = "POOL_CACHE_TTL_MS", default_value_t = 2000)]
    pub pool_cache_ttl: u64,

    /// Path to a static pool list (JSON). Optional.
    #[arg(long, env = "POOLS_FILE")]
    pub pools_file: Option<String>,

    /// Log level (trace, debug, info, warn, error).
    /// Default is "warn" for production use; set to "info" for development.
    #[arg(long, env = "LOG_LEVEL", default_value = "warn")]
    pub log_level: String,

    /// Geyser gRPC endpoint URL.
    #[arg(long, env = "GEYSER_ENDPOINT")]
    pub geyser_endpoint: Option<String>,

    /// Geyser gRPC auth token.
    #[arg(long, env = "GEYSER_TOKEN")]
    pub geyser_token: Option<String>,

    /// Path to L2 warm storage binary file.
    #[arg(long, env = "WARM_STORAGE_PATH", default_value = "./pool-state.bin")]
    pub warm_storage_path: String,

    /// Path to L3 JSON snapshot file.
    #[arg(long, env = "SNAPSHOT_PATH", default_value = "./pool-snapshot.json")]
    pub snapshot_path: String,

    /// L3 snapshot save interval in seconds.
    #[arg(long, env = "SNAPSHOT_INTERVAL_SECS", default_value_t = 300)]
    pub snapshot_interval_secs: u64,

    /// Warm storage save interval in seconds.
    #[arg(long, env = "WARM_SAVE_INTERVAL_SECS", default_value_t = 30)]
    pub warm_save_interval_secs: u64,

    /// Discovery mode: auto, none.
    #[arg(long, env = "DISCOVERY_MODE", default_value = "auto")]
    pub discovery_mode: String,

    /// Comma-separated Address Lookup Table addresses (base58).
    /// These are fetched on startup and used to build v0 versioned transactions.
    #[arg(long, env = "ALT_ADDRESSES")]
    pub alt_addresses: Option<String>,

    /// ALT refresh interval in seconds.
    #[arg(long, env = "ALT_REFRESH_INTERVAL_SECS", default_value_t = 300)]
    pub alt_refresh_interval_secs: u64,

    /// Enable block scanning for live pool discovery (default: true).
    #[arg(long, env = "BLOCK_SCAN_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub block_scan_enabled: bool,

    /// Block scan interval in milliseconds (how often to check for new blocks).
    #[arg(long, env = "BLOCK_SCAN_INTERVAL_MS", default_value_t = 2000)]
    pub block_scan_interval_ms: u64,

    /// Path to SQLite pool database.
    #[arg(long, env = "POOL_DB_PATH", default_value = "./pools.db")]
    pub pool_db_path: String,

    /// Interval in seconds between stale pool pruning runs.
    #[arg(long, env = "PRUNE_INTERVAL_SECS", default_value_t = 3600)]
    pub prune_interval_secs: u64,

    /// Maximum age in days before a pool is considered stale and pruned.
    #[arg(long, env = "PRUNE_MAX_AGE_DAYS", default_value_t = 7)]
    pub prune_max_age_days: u64,

    // ── Swap stream ──

    /// Enable the live swap stream (`/swap-stream` WebSocket). Default: true.
    /// When enabled, every confirmed DEX swap on a known program is parsed
    /// from the Geyser block stream and broadcast to subscribers with
    /// native + USD prices.
    #[arg(long, env = "SWAP_STREAM_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub swap_stream_enabled: bool,

    /// Per-subscriber lossy broadcast buffer size. Slow consumers lag (drop
    /// messages) — never block the parsing pipeline. Default: 8192.
    #[arg(long, env = "SWAP_STREAM_BUFFER_SIZE", default_value_t = 8192)]
    pub swap_stream_buffer_size: usize,

    /// SOL/USD oracle refresh interval in seconds. The oracle is refreshed
    /// once at startup and then on this interval via a background task.
    /// Sources: Binance public ticker (primary), DexScreener (fallback).
    /// Default: 10 seconds.
    #[arg(long, env = "SOL_PRICE_REFRESH_SECS", default_value_t = 10)]
    pub sol_price_refresh_secs: u64,

    // ── Fee Collection ──
    // Fee rate and protocol fee account are enforced by the on-chain config PDA.
    // Users can only configure the referral account (integrator revenue share).

    /// Integrator referral token account (base58). Receives referral_split_bps of platform fee.
    /// Set this to earn revenue as an integrator. If not set, protocol gets 100%.
    #[arg(long, env = "REFERRAL_ACCOUNT")]
    pub referral_account: Option<String>,

    /// flow-router program id. Defaults to the immutable legacy deployment;
    /// any other id is assumed to be a `transfer_checked` router (new account
    /// layout with `output_mint`).
    #[arg(long, env = "ROUTER_PROGRAM_ID")]
    pub router_program_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_listen_addr() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080");
    }

    #[test]
    fn test_default_cache_ttl() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.pool_cache_ttl, 2000);
    }

    #[test]
    fn test_custom_listen_addr() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--listen", "0.0.0.0:9090",
        ])
        .unwrap();
        assert_eq!(config.listen, "0.0.0.0:9090");
    }

    #[test]
    fn test_pools_file_optional() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert!(config.pools_file.is_none());
    }

    #[test]
    fn test_rpc_url_defaults_empty() {
        // rpc_url defaults to empty string — validated at runtime in main.rs
        let config = Config::try_parse_from(["flow-trades"]).unwrap();
        assert!(config.rpc_url.is_empty());
    }

    #[test]
    fn test_default_warm_storage_path() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.warm_storage_path, "./pool-state.bin");
    }

    #[test]
    fn test_default_snapshot_path() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.snapshot_path, "./pool-snapshot.json");
    }

    #[test]
    fn test_default_snapshot_interval() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.snapshot_interval_secs, 300);
    }

    #[test]
    fn test_default_warm_save_interval() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.warm_save_interval_secs, 30);
    }

    #[test]
    fn test_custom_geyser_config() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--geyser-endpoint", "http://geyser:10000",
            "--geyser-token", "my-token",
        ])
        .unwrap();
        assert_eq!(config.geyser_endpoint.unwrap(), "http://geyser:10000");
        assert_eq!(config.geyser_token.unwrap(), "my-token");
    }

    #[test]
    fn test_geyser_config_optional() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert!(config.geyser_endpoint.is_none());
        assert!(config.geyser_token.is_none());
    }

    #[test]
    fn test_custom_persistence_paths() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--warm-storage-path", "/data/warm.bin",
            "--snapshot-path", "/data/snapshot.json",
        ])
        .unwrap();
        assert_eq!(config.warm_storage_path, "/data/warm.bin");
        assert_eq!(config.snapshot_path, "/data/snapshot.json");
    }

    #[test]
    fn test_custom_persistence_intervals() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--snapshot-interval-secs", "60",
            "--warm-save-interval-secs", "10",
        ])
        .unwrap();
        assert_eq!(config.snapshot_interval_secs, 60);
        assert_eq!(config.warm_save_interval_secs, 10);
    }

    #[test]
    fn test_default_discovery_mode() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.discovery_mode, "auto");
    }

    #[test]
    fn test_custom_discovery_mode_none() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--discovery-mode", "none",
        ])
        .unwrap();
        assert_eq!(config.discovery_mode, "none");
    }

    #[test]
    fn test_alt_addresses_optional() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert!(config.alt_addresses.is_none());
    }

    #[test]
    fn test_alt_addresses_custom() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--alt-addresses", "11111111111111111111111111111111,22222222222222222222222222222222",
        ])
        .unwrap();
        assert_eq!(
            config.alt_addresses.unwrap(),
            "11111111111111111111111111111111,22222222222222222222222222222222"
        );
    }

    #[test]
    fn test_default_alt_refresh_interval() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.alt_refresh_interval_secs, 300);
    }

    #[test]
    fn test_custom_alt_refresh_interval() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--alt-refresh-interval-secs", "60",
        ])
        .unwrap();
        assert_eq!(config.alt_refresh_interval_secs, 60);
    }

    #[test]
    fn test_default_block_scan_enabled() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert!(config.block_scan_enabled);
    }

    #[test]
    fn test_block_scan_disabled() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--block-scan-enabled", "false",
        ])
        .unwrap();
        assert!(!config.block_scan_enabled);
    }

    #[test]
    fn test_default_block_scan_interval() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.block_scan_interval_ms, 2000);
    }

    #[test]
    fn test_custom_block_scan_interval() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--block-scan-interval-ms", "500",
        ])
        .unwrap();
        assert_eq!(config.block_scan_interval_ms, 500);
    }

    #[test]
    fn test_default_pool_db_path() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.pool_db_path, "./pools.db");
    }

    #[test]
    fn test_custom_pool_db_path() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--pool-db-path", "/data/my-pools.db",
        ])
        .unwrap();
        assert_eq!(config.pool_db_path, "/data/my-pools.db");
    }

    #[test]
    fn test_default_prune_interval() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.prune_interval_secs, 3600);
    }

    #[test]
    fn test_default_prune_max_age() {
        let config = Config::try_parse_from(["flow-trades", "--rpc-url", "http://localhost:8899"])
            .unwrap();
        assert_eq!(config.prune_max_age_days, 7);
    }

    #[test]
    fn test_custom_prune_settings() {
        let config = Config::try_parse_from([
            "flow-trades",
            "--rpc-url", "http://localhost:8899",
            "--prune-interval-secs", "1800",
            "--prune-max-age-days", "3",
        ])
        .unwrap();
        assert_eq!(config.prune_interval_secs, 1800);
        assert_eq!(config.prune_max_age_days, 3);
    }
}
