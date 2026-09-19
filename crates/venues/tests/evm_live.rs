//! Against real EVM chains (public RPC, read-only). Ignored by default:
//! `cargo test -p searcher-venues --test evm_live -- --ignored --nocapture`

use searcher_core::config::Config;
use searcher_venues::evm::{EvmRpc, UniV3Pool, quote, word, word_u128_of};
use searcher_venues::evm_trade::{simulate, swap_calldata};

#[tokio::test]
#[ignore]
async fn uniswap_v3_state_and_quotes_on_the_default_chains() {
    let cfg = Config::default();
    for name in ["ethereum", "base", "arbitrum"] {
        let v = &cfg.venues[name];
        let rpc = EvmRpc::new(&v.resolved_url(), 3.0).unwrap();
        assert_eq!(rpc.chain_id().await.unwrap(), v.chain_id.unwrap(), "{name}: wrong chain behind the RPC");
        let pool = UniV3Pool::load(&rpc, &v.pools[0]).await.unwrap();
        assert_eq!(pool.market(), "WETH/USDC", "{name}");
        let st = pool.state(&rpc).await.unwrap();
        let mid = pool.mid(st.sqrt_price_x96, "WETH").unwrap();
        assert!(mid > 100.0 && mid < 100_000.0, "{name}: mid {mid}");
        let weth = pool.token("WETH").unwrap().clone();
        let q = quote(&rpc, &v.quoter, &pool, &weth, 10u128.pow(17)).await.unwrap();
        let px = q.amount_out as f64 / 1e6 / 0.1;
        // selling 0.1 WETH: fee + impact make it a bit worse than mid, never better
        assert!(px < mid && px > mid * 0.99, "{name}: quote {px} vs mid {mid}");
        println!(
            "{name:<9} block {} · {} fee {:.2} % · mid {mid:.2} · sell 0.1 WETH → {px:.2} ({:+.1} bp vs mid, gas {})",
            st.block,
            pool.market(),
            pool.fee as f64 / 10_000.0,
            (px / mid - 1.0) * 10_000.0,
            q.gas_estimate
        );
    }
}

/// Our swap calldata against the real SwapRouter02, simulated with eth_call
/// (nothing is signed or sent). The WETH contract, which holds ETH, "pays"
/// with msg.value; the router wraps it. The router's output must equal
/// QuoterV2's for the same block, and an unfunded sender must revert.
#[tokio::test]
#[ignore]
async fn router_swap_simulation_matches_the_quoter() {
    let cfg = Config::default();
    for name in ["ethereum", "base", "arbitrum"] {
        let v = &cfg.venues[name];
        let rpc = EvmRpc::new(&v.resolved_url(), 3.0).unwrap();
        let pool = UniV3Pool::load(&rpc, &v.pools[0]).await.unwrap();
        let weth = pool.token("WETH").unwrap().clone();
        let amount = 10u128.pow(17);
        let recipient = "0x000000000000000000000000000000000000dEaD";
        let data = swap_calldata(&pool, &weth, recipient, amount, 0);
        let out = simulate(&rpc, &weth.address, &v.router, &data, amount).await.unwrap();
        let routed = word_u128_of(word(&out, 0).unwrap()).unwrap();
        let quoted = quote(&rpc, &v.quoter, &pool, &weth, amount).await.unwrap().amount_out;
        let diff_bp = (routed as f64 / quoted as f64 - 1.0) * 10_000.0;
        println!(
            "{name:<9} router {:.6} USDC · quoter {:.6} USDC · {diff_bp:+.3} bp",
            routed as f64 / 1e6,
            quoted as f64 / 1e6
        );
        assert!(diff_bp.abs() < 1.0, "{name}: router and quoter disagree by {diff_bp} bp (a block apart at most)");
        let min_too_high = swap_calldata(&pool, &weth, recipient, amount, routed * 2);
        assert!(
            simulate(&rpc, &weth.address, &v.router, &min_too_high, amount).await.is_err(),
            "{name}: min_out is enforced"
        );
        assert!(simulate(&rpc, recipient, &v.router, &data, 0).await.is_err(), "{name}: unfunded sender reverts");
    }
}
