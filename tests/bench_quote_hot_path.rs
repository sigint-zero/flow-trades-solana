//! In-process hot-path latency of the quote engine: NO network, synthetic
//! registry + cache + tick data shaped like production. Prints
//! p50/p90/p95/p99/max per scenario and asserts the hot path never touches
//! RPC (the world's RPC URL is unreachable; a cold miss would error).
//!
//! `cargo test --release --test bench_quote_hot_path -- --nocapture`
//! Criterion: `cargo bench --bench quote_hot_path`
//! Flamegraph: `cargo bench --bench quote_hot_path -- --profile-time 5`
//!   → target/criterion/<bench>/profile/flamegraph.svg

#[path = "../benches/world.rs"]
mod world;

use std::time::Instant;

use flow_trades::constants::{SOL_NATIVE_MINT, USDC_MINT};
use flow_trades::quote::{QuoteRequest, Quoter};
use world::{build, pct, req};

async fn time(label: &str, q: &Quoter, r: &QuoteRequest, iters: usize) {
    for _ in 0..20 {
        let _ = q.quote(r).await;
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let res = q.quote(r).await;
        samples.push(t.elapsed().as_nanos() as f64 / 1000.0);
        assert!(res.is_ok(), "{label}: {:?}", res.err());
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "{label:<44} p50 {:>7.1}  p90 {:>7.1}  p95 {:>7.1}  p99 {:>7.1}  max {:>8.1}  µs",
        pct(&samples, 0.50), pct(&samples, 0.90), pct(&samples, 0.95), pct(&samples, 0.99), samples[samples.len() - 1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_quote_hot_path() {
    for (n, ticks) in [(20usize, 20usize), (300, 20), (300, 200)] {
        let w = build(n, ticks);
        eprintln!("\n=== {n} SOL/USDC pools (half Orca w/ {ticks} ticks/side), 5 X/SOL pAMM, 40+40 USDT bridge ===");
        time("direct  SOL→USDC 1 SOL", &w.quoter, &req(SOL_NATIVE_MINT, USDC_MINT, 1_000_000_000, true), 300).await;
        time("direct  SOL→USDC 500 SOL (crosses ticks)", &w.quoter, &req(SOL_NATIVE_MINT, USDC_MINT, 500_000_000_000, true), 300).await;
        time("direct  X→SOL (5 pAMM pools)", &w.quoter, &req(w.x, SOL_NATIVE_MINT, 1_000_000_000, true), 300).await;
        time("routed  X→USDC (direct + 2-hop + 3-hop)", &w.quoter, &req(w.x, USDC_MINT, 1_000_000_000, false), 100).await;
        time("routed  SOL→USDC (direct + 2-hop + 3-hop)", &w.quoter, &req(SOL_NATIVE_MINT, USDC_MINT, 1_000_000_000, false), 100).await;
    }
}
