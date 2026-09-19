//! HTTP behaviour against a local mock server: 429 → backoff (no retry storm),
//! 400 no-route classification, 200 fixture decode.

use searcher_core::address::well_known;
use searcher_core::model::{DexFilter, RoutingMode, ServiceId, ServiceState, SlippageSpec};
use searcher_core::{Address, Ts};
use searcher_jupiter::{BuildRequest, JupiterClient, JupiterError};
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const FIXTURE: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_rtse.json");

type Canned = Vec<(u16, Vec<(&'static str, String)>, String)>;

/// Serves `responses` in order (status, extra headers, body); counts requests.
async fn mock(responses: Canned) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        let mut responses = responses.into_iter();
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 16 * 1024];
            let _ = sock.read(&mut buf).await;
            h.fetch_add(1, Ordering::SeqCst);
            let (status, headers, body) = responses.next().unwrap_or((500, vec![], "{\"error\":\"exhausted\"}".into()));
            let mut resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(&body);
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), hits)
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

fn client(url: &str, tel: Arc<Telemetry>) -> JupiterClient {
    let mut cfg = LimiterConfig::new(50.0, 5);
    cfg.base_backoff = Duration::from_millis(400);
    JupiterClient::new(url, None, cfg, Duration::from_secs(2), tel).unwrap()
}

#[tokio::test]
async fn rate_limit_429_backs_off_and_does_not_retry() {
    let reset = (Ts::now().micros() / 1_000_000 + 1).to_string();
    let (url, hits) = mock(vec![
        (
            429,
            vec![("x-ratelimit-reset", reset), ("x-ratelimit-remaining", "-1".into())],
            "[API Gateway] Too many requests".into(),
        ),
        (200, vec![("x-ratelimit-remaining", "8".into())], FIXTURE.into()),
    ])
    .await;
    let tel = Arc::new(Telemetry::new());
    let c = client(&url, tel.clone());

    let err = c.build(&req(), 0).await.expect_err("429 must surface as an error");
    let JupiterError::RateLimited(backoff) = err else { panic!("expected RateLimited, got {err:?}") };
    assert!(backoff >= Duration::from_millis(200));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a 429 must not trigger an automatic retry");
    let snap = tel.snapshot(ServiceId::Jupiter, Ts::now());
    assert_eq!(snap.state, ServiceState::RateLimited);
    assert_eq!(snap.rate_limited, 1);
    assert_eq!(snap.quota_remaining, Some(-1));

    // The next call waits for the backoff instead of hammering the server.
    let t0 = Instant::now();
    let ok = c.build(&req(), 0).await.expect("second call succeeds after backoff");
    assert!(t0.elapsed() >= backoff.saturating_sub(Duration::from_millis(50)), "waited {:?}", t0.elapsed());
    assert_eq!(ok.leg.out_amount, 10_538_567);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(c.limiter().bucket().unwrap().state(Instant::now()).attempt, 0, "success resets backoff exponent");
}

#[tokio::test]
async fn no_route_and_auth_errors_are_classified() {
    let (url, _) = mock(vec![
        (400, vec![], "{\"error\":\"No routes found\"}".into()),
        (401, vec![], "{\"message\":\"invalid api key\"}".into()),
        (400, vec![], "{\"error\":\"dexes and excludeDexes are mutually exclusive\"}".into()),
        (200, vec![], "{not json".into()),
        (400, vec![], "{\"error\":\"503: upstream connect error or disconnect/reset before headers\"}".into()),
    ])
    .await;
    let tel = Arc::new(Telemetry::new());
    let c = client(&url, tel.clone());
    assert!(matches!(c.build(&req(), 0).await, Err(JupiterError::NoRoute(_))));
    assert!(matches!(c.build(&req(), 0).await, Err(JupiterError::Auth(401, _))));
    assert!(matches!(c.build(&req(), 0).await, Err(JupiterError::BadRequest(_))));
    assert!(matches!(c.build(&req(), 0).await, Err(JupiterError::Decode(_))));
    assert!(
        matches!(c.build(&req(), 0).await, Err(JupiterError::Http(503, _))),
        "wrapped upstream outage is transient"
    );
}
