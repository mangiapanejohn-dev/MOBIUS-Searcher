//! The research gate against a local Jupiter stub: with the rate-limit
//! window nearly full, a lag entry starts at once while exits and the rest
//! wait for slots to free.

use mobius_searcher::research::{Gate, Priority};
use searcher_core::Address;
use searcher_core::address::well_known;
use searcher_core::model::{DexFilter, RoutingMode, SlippageSpec};
use searcher_jupiter::{BuildRequest, JupiterClient};
use searcher_telemetry::{Limiter, Telemetry, WindowLimiter};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const FIXTURE: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_rtse.json");

/// Answers every request with the fixture.
async fn stub() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{FIXTURE}",
                    FIXTURE.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn req() -> BuildRequest {
    BuildRequest {
        input_mint: well_known::addr(well_known::WSOL_MINT),
        output_mint: well_known::addr(well_known::USDC_MINT),
        amount: 100_000_000,
        taker: Address([3; 32]),
        slippage: SlippageSpec::Rtse,
        mode: RoutingMode::Normal,
        dex_filter: DexFilter::Any,
        cu_price_percentile: "high".into(),
        max_accounts: None,
        blockhash_slots_to_expiry: 150,
        for_jito_bundle: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lag_entry_is_not_held_behind_exits_or_the_rest() {
    let url = stub().await;
    // 4 requests per 3 s, no safety slots
    let limiter = Limiter::Window(WindowLimiter::new("test", 4, Duration::from_secs(3), 0));
    let jup = Arc::new(
        JupiterClient::with_limiter(&url, None, limiter, Duration::from_secs(2), Arc::new(Telemetry::new())).unwrap(),
    );
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (gate, _task) = Gate::spawn(jup, stop_rx);

    let t0 = Instant::now();
    let timed = |g: Gate, p: Priority| {
        tokio::spawn(async move {
            let r = g.build(req(), p).await;
            (r.is_ok(), t0.elapsed())
        })
    };
    // the rest may use 4 − 2 = 2 slots, exits 4 − 1 = 3
    let rest: Vec<_> = (0..3).map(|_| timed(gate.clone(), Priority::Normal)).collect();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let exits: Vec<_> = (0..2).map(|_| timed(gate.clone(), Priority::Exit)).collect();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let entry = timed(gate.clone(), Priority::High);

    let (ok, at) = entry.await.unwrap();
    assert!(ok);
    assert!(at < Duration::from_millis(1_500), "entry waited {at:?}");
    let mut late = 0;
    for h in rest.into_iter().chain(exits) {
        let (ok, at) = h.await.unwrap();
        assert!(ok);
        late += (at >= Duration::from_millis(2_500)) as u32;
    }
    // 2 of the rest + 1 exit + the entry fit the first window; 1 rest + 1 exit wait for it to roll
    assert_eq!(late, 2, "requests that had to wait for the window");
}
