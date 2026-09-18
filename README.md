# flow-trades

Self-hosted Solana swap API. Direct AMM execution across 21 pool types (20 unique DEX programs + Pumpup AMM/bonding split) powered by Yellowstone Geyser gRPC. **40µs quotes, zero RPC on hot path, all swaps routed through on-chain fee program.**

```
GET  /quote              Best swap route + expected output
POST /swap               Unsigned versioned transaction (base64)
GET  /health             Server status, pool counts, stream stats
GET  /metrics            Prometheus metrics
GET  /program-id-to-label  DEX program ID → human label
WS   /quote-ws           Streaming quote updates
WS   /swap-stream        Live swaps (every confirmed DEX swap, with native + USD prices)
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
  WS /quote-ws ──────►│  │  :8080   │  │ (40µs)   │  │  + ALTs        │            │
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

### Data Flow

1. **Account stream** (Filter 1) pushes pool state changes for 21 DEX programs (incl. OnChain Labs DEX V2 for inner-instruction discovery). 14 sync-parseable types are deserialized inline from raw bytes. Async types (Raydium V4, PumpFun, PumpFun AMM, Meteora Standard, Meteora DBC, Pumpup bonding) use cached companion data from the mirror or a separate RPC for first-time mint discovery.
2. **Block stream** (Filter 3) delivers full blocks with all DEX transactions. Pool candidates extracted from top-level + inner instructions (CPI routes). Pools registered immediately from block data. `post_token_balances` parsed for vault balance updates — same-slot freshness.
3. **Vault balances** fed by three channels: (a) Geyser account subscription, (b) block-stream `post_token_balances`, (c) RPC seed on first cache miss. After first access, all subsequent reads are in-memory.
4. **Companion accounts** (Serum markets, PumpFun global, Meteora vault configs) are fetched once via RPC on first encounter, then subscribed in Geyser for real-time updates.
5. **Quote engine** evaluates all matching pools in parallel. Reads pool state + vault balances from the Account Mirror. Zero RPC after initial seeding.
6. **TX builder** assembles instructions, wraps in flow-router CPI (on-chain fee enforcement), builds v0 versioned transaction. Blockhash from Geyser gRPC cache.

### RPC Call Profile

In steady state, **zero RPC calls** for `/quote` and `/swap`. The only RPC calls are:

| Operation | When | Frequency |
|-----------|------|-----------|
| Mint token program lookup | First `/swap` per unique mint | Once, cached forever |
| Companion data fetch | First encounter per async pool | Once, then Geyser-streamed |
| pump.fun AMM fee/buyback accounts | First `/swap` per pAMM pool (`getSignaturesForAddress` + `getTransaction`) | Once per pool, carried across Geyser refreshes |
| Transaction simulation | `/swap` with `simulate: true` | User opt-in only |

**pump.fun AMM (PumpSwap) with the `pump_fees` buyback accounts:** every swap must carry
the pool's buyback fee-recipient accounts as trailing remaining accounts (error 6058 without them,
6023 if incomplete) and the currently valid protocol fee recipient, which rotates. Neither is a
derivable PDA, so they are read verbatim from a recent swap on the pool the first time the pool is
traded and kept in `PoolState::PumpFunAmm`. A brand-new pool with no swap yet cannot be traded
until someone else swaps on it. With those accounts a pAMM swap through the router no longer fits
a legacy transaction (~1.7 KB); it needs the v0 + Address Lookup Table path (`ALT_ADDRESSES`),
which packs it to ~1.05 KB, or a v1 transaction (below).

**pAMM buys are exact-input** (`buy_exact_quote_in`): the user spends exactly `amount_in` and both
the program and the router enforce `minimum_out`.

**Fees are measured, not assumed.** Every streamed swap carries the pool's vault balances before
and after in its meta, so for a constant-product pool the fee the trader actually paid is
`1 − implied_in / paid_in` (`implied_in = out × R_in / (R_out − out)`). The stream records that per
pool (`stream::observed_fees`, 10-minute freshness) and the quote engine prefers it over any table.
pump.fun AMM does not use it: its curve and fees are modelled exactly (table below; market cap for
the fee tier = `(quote_reserve + virtual_quote_reserve) × base_supply / base_reserve`, tiers read from
the fee program's config at startup). Raydium CPMM reads `trade_fee_rate` and `creator_fee_rate` from
its `AmmConfig`; the Pumpup AMM default is a deliberately high 1% until a swap is observed.

**Swap stream legs are the pool's own vault deltas** (`vault_legs`): one swap per pool per
transaction with the amounts that pool actually took and gave, so a routed 2-hop trade yields two
leg-accurate swaps instead of the same user delta twice. The fee payer's net deltas are the
fallback, used at most once per transaction. OnChain Labs DEX V2 (aggregator into private venues)
is streamed under its program id.

**Transaction versions:** every RPC block/transaction request declares
`stream::tx_version::MAX_SUPPORTED_TX_VERSION` (currently 1 — SIMD-0385, active on mainnet). A
lower number does not skip newer transactions; the RPC refuses the whole block (`-32015`) or sends
`block: null` on `blockSubscribe`. The Geyser path is version-agnostic. A unit test fails the build
if the version is declared anywhere else.

**v1 transactions on `/swap`:** pass `"tx_version": 1` to receive a SIMD-0385 v1 transaction
(4,096-byte limit, every account inline, no lookup tables, compute budget and a 64 MiB
loaded-accounts budget in the header — v1 has no implicit budget; first byte `0x81`). Use it for
multi-hop routes that exceed the 1,232-byte v0 limit even with lookup tables — `/swap` rejects an
oversized v0 transaction with a message saying so. The signer must support v1.

**Known limits:** the on-chain router collects its fee with a plain SPL `transfer`, which Token-2022
mints with extensions reject (error 0x1f) — such output mints cannot be traded through the deployed
router (it is immutable; see the router layout note under Fee Structure). Venues without curve math
are streamed but not quoted (table below). Saros pools deliver more than their on-chain fee schedule
implies, so Saros is quoted from observed fees. pump.fun AMM pools without a coin creator use a fee
schedule that is not part of the 25-tier config and are under-quoted by ~65 bps.

**Pool discovery from blocks** keys on program **and instruction discriminator**
(`block_scanner::swap_pool_index`): the pool sits at a different account position per swap variant
(Orca `swap` [2] vs `swapV2` [4], Raydium CPMM [3], Raydium LP [4], Meteora/DLMM [0], DAMM [1],
pump.fun bonding [3]) and non-swap instructions carry no pool. Candidates whose fetch fails are
remembered for 10 minutes instead of being re-fetched on every block.

---

## Supported DEXes (21 pool types)

| # | DEX | Type |
|---|-----|------|
| 1 | Raydium V4 | Legacy AMM |
| 2 | Raydium CPMM | Constant Product |
| 3 | Raydium CLMM | Concentrated Liquidity — exact tick-array walk (`quote/clmm.rs`) |
| 4 | Raydium LaunchLab (`LanMV9…`) | Bonding curve on virtual + real reserves (`quote/launchlab.rs`) |
| 5 | PumpFun | Bonding Curve |
| 6 | PumpFun AMM | x·y=k on `base_vault` × (`quote_vault` + the pool's virtual quote reserve); per-component ceil fees from the market-cap tier; exact-in buys put `x−1` into the curve (`amms/pumpfun_amm.rs::pamm_quote_exact_in`) |
| 7 | Orca Whirlpool | Concentrated Liquidity — exact tick-array walk |
| 8 | Meteora Standard | Constant product on the pool's LP share of two dynamic vaults (`quote/meteora_std.rs`; stable-curve pools not quoted) |
| 9 | Meteora DLMM | Dynamic LMM |
| 10 | Meteora DAMM v2 | Single-range sqrt-price curve (liquidity ×2^64) + base-fee scheduler + dynamic fee + `collect_fee_mode` (`quote/damm_v2.rs`) |
| 11 | Meteora DBC | Dynamic Bonding Curve |
| 12 | FluxBeam | SPL Token Swap fork |
| 13 | DefiTuna Fusion | CLMM (Orca layout) — exact tick-array walk |
| 14 | DefiTuna Pools | Position manager — **stale**: the program emits no swaps; discovered only, not quoted |
| 15 | Saros | SPL Token Swap fork |
| 16 | Dooar | SPL Token Swap fork |
| 17 | PancakeSwap | CLMM (Raydium fork) — exact tick-array walk; bitmap extension + initialised arrays passed per swap version |
| 18 | FlashTrade | **stale / misattributed**: `FLASHX8DrLb…` is a routing program that CPIs into pump.fun venues, not the FlashTrade perpetuals program (`FLASH6Lo6…`). Its first account is the user's wallet, so swaps and pools attributed to it in the stream are not real pools. Not quoted |
| 19 | Byreal | CLMM — **not quotable**: the program is a Raydium CLMM fork (pool, AmmConfig, tick-array and bitmap-extension layouts identical to Raydium; swap discriminator `e52ed584692828e4` with `swap_v2` account order, pool at account index 2) while this crate's parser and executor use the Orca layout |
| 20 | Pumpup AMM | Constant Product (post-graduation) — **stale**: the program no longer trades; discovered + executed, not quoted |
| 21 | Pumpup Bonding | Bonding Curve (pre-graduation, native SOL) — **stale** (same program) |

Plus **OnChain Labs DEX V2** (`proVF4...`) — aggregator router into 80+
private MM venues. Discovery-only: pool addresses behind it are picked
up by the block scanner and registered against their underlying
DEXes; we do not quote or execute against the aggregator program
itself.

---

## Quick Start

```toml
# config.toml
rpc_url = "https://your-solana-rpc.com"

[streaming]
geyser_endpoint = "http://your-geyser-node:10000"
# geyser_token = "your-auth-token"  # if remote provider requires it
```

```bash
export OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu
export OPENSSL_INCLUDE_DIR=/usr/include
cargo build --release
./target/release/flow-trades
```

Server starts on `127.0.0.1:8080`. Pools discovered automatically from Geyser, persisted to SQLite.

```bash
# Get a quote
curl "http://localhost:8080/quote?input=So11111111111111111111111111111111111111112&output=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000000"

# Check health
curl http://localhost:8080/health
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
| `mode` | string | No | `ExactIn` | `ExactIn` or `ExactOut` |

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

## Configuration

| Option | CLI Flag | Env Var | Default |
|--------|----------|---------|---------|
| `rpc_url` | `--rpc-url` | `RPC_URL` | **(required)** |
| `listen` | `--listen` | `LISTEN_ADDR` | `127.0.0.1:8080` |
| `geyser_endpoint` | `--geyser-endpoint` | `GEYSER_ENDPOINT` | -- |
| `geyser_token` | `--geyser-token` | `GEYSER_TOKEN` | -- |
| `referral_account` | `--referral-account` | `REFERRAL_ACCOUNT` | -- |
| `pool_db` | `--pool-db-path` | `POOL_DB_PATH` | `./pools.db` |
| `pool_cache_ttl` | `--pool-cache-ttl` | `POOL_CACHE_TTL_MS` | `2000` (disabled with Geyser) |
| `alt_addresses` | `--alt-addresses` | `ALT_ADDRESSES` | -- |
| `discovery_mode` | `--discovery-mode` | `DISCOVERY_MODE` | `auto` |
| `block_scan_enabled` | `--block-scan-enabled` | `BLOCK_SCAN_ENABLED` | `true` |
| `swap_stream_enabled` | `--swap-stream-enabled` | `SWAP_STREAM_ENABLED` | `true` |
| `swap_stream_buffer_size` | `--swap-stream-buffer-size` | `SWAP_STREAM_BUFFER_SIZE` | `8192` |
| `sol_price_refresh_secs` | `--sol-price-refresh-secs` | `SOL_PRICE_REFRESH_SECS` | `10` |
| `log_level` | `--log-level` | `LOG_LEVEL` | `warn` |

Config can also be set via `config.toml` (see `config.toml` for full example). Priority: CLI flags > env vars > TOML file > defaults.

---

## Fee Structure

All swaps are routed through the on-chain flow-router program. No swap can bypass the fee wrapper. The program is **immutable** — upgrade authority permanently burned.

| Property | Value |
|----------|-------|
| Router Program ID | `FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm` |
| Config PDA | `HKkiUSLkmE2CvpAa3E4fk7ATivuc1MNqmnzhNfnf7VVY` |
| Treasury | `2yL7tWs2TULhicDtdDV7A8P8Agh79EwCFLKeyKL5fMr3` |
| Platform Fee | 0.5% (50 bps) on output token |
| Integrator Share | 70% of fee |
| Protocol Share | 30% of fee |
| Upgrade Authority | `none` (immutable) |

Fee ATAs (treasury + referral) are auto-created idempotently on the first swap for each output mint (~0.002 SOL rent, paid by signer). Cached after creation.

**Token-2022 outputs and the router layout.** The immutable `FLoW…` deployment collects its fee with a
plain SPL `Transfer`, which Token-2022 rejects (`0x1f`) for mints carrying extensions. A successor
router program using `TransferChecked` (new `output_mint` account at index N+6, DEX accounts from N+7,
instruction data unchanged) exists as source but is **not deployed**. flow-trades already supports both
layouts and selects by program id: `ROUTER_PROGRAM_ID` (`--router-program-id`) unset or equal to the
legacy id → legacy layout; any other id → `transfer_checked` layout (`execution/router.rs::RouterLayout`).
Until a successor is deployed, leave `ROUTER_PROGRAM_ID` unset and expect Token-2022 outputs to fail.

---

## Quoting rules

The quote engine never guesses and never quotes optimistically:

- **Every venue is priced with the program's own arithmetic**: CLMM venues (Raydium CLMM, Orca,
  PancakeSwap, DefiTuna Fusion) walk the real initialised ticks in Q64.64 with 256-bit intermediates
  (`quote/clmm.rs`); a pool whose tick arrays are not in memory is cold (loaded, then quoted), never
  approximated. pump.fun AMM, Meteora DAMM v2, Raydium
  LaunchLab and Meteora Standard have their own modules (table above). Constant-product venues take
  their fee from an explicit exhaustive table, Raydium CPMM from its `AmmConfig`, Raydium CLMM /
  PancakeSwap from theirs (ppm, layout-checked against the pool's tick spacing).
- **Token-2022 transfer fees** are applied on both legs: the DEX receives `amount − fee(amount)` and
  the user receives `out − fee(out)` — the router's slippage check measures the latter
  (`pool/mints.rs`; mint facts loaded once per mint, off the quote path).
- **Observed fees** (`stream/observed_fees`, implied from streamed swaps, 10-min fresh) remain the
  fallback for constant-product venues without a config account.
- A quote's `amount_out` is the gross DEX output before the platform fee. It can be checked against
  the router's `flow-router: N hop(s), X in, Y out` line in a `/swap {"simulate": true}` response.

### Quote accuracy

Quote vs router-simulated output on mainnet, direct routes, 0.05–50 SOL, both directions
(`err = (quoted − actual) / actual`):

| Venue | Error |
|---|---|
| Raydium CPMM, Raydium CLMM, Raydium LaunchLab, Orca, PancakeSwap, DefiTuna Fusion, Meteora Standard, Meteora DAMM v2 | 0 bps on a fresh state; up to ~2 bps when the pool trades between quote and simulation |
| pump.fun AMM (pools with a coin creator) | 0 bps |
| pump.fun AMM (no coin creator) | about −65 bps (fee schedule not in the tier config) |
| Saros | −10 … −25 bps (observed-fee path) |
| Multi-hop (v1 transactions) | within ~5 bps of the quote (later hops are quoted on the previous hop's guaranteed output) |

Not quoted (streamed only): Meteora DLMM, Meteora DBC, pump.fun bonding, FlashTrade, Byreal,
Pumpup, FluxBeam, DefiTuna Pools; Raydium V4 is not indexed.

## Pool Discovery

Geyser-native dual-channel discovery — zero RPC:

1. **Account stream**: catches every pool whose on-chain state changes. 14/19 types parsed inline from raw bytes. 5 async types use cached companion data.
2. **Block stream**: processes all DEX transactions (top-level + inner CPI instructions). Pools registered immediately from block data. PumpFun mints extracted from instruction accounts.

### Live Stats (sample 10-minute mainnet run, single Geyser endpoint)

| Metric | Value |
|--------|-------|
| Pools discovered | 5,007 |
| Discovery rate | ~500 pools/min |
| Stream updates | 19,754 (~33/sec) |
| Stream errors | 0 |
| Blockhash freshness | <400ms |
| RPC calls | 0 |

### SQLite Persistence

- **WAL mode** — crash-safe, concurrent reads
- **Full load**: 163ms for 250K pools
- **Auto-pruning**: pools not seen in 7 days removed
- **Bootstrap chain**: SQLite → warm storage (bincode) → JSON snapshot → empty

---

## Performance

### Hot path vs cold path

A quote has two paths. The **hot path** is synchronous and allocation-free: registry lookup →
`PoolCache::with_state` (borrow, no clone) → pure pricing on in-memory state (mirror vault balances,
tick arrays in `quote::clmm::TICKS`, curve fields in the state) → response. The **cold path** is
everything that needs RPC: a pool whose state was never fetched, a CLMM pool whose ticks are not
loaded, a Meteora Standard pool whose vault-share reserves were never computed, a constant-product
pool whose vaults are not mirrored. Cold pools are priced concurrently after the hot loop and seed
the caches so the next quote is hot; `evaluate_many` logs `quote cold path` with the pools whenever
it happens (steady state: never).

**Cold path costs** (what needs an RPC round trip, and why it is kept off the hot path):

- First quote of a never-seen pool: 1–3 round trips (state; CLMM ticks = one `getMultipleAccounts`
  of 7 arrays + bitmap extension; Meteora Standard = one batch of 6 accounts; pAMM = the buyback
  resolve, `getSignatures` + `getTransaction`s). Cost is the RPC's round-trip time × 1–3.
- Without Geyser, freshness is block-driven (`stream/block_refresh.rs`): vault balances from
  `postTokenBalances` are applied synchronously per block; state-priced pools touched by the block
  are re-read in batches (≤300 pools/block, 100 per call) plus their tick arrays. A pool that trades
  in the ~1–2 s between the block notification and the batch read is quoted on the previous state.
  Geyser closes this gap.
- Unknown mints: the first quote touching a Token-2022 mint whose transfer fee is unknown looks it up
  (once, batched); until then the fee is treated as 0.
- A pool that keeps failing its cold path is skipped after 10 consecutive failures (`is_dormant`), so
  a dead account cannot tax every quote on its pair.

### End-to-end latency (live server, mainnet, WebSocket fallback, no Geyser)

Measured over HTTP against a server with ~25k registered pools, RPC round-trip time 490 ms
(a slow public endpoint — every cold or build-time RPC cost below scales with it). Nine pairs
(SOL/USDC/USDT/JUP/BONK/WIF), direct and routed, 12 warm samples each.

| Operation | Typical | Range | What it is |
|---|---|---|---|
| `/quote`, warm, direct or routed (1–3 hops) | **0.1–0.5 ms** p50 | p90 ≤ 1.4 ms, max 2.1 ms | in-memory pricing, HTTP included |
| `/quote`, first quote of a pair with unfetched pools | 0.5–2.0 s | one to four cold RPC rounds | then warm |
| `/swap` build, all inputs cached | **0.5–1.6 ms** | | instruction build + tx packing |
| `/swap` build, uncached | 160–520 ms | 1–3 RPC round trips | mint program, fee-ATA existence, pAMM resolve |
| `/swap` with `simulate: true` | +160–490 ms | 1–2 RPC round trips | `simulateTransaction` |
| Transaction size, single hop | 816–1,044 B (v0) / 840–1,233 B (v1) | | |
| Transaction size, 2 hops | 1,279 B v0 (rejected: over the 1,232 limit) / 1,338 B v1 | | v1 required for pump.fun multi-hop |

Multi-hop routes are chained on each hop's guaranteed (slippage-adjusted) output, so the quote equals
what the route delivers when every hop lands at or above its floor; the remainder of a hop that
delivers more than its floor stays in the user's intermediate token account.

### Quote engine latency (in-process, synthetic world, no network)

`cargo test --release --test bench_quote_hot_path -- --nocapture` prints p50/p90/p95/p99/max;
`cargo bench --bench quote_hot_path` is the criterion suite. 300 SOL/USDC pools (half Orca
whirlpools with real tick walks, half pAMM), 5 X/SOL pAMM pools, 40+40 USDT bridge pools:

| Scenario | p50 | p99 | criterion mean |
|---|---|---|---|
| direct SOL→USDC, 1 SOL, 300 pools, 20 ticks/side | 49.5 µs | 71.7 µs | 61.5 µs |
| direct SOL→USDC, 500 SOL (crosses ticks) | 53.7 µs | 55.4 µs | 66.3 µs |
| direct SOL→USDC, 300 pools, 200 ticks/side | 51.9 µs | 53.8 µs | 63.6 µs |
| routed X→USDC (direct + 2-hop + 3-hop), 300 pools | 69.8 µs | 72.6 µs | 85.5 µs |
| 20 SOL/USDC pools, direct / routed | 6.3 µs / 41 µs | 8 µs / 50 µs | 4.8 µs / 28.3 µs |
| one CLMM tick walk, 1 SOL / 50 SOL / 500 SOL | | | 80 ns / 80 ns / 109 ns |

(criterion runs the quoter inside a 2-worker tokio runtime per iteration, hence its higher means.)

**A/B testing a change**: `cargo bench --bench quote_hot_path -- --save-baseline before`, make the
change, then `cargo bench --bench quote_hot_path -- --baseline before`; criterion reports the
per-benchmark delta with confidence intervals. **Flamegraph**: `cargo bench --bench quote_hot_path
-- --profile-time 5 <filter>` writes `target/criterion/<group>/<bench>/profile/flamegraph.svg`
(pprof sampling, no `perf` needed). In the 300-pool CLMM scenario ~43 % of samples are inside
`evaluate_hot`, of which the tick walk (`swap_exact_in`) is ~12 %; registry lookup and filtering ~9 %.

### Stream and refresh throughput (live, WebSocket fallback)

| Metric | Value |
|---|---|
| Swaps streamed | ~105–115 per slot (≈20k per minute) from full blocks |
| Vault balances mirrored | ~600k–750k per 30 s |
| State-priced pools re-read | ~1.2k–4.6k per 30 s (+ tick arrays), bounded by RPC speed |
| Pools discovered | ~400–1,000 per 30 s |

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

## Testing

```bash
export OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu
export OPENSSL_INCLUDE_DIR=/usr/include

cargo test --lib     # 572 unit tests, no RPC needed
cargo test           # + E2E tests (needs RPC_URL + SIM_PRIVATE_KEY)
```

### Test Suites

| Suite | Tests | Description |
|-------|-------|-------------|
| Unit tests (`--lib`) | 572 | All modules, no external deps |
| `e2e_hot_path` | 5 | Full pipeline showcase with mainnet pools |
| `e2e_exhaustive` | 28 | All AMMs, vault fetching, math validation |
| `e2e_streaming` | 4 | Cache benchmarks (cold/hot, pipeline, concurrent) |
| `e2e_stress` | 6 | Sustained load, cache stampede, error fuzzing |
| `e2e_rpc_profile` | 6 | RPC call counting, latency buckets, cache hit rates |
| `e2e_pamm_buyback` | 4 | pump.fun AMM buyback accounts resolve; exact-input buy + sell simulate through the router (v0 + ALT); the pre-buyback layout is rejected on-chain (6058); quote uses the pool's fee tier and its floor is in the instruction |
| `e2e_txv1` | 3 | v1 transactions via `getBlock` and `blockSubscribe` (WS), both block consumers run over them; stale-version negative control (`-32015`) |

**No-Geyser mode is tested live too:** the server started with only an RPC URL (blockSubscribe
fallback, RPC blockhash refresh, production ALTs) discovers ~450 pools in 40 s, streams ~250
swaps/s on `/swap-stream`, and `/quote` + `/swap {simulate:true}` on a freshly discovered pAMM pool
simulates through the router with the buyback accounts.
| `e2e_quote` | 10 | Quote API E2E |
| `e2e_router` | 7 | Router wrapping + filtering |
| `e2e_router_mainnet` | 4 | Live: config init + 1/2/3-hop with ALTs |
| `e2e_alt` | 8 | Address Lookup Table versioned TX |
| `e2e_mainnet_swaps` | 8 | Live mainnet swap building |
| `e2e_live_swaps` | 1 | Fetch + build + simulate across 15 hardcoded AMMs |
| `e2e_fresh_markets` | 1 | Discovers a fresh active pool per AMM at run-time, then fetch → build → sim. 18/20 pipeline-pass typical (FlashTrade/Saros skip on no recent activity). |
| `e2e_fresh_roundtrips` | 1 | Live mainnet buy + sell on every SOL-paired AMM using fresh-discovered pools. Falls back to verified-deep pools on thin-bin errors. |
| `e2e_pumpup_bonding` | 1 | Live mainnet round-trip on Pumpup pre-graduation bonding curve |
| `e2e_pumpup_latency` | 1 | Cold fetch / hot cache / build / quote latency probe |
| `e2e_swap_stream` | 2 | `/swap-stream` WebSocket: disabled-mode error frame, enabled-mode subscribe → ping protocol round-trip |

---

## Contributing & Maintainers

**We're open to new maintainers.** If you've shipped Solana code, know your way around AMMs, gRPC, or Rust async, and want to help shape where flow-trades goes next, we'd love to hear from you.

The fastest way to get involved is to **[join our Telegram](https://t.me/+3BPRvJoUvlViMzg1)** and chat with the team directly. Pull requests, issues, and ideas all welcome on this repo too.

---

## License

MIT — see [LICENSE](./LICENSE). Free to use, fork, modify, and ship commercially.
