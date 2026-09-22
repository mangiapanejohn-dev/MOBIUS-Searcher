//! DEX lag: on-chain pool mids (the `[feeds]` pools, pushed over the chain
//! WebSocket) against the CEX price (OKX and Binance best bid/ask streams).
//!
//! * Every 5 s each pool's gap to the CEX mid is recorded, whatever its size
//!   (the unconditional distribution).
//! * A gap wider than `lag_trigger_bps` opens an episode; one Jupiter quote
//!   on that DEX (high priority) measures what is executable, and the CEX and
//!   pool mids are recorded afterwards (markouts: who moved, DEX or CEX).
//! * Every `lag_control_every_s` the same quote is taken without a trigger:
//!   the control sample tells whether the trigger finds anything chance does not.
//!
//! Pool mids are not executable and the public RPC pushes pool changes only
//! every few seconds; both limits are recorded with each sample (pool age).

use super::{Ctx, Priority};
use anyhow::Result;
use parking_lot::Mutex;
use searcher_core::config::FeedsConfig;
use searcher_core::event::LogLevel;
use searcher_core::model::DexFilter;
use searcher_core::{Event, Ts};
use searcher_market::hot::HotTick;
use searcher_market::{ChainState, RpcClient, feed};
use searcher_storage::research::{Episode, LagTick};
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// A CEX quote older than this is not used.
const CEX_MAX_AGE: Duration = Duration::from_secs(3);
/// An episode ends when the gap falls below half the trigger, flips sign, or
/// after this long.
const EPISODE_MAX: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug)]
pub struct Top {
    pub bid: f64,
    pub ask: f64,
    pub at: Instant,
}

impl Top {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }
}

/// Best bid/ask per exchange.
#[derive(Default)]
pub struct Cex {
    pub okx: Option<Top>,
    pub binance: Option<Top>,
}

/// The fair price used: the freshest quote under [`CEX_MAX_AGE`] (Binance on
/// a tie: its SOL/USDC book is the deeper one).
#[derive(Clone, Copy, Debug)]
pub struct Fair {
    pub top: Top,
    pub src: &'static str,
}

impl Cex {
    pub fn fair(&self, now: Instant) -> Option<Fair> {
        let fresh = |t: Option<Top>, src| {
            t.filter(|t| now.saturating_duration_since(t.at) < CEX_MAX_AGE).map(|top| Fair { top, src })
        };
        match (fresh(self.binance, "binance"), fresh(self.okx, "okx")) {
            (Some(b), Some(o)) => Some(if o.top.at > b.top.at { o } else { b }),
            (b, o) => b.or(o),
        }
    }
}

/// OKX `bbo-tbt` push: `{"arg":…,"data":[{"asks":[["px","sz",…]],"bids":[…],"ts":"…"}]}`.
pub fn parse_okx(text: &str) -> Option<(f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let d = v.get("data")?.get(0)?;
    let px = |side: &str| d.get(side)?.get(0)?.get(0)?.as_str()?.parse::<f64>().ok();
    Some((px("bids")?, px("asks")?)).filter(|(b, a)| *b > 0.0 && a >= b)
}

/// Binance `bookTicker`: `{"u":…,"s":"SOLUSDC","b":"px","B":"qty","a":"px","A":"qty"}`.
pub fn parse_binance(text: &str) -> Option<(f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let px = |k: &str| v.get(k)?.as_str()?.parse::<f64>().ok();
    Some((px("b")?, px("a")?)).filter(|(b, a)| *b > 0.0 && a >= b)
}

/// Signed gap of the pool over the CEX mid, in bp.
pub fn gap_bps(pool_mid: f64, cex_mid: f64) -> f64 {
    (pool_mid - cex_mid) / cex_mid * 1e4
}

/// How much better than fair an executed price is (bp, positive = better):
/// buying below or selling above the reference.
pub fn edge_bps(buy: bool, exec_px: f64, reference: f64) -> f64 {
    if buy { (reference - exec_px) / reference * 1e4 } else { (exec_px - reference) / reference * 1e4 }
}

struct Pool {
    mid: f64,
    at: Instant,
}

struct Open {
    ep: Episode,
    started: Instant,
    closed: bool,
    confirmed: bool,
    /// (seconds after start, due) not yet recorded
    markouts_due: Vec<(u32, Instant)>,
    markouts: Vec<serde_json::Value>,
}

struct Confirmed {
    id: i64,
    ts: i64,
    ms: i64,
    result: Result<(f64, String), String>,
}

pub fn spawn(
    ctx: Arc<Ctx>,
    telemetry: Arc<Telemetry>,
    shutdown: watch::Receiver<bool>,
) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let cfg = &ctx.cfg;
    let r = &cfg.research;
    let cex = Arc::new(Mutex::new(Cex::default()));
    let mut tasks = Vec::new();

    // CEX streams
    {
        let cex = cex.clone();
        let sub = serde_json::json!({"op": "subscribe", "args": [{"channel": "bbo-tbt", "instId": r.lag_okx_inst}]});
        tasks.push(tokio::spawn(feed::run_subscribed_stream(
            r.lag_okx_ws_url.clone(),
            vec![sub.to_string()],
            Duration::from_secs(30),
            move |t| {
                if let Some((bid, ask)) = parse_okx(t) {
                    cex.lock().okx = Some(Top { bid, ask, at: Instant::now() });
                }
            },
            shutdown.clone(),
        )));
    }
    {
        let cex = cex.clone();
        tasks.push(tokio::spawn(feed::run_text_stream(
            r.lag_binance_ws_url.clone(),
            Duration::from_secs(30),
            move |t| {
                if let Some((bid, ask)) = parse_binance(t) {
                    cex.lock().binance = Some(Top { bid, ask, at: Instant::now() });
                }
            },
            shutdown.clone(),
        )));
    }

    // Chain feed: the configured pools only (no oracles needed here)
    let feeds = FeedsConfig { oracles: Vec::new(), ..cfg.feeds.clone() };
    let watches = feed::watches_from_config(&feeds, &cfg.tokens()).map_err(anyhow::Error::msg)?;
    let rpc = Arc::new(RpcClient::new(
        &cfg.rpc.resolved_url(),
        LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
        cfg.rpc.simulate_rps,
        Duration::from_millis(cfg.rpc.timeout_ms),
        telemetry.clone(),
    )?);
    let (tick_tx, tick_rx) = mpsc::channel::<(String, f64, Instant)>(1_024);
    let hot: searcher_market::hot::HotSink = Arc::new(move |t| {
        if let HotTick::Pool { dex, mid, received, .. } = t {
            let _ = tick_tx.try_send((dex.to_string(), mid, received));
        }
    });
    let emit: feed::Emit = Arc::new(|e| match e {
        Event::Error { service, message, .. } => eprintln!("research lag: {service}: {message}"),
        Event::Log { level: LogLevel::Warn | LogLevel::Error, message, .. } => eprintln!("research lag: {message}"),
        _ => {}
    });
    tasks.push(tokio::spawn(feed::run_chain_ws(
        cfg.rpc.resolved_ws_url(),
        feed::AccountFeed::new(watches, Duration::from_millis(cfg.feeds.emit_interval_ms)),
        rpc,
        Arc::new(ChainState::default()),
        telemetry,
        emit,
        Some(hot),
        shutdown.clone(),
    )));

    tasks.push(tokio::spawn(evaluate(ctx, cex, tick_rx, shutdown)));
    Ok(tasks)
}

async fn evaluate(
    ctx: Arc<Ctx>,
    cex: Arc<Mutex<Cex>>,
    mut ticks: mpsc::Receiver<(String, f64, Instant)>,
    mut shutdown: watch::Receiver<bool>,
) {
    let r = ctx.cfg.research.clone();
    let mut pools: BTreeMap<String, Pool> = BTreeMap::new();
    let mut open: BTreeMap<i64, Open> = BTreeMap::new();
    let mut last_confirm: BTreeMap<String, Instant> = BTreeMap::new();
    let (confirm_tx, mut confirm_rx) = mpsc::channel::<Confirmed>(64);
    let mut next_id: i64 = 0;
    let mut control_turn = 0usize;
    let mut every_1s = tokio::time::interval(Duration::from_secs(1));
    let mut every_5s = tokio::time::interval(Duration::from_secs(5));
    let control_every = Duration::from_secs(r.lag_control_every_s.max(1));
    let mut next_control = Instant::now() + control_every;

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some((dex, mid, at)) = ticks.recv() => { pools.insert(dex, Pool { mid, at }); }
            Some(c) = confirm_rx.recv() => {
                if let Some(o) = open.get_mut(&c.id) {
                    o.confirmed = true;
                    o.ep.confirm_ts = Some(c.ts);
                    o.ep.confirm_ms = Some(c.ms);
                    let fair = cex.lock().fair(Instant::now());
                    match c.result {
                        Ok((px, dexes)) => {
                            let buy = o.ep.side == "buy_on_dex";
                            o.ep.exec_px = Some(px);
                            o.ep.exec_dexes = Some(dexes);
                            if let Some(f) = fair {
                                o.ep.exec_gap_bps = Some(edge_bps(buy, px, f.top.mid()));
                                // unwinding on the CEX: sell at its bid after buying, buy at its ask after selling
                                o.ep.exec_gap_touch_bps = Some(edge_bps(buy, px, if buy { f.top.bid } else { f.top.ask }));
                            }
                        }
                        Err(e) => o.ep.confirm_err = Some(e),
                    }
                    ctx.record("lag episode", |s| s.upsert_episode(&ctx.run, &o.ep));
                }
            }
            _ = every_5s.tick() => {
                let now = Instant::now();
                let Some(f) = cex.lock().fair(now) else { continue };
                for (dex, p) in &pools {
                    let t = LagTick {
                        ts: Ts::now().0,
                        dex: dex.clone(),
                        pool_mid: p.mid,
                        pool_age_ms: now.saturating_duration_since(p.at).as_millis() as i64,
                        cex_mid: f.top.mid(),
                        cex_src: f.src.into(),
                        cex_age_ms: now.saturating_duration_since(f.top.at).as_millis() as i64,
                        gap_bps: gap_bps(p.mid, f.top.mid()),
                    };
                    ctx.record("lag tick", |s| s.insert_lag_tick(&ctx.run, &t));
                }
            }
            _ = every_1s.tick() => {
                let now = Instant::now();
                let Some(f) = cex.lock().fair(now) else { continue };
                ctx.sol_usd.lock().replace(f.top.mid());

                // open episodes: peak, end, markouts
                for o in open.values_mut() {
                    let pool = pools.get(&o.ep.dex);
                    let mut changed = false;
                    if let Some(p) = pool && !o.closed {
                        let g = gap_bps(p.mid, f.top.mid());
                        let same_side = (g < 0.0) == (o.ep.start_gap_bps < 0.0);
                        if same_side && g.abs() > o.ep.peak_gap_bps.abs() {
                            o.ep.peak_gap_bps = g;
                        }
                        let over = o.ep.kind == "control"
                            || !same_side
                            || g.abs() < o.ep.trigger_bps / 2.0
                            || now.duration_since(o.started) > EPISODE_MAX;
                        if over {
                            o.closed = true;
                            o.ep.end_ts = Some(Ts::now().0);
                            changed = true;
                        }
                    }
                    while let Some(&(s, due)) = o.markouts_due.first() && due <= now {
                        o.markouts_due.remove(0);
                        o.markouts.push(serde_json::json!({
                            "s": s,
                            "cex_mid": f.top.mid(),
                            "pool_mid": pool.map(|p| p.mid),
                        }));
                        o.ep.markouts = Some(serde_json::Value::Array(o.markouts.clone()).to_string());
                        changed = true;
                    }
                    if changed {
                        ctx.record("lag episode", |s| s.upsert_episode(&ctx.run, &o.ep));
                    }
                }
                open.retain(|_, o| !(o.closed && o.confirmed && o.markouts_due.is_empty()));

                // new trigger episodes
                let busy: Vec<String> = open.values().filter(|o| !o.closed).map(|o| o.ep.dex.clone()).collect();
                let mut starts: Vec<(String, &'static str)> = Vec::new();
                for (dex, p) in &pools {
                    let g = gap_bps(p.mid, f.top.mid());
                    let cooled = last_confirm.get(dex).is_none_or(|t| now.duration_since(*t).as_secs() >= r.lag_confirm_cooldown_s);
                    if g.abs() >= r.lag_trigger_bps && cooled && !busy.contains(dex) {
                        starts.push((dex.clone(), "trigger"));
                    }
                }
                // control sample: next pool in turn, whatever its gap
                if r.lag_control_every_s > 0 && now >= next_control && !pools.is_empty() {
                    next_control = now + control_every;
                    let dex = pools.keys().nth(control_turn % pools.len()).cloned().unwrap_or_default();
                    control_turn += 1;
                    if !busy.contains(&dex) && !starts.iter().any(|(d, _)| *d == dex) {
                        starts.push((dex, "control"));
                    }
                }
                for (dex, kind) in starts {
                    let p = &pools[&dex];
                    let g = gap_bps(p.mid, f.top.mid());
                    // pool below the CEX: buy SOL on the DEX; above: sell SOL on it
                    let buy = g < 0.0;
                    next_id += 1;
                    let ep = Episode {
                        id: next_id,
                        kind: kind.into(),
                        dex: dex.clone(),
                        side: if buy { "buy_on_dex" } else { "sell_on_dex" }.into(),
                        start_ts: Ts::now().0,
                        start_gap_bps: g,
                        peak_gap_bps: g,
                        trigger_bps: r.lag_trigger_bps,
                        pool_age_ms: now.saturating_duration_since(p.at).as_millis() as i64,
                        cex_mid: f.top.mid(),
                        cex_bid: Some(f.top.bid),
                        cex_ask: Some(f.top.ask),
                        cex_src: f.src.into(),
                        size: Some(r.lag_confirm_lamports),
                        ..Default::default()
                    };
                    ctx.record("lag episode", |s| s.upsert_episode(&ctx.run, &ep));
                    last_confirm.insert(dex.clone(), now);
                    // the 0.1 SOL confirmation first (comparable with every other episode), then bigger sizes
                    let wide = kind == "trigger"
                        && r.lag_scale_trigger_bps > 0.0
                        && g.abs() >= r.lag_scale_trigger_bps
                        && !r.lag_scale_lamports.is_empty();
                    tokio::spawn(confirm(ctx.clone(), next_id, dex.clone(), buy, f.top.mid(), confirm_tx.clone()));
                    if wide {
                        tokio::spawn(scale(ctx.clone(), cex.clone(), next_id, dex, buy, f.top.mid()));
                    }
                    open.insert(next_id, Open {
                        ep,
                        started: now,
                        closed: false,
                        confirmed: false,
                        markouts_due: r.lag_markouts_s.iter().map(|&s| (s, now + Duration::from_secs(s as u64))).collect(),
                        markouts: Vec::new(),
                    });
                }
            }
        }
    }
    // stopping: what is open is written as it is (no end time = cut off by the stop)
    for o in open.values() {
        ctx.record("lag episode", |s| s.upsert_episode(&ctx.run, &o.ep));
    }
}

/// One Jupiter quote restricted to `dex` for `lamports` of SOL: buy SOL with
/// USDC or sell SOL for USDC. Returns (latency ms, (USDC per SOL, DEX labels)).
async fn quote_on_dex(
    ctx: &Ctx,
    dex: &str,
    buy: bool,
    cex_mid: f64,
    lamports: u64,
) -> (i64, Result<(f64, String), String>) {
    let tokens = ctx.cfg.tokens();
    let (sol, usdc) = (tokens.sol().mint, tokens.get("USDC").map(|t| t.mint).unwrap_or_default());
    let spec = searcher_strategy::LegSpec {
        input: if buy { usdc } else { sol },
        output: if buy { sol } else { usdc },
        dex_filter: DexFilter::Only(vec![dex.to_string()]),
        mode: Default::default(),
        max_accounts: None,
    };
    // the USDC amount worth the SOL size at the CEX mid
    let amount = if buy { (lamports as f64 / 1e9 * cex_mid * 1e6).round() as u64 } else { lamports };
    let t = Instant::now();
    let r = ctx.gate.build(ctx.request(spec.input, spec.output, amount, Some(&spec)), Priority::High).await;
    let ms = t.elapsed().as_millis() as i64;
    let result = r.and_then(|b| {
        let (sol_amt, usdc_amt) = if buy { (b.leg.out_amount, amount) } else { (amount, b.leg.out_amount) };
        if sol_amt == 0 {
            return Err("zero output".into());
        }
        Ok((usdc_amt as f64 / 1e6 / (sol_amt as f64 / 1e9), b.leg.dex_labels().join("+")))
    });
    (ms, result)
}

async fn confirm(ctx: Arc<Ctx>, id: i64, dex: String, buy: bool, cex_mid: f64, tx: mpsc::Sender<Confirmed>) {
    let (ms, result) = quote_on_dex(&ctx, &dex, buy, cex_mid, ctx.cfg.research.lag_confirm_lamports).await;
    let _ = tx.send(Confirmed { id, ts: Ts::now().0, ms, result }).await;
}

/// Wide gap: the same side quoted at the bigger sizes, measured against the
/// CEX price when each answer arrives.
async fn scale(ctx: Arc<Ctx>, cex: Arc<Mutex<Cex>>, id: i64, dex: String, buy: bool, cex_mid: f64) {
    for &size in &ctx.cfg.research.lag_scale_lamports {
        let (ms, result) = quote_on_dex(&ctx, &dex, buy, cex_mid, size).await;
        let fair = cex.lock().fair(Instant::now());
        let mut row = searcher_storage::research::ScaleRow {
            run_id: ctx.run.clone(),
            episode: id,
            size,
            ts: Ts::now().0,
            confirm_ms: Some(ms),
            ..Default::default()
        };
        match result {
            Ok((px, _)) => {
                row.exec_px = Some(px);
                if let Some(f) = fair {
                    row.exec_gap_bps = Some(edge_bps(buy, px, f.top.mid()));
                    row.exec_gap_touch_bps = Some(edge_bps(buy, px, if buy { f.top.bid } else { f.top.ask }));
                }
            }
            Err(e) if e == "stopped" => return,
            Err(e) => row.err = Some(e),
        }
        ctx.record("lag scale", |s| s.insert_scale(&row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exchange_messages_recorded_2026_09_22() {
        let okx = r#"{"arg":{"channel":"bbo-tbt","instId":"SOL-USDC"},"data":[{"asks":[["117.36","62.36109","0","3"]],"bids":[["117.31","4.41916","0","1"]],"ts":"1790071390505","seqId":4153112115}]}"#;
        assert_eq!(parse_okx(okx), Some((117.31, 117.36)));
        assert_eq!(parse_okx(r#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"SOL-USDC"}}"#), None);
        let bn = r#"{"u":6857599072,"s":"SOLUSDC","b":"117.28000000","B":"11.52700000","a":"117.29000000","A":"22.18600000"}"#;
        assert_eq!(parse_binance(bn), Some((117.28, 117.29)));
    }

    #[test]
    fn gaps_and_edges_have_the_right_sign() {
        assert!((gap_bps(99.0, 100.0) + 100.0).abs() < 1e-9); // pool 1 % cheap
        // bought at 99 with fair 100: +100 bp; sold at 99: −100 bp
        assert!((edge_bps(true, 99.0, 100.0) - 100.0).abs() < 1e-9);
        assert!((edge_bps(false, 99.0, 100.0) + 100.0).abs() < 1e-9);
    }

    #[test]
    fn fair_price_prefers_fresh_quotes() {
        let now = Instant::now();
        let old = now - Duration::from_secs(10);
        let c =
            Cex { okx: Some(Top { bid: 1.0, ask: 1.2, at: now }), binance: Some(Top { bid: 2.0, ask: 2.0, at: old }) };
        let f = c.fair(now).unwrap();
        assert_eq!(f.src, "okx");
        assert!(Cex { okx: Some(Top { bid: 1.0, ask: 1.0, at: old }), binance: None }.fair(now).is_none());
    }
}
