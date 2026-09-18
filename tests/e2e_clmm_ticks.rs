//! Live (simulate-free) probe of the CLMM tick-array machinery against mainnet:
//! loads a pool's ticks, prints what the executor would pass, and verifies
//! every passed account exists with the expected owner + discriminator.
//! Needs `RPC_URL`. `cargo test --release --test e2e_clmm_ticks -- --nocapture --ignored`
use std::str::FromStr;
use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use flow_trades::constants::*;
use flow_trades::pool::fetcher::fetch_pool_state;
use flow_trades::pool::ticks::{load_clmm_ticks, tick_source};
use flow_trades::pool::types::PoolType;
use flow_trades::execution::amms::raydium_clmm::clmm_swap_tick_arrays;

fn rpc() -> Arc<RpcClient> {
    let url = std::env::var("RPC_URL").or_else(|_| std::env::var("SOL_HTTPS_ENDPOINT")).expect("RPC_URL");
    Arc::new(RpcClient::new(url))
}

async fn probe(rpc: &RpcClient, pool_type: PoolType, pool: &str) {
    let pool = Pubkey::from_str(pool).unwrap();
    let st = fetch_pool_state(rpc, pool_type, &pool).await.expect("state");
    let (layout, program, _, tick, spacing) = tick_source(&st).unwrap();
    let td = load_clmm_ticks(rpc, &st).await.expect("ticks");
    eprintln!("\n== {pool_type:?} {pool} tick {tick} spacing {spacing} layout {layout:?}");
    eprintln!("   ticks {} covered [{}, {}] initialized_arrays {:?} ext {:?}", td.ticks.len(), td.covered_lo, td.covered_hi, td.initialized_arrays, td.bitmap_extension);
    for a_to_b in [true, false] {
        let (ext, arrays) = clmm_swap_tick_arrays(&program, &pool, tick, spacing, a_to_b);
        let keys: Vec<Pubkey> = ext.into_iter().chain(arrays).collect();
        let accts = rpc.get_multiple_accounts(&keys).await.unwrap();
        for (k, a) in keys.iter().zip(accts.iter()) {
            match a {
                Some(a) => eprintln!("   a_to_b={a_to_b} {k} owner_ok={} len={} disc={}", a.owner == program, a.data.len(), hex(&a.data[..8])),
                None => eprintln!("   a_to_b={a_to_b} {k} MISSING"),
            }
        }
    }
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

#[tokio::test]
#[ignore]
async fn probe_pancake_and_raydium_tick_arrays() {
    let rpc = rpc();
    for p in ["DJNtGuBGEQiUCWE8F981M2C3ZghZt2XLD8f2sQdZ6rsZ", "22HUWiJaTNph96KQTKZVy2wg8KzfCems5nyW7E5H5J6w"] {
        probe(&rpc, PoolType::PancakeSwap, p).await;
    }
    for p in ["4pCZCVEiYyT4efNdXUdL2tJF8VGMgiMXrZWq6FiNXhRw"] {
        probe(&rpc, PoolType::RaydiumCl, p).await;
    }
    let _ = (RAYDIUM_CL_PROG_ID, PANCAKESWAP_PROG_ID);
}
