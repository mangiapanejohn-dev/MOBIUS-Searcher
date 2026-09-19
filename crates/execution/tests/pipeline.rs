//! Pipeline end-to-end against local mock Jupiter + RPC servers (real
//! fixtures): profitable cycle → simulate → risk → PAPER fill; simulation
//! failure → skipped, never executed.

use searcher_core::address::well_known;
use searcher_core::config::Config;
use searcher_core::model::*;
use searcher_core::{Address, Event, Ts};
use searcher_execution::live::LiveParams;
use searcher_execution::{Pipeline, PipelineConfig, RuntimeView};
use searcher_jito::TipPolicy;
use searcher_jupiter::JupiterClient;
use searcher_market::{ChainState, RpcClient};
use searcher_risk::{KillSwitch, RiskEngine, RiskLimits};
use searcher_strategy::{CrossDex, FastPairs, Strategy};
use searcher_telemetry::{EventBus, LimiterConfig, Telemetry};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const LEG1: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_raydiumclmm.json");
const LEG2: &str = include_str!("../../../fixtures/jupiter_build_usdc_sol_whirlpool.json");

struct Mock {
    sim_fails: AtomicBool,
    sims: AtomicUsize,
    builds: AtomicUsize,
}

/// Leg 2 made profitable (+1% gross) so the cycle is a real candidate.
fn profitable_leg2() -> String {
    let mut v: serde_json::Value = serde_json::from_str(LEG2).unwrap();
    v["outAmount"] = "1010000000".into();
    v["otherAmountThreshold"] = "1009000000".into();
    v.to_string()
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let s = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = s.find("\r\n\r\n") {
            let len = s
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

async fn serve(mock: Arc<Mock>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let mock = mock.clone();
            tokio::spawn(async move {
                let req = read_request(&mut sock).await;
                let body = if req.starts_with("GET /swap/v2/build") {
                    mock.builds.fetch_add(1, Ordering::SeqCst);
                    if req.contains(&format!("inputMint={}", well_known::WSOL_MINT)) {
                        LEG1.to_string()
                    } else {
                        profitable_leg2()
                    }
                } else if req.contains("getMultipleAccounts") {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[{"lamports":2039280},{"lamports":2039280},{"lamports":2039280}]}}"#.into()
                } else if req.contains("simulateTransaction") {
                    mock.sims.fetch_add(1, Ordering::SeqCst);
                    if mock.sim_fails.load(Ordering::SeqCst) {
                        r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":7},"value":{"err":{"InstructionError":[5,{"Custom":6001}]},"logs":["Program log: Error: SlippageToleranceExceeded"],"unitsConsumed":91000}}}"#.into()
                    } else {
                        r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":7},"value":{"err":null,"logs":["ok"],"unitsConsumed":154250,"fee":7161}}}"#.into()
                    }
                } else {
                    r#"{"jsonrpc":"2.0","id":1,"result":null}"#.into()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn pipeline(url: &str, bus: Arc<EventBus>) -> Arc<Pipeline> {
    let cfg = Config::default();
    let tel = Arc::new(Telemetry::new());
    let jup = Arc::new(
        JupiterClient::new(url, None, LimiterConfig::new(100.0, 10), Duration::from_secs(3), tel.clone()).unwrap(),
    );
    let rpc = Arc::new(
        RpcClient::new(url, LimiterConfig::new(100.0, 10), 100.0, Duration::from_secs(3), tel.clone()).unwrap(),
    );
    let risk = Arc::new(RiskEngine::new(RiskLimits::from_config(&cfg.risk).unwrap(), Arc::new(KillSwitch::default())));
    let view = Arc::new(RuntimeView::default());
    view.set_tip_accounts(vec![searcher_execution::KNOWN_TIP_ACCOUNT.parse().unwrap()]);
    let pcfg = PipelineConfig {
        mode: Mode::Paper,
        live_enabled: false,
        taker: Some("F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe".parse().unwrap()),
        slippage: SlippageSpec::Rtse,
        cu_price_percentile: "high".into(),
        blockhash_slots_to_expiry: 150,
        for_jito_bundle: true,
        cost_params: cfg.profit.cost_params(),
        guards: cfg.profit.guards().unwrap(),
        protect_min_out: false,
        prefer_single_tx: true,
        max_quote_age_ms: 10_000,
        simulate_unprofitable: true,
        confirm_timeout: Duration::from_secs(1),
        dont_front: false,
        paper_equity_lamports: 1_000_000_000,
        live: LiveParams {
            min_wallet_lamports: 0,
            blockhash_margin: 10,
            poll: Duration::from_millis(10),
            timeout: Duration::from_millis(50),
        },
    };
    Arc::new(Pipeline::new(
        pcfg,
        cfg.tokens(),
        jup,
        rpc,
        risk,
        Arc::new(ChainState::default()),
        bus,
        view,
        TipPolicy::new(cfg.jito.tip_policy.clone()),
        None,
        tel.clone(),
        Arc::new(searcher_execution::Probe::new(
            tel.latency.clone(),
            ["Whirlpool".to_string()],
            (Address::default(), Address::default()),
        )),
    ))
}

fn plan() -> searcher_strategy::CandidatePlan {
    let t = searcher_core::token::TokenRegistry::defaults();
    let mut s = CrossDex::new(
        t.get("SOL").unwrap().clone(),
        t.get("USDC").unwrap().clone(),
        1_000_000_000,
        1,
        vec!["Raydium CLMM".into(), "Whirlpool".into()],
        FastPairs(vec![]),
        Some(30),
    );
    s.next_plan().unwrap()
}

fn events(rx: &std::sync::mpsc::Receiver<Event>) -> Vec<Event> {
    rx.try_iter().collect()
}

#[tokio::test]
async fn profitable_cycle_is_simulated_risk_checked_and_paper_filled() {
    let mock =
        Arc::new(Mock { sim_fails: AtomicBool::new(false), sims: AtomicUsize::new(0), builds: AtomicUsize::new(0) });
    let url = serve(mock.clone()).await;
    let (tx, rx) = std::sync::mpsc::sync_channel(10_000);
    let bus = Arc::new(EventBus::new(None, Some(tx)));
    let p = pipeline(&url, bus);
    let job = p.evaluate(plan()).await.expect("assembled job");
    assert_eq!(job.opp.status, OppStatus::Quoted, "{:?}", job.opp.status);
    assert_eq!(job.plan, PlanKind::SingleTx);
    assert_eq!(mock.builds.load(Ordering::SeqCst), 2, "one /build per leg");
    p.simulate_job(job).await;
    assert_eq!(mock.sims.load(Ordering::SeqCst), 1);
    let ev = events(&rx);
    let last_opp =
        ev.iter().filter_map(|e| if let Event::Opportunity(o) = e { Some(o) } else { None }).next_back().unwrap();
    assert_eq!(last_opp.status, OppStatus::PaperFilled);
    assert_eq!(last_opp.costs.compute_units_used, Some(154_250), "fees recomputed from simulated CU");
    assert_eq!(last_opp.costs.compute_units_limit, 185_100, "limit = used × 1.2");
    assert!(ev.iter().any(|e| matches!(e, Event::Risk(r) if r.approved)));
    assert!(ev.iter().any(|e| matches!(e, Event::Trade(t) if t.paper && t.net > 0)));
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Execution(x) if x.state == ExecState::PaperFilled && x.signatures.is_empty()))
    );
    assert!(ev.iter().any(|e| matches!(e, Event::RawQuote { .. })), "provider snapshot recorded");
}

#[tokio::test]
async fn simulation_failure_is_never_executed() {
    let mock =
        Arc::new(Mock { sim_fails: AtomicBool::new(true), sims: AtomicUsize::new(0), builds: AtomicUsize::new(0) });
    let url = serve(mock.clone()).await;
    let (tx, rx) = std::sync::mpsc::sync_channel(10_000);
    let bus = Arc::new(EventBus::new(None, Some(tx)));
    let p = pipeline(&url, bus);
    let job = p.evaluate(plan()).await.expect("assembled job");
    assert_eq!(job.opp.status, OppStatus::Quoted);
    p.simulate_job(job).await;
    let ev = events(&rx);
    let sim = ev.iter().find_map(|e| if let Event::Simulation(s) = e { Some(s) } else { None }).unwrap();
    assert!(!sim.ok);
    assert_eq!(sim.failure.as_ref().unwrap().class, SimFailureClass::SlippageExceeded);
    let last =
        ev.iter().filter_map(|e| if let Event::Opportunity(o) = e { Some(o) } else { None }).next_back().unwrap();
    assert_eq!(last.status, OppStatus::Skipped(SkipReason::Slippage));
    assert!(
        !ev.iter().any(|e| matches!(e, Event::Execution(_) | Event::Trade(_) | Event::Risk(_))),
        "no risk pass, no execution"
    );
}

#[tokio::test]
async fn stale_quotes_are_dropped_before_simulation() {
    let mock =
        Arc::new(Mock { sim_fails: AtomicBool::new(false), sims: AtomicUsize::new(0), builds: AtomicUsize::new(0) });
    let url = serve(mock.clone()).await;
    let (tx, rx) = std::sync::mpsc::sync_channel(10_000);
    let p = pipeline(&url, Arc::new(EventBus::new(None, Some(tx))));
    let mut job = p.evaluate(plan()).await.unwrap();
    for l in job.opp.route.legs.iter_mut() {
        l.quoted_at = Ts(Ts::now().0 - 60_000_000);
    }
    p.simulate_job(job).await;
    assert_eq!(mock.sims.load(Ordering::SeqCst), 0, "stale quote must not be simulated");
    let last = events(&rx)
        .into_iter()
        .filter_map(|e| if let Event::Opportunity(o) = e { Some(o) } else { None })
        .next_back()
        .unwrap();
    assert_eq!(last.status, OppStatus::Skipped(SkipReason::StaleQuote));
}
