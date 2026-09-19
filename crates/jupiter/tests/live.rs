//! Live smoke test against api.jup.ag. Ignored by default; run with
//! `JUPITER_API_KEY=… cargo test -p searcher-jupiter --test live -- --ignored`.

use searcher_core::Address;
use searcher_core::address::well_known;
use searcher_core::model::{DexFilter, RoutingMode, SlippageSpec};
use searcher_jupiter::{ApiKey, BuildRequest, JupiterClient};
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn live_build_fast_and_dex_constrained() {
    let key = std::env::var("JUPITER_API_KEY").ok().and_then(ApiKey::new);
    let c = JupiterClient::new(
        "https://api.jup.ag",
        key,
        LimiterConfig::new(0.5, 1),
        Duration::from_secs(5),
        Arc::new(Telemetry::new()),
    )
    .unwrap();
    let taker: Address = "CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX".parse().unwrap();
    let mut r = BuildRequest {
        input_mint: well_known::addr(well_known::WSOL_MINT),
        output_mint: well_known::addr(well_known::USDC_MINT),
        amount: 1_000_000_000,
        taker,
        slippage: SlippageSpec::Rtse,
        mode: RoutingMode::Fast,
        dex_filter: DexFilter::Only(vec!["Raydium CLMM".into()]),
        cu_price_percentile: "high".into(),
        max_accounts: Some(30),
        blockhash_slots_to_expiry: 150,
        for_jito_bundle: true,
    };
    let b = c.build(&r, 0).await.expect("fast + dexes + forJitoBundle build");
    println!(
        "fast/raydium clmm: out={} min={} dexes={:?} {}ms",
        b.leg.out_amount,
        b.leg.min_out,
        b.leg.dex_labels(),
        b.leg.latency_ms
    );
    assert!(b.leg.dex_labels().iter().all(|d| d == "Raydium CLMM"));
    assert_eq!(b.leg.hops.len(), 1, "fast mode does not split routes");

    r.mode = RoutingMode::Normal;
    r.dex_filter = DexFilter::Only(vec!["Whirlpool".into()]);
    let b = c.build(&r, 0).await.expect("normal + whirlpool");
    println!("normal/whirlpool: out={} dexes={:?} {}ms", b.leg.out_amount, b.leg.dex_labels(), b.leg.latency_ms);
    assert!(b.leg.dex_labels().iter().all(|d| d == "Whirlpool"));
}
