# flow-trades-solana

Self-hosted Solana swap API: quotes and unsigned transactions across 21 DEX pool types, fed by
Yellowstone Geyser or a plain RPC `blockSubscribe`, every swap wrapped in the on-chain flow-router.

```
GET  /quote              Best route + expected output
POST /swap               Unsigned versioned transaction (v0 or v1, base64)
GET  /health             Server status, pool counts, stream stats
GET  /metrics            Prometheus metrics
GET  /program-id-to-label  DEX program id → label
WS   /quote-ws           Streaming quotes for a pair
WS   /swap-stream        Every confirmed DEX swap, with native and USD prices
```

---

## Architecture

```
                      ┌──────────────────────────────────────────────────────────────┐
                      │                      flow-trades                              │
                      │                    (single binary)                            │
                      │                                                              │
  GET /quote ────────►│  ┌──────────┐  ┌──────────┐  ┌────────────────┐            │
  POST /swap ────────►│  │  Axum    │  │  Quote   │  │  TX Builder    │            │
  GET /health ───────►│  │  API     │─►│  Engine  │─►│  + Router CPI  │            │
  WS /quote-ws ──────►│  │  :8080   │  │ (50µs)   │  │  + ALTs        │            │
                      │  └──────────┘  └────┬─────┘  └────────────────┘            │
                      │                     │                                        │
                      │        ┌────────────┴────────────┐                          │
                      │        ▼                         ▼                          │
                      │  ┌──────────┐    ┌──────────────────────────────┐           │
                      │  │ Registry │    │       Account Mirror         │           │
                      │  │ (DashMap) │    │                              │           │
                      │  │ mint-pair│    │  Pool State    Vault Balances │           │
                      │  │  index   │    │  (DashMap)     (DashMap)      │           │
                      │  │          │    │                              │           │
                      │  └─────┬────┘    │  Companion   Mint Program   │           │
                      │        │         │  Cache       Cache          │           │
                      │  ┌─────┴──────┐  │                              │           │
                      │  │  SQLite    │  └────────────┬───────────────┘           │
                      │  │ (pools.db) │               │                            │
                      │  └────────────┘               │                            │
                      │                               │                            │
                      │  ┌────────────────────────────┴────────────────────────┐   │
                      │  │            Geyser gRPC (Yellowstone)                 │   │
                      │  │           Bidirectional subscribe                    │   │
                      │  │                                                     │   │
                      │  │  Filter 1: 21 DEX program owners                    │   │
                      │  │    → pool state updates + inline discovery           │   │
                      │  │                                                     │   │
                      │  │  Filter 2: vault + companion + pool accounts        │   │
                      │  │    → balance updates, dynamically subscribed        │   │
                      │  │                                                     │   │
                      │  │  Filter 3: blocks with DEX transactions             │   │
                      │  │    → pool discovery from inner instructions          │   │
                      │  │    → vault balances from post_token_balances         │   │
                      │  │                                                     │   │
                      │  │  Blockhash: get_latest_blockhash() every 400ms      │   │
                      │  └─────────────────────────────┬───────────────────────┘   │
                      └────────────────────────────────┼───────────────────────────┘
                                                       │
                                            ┌──────────▼──────────┐
                                            │  Yellowstone Geyser  │
                                            │  (gRPC, port 10000)  │
                                            └──────────┬──────────┘
                                                       │
                                            ┌──────────▼──────────┐
                                            │   Solana Validator   │
                                            └─────────────────────┘
```


Pool state, vault balances and tick arrays live in memory and are refreshed by the Geyser account
and block streams, or, without Geyser, by the block scanner: vault balances from
`postTokenBalances` on every block, touched pools re-read in batches. Quotes never wait on RPC for a
pool the server has seen; the first quote of an unseen pool costs one to three RPC round trips.

---

## Venues

| Venue | Pricing | Notes |
|---|---|---|
| Raydium CPMM | x·y=k; trade + creator fee from `AmmConfig` | |
| Raydium CLMM | tick-array walk with the program's integer tick math; fee from `AmmConfig`, fee side (input / token 0 / token 1), dynamic fee, limit orders | initialised tick arrays located through the pool's bitmap |
| Raydium LaunchLab | bonding curve on virtual + real reserves; config fees, rounded once on the summed rate | |
| Raydium V4 | x·y=k on vault − `need_take_pnl`; pool fee, ceil off the input | swaps via `swap_base_in_v2` (no OpenBook accounts) |
| pump.fun AMM | x·y=k on vault quote + virtual quote reserve; fee program schedule: market-cap tiers (graduated pools quoted in SOL), stable tiers (USDC), flat / exotic schedule (other pools), per-pool creator fee; exact-input buys | fee recipients and trailing accounts derived from the global config (cashback coins included); a sell never pays out more than the real quote vault |
| pump.fun bonding | x·y=k on the curve's virtual reserves; protocol + creator fee from the fee config (per-curve creator fee when set); exact-input buys | native SOL: a sell wraps its guaranteed output into WSOL inside the router (the router fee is taken on that amount); completed, non-SOL-quoted and cashback curves are not quoted |
| Orca Whirlpool | tick-array walk with Orca's tick table; adaptive fee (oracle) | three sequential tick arrays per swap |
| PancakeSwap | tick-array walk (Raydium CLMM fork); fee from `AmmConfig` | initialised tick arrays located through the pool's bitmap |
| DefiTuna Fusion | tick-array walk incl. limit orders on ticks | current tick array and one neighbour per swap |
| Byreal | tick-array walk (Raydium layout, incl. sparse arrays); `AmmConfig` fee, per-pool override, launch decay fee, dynamic fee (Pyth arbitrage term) | Raydium CLMM fork; `swap_v3_dyn` for dynamic-fee pools; a swap the imbalance term would apply to is not quoted |
| Meteora DAMM v2 | sqrt-price curve, or x·y=k on the tracked reserves (compounding pools); linear / exponential / market-cap / rate-limiter base fee, dynamic fee, collect mode | |
| Meteora Standard | x·y=k on the pool's LP share of two dynamic vaults, with the vaults' deposit / withdraw rounding | stable curve not quoted |
| Meteora DLMM | bin walk; per-bin dynamic fee (volatility accumulator), fee mode, limit orders, bitmap extension | up to 3 bin arrays per swap |
| Meteora DBC | sqrt-price walk over the config's curve segments; scheduler / rate-limiter base fee, dynamic fee, collect mode | migrated or completed curves and buys that reach the migration threshold are not quoted |
| Saros | x·y=k; fee observed from the pool's own swaps | the pool's fee fields do not match what the program charges |
| Dooar | x·y=k; on-chain fee schedule | |
| FluxBeam | x·y=k; on-chain fee schedule (trade + owner fee, both off the input) | most pools charge 90–99 % owner fees |
| FlashTrade | — | stale: `FLASHX8…` is a routing program, not FlashTrade perps; its pools are wallets |
| DefiTuna Pools | — | stale: no swaps |
| Pumpup AMM / bonding | — | stale: program no longer trades |
| OnChain Labs DEX V2 | — | aggregator into private market makers; discovery only |

Token-2022 transfer fees are applied on both legs of a quote. Constant-product venues without a
config account use the fee observed from their own recent swaps. Concentrated-liquidity quotes walk
no further than the tick arrays the swap instruction carries, so a quote the instruction cannot
execute is not offered. Time-based fees run on the cluster clock taken from the block stream.

### Quote accuracy

Quote vs the router's simulated output on mainnet (quote → `/swap` with `simulate: true`), direct
routes, several sizes, both directions:

| Venue | Error |
|---|---|
| Raydium V4, Raydium CPMM, Raydium CLMM, LaunchLab, PancakeSwap, Byreal, Orca, DefiTuna Fusion, Meteora Standard, Meteora DAMM v2, Meteora DLMM, Meteora DBC, pump.fun AMM, pump.fun bonding, FluxBeam, Dooar | 0 atoms when the pool does not trade between quote and simulation |
| Saros | within ~1.5 bps (fee observed from the pool's own swaps) |
| Multi-hop (2–3 hops, v1) | 0 atoms on still pools; later hops are quoted on the previous hop's guaranteed output |

State is as fresh as the stream that feeds it: without Geyser the block-driven refresh runs 1–2 s
behind the chain, and a pool that trades several times a second (launch pools, pump.fun curves)
moves by tens of bps — sometimes percent — in that time. The router's `minimum_out` absorbs it; a
Geyser feed narrows it.

---

## Quick Start

```toml
# config.toml
rpc_url = "https://your-solana-rpc.com"

[streaming]
geyser_endpoint = "http://your-geyser-node:10000"   # optional; without it the RPC blockSubscribe fallback is used
# geyser_token = "your-auth-token"
```

```bash
cargo build --release
./target/release/flow-trades
curl "http://localhost:8080/quote?input=So11111111111111111111111111111111111111112&output=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000000"
```

---

## API

### `GET /quote`

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `input` | string | Yes | -- | Input token mint (base58) |
| `output` | string | Yes | -- | Output token mint (base58) |
| `amount` | string | Yes | -- | Raw amount in smallest units |
| `slippage` | u16 | No | `50` | Slippage tolerance in bps |
| `direct_only` | bool | No | `true` | `false` enables multi-hop routing |
| `exclude` | string | No | -- | DEX labels to exclude (comma-separated) |
| `dexes` | string | No | -- | DEX labels to whitelist (comma-separated) |
| `mode` | string | No | `ExactIn` | `ExactIn` only (the router executes exact-input swaps) |

### `POST /swap`

All swaps are routed through the on-chain flow-router program for fee enforcement.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `wallet` | string | Yes | -- | Signer public key (base58) |
| `quote` | object | Yes | -- | Full quote response from `/quote` |
| `priority_fee` | u64 | No | `5000` | Priority fee in lamports |
| `compute_limit` | u32 | No | auto | Compute unit limit (auto: 400K/600K/800K by hops) |
| `simulate` | bool | No | `false` | Simulate first for CU estimation (1 RPC call) |
| `dynamic_cu` | bool | No | `false` | Same as simulate — estimate CU from simulation |
| `tip` | object | No | -- | `{ "address": "base58", "lamports": u64 }` for Jito tips |

All swaps routed through the on-chain router via generic N-hop `wrap_swap()`. Supports 1 to 5 hops — account layout scales automatically with intermediate token accounts.

### `GET /health`

Returns JSON with server status, pool counts, stream stats, blockhash age.

### `GET /metrics`

Prometheus exposition format. Gauges for cache/registry/SQLite sizes, counters for quotes served, stream updates, stream errors, scanner discoveries.

### `WS /quote-ws`

WebSocket endpoint for push-based quote streaming. Client sends a subscription message with input/output/amount, server pushes quote updates at a configurable interval (min 100ms).


`tx_version: 1` returns a SIMD-0385 v1 transaction (4,096-byte limit, all accounts inline, compute
and loaded-accounts budget in the header). Without `tx_version`, `/swap` builds v0 and falls back to
v1 when the route does not fit v0 even with lookup tables (`ALT_ADDRESSES`); the response's
`tx_version` says which. An explicit `tx_version: 0` rejects an oversized build with a message.
Multi-hop routes spend each hop's guaranteed (slippage-adjusted) output on the next hop; any
surplus stays in the wallet's intermediate token account.

### `WS /swap-stream`

Live broadcast of every confirmed DEX swap parsed off the block stream, enriched with native price and USD price. Distinct from `/swap` (which builds an unsigned tx) and `/quote-ws` (which streams projected prices for a chosen pair).

Protocol:

1. Client connects to `/swap-stream`.
2. (Optional) Client sends a JSON filter on the first frame:
   ```json
   {
     "type": "subscribe",
     "filter": {
       "dex": ["Raydium V4", "Pumpup Bonding"],   // optional
       "mint": "9U3F…",                            // optional, matches input or output
       "pool": "AQxK…",                            // optional, exact pool address
       "min_amount_usd": 10.0                       // optional, filters dust
     }
   }
   ```
   Skipping the subscribe message defaults to "match all" after a 5 s grace.
3. Server replies once with `{"type":"subscribed"}`, then streams `{"type":"swap", …}` JSON frames.
4. Server emits `{"type":"ping"}` every 30 s as a keep-alive.
5. If the broadcast channel lags this subscriber (slow consumer), the server emits `{"type":"lagged","skipped":N}` — connection stays open.

Wire format per swap:

```json
{
  "type": "swap",
  "signature": "5xY…",
  "slot": 412341234,
  "block_time": 1745000000,
  "dex": "Pumpup Bonding",
  "pool": "AQxK…",
  "user": "7AakHWVQ…",
  "input":  {"mint":"…","amount":"1000000","decimals":9,"ui_amount":"0.001"},
  "output": {"mint":"…","amount":"24637903819","decimals":6,"ui_amount":"24637.9"},
  "price_native":          "24637903.82",   // output ui per input ui
  "price_native_inverted": "0.0000000406",  // input ui per output ui
  "price_usd":             "0.00000647",    // USD per output token
  "amount_usd":            "0.16"           // total trade size in USD
}
```

USD enrichment: USDC/USDT/PYUSD are treated as $1.00. SOL is priced via the in-process oracle (Binance public ticker primary, DexScreener fallback, refreshed every 10 s by default). When neither side is SOL nor a stable, `price_usd` and `amount_usd` are `null` (no external lookup).

Disable with `--swap-stream-enabled false` if you don't need the broadcast hub.

---

---

## Configuration

| Option | CLI Flag | Env Var | Default |
|--------|----------|---------|---------|
| `rpc_url` | `--rpc-url` | `RPC_URL` | **(required)** |
| `listen` | `--listen` | `LISTEN_ADDR` | `127.0.0.1:8080` |
| `geyser_endpoint` | `--geyser-endpoint` | `GEYSER_ENDPOINT` | -- |
| `geyser_token` | `--geyser-token` | `GEYSER_TOKEN` | -- |
| `referral_account` | `--referral-account` | `REFERRAL_ACCOUNT` | -- |
| `pool_db` | `--pool-db-path` | `POOL_DB_PATH` | `./pools.db` |
| `pool_cache_ttl` | `--pool-cache-ttl` | `POOL_CACHE_TTL_MS` | `2000` — unused: state is kept fresh by the account / block stream, and quiet pools are re-read in the background after 10 s |
| `alt_addresses` | `--alt-addresses` | `ALT_ADDRESSES` | -- |
| `discovery_mode` | `--discovery-mode` | `DISCOVERY_MODE` | `auto` |
| `block_scan_enabled` | `--block-scan-enabled` | `BLOCK_SCAN_ENABLED` | `true` |
| `swap_stream_enabled` | `--swap-stream-enabled` | `SWAP_STREAM_ENABLED` | `true` |
| `swap_stream_buffer_size` | `--swap-stream-buffer-size` | `SWAP_STREAM_BUFFER_SIZE` | `8192` |
| `sol_price_refresh_secs` | `--sol-price-refresh-secs` | `SOL_PRICE_REFRESH_SECS` | `10` |
| `log_level` | `--log-level` | `LOG_LEVEL` | `warn` |

Config can also be set via `config.toml` (see `config.toml` for full example). Priority: CLI flags > env vars > TOML file > defaults.

---
| `router_program_id` | `--router-program-id` | `ROUTER_PROGRAM_ID` | first-generation router |

Priority: CLI flags > env vars > `config.toml` > defaults.

---

## Fee Structure

Every swap is wrapped in the on-chain [flow-router](https://github.com/sigint-zero/flow-router-solana),
which enforces the route minimum and, when the router's config carries a non-zero `fee_bps`,
collects that share of the output token. The fee
rate, treasury and referral split are read from the router's config PDA at startup and reported by
`/health` and in every quote's `platform_fee`; a zero-fee router creates no fee token accounts.

| Property | Value |
|----------|-------|
| First-generation router | `FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm`, immutable, 50 bps, plain SPL `Transfer` (Token-2022 mints with extensions fail with `0x1f`) |
| Current generation | [flow-router-solana](https://github.com/sigint-zero/flow-router-solana): `TransferChecked` fee collection (extra `output_mint` account), upgradeable, configured via `ROUTER_PROGRAM_ID` |
| Fee side | Output token, after all hops and the slippage check (pump.fun bonding sells pay native SOL: the fee is taken on the wrapped route minimum) |
| Transfer hooks | Output mints whose transfer hook is set are refused with either router: neither passes the hook's accounts |

---

## Performance

### End-to-end (live, mainnet, RPC fallback, ~25k pools, RPC round trip 490 ms)

| Operation | Typical | Notes |
|---|---|---|
| `/quote`, warm, direct or 1–3 hops | 0.1–0.5 ms p50, max 2.1 ms | in-memory |
| `/quote`, first quote of a pair with unfetched pools | 0.5–2 s | 1–4 RPC rounds, then warm |
| `/swap` build, cached | 0.5–1.6 ms | |
| `/swap` build, uncached | 160–520 ms | 1–3 RPC rounds |
| `/swap` with `simulate: true` | +160–490 ms | 1–2 RPC rounds |
| Transaction size | 816–1,044 B single hop; 2 hops 1,338 B (v1) | |
| Swap stream | ~105–115 swaps per slot | full blocks |
| Refresh | ~600k vault balances, 1–5k pool states per 30 s | bounded by RPC speed |

### Quote engine (in-process, no network)

300 SOL/USDC pools (half CLMM with real tick walks), 5 X/SOL pools, 80 bridge pools:

| Scenario | p50 | p99 |
|---|---|---|
| direct, 1 SOL | 50 µs | 72 µs |
| direct, 500 SOL (crosses ticks) | 54 µs | 55 µs |
| routed (direct + 2-hop + 3-hop) | 70 µs | 73 µs |
| 20 pools, direct / routed | 6 µs / 41 µs | 8 µs / 50 µs |
| one CLMM tick walk | 80–109 ns | |

```bash
cargo test --release --test bench_quote_hot_path -- --nocapture          # p50/p90/p95/p99/max
cargo bench --bench quote_hot_path -- --save-baseline before            # criterion baseline
cargo bench --bench quote_hot_path -- --baseline before                 # A/B against it
cargo bench --bench quote_hot_path -- --profile-time 5 <filter>          # flamegraph.svg under target/criterion
```

### Stress Test Results

| Test | Duration | Result |
|------|----------|--------|
| Sustained quoting (10 workers) | 60s | 5,131 quotes, 114/s |
| Cache stampede (100 tasks × 10K reads) | 340ms | 2.9M reads/s, 344ns/read |
| Quote→TX pipeline (50 workers) | 30s | 406K pipelines, 2,689/s |
| 2-hop routing (20 workers) | 30s | 1,348 routes, 44/s, 0 errors |
| Error fuzzing (10K invalid requests) | 254ms | 0 panics |
| Mixed workload (10 workers) | 120s | 329K ops, 2,727/s |

### Steady State Resource Usage (sample mainnet run)

| Metric | Value |
|--------|-------|
| CPU | <1% |
| RSS | ~35 MB |
| Network | ~140 KB/s in, ~13 KB/s out |
| Threads | 5 |
| Stream errors | 0 |

---

---

## Pool Discovery and Persistence

Pools are discovered from the Geyser account stream and from blocks (top-level and inner
instructions, keyed on program and instruction discriminator). SQLite (WAL) persists the registry;
bootstrap order is SQLite → warm state file → JSON snapshot → empty. Pools unseen for 7 days are
pruned.

---

## Geyser Node Configuration

Recommended Yellowstone Geyser settings:

```json
{
  "channel_capacity": "10_000_000",
  "max_decoding_message_size": "67_108_864",
  "filter_limits": {
    "accounts": { "account_max": 100000 }
  }
}
```

`channel_capacity` 10M prevents dropped updates. `account_max` 100K supports dynamic vault + companion subscriptions. `max_decoding_message_size` 64MB handles large subscription requests.

---

---

## Testing

```bash
cargo test --lib                       # 604 unit tests, no network
RPC_URL=... cargo test --release       # live suites: simulate only, never submit
```

Live suites cover every venue's build → simulate path, pump.fun AMM buyback accounts, transaction
v1 over `getBlock` and `blockSubscribe`, tick-array loading, the router wrapper with 1–3 hops and
lookup tables, the swap stream protocol and the quote API.

---

## Contributing

Pull requests and issues are welcome. To get involved, [join the Telegram](https://t.me/+3BPRvJoUvlViMzg1).

## License

MIT — see [LICENSE](./LICENSE).
