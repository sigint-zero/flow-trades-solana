//! Criterion benchmarks for the quote hot path (no network), with an
//! in-process sampling profiler for flamegraphs (`perf` is not needed):
//!
//! `cargo bench --bench quote_hot_path`                        # timings, HTML under target/criterion
//! `cargo bench --bench quote_hot_path -- --profile-time 5`    # flamegraph.svg per benchmark

mod world;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pprof::criterion::{Output, PProfProfiler};

use flow_trades::constants::{SOL_NATIVE_MINT, USDC_MINT};
use flow_trades::quote::clmm::{swap_exact_in, TickData};
use world::{build, req, ticks_around};

fn quote_benches(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut g = c.benchmark_group("quote");
    g.throughput(Throughput::Elements(1));
    for (n, ticks) in [(20usize, 20usize), (300, 20), (300, 200)] {
        let w = build(n, ticks);
        let id = format!("{n}pools_{ticks}ticks");
        let r = req(SOL_NATIVE_MINT, USDC_MINT, 1_000_000_000, true);
        g.bench_with_input(BenchmarkId::new("direct_sol_usdc_1sol", &id), &r, |b, r| b.to_async(&rt).iter(|| w.quoter.quote(r)));
        let r = req(SOL_NATIVE_MINT, USDC_MINT, 500_000_000_000, true);
        g.bench_with_input(BenchmarkId::new("direct_sol_usdc_500sol", &id), &r, |b, r| b.to_async(&rt).iter(|| w.quoter.quote(r)));
        let r = req(w.x, USDC_MINT, 1_000_000_000, false);
        g.bench_with_input(BenchmarkId::new("routed_x_usdc", &id), &r, |b, r| b.to_async(&rt).iter(|| w.quoter.quote(r)));
    }
    g.finish();
}

fn clmm_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("clmm_walk");
    let td: TickData = ticks_around(200);
    let (p, l) = (world::orca_sqrt_price(), 900_000_000_000_000u128);
    for amount in [1_000_000_000u64, 50_000_000_000, 500_000_000_000] {
        g.bench_with_input(BenchmarkId::new("exact_in_a_to_b", amount), &amount, |b, a| {
            b.iter(|| swap_exact_in(p, l, world::ORCA_TICK, 400, &td, true, *a))
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = quote_benches, clmm_benches
}
criterion_main!(benches);
