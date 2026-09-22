//! SQLite persistence, replay round-trip and report statistics.

use searcher_core::costs::CostBreakdown;
use searcher_core::event::{SessionInfo, Stage, StageEvent};
use searcher_core::metrics::MetricId;
use searcher_core::model::*;
use searcher_core::profit::ProfitEval;
use searcher_core::{Address, Event, Ppm, Ts, UsdPrice};
use searcher_storage::store::BLOCK_EVENTS;
use searcher_storage::{Retention, Store, build_report, render_report, spawn_recorder_with};
use searcher_telemetry::Telemetry;
use std::sync::Arc;

fn session(id: &str) -> SessionInfo {
    SessionInfo {
        session_id: id.into(),
        started_at: Ts(1_000_000),
        mode: Mode::Paper,
        version: "test".into(),
        config_summary: "mode=PAPER".into(),
        taker: None,
        paper_equity_lamports: Some(1_000_000_000),
        limits: vec![],
    }
}

fn opp(id: u64, key: &str, strategy: StrategyKind, ts: i64, gross: i64, status: OppStatus) -> Opportunity {
    let input = 1_000_000_000u64;
    let costs = CostBreakdown { base_fee: 5_000, priority_fee: 300, jito_tip: 1_126, ..Default::default() };
    let net = gross - costs.total() as i64;
    Opportunity {
        id: OpportunityId(id),
        key: key.into(),
        strategy,
        label: key.into(),
        detected_at: Ts(ts),
        slot: Some(1),
        base_mint: Address([1; 32]),
        input,
        gross_output: (input as i64 + gross) as u64,
        route: Route::default(),
        costs,
        eval: ProfitEval {
            gross_pnl: gross,
            expected_net: net,
            gross_edge: Ppm::ratio(gross as i128, input as i128).unwrap(),
            net_edge: Ppm::ratio(net as i128, input as i128).unwrap(),
            ..Default::default()
        },
        status,
        updated_at: Ts(ts),
        sol_price: Some(UsdPrice::new(105_000_000)),
        simulation: None,
        risk: None,
        guard: None,
    }
}

fn sim(id: u64, ts: i64, ok: bool) -> SimulationResult {
    SimulationResult {
        opportunity: OpportunityId(id),
        plan: PlanKind::SingleTx,
        fidelity: SimFidelity::Exact,
        ok,
        failure: (!ok).then(|| SimFailure {
            class: SimFailureClass::SlippageExceeded,
            tx_index: 0,
            message: "0x1771".into(),
        }),
        txs: vec![],
        latency_ms: 300,
        simulated_at: Ts(ts),
        context_slot: Some(2),
    }
}

fn events() -> Vec<Event> {
    let mut v = vec![Event::Session(session("S1"))];
    // same cross-dex key: positive, positive, negative → one episode
    v.push(Event::Opportunity(Box::new(opp(
        1,
        "xd:A>B",
        StrategyKind::CrossDex,
        2_000_000,
        40_000,
        OppStatus::Quoted,
    ))));
    v.push(Event::Opportunity(Box::new(opp(
        2,
        "xd:A>B",
        StrategyKind::CrossDex,
        3_000_000,
        20_000,
        OppStatus::Quoted,
    ))));
    v.push(Event::Opportunity(Box::new(opp(
        3,
        "xd:A>B",
        StrategyKind::CrossDex,
        5_000_000,
        -9_000,
        OppStatus::Skipped(SkipReason::EdgeTooSmall),
    ))));
    v.push(Event::Opportunity(Box::new(opp(
        4,
        "rt",
        StrategyKind::RoundTrip,
        6_000_000,
        -250_000,
        OppStatus::Skipped(SkipReason::EdgeTooSmall),
    ))));
    v.push(Event::Opportunity(Box::new(opp(
        5,
        "tri",
        StrategyKind::Triangular,
        7_000_000,
        0,
        OppStatus::Skipped(SkipReason::NoRoute),
    ))));
    // opp 1 was executed on paper
    v.push(Event::Simulation(Box::new(sim(1, 2_100_000, true))));
    v.push(Event::Simulation(Box::new(sim(4, 6_100_000, false))));
    v.push(Event::Opportunity(Box::new(opp(
        1,
        "xd:A>B",
        StrategyKind::CrossDex,
        2_000_000,
        40_000,
        OppStatus::PaperFilled,
    ))));
    v.push(Event::Trade(TradeResult {
        opportunity: OpportunityId(1),
        strategy: StrategyKind::CrossDex,
        label: "A → B".into(),
        mode: Mode::Paper,
        paper: true,
        entry_ts: Ts(2_000_000),
        exit_ts: Ts(2_200_000),
        input: 1_000_000_000,
        output: 1_000_040_000,
        fees_lamports: 5_300,
        tip_lamports: 1_126,
        expected_net: 33_574,
        net: 33_574,
        net_usd: Some(searcher_core::UsdMicros(3_525)),
    }));
    v.push(Event::Stage(StageEvent {
        ts: Ts(2_200_000),
        opportunity: OpportunityId(1),
        stage: Stage::Paper,
        ok: true,
        subject: "simulated fill".into(),
        value: "+$0.0035".into(),
        detail: String::new(),
    }));
    v.push(Event::Metric { ts: Ts(2_200_000), metric: MetricId::Pnl, value: 0.0035 });
    v.push(Event::Metric { ts: Ts(2_200_000), metric: MetricId::JupiterLatency, value: 812.0 });
    v.push(Event::RateLimited { ts: Ts(4_000_000), service: ServiceId::Jupiter, backoff_ms: 900, attempt: 1 });
    v.push(Event::RawQuote {
        ts: Ts(2_000_000),
        opportunity: OpportunityId(1),
        leg: 0,
        body: "{\"inAmount\":\"1\"}".into(),
    });
    v
}

#[test]
fn batch_write_list_and_replay_roundtrip() {
    let mut s = Store::open_in_memory().unwrap();
    s.begin_session(&session("S1")).unwrap();
    let evs = events();
    let batch: Vec<(u64, Event)> = evs.iter().cloned().enumerate().map(|(i, e)| (i as u64 + 1, e)).collect();
    s.write_batch("S1", &batch[..5]).unwrap();
    s.write_batch("S1", &batch[5..]).unwrap();
    s.end_session("S1", Ts(9_000_000), 0).unwrap();

    let sessions = s.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].events, evs.len() as i64);
    assert_eq!(sessions[0].opportunities, 5, "upsert keeps one row per opportunity");

    let replayed = s.load_events("S1").unwrap();
    assert_eq!(replayed, evs, "replay stream is identical to what was emitted");
    assert!(s.load_events("nope").is_err());

    let status: String = s
        .conn()
        .query_row("SELECT status FROM opportunities WHERE session_id='S1' AND id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "paper_filled", "latest state wins");
    let raw: String = s.conn().query_row("SELECT raw FROM quotes", [], |r| r.get(0)).unwrap();
    assert!(raw.contains("inAmount"));
    let lat: i64 = s.conn().query_row("SELECT COUNT(*) FROM latency", [], |r| r.get(0)).unwrap();
    assert_eq!(lat, 1);
}

#[test]
fn report_counts_every_evaluation_and_lifetimes() {
    let mut s = Store::open_in_memory().unwrap();
    s.begin_session(&session("S1")).unwrap();
    let batch: Vec<(u64, Event)> = events().into_iter().enumerate().map(|(i, e)| (i as u64 + 1, e)).collect();
    s.write_batch("S1", &batch).unwrap();
    let r = build_report(&s, "S1").unwrap();
    assert_eq!(r.scanned, 5);
    assert_eq!(r.priced, 4, "NO_ROUTE is not priced");
    assert_eq!(r.positive_gross, 2);
    assert_eq!(r.positive_net, 2);
    assert_eq!(r.executable, 1);
    assert_eq!(r.paper_fills, 1);
    assert_eq!(r.paper_net_lamports, 33_574);
    assert_eq!(r.simulations, 2);
    assert_eq!(r.sim_failures, 1);
    assert_eq!(r.sim_failure_rate, Some(0.5));
    assert_eq!(r.lifetime_gross_positive.episodes, 1);
    assert_eq!(r.lifetime_gross_positive.median_lower_ms, Some(1_000));
    assert_eq!(r.lifetime_gross_positive.median_upper_ms, Some(3_000));
    assert_eq!(r.rate_limited_429, 1);
    assert!(r.skip_reasons.iter().any(|(k, n)| k == "EDGE_TOO_SMALL" && *n == 2));
    let cross = r.strategies.iter().find(|s| s.strategy == "cross-dex").unwrap();
    assert_eq!((cross.scanned, cross.positive_gross, cross.executable), (3, 2, 1));
    let text = render_report(&r);
    assert!(text.contains("net-positive"));
    assert!(text.contains("cross-dex"));
}

#[test]
fn recorder_thread_persists_and_closes_session() {
    let dir = std::env::temp_dir().join(format!("searcher-rec-{}", std::process::id()));
    let path = dir.join("t.sqlite");
    let _ = std::fs::remove_file(&path);
    let (tx, rx) = std::sync::mpsc::sync_channel(1024);
    // fixture times are in 1970: keep them away from the janitor's cutoff
    let keep = Retention { keep_days: 100_000, ..Default::default() };
    let h =
        spawn_recorder_with(path.clone(), session("S2"), rx, Arc::new(Telemetry::new()), Arc::new(|| 3), keep).unwrap();
    for e in events().into_iter().skip(1) {
        tx.send(e).unwrap();
    }
    drop(tx);
    let stats = h.join().unwrap();
    assert_eq!(stats.written as usize, events().len() - 1);
    let s = Store::open(&path).unwrap();
    let row = s.list_sessions().unwrap().into_iter().find(|r| r.id == "S2").unwrap();
    assert!(row.ended_at.is_some());
    assert_eq!(row.dropped, 3);
    assert_eq!(s.load_events("S2").unwrap().len(), events().len() - 1);
    std::fs::remove_dir_all(&dir).ok();
}

fn batch(evs: Vec<Event>, first_seq: u64) -> Vec<(u64, Event)> {
    evs.into_iter().enumerate().map(|(i, e)| (first_seq + i as u64, e)).collect()
}

fn metric(ts: i64, m: MetricId, v: f64) -> Event {
    Event::Metric { ts: Ts(ts), metric: m, value: v }
}

#[test]
fn log_is_compacted_into_blocks_and_replays_identically() {
    let mut s = Store::open_in_memory().unwrap();
    s.begin_session(&session("S1")).unwrap();
    // 2.5 blocks of metrics one second apart (none thinned) plus the fixture
    let mut evs: Vec<Event> = (0..(BLOCK_EVENTS as i64 * 5 / 2))
        .map(|i| metric(10_000_000 + i * 1_000_000, MetricId::Price, i as f64))
        .collect();
    evs.extend(events());
    s.write_batch("S1", &batch(evs.clone(), 1)).unwrap();
    let now = Ts(10_000_000);
    assert_eq!(s.compact("S1", now, false).unwrap(), 2 * BLOCK_EVENTS, "full blocks only");
    let tail: i64 = s.conn().query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
    assert_eq!(tail as usize, evs.len() - 2 * BLOCK_EVENTS);
    assert_eq!(s.load_events("S1").unwrap(), evs, "blocks + tail replay in order");
    s.end_session("S1", Ts(99_000_000), 0).unwrap();
    let (blocks, raw, packed): (i64, i64, i64) = s
        .conn()
        .query_row("SELECT COUNT(*), SUM(raw_bytes), SUM(length(data)) FROM event_blocks", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(blocks, 3, "the rest compacted at session end");
    assert!(packed * 5 < raw, "compressed {packed} of {raw} bytes");
    assert_eq!(s.load_events("S1").unwrap(), evs);
    assert_eq!(s.list_sessions().unwrap()[0].events, evs.len() as i64);
    let r = build_report(&s, "S1").unwrap();
    assert_eq!(r.rate_limited_429, 1, "counted from event_counts, not the compressed log");
}

#[test]
fn high_rate_metrics_and_health_are_thinned_pnl_is_not() {
    let mut s = Store::open_in_memory().unwrap();
    s.begin_session(&session("S1")).unwrap();
    let health = |ts: i64, state: ServiceState| Event::Health {
        ts: Ts(ts),
        service: ServiceId::Jupiter,
        snapshot: ServiceSnapshot { state, ..Default::default() },
    };
    let mut evs: Vec<Event> = (0..10).map(|i| metric(1_000_000 + i * 50_000, MetricId::NetEdge, i as f64)).collect();
    evs.extend((0..10).map(|i| metric(1_000_000 + i * 50_000, MetricId::Pnl, i as f64)));
    evs.push(metric(2_100_000, MetricId::NetEdge, 99.0));
    evs.extend([
        health(1_000_000, ServiceState::Ok),
        health(2_000_000, ServiceState::Ok),
        health(3_000_000, ServiceState::Degraded),
    ]);
    evs.push(health(9_000_000, ServiceState::Degraded));
    s.write_batch("S1", &batch(evs, 1)).unwrap();
    let got = s.load_events("S1").unwrap();
    let net: Vec<f64> = got
        .iter()
        .filter_map(|e| match e {
            Event::Metric { metric: MetricId::NetEdge, value, .. } => Some(*value),
            _ => None,
        })
        .collect();
    assert_eq!(net, vec![0.0, 99.0], "one per second");
    assert_eq!(got.iter().filter(|e| matches!(e, Event::Metric { metric: MetricId::Pnl, .. })).count(), 10, "PnL kept");
    let hs: Vec<i64> = got
        .iter()
        .filter_map(|e| match e {
            Event::Health { ts, .. } => Some(ts.0),
            _ => None,
        })
        .collect();
    assert_eq!(hs, vec![1_000_000, 3_000_000, 9_000_000], "state changes and 5 s heartbeats");
}

#[test]
fn full_snapshots_and_raw_quotes_only_for_notable_opportunities() {
    let mut s = Store::open_in_memory().unwrap();
    s.begin_session(&session("S1")).unwrap();
    let quote = |id: u64| Event::RawQuote {
        ts: Ts(1_000_000),
        opportunity: OpportunityId(id),
        leg: 0,
        body: format!("{{\"id\":{id}}}"),
    };
    let evs = vec![
        quote(7), // arrives before its (plain) opportunity
        Event::Opportunity(Box::new(opp(
            7,
            "k7",
            StrategyKind::CrossDex,
            1_000_000,
            -5_000,
            OppStatus::Skipped(SkipReason::EdgeTooSmall),
        ))),
        quote(8), // before a gross-positive one
        Event::Opportunity(Box::new(opp(
            8,
            "k8",
            StrategyKind::CrossDex,
            1_000_000,
            7_000,
            OppStatus::Skipped(SkipReason::EdgeTooSmall),
        ))),
        quote(9), // after an executable one
        Event::Opportunity(Box::new(opp(9, "k9", StrategyKind::CrossDex, 1_000_000, -1, OppStatus::Executable))),
        quote(9),
    ];
    s.write_batch("S1", &batch(evs.clone(), 1)).unwrap();
    let snap = |id: i64| -> String {
        s.conn().query_row("SELECT snapshot FROM opportunities WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    };
    assert_eq!(snap(7), "", "plain skipped: numeric row only");
    assert!(snap(8).contains("\"k8\"") && snap(9).contains("\"k9\""));
    let ids: Vec<i64> = {
        let mut st = s.conn().prepare("SELECT opportunity_id FROM quotes ORDER BY 1").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(ids, vec![8, 9], "raw quotes of notable opportunities");
    assert_eq!(s.load_events("S1").unwrap(), evs, "the log keeps everything");
}

#[test]
fn retention_by_age_protects_trading_and_running_sessions_and_caps_size() {
    let day = 86_400_000_000i64;
    let now = Ts(100 * day);
    let mut s = Store::open_in_memory().unwrap();
    let mut seq = 1;
    let mut add = |s: &mut Store, id: &str, at: i64, trade: bool| {
        let mut info = session(id);
        info.started_at = Ts(at);
        s.begin_session(&info).unwrap();
        let mut evs: Vec<Event> = (0..50).map(|i| metric(at + i * 1_000_000, MetricId::Price, i as f64)).collect();
        if trade {
            let mut t = match events().into_iter().find(|e| matches!(e, Event::Trade(_))) {
                Some(Event::Trade(t)) => t,
                _ => unreachable!(),
            };
            t.exit_ts = Ts(at);
            evs.push(Event::Trade(t));
        }
        s.write_batch(id, &batch(evs, seq)).unwrap();
        seq += 100;
        s.end_session(id, Ts(at + 60_000_000), 0).unwrap();
    };
    add(&mut s, "old", 80 * day, false);
    add(&mut s, "old-trade", 80 * day, true);
    add(&mut s, "ancient-trade", 5 * day, true);
    add(&mut s, "recent", 98 * day, false);
    add(&mut s, "running", 50 * day, false); // the one being recorded
    // open, written to a minute ago by another instance
    let mut other = session("other-instance");
    other.started_at = Ts(now.0 - 600_000_000);
    s.begin_session(&other).unwrap();
    s.write_batch("other-instance", &batch(vec![metric(now.0 - 60_000_000, MetricId::Price, 1.0)], 1)).unwrap();
    let rep = s.prune(&Retention::default(), now, Some("running")).unwrap();
    let mut gone = rep.sessions_deleted.clone();
    gone.sort();
    assert_eq!(gone, vec!["ancient-trade", "old"]);
    let left: Vec<String> = s.list_sessions().unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(left.len(), 4);
    for t in ["events", "event_blocks", "event_counts", "trades"] {
        let n: i64 = s
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {t} WHERE session_id IN ('old','ancient-trade')"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "{t} rows of deleted sessions");
    }
    // the running session is older than a week: its old detail goes, the session stays
    assert!(rep.detail_rows_deleted > 0);
    assert!(s.load_events("running").unwrap().is_empty());
    // a tiny size cap removes unprotected, non-running sessions only
    let rep = s.prune(&Retention { max_db_mb: 0, ..Default::default() }, now, Some("running")).unwrap();
    assert_eq!(rep.sessions_deleted, vec!["recent"]);
    let left: Vec<String> = s.list_sessions().unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(["old-trade", "running", "other-instance"].iter().all(|k| left.contains(&k.to_string())));
}

#[test]
fn a_file_database_shrinks_after_prune() {
    let dir = std::env::temp_dir().join(format!("searcher-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("p.sqlite");
    let mut s = Store::open(&path).unwrap();
    for k in 0..3 {
        let id = format!("S{k}");
        let mut info = session(&id);
        info.started_at = Ts(k * 1_000_000);
        s.begin_session(&info).unwrap();
        // incompressible payloads so the file really grows
        let evs: Vec<Event> = (0..4000)
            .map(|i| Event::Log {
                ts: Ts(i * 1_000),
                level: searcher_core::event::LogLevel::Info,
                message: format!("{:x}", (i as u64 * 2654435761 + k as u64).wrapping_mul(0x9E3779B97F4A7C15)),
            })
            .collect();
        s.write_batch(&id, &batch(evs, 1)).unwrap();
        s.end_session(&id, Ts(10_000_000), 0).unwrap();
    }
    s.release().unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let info = s.db_info(&path).unwrap();
    assert!(info.incremental_vacuum && info.sessions.len() == 3);
    let rep = s.prune(&Retention { max_db_mb: 0, ..Default::default() }, Ts(1), Some("S2")).unwrap();
    assert_eq!(rep.sessions_deleted, vec!["S0", "S1"]);
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(after < before, "file {before} → {after} bytes");
    assert!(rep.bytes_after < rep.bytes_before);
    drop(s);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn logs_written_before_compaction_existed_still_replay() {
    let s = Store::open_in_memory().unwrap();
    s.conn()
        .execute(
            "INSERT INTO sessions(id, started_at, mode, version, config_summary) VALUES ('L','1','PAPER','old','')",
            [],
        )
        .unwrap();
    let evs = events();
    for (i, e) in evs.iter().enumerate() {
        s.conn()
            .execute(
                "INSERT INTO events VALUES ('L', ?1, ?2, ?3, ?4)",
                rusqlite::params![i as i64 + 1, e.ts().0, e.kind(), serde_json::to_string(e).unwrap()],
            )
            .unwrap();
    }
    assert_eq!(s.load_events("L").unwrap(), evs);
    assert_eq!(s.list_sessions().unwrap()[0].events, evs.len() as i64, "counted row by row");
    assert_eq!(s.kind_count("L", "rate_limited").unwrap(), 1);
}

#[test]
fn attribution_records_the_failed_guard_executed_outputs_and_created_accounts() {
    use searcher_core::profit::GuardFailure;
    let leg = |out: u64| Leg {
        index: 0,
        input_mint: Address([1; 32]),
        output_mint: Address([2; 32]),
        in_amount: 1,
        out_amount: out,
        min_out: out,
        slippage_bps: 0,
        slippage_spec: SlippageSpec::Fixed(0),
        price_impact: Ppm::ZERO,
        hops: vec![],
        mode: RoutingMode::Normal,
        dex_filter: DexFilter::Any,
        quoted_at: Ts(1),
        latency_ms: 1,
        cu_price_micro: None,
        last_valid_block_height: 1,
        request_id: None,
    };
    let tx = |ok: bool, outs: Vec<u64>, created: Vec<(Address, u64)>| TxSim {
        index: 0,
        ok,
        units_consumed: 200_000,
        cu_limit: 240_000,
        cu_price_micro: 1_000,
        fee: Some(5_000),
        size_bytes: 900,
        accounts: 30,
        logs: vec![],
        err: None,
        taker_lamports: None,
        leg_outputs: outs,
        created,
    };
    // #1: first leg short of its quote → the second leg failed (Jupiter 6024)
    let mut a = opp(1, "xd:A>B", StrategyKind::CrossDex, 2_000_000, -20_000, OppStatus::Skipped(SkipReason::SimFailed));
    a.route = Route { legs: vec![leg(11_568_000), leg(99_970_000)] };
    a.simulation =
        Some(SimulationResult { txs: vec![tx(false, vec![11_567_999], vec![])], ..sim(1, 2_100_000, false) });
    // #2: both legs delivered; the route created a 2,440-byte account
    let created = Address([9; 32]);
    let mut b =
        opp(2, "xd:A>C", StrategyKind::CrossDex, 3_000_000, -23_000, OppStatus::Skipped(SkipReason::EdgeTooSmall));
    b.route = Route { legs: vec![leg(10_796_000), leg(100_003_099)] };
    b.simulation = Some(SimulationResult {
        txs: vec![tx(true, vec![10_796_647, 99_988_662], vec![(created, 13_045_440)])],
        ..sim(2, 3_100_000, true)
    });
    b.guard = Some(GuardFailure::Lamports { net: -13_156_025, min: 10_000 });
    // #3: priced only, the USD guard failed
    let mut c = opp(3, "rt", StrategyKind::RoundTrip, 4_000_000, -9_000, OppStatus::Skipped(SkipReason::EdgeTooSmall));
    c.guard = Some(GuardFailure::Usd {
        net_usd: searcher_core::units::UsdMicros(-900),
        min: searcher_core::units::UsdMicros(2_000),
    });

    let mut st = Store::open_in_memory().unwrap();
    st.begin_session(&session("S1")).unwrap();
    let evs = vec![
        Event::Simulation(Box::new(a.simulation.clone().unwrap())),
        Event::Opportunity(Box::new(a)),
        Event::Simulation(Box::new(b.simulation.clone().unwrap())),
        Event::Opportunity(Box::new(b)),
        Event::Opportunity(Box::new(c)),
    ];
    st.write_batch("S1", &batch(evs, 1)).unwrap();

    let r = build_report(&st, "S1").unwrap();
    assert_eq!(r.guards, vec![("lamports".to_string(), 1), ("usd".to_string(), 1)]);
    assert_eq!((r.first_leg_checked, r.first_leg_short, r.first_leg_short_failed), (2, 1, 1));
    assert_eq!(r.created_accounts, vec![(created.to_string(), 1, 13_045_440)]);
    let text = render_report(&r);
    assert!(text.contains("PROFIT GUARD THAT FAILED") && text.contains("ACCOUNTS THE TRANSACTIONS CREATE"), "{text}");
}

#[test]
fn ledger_splits_wallet_value_into_trades_deposits_and_revaluation() {
    let mut st = Store::open_in_memory().unwrap();
    st.begin_session(&session("L1")).unwrap();
    let inv = |ts: i64, sol: u64, usdc: u64, px: u64| Event::Inventory {
        ts: Ts(ts),
        sol_lamports: sol,
        usdc_atoms: Some(usdc),
        sol_usd_micros: Some(px),
    };
    // 0.12 SOL + $5 at $100 → 0.12 SOL + $5 at $110, nothing traded
    st.write_batch(
        "L1",
        &batch(vec![inv(1, 120_000_000, 5_000_000, 100_000_000), inv(2, 120_000_000, 5_000_000, 110_000_000)], 1),
    )
    .unwrap();
    let l = build_report(&st, "L1").unwrap().ledger.unwrap();
    assert!((l.start_usd - 17.0).abs() < 1e-9 && (l.end_usd - 18.2).abs() < 1e-9);
    assert!((l.revaluation_usd - 1.2).abs() < 1e-9, "all of the change is the SOL price");
    assert!(l.unexplained_usd.abs() < 1e-9);
    assert_eq!(l.trades_usd, 0.0);
    assert!(render_report(&build_report(&st, "L1").unwrap()).contains("WALLET LEDGER"));
}
