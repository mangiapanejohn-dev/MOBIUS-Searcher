//! `--research-pools`: the round trip between each two pools, worked out
//! from the pools' own accounts read at one slot. No quote API is asked, so
//! this measures the market itself, without our latency in it: is there
//! ever a gap between two pools wider than their fees, how wide, and for how
//! long. Nothing is signed or sent, and no Jupiter budget is used.
//!
//! Each snapshot of a bin pool (Meteora DLMM) is also kept with the exchange
//! price of that moment, for [`super::maker`]: what an order resting in one
//! of its bins would have met.

use super::lag::{Cex, Top, parse_binance, parse_okx};
use super::report::Dist;
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use searcher_core::config::{Config, PoolKind};
use searcher_core::{Address, Ts};
use searcher_market::accounts::decode_pool;
use searcher_market::amm::{self, Pool};
use searcher_market::feed;
use searcher_storage::ResearchStore;
use searcher_storage::research::{PoolEdge, PoolSnap};
use searcher_telemetry::Telemetry;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The chain's clock (unix time at offset 32): Meteora's fee depends on it.
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";
/// One transaction's fixed cost: base fee of one signature plus the minimum Jito tip.
const FIXED_LAMPORTS: f64 = 6_000.0;

struct Watched {
    name: String,
    kind: PoolKind,
    address: Address,
    base: Address,
    /// Raw quote units per raw base unit → quote per base.
    unit: f64,
    /// The tick or bin arrays (and fee configuration) the last snapshot named.
    deps: Vec<Address>,
}

/// A pool ready to quote, and whether the base token is its first token.
type Ready = (Pool, bool);

/// Every ordered pair of ready pools at every size: sell the base on the
/// first, buy it back on the second.
fn round_trips(pools: &[(&str, Option<&Ready>)], sizes: &[u64], now: i64) -> Vec<(u64, String, String, f64)> {
    let mut out = Vec::new();
    for &size in sizes {
        for (sell_on, sell) in pools {
            let Some((sell_pool, sell_base_first)) = sell else { continue };
            let Some(quote) = sell_pool.swap(*sell_base_first, size, now) else { continue };
            for (buy_on, buy) in pools {
                let Some((buy_pool, buy_base_first)) = buy else { continue };
                if sell_on == buy_on {
                    continue;
                }
                if let Some(back) = buy_pool.swap(!*buy_base_first, quote, now) {
                    let gross = (back as f64 - size as f64) / size as f64 * 1e4;
                    out.push((size, sell_on.to_string(), buy_on.to_string(), gross));
                }
            }
        }
    }
    out
}

pub async fn run(cfg: Config, duration: Option<u64>) -> Result<()> {
    let tokens = cfg.tokens();
    let mut pools = Vec::new();
    for p in &cfg.feeds.pools {
        let token = |s: &str| tokens.get(s).with_context(|| format!("feeds.pools {}: unknown token {s}", p.dex));
        let (base, quote) = (token(&p.base)?, token(&p.quote)?);
        let address = p.address.parse().map_err(|e| anyhow::anyhow!("feeds.pools {}: {e}", p.dex))?;
        let unit = 10f64.powi(base.decimals as i32 - quote.decimals as i32);
        pools.push(Watched { name: p.dex.clone(), kind: p.kind, address, base: base.mint, unit, deps: Vec::new() });
    }
    if pools.len() < 2 {
        bail!("feeds.pools lists {} pool(s): a round trip needs two", pools.len());
    }
    let sizes = cfg.research.pool_sizes_lamports.clone();
    let every = Duration::from_millis(cfg.research.pool_every_ms);
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let run_id = searcher_storage::new_session_id();
    let telemetry = Arc::new(Telemetry::new());
    let rpc = crate::doctor::rpc_client(&cfg, &telemetry).map_err(anyhow::Error::msg)?;
    let clock: Address = CLOCK.parse().expect("sysvar address");
    println!(
        "pools {run_id} · {} · {} pools · sizes {} SOL · every {} ms · writing {}",
        searcher_core::config::display_url(&cfg.rpc.resolved_url()),
        pools.len(),
        sizes.iter().map(|s| format!("{}", *s as f64 / 1e9)).collect::<Vec<_>>().join(", "),
        every.as_millis(),
        db.display()
    );
    println!("  Ctrl-C to stop; the report follows.");

    // the exchange price next to each snapshot (best bid/ask of OKX and Binance)
    let (stop, stopped) = watch::channel(false);
    let cex = Arc::new(Mutex::new(Cex::default()));
    let r = &cfg.research;
    let okx = serde_json::json!({"op": "subscribe", "args": [{"channel": "bbo-tbt", "instId": r.lag_okx_inst}]});
    let on_okx = {
        let cex = cex.clone();
        move |t: &str| {
            if let Some((bid, ask)) = parse_okx(t) {
                cex.lock().okx = Some(Top { bid, ask, at: Instant::now() });
            }
        }
    };
    let on_binance = {
        let cex = cex.clone();
        move |t: &str| {
            if let Some((bid, ask)) = parse_binance(t) {
                cex.lock().binance = Some(Top { bid, ask, at: Instant::now() });
            }
        }
    };
    let idle = Duration::from_secs(30);
    let streams = [
        tokio::spawn(feed::run_subscribed_stream(
            r.lag_okx_ws_url.clone(),
            vec![okx.to_string()],
            idle,
            on_okx,
            stopped.clone(),
        )),
        tokio::spawn(feed::run_text_stream(r.lag_binance_ws_url.clone(), idle, on_binance, stopped)),
    ];

    let started = tokio::time::Instant::now(); // monotonic: does not advance while asleep
    let deadline = duration.map(|s| started + Duration::from_secs(s));
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut status = tokio::time::interval(Duration::from_secs(30));
    status.tick().await;
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let terminated = async move { term.recv().await };
    #[cfg(not(unix))]
    let terminated = std::future::pending::<Option<()>>();
    tokio::pin!(terminated);

    let (mut last_slot, mut snapshots, mut recorded, mut errors) = (0u64, 0u64, 0u64, 0u64);
    let mut best: BTreeMap<u64, f64> = BTreeMap::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut terminated => break,
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
            _ = status.tick() => {
                let best: Vec<String> = best.iter().map(|(s, b)| format!("{} SOL {b:+.2} bp", *s as f64 / 1e9)).collect();
                println!(
                    "{}  snapshots {snapshots:>6}  new slots {recorded:>6}  errors {errors:>3}  best so far: {}",
                    Ts::now().hms(),
                    if best.is_empty() { "-".to_string() } else { best.join(" · ") }
                );
            }
            _ = tick.tick() => {
                // every pool, what each depended on last time, and the clock: one request, one slot
                let mut addresses: Vec<Address> = pools.iter().map(|p| p.address).collect();
                addresses.extend(pools.iter().flat_map(|p| p.deps.iter().copied()));
                addresses.push(clock);
                let (slot, mut datas) = match rpc.get_account_datas(&addresses).await {
                    Ok((Some(slot), datas)) if datas.len() == addresses.len() => (slot, datas),
                    _ => {
                        errors += 1;
                        continue;
                    }
                };
                snapshots += 1;
                let now = datas.pop().flatten().and_then(|(_, d)| Some(i64::from_le_bytes(d.get(32..40)?.try_into().ok()?)));
                let mut dep_datas = datas.split_off(pools.len()).into_iter();
                let mut ready: Vec<Option<Ready>> = Vec::new();
                for (p, data) in pools.iter_mut().zip(datas) {
                    let mine: Vec<Option<Vec<u8>>> = dep_datas.by_ref().take(p.deps.len()).map(|d| d.map(|(_, d)| d)).collect();
                    let state = data.and_then(|(_, pool)| {
                        let deps = amm::dependencies(p.kind, &p.address, &pool)?;
                        // the arrays were chosen from the previous snapshot: usable if the price is still among them
                        let state = (deps == p.deps).then(|| amm::decode(p.kind, &pool, &mine)).flatten();
                        p.deps = deps;
                        let mints = decode_pool(p.kind, &pool, |m| tokens.by_mint(m).map(|t| t.decimals))?;
                        Some((state?, mints.mint_a == p.base))
                    });
                    ready.push(state);
                }
                let (Some(now), true) = (now, slot > last_slot) else { continue };
                last_slot = slot;
                let ts = Ts::now().0;
                let fair = cex.lock().fair(Instant::now());
                for (p, state) in pools.iter().zip(&ready) {
                    // bin pools whose base token is the first one: bins upwards are higher prices of the base
                    let Some((Pool::Dlmm(d), true)) = state else { continue };
                    let Some(bin) = d.bins.get(&d.active_id) else { continue };
                    let snap = PoolSnap {
                        run: run_id.clone(),
                        ts,
                        slot,
                        pool: p.name.clone(),
                        active_id: d.active_id,
                        bin_step: d.bin_step,
                        price: bin.price as f64 / 2f64.powi(64) * p.unit,
                        lp_fee_bps: d.fee_rate_at(d.active_id, now) as f64 / 1e5 * (1.0 - d.protocol_share as f64 / 1e4),
                        cex_mid: fair.map(|f| f.top.mid()),
                        cex_src: fair.map(|f| f.src.to_string()),
                    };
                    if let Err(e) = store.insert_pool_snap(&snap) {
                        eprintln!("research: writing pool_snap: {e}");
                    }
                }
                let named: Vec<(&str, Option<&Ready>)> = pools.iter().zip(&ready).map(|(p, r)| (p.name.as_str(), r.as_ref())).collect();
                let trips = round_trips(&named, &sizes, now);
                if trips.is_empty() {
                    continue;
                }
                recorded += 1;
                let rows: Vec<PoolEdge> = trips
                    .into_iter()
                    .map(|(size, sell_on, buy_on, gross_bps)| {
                        let b = best.entry(size).or_insert(f64::MIN);
                        *b = b.max(gross_bps);
                        PoolEdge { run: run_id.clone(), ts, slot, size, sell_on, buy_on, gross_bps }
                    })
                    .collect();
                if let Err(e) = store.insert_pool_edges(&rows) {
                    eprintln!("research: writing pool_edge: {e}");
                }
            }
        }
    }
    println!(
        "\n{snapshots} snapshots in {} s, {recorded} at a new slot, {errors} failed requests",
        started.elapsed().as_secs()
    );
    let _ = stop.send(true);
    for s in streams {
        let _ = tokio::time::timeout(Duration::from_secs(3), s).await;
    }
    let this = std::slice::from_ref(&run_id);
    print!("\n{}", render(&build(&store.pool_edges(this)?)));
    print!("\n{}", super::maker::render(&super::maker::build(&store.pool_snaps(this)?)));
    Ok(())
}

#[derive(Serialize, Default, Debug)]
pub struct PoolsReport {
    pub runs: usize,
    /// Snapshots (distinct slots) with at least one round trip.
    pub snapshots: usize,
    /// Seconds between the first and the last snapshot of each run, summed.
    pub span_s: f64,
    /// Median milliseconds between snapshots.
    pub every_ms: Option<f64>,
    /// size → the best pair's round trip at each snapshot (bp), and how many
    /// snapshots had one above the fixed cost of a transaction.
    pub best: BTreeMap<u64, (Dist, usize)>,
    /// "sell on → buy on" → size → every round trip (bp).
    pub pairs: BTreeMap<String, BTreeMap<u64, Dist>>,
    /// size → runs of consecutive snapshots in which one pair stayed above
    /// the fixed cost: (how many, seconds they lasted, their best bp after it).
    pub episodes: BTreeMap<u64, (usize, Dist, Dist)>,
}

/// One pair at one size in one run: (run, "sell on → buy on", size).
type Series<'a> = (&'a str, String, u64);

/// One transaction's fixed cost in bp of `size`.
fn fixed_bps(size: u64) -> f64 {
    FIXED_LAMPORTS / size as f64 * 1e4
}

pub fn build(edges: &[PoolEdge]) -> PoolsReport {
    let mut r = PoolsReport::default();
    // (run, ts) → size → best bp; and per pair-size the series in time order
    let mut snapshots: BTreeMap<(&str, i64), BTreeMap<u64, f64>> = BTreeMap::new();
    let mut series: BTreeMap<Series, Vec<(i64, f64)>> = BTreeMap::new();
    for e in edges {
        let best = snapshots.entry((&e.run, e.ts)).or_default().entry(e.size).or_insert(f64::MIN);
        *best = best.max(e.gross_bps);
        series.entry((&e.run, format!("{} → {}", e.sell_on, e.buy_on), e.size)).or_default().push((e.ts, e.gross_bps));
    }
    r.snapshots = snapshots.len();
    let mut gaps = Vec::new();
    let mut runs: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    let mut last: Option<(&str, i64)> = None;
    for (run, ts) in snapshots.keys() {
        let span = runs.entry(run).or_insert((*ts, *ts));
        span.1 = *ts;
        if let Some((_, t0)) = last.filter(|(r0, _)| r0 == run) {
            gaps.push((*ts - t0) as f64 / 1e3);
        }
        last = Some((run, *ts));
    }
    r.runs = runs.len();
    r.span_s = runs.values().map(|(a, b)| (b - a) as f64 / 1e6).sum();
    r.every_ms = Dist::of(gaps).p50;

    let mut best: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
    for sizes in snapshots.values() {
        for (size, bp) in sizes {
            best.entry(*size).or_default().push(*bp);
        }
    }
    for (size, bps) in best {
        let above = bps.iter().filter(|b| **b > fixed_bps(size)).count();
        r.best.insert(size, (Dist::of(bps), above));
    }
    let mut episodes: BTreeMap<u64, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for ((_, pair, size), points) in &series {
        r.pairs.entry(pair.clone()).or_default().insert(*size, Dist::of(points.iter().map(|p| p.1).collect()));
        // consecutive snapshots of this pair above the fixed cost
        let fixed = fixed_bps(*size);
        let mut open: Option<(i64, i64, f64)> = None; // first ts, last ts, best bp after the fixed cost
        for (ts, bp) in points.iter().copied().chain(std::iter::once((i64::MAX, f64::MIN))) {
            if bp > fixed {
                let o = open.get_or_insert((ts, ts, bp - fixed));
                (o.1, o.2) = (ts, o.2.max(bp - fixed));
            } else if let Some((first, last, peak)) = open.take() {
                let e = episodes.entry(*size).or_default();
                e.0.push((last - first) as f64 / 1e6);
                e.1.push(peak);
            }
        }
    }
    for size in r.best.keys() {
        let (lasted, peaks) = episodes.remove(size).unwrap_or_default();
        r.episodes.insert(*size, (lasted.len(), Dist::of(lasted), Dist::of(peaks)));
    }
    r
}

fn cols(d: &Dist) -> String {
    let f = |v: Option<f64>| v.map(|x| format!("{x:+.2}")).unwrap_or_else(|| "-".into());
    format!(
        "{:>7} {:>6}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        d.n,
        d.positive,
        f(d.p5),
        f(d.p25),
        f(d.p50),
        f(d.p75),
        f(d.p95),
        f(d.max)
    )
}

const HEAD: &str = "      n     >0        p5      p25      p50      p75      p95      max";

pub fn render(r: &PoolsReport) -> String {
    let mut o = String::new();
    let sol = |size: &u64| format!("{} SOL", *size as f64 / 1e9);
    let _ = writeln!(
        o,
        "POOL-TO-POOL ROUND TRIPS · {} run(s) · {} snapshots over {:.1} h · one every {}",
        r.runs,
        r.snapshots,
        r.span_s / 3600.0,
        r.every_ms.map(|m| format!("{:.1} s", m / 1e3)).unwrap_or_else(|| "-".into())
    );
    let _ = writeln!(o, "  Sell the base token on one pool, buy it back on another, both worked out from the pools'");
    let _ = writeln!(
        o,
        "  own accounts at one slot. After both pools' fees; before any transaction cost. bp of the input."
    );
    if r.snapshots == 0 {
        return o;
    }
    let _ = writeln!(o, "\n1. The best pair at each snapshot");
    let _ = writeln!(o, "   {:<10}{HEAD}   above one transaction's cost", "size");
    for (size, (d, above)) in &r.best {
        let _ = writeln!(
            o,
            "   {:<10}{}   {above} ({:.2} %; the cost is {:.3} bp)",
            sol(size),
            cols(d),
            *above as f64 / d.n.max(1) as f64 * 100.0,
            fixed_bps(*size)
        );
    }
    let _ =
        writeln!(o, "   one transaction's cost = base fee + the minimum Jito tip (6,000 lamports); priority fees and");
    let _ = writeln!(o, "   a competitive tip come on top, and someone else may take the same gap first.");

    let _ = writeln!(o, "\n2. Each pair");
    for (pair, sizes) in &r.pairs {
        let _ = writeln!(o, "   {pair}");
        let _ = writeln!(o, "   {:<10}{HEAD}", "");
        for (size, d) in sizes {
            let _ = writeln!(o, "   {:<10}{}", sol(size), cols(d));
        }
    }

    let _ = writeln!(o, "\n3. How long a gap that pays for its transaction lasts");
    let _ = writeln!(o, "   Runs of consecutive snapshots in which one pair stayed above the cost. A gap seen in a");
    let _ = writeln!(o, "   single snapshot lasted 0 s here: less than the time between two snapshots.");
    let _ = writeln!(
        o,
        "   {:<10} {:>8}   {:>26}   {:>30}",
        "size", "gaps", "lasted s (p50 · p95 · max)", "best bp after the cost (p50 · max)"
    );
    for (size, (n, lasted, peak)) in &r.episodes {
        let f = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into());
        let g = |v: Option<f64>| v.map(|x| format!("{x:+.2}")).unwrap_or_else(|| "-".into());
        let _ = writeln!(
            o,
            "   {:<10} {n:>8}   {:>26}   {:>30}",
            sol(size),
            format!("{} · {} · {}", f(lasted.p50), f(lasted.p95), f(lasted.max)),
            format!("{} · {}", g(peak.p50), g(peak.max))
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_market::amm::dlmm::{Bin, Dlmm};

    /// A one-bin pool at `price` quote per base (raw units), no fee, deep.
    fn flat(price: u128) -> Ready {
        let bin = Bin { amount_x: u64::MAX / 4, amount_y: u64::MAX / 4, price: price << 64 };
        let pool = Dlmm {
            active_id: 0,
            bin_step: 1,
            base_factor: 0,
            base_fee_power: 0,
            variable_fee_control: 0,
            max_volatility_accumulator: 0,
            protocol_share: 0,
            filter_period: 0,
            decay_period: 0,
            reduction_factor: 0,
            volatility_accumulator: 0,
            volatility_reference: 0,
            index_reference: 0,
            last_update: 0,
            bins: BTreeMap::from([(0, bin)]),
        };
        (Pool::Dlmm(pool), true)
    }

    #[test]
    fn a_round_trip_sells_on_one_pool_and_buys_back_on_the_other() {
        let (cheap, dear) = (flat(100), flat(101));
        let pools = [("cheap", Some(&cheap)), ("dear", Some(&dear)), ("absent", None)];
        let trips = round_trips(&pools, &[1_000_000], 0);
        // two ordered pairs; the pool without a state is in none
        assert_eq!(trips.len(), 2);
        let bp = |sell: &str| trips.iter().find(|t| t.1 == sell).unwrap().3;
        // sell at 101, buy back at 100: +1 %; the other way −0.99 %
        assert!((bp("dear") - 100.0).abs() < 1e-6, "{}", bp("dear"));
        assert!((bp("cheap") + 99.0099).abs() < 1e-3, "{}", bp("cheap"));
        assert!(round_trips(&pools, &[], 0).is_empty());
    }

    fn edge(ts_s: i64, sell_on: &str, buy_on: &str, size: u64, gross_bps: f64) -> PoolEdge {
        PoolEdge {
            run: "p".into(),
            ts: ts_s * 1_000_000,
            slot: ts_s as u64,
            size,
            sell_on: sell_on.into(),
            buy_on: buy_on.into(),
            gross_bps,
        }
    }

    #[test]
    fn the_report_takes_the_best_pair_per_snapshot_and_finds_the_gaps_that_last() {
        let size = 1_000_000_000; // 1 SOL: the fixed cost is 0.06 bp
        let mut edges = Vec::new();
        // A → B: below the cost, above it for three snapshots (2 s), below, above once
        for (t, bp) in [(0, -3.0), (1, 0.5), (2, 1.5), (3, 0.2), (4, -1.0), (5, 0.1), (6, -2.0)] {
            edges.push(edge(t, "A", "B", size, bp));
            edges.push(edge(t, "B", "A", size, -4.0));
        }
        let r = build(&edges);
        assert_eq!((r.runs, r.snapshots), (1, 7));
        assert_eq!(r.every_ms, Some(1_000.0));
        assert!((r.span_s - 6.0).abs() < 1e-9);
        let (best, above) = &r.best[&size];
        assert_eq!((best.n, best.positive, *above), (7, 4, 4));
        assert_eq!(best.max, Some(1.5));
        assert_eq!(r.pairs["B → A"][&size].positive, 0);
        let (gaps, lasted, peak) = &r.episodes[&size];
        assert_eq!(*gaps, 2);
        assert_eq!((lasted.max, lasted.p5), (Some(2.0), Some(0.0)), "one lasted 2 s, one was seen once");
        assert!((peak.max.unwrap() - 1.44).abs() < 1e-9, "1.5 bp less the 0.06 bp cost");
        let text = render(&r);
        assert!(text.contains("7 snapshots") && text.contains("A → B"), "{text}");
        assert!(render(&build(&[])).contains("0 snapshots"));
    }
}
