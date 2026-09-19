//! Paper-run statistics computed from the recorded session. Every evaluated
//! cycle counts (negative ones included) — this is the survivorship-bias check.

use crate::store::{SessionRow, Store, StoreError};
use rusqlite::params;
use searcher_core::units::format_atoms;
use searcher_core::{Ppm, time::fmt_duration_us};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct StrategyStats {
    pub strategy: String,
    pub scanned: i64,
    pub priced: i64,
    pub positive_gross: i64,
    pub positive_net: i64,
    pub executable: i64,
    pub median_gross_edge_bps: Option<f64>,
    pub median_net_edge_bps: Option<f64>,
    pub best_net_edge_bps: Option<f64>,
    pub sum_expected_net_lamports: i64,
    pub simulations: i64,
    pub sim_ok: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Lifetime {
    pub episodes: usize,
    /// Median of (last positive − first positive) within an episode.
    pub median_lower_ms: Option<i64>,
    /// Median of (first non-positive after − first positive).
    pub median_upper_ms: Option<i64>,
    /// Median gap between consecutive evaluations of the same route key
    /// (the resolution of the lifetime measurement).
    pub median_sampling_gap_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub session_id: String,
    pub mode: String,
    pub duration_ms: i64,
    pub scanned: i64,
    pub priced: i64,
    pub positive_gross: i64,
    pub positive_net: i64,
    pub executable: i64,
    pub paper_fills: i64,
    pub gross_expected_pnl_all: i64,
    pub net_expected_pnl_all: i64,
    pub gross_expected_pnl_executable: i64,
    pub net_expected_pnl_executable: i64,
    pub paper_net_lamports: i64,
    pub paper_net_usd_micros: i64,
    pub median_gross_edge_bps: Option<f64>,
    pub median_net_edge_bps: Option<f64>,
    pub p90_gross_edge_bps: Option<f64>,
    pub max_gross_edge_bps: Option<f64>,
    pub lifetime_gross_positive: Lifetime,
    pub simulations: i64,
    pub sim_failures: i64,
    pub sim_failure_rate: Option<f64>,
    pub sim_failure_classes: Vec<(String, i64)>,
    /// (plan/fidelity, simulations, failures)
    pub sim_by_plan: Vec<(String, i64, i64)>,
    pub median_sim_latency_ms: Option<i64>,
    pub median_cu: Option<i64>,
    pub expected_landing_cost_lamports: Option<i64>,
    pub median_tip_lamports: Option<i64>,
    /// Median (simulated net − model expected net) over exact simulations.
    pub median_sim_minus_model_lamports: Option<i64>,
    pub median_quote_latency_ms: Option<i64>,
    pub skip_reasons: Vec<(String, i64)>,
    pub strategies: Vec<StrategyStats>,
    pub errors: i64,
    pub rate_limited_429: i64,
    pub dropped_events: i64,
}

fn median(v: &mut [i64]) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

fn pct(v: &mut [i64], q: f64) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    Some(v[i])
}

fn bps(ppm: Option<i64>) -> Option<f64> {
    ppm.map(|p| Ppm(p).bps_f64())
}

struct Row {
    strategy: String,
    key: String,
    ts: i64,
    status: String,
    skip: Option<String>,
    gross: i64,
    net: i64,
    gross_edge: i64,
    net_edge: i64,
    sim_net: Option<i64>,
    landing: i64,
    tip: i64,
    latency: i64,
}

const PRICED_EXCLUDED: [&str; 3] = ["NO_ROUTE", "BUILD_FAILED", "RATE_LIMITED"];
const EXECUTABLE: [&str; 6] = ["executable", "paper_filled", "landed", "submitted", "awaiting_confirm", "failed"];

pub fn build(store: &Store, session: &str) -> Result<Report, StoreError> {
    let c = store.conn();
    let s: SessionRow = store
        .list_sessions()?
        .into_iter()
        .find(|r| r.id == session)
        .ok_or_else(|| StoreError::NoSession(session.into()))?;
    let (first, last) = store.session_span(session)?;
    let mut st = c.prepare(
        "SELECT strategy, key, detected_at, status, skip_reason, gross_pnl, expected_net, gross_edge_ppm, net_edge_ppm,
                simulated_net, base_fee + priority_fee + jito_tip, jito_tip, quote_latency_ms
         FROM opportunities WHERE session_id = ?1 ORDER BY detected_at",
    )?;
    let rows: Vec<Row> = st
        .query_map(params![session], |r| {
            Ok(Row {
                strategy: r.get(0)?,
                key: r.get(1)?,
                ts: r.get(2)?,
                status: r.get(3)?,
                skip: r.get(4)?,
                gross: r.get(5)?,
                net: r.get(6)?,
                gross_edge: r.get(7)?,
                net_edge: r.get(8)?,
                sim_net: r.get(9)?,
                landing: r.get(10)?,
                tip: r.get(11)?,
                latency: r.get(12)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    let priced = |r: &&Row| r.skip.as_deref().is_none_or(|k| !PRICED_EXCLUDED.contains(&k));
    let exec = |r: &&Row| EXECUTABLE.contains(&r.status.as_str()) && r.status != "failed";
    let mut rep = Report {
        session_id: s.id.clone(),
        mode: s.mode.clone(),
        duration_ms: match (first, last) {
            (Some(a), Some(b)) => (b - a) / 1_000,
            _ => 0,
        },
        scanned: rows.len() as i64,
        dropped_events: s.dropped,
        ..Default::default()
    };
    let pr: Vec<&Row> = rows.iter().filter(priced).collect();
    rep.priced = pr.len() as i64;
    rep.positive_gross = pr.iter().filter(|r| r.gross > 0).count() as i64;
    rep.positive_net = pr.iter().filter(|r| r.net > 0).count() as i64;
    rep.gross_expected_pnl_all = pr.iter().map(|r| r.gross).sum();
    rep.net_expected_pnl_all = pr.iter().map(|r| r.net).sum();
    let ex: Vec<&Row> = rows.iter().filter(exec).collect();
    rep.executable = ex.len() as i64;
    rep.gross_expected_pnl_executable = ex.iter().map(|r| r.gross).sum();
    rep.net_expected_pnl_executable = ex.iter().map(|r| r.net).sum();
    let mut ge: Vec<i64> = pr.iter().map(|r| r.gross_edge).collect();
    let mut ne: Vec<i64> = pr.iter().map(|r| r.net_edge).collect();
    rep.median_gross_edge_bps = bps(median(&mut ge));
    rep.median_net_edge_bps = bps(median(&mut ne));
    rep.p90_gross_edge_bps = bps(pct(&mut ge, 0.9));
    rep.max_gross_edge_bps = bps(ge.iter().max().copied());
    let mut lat: Vec<i64> = pr.iter().map(|r| r.latency).collect();
    rep.median_quote_latency_ms = median(&mut lat);
    let mut tips: Vec<i64> = pr.iter().map(|r| r.tip).collect();
    rep.median_tip_lamports = median(&mut tips);

    // skip reasons (all rows)
    let mut skips: BTreeMap<String, i64> = BTreeMap::new();
    for r in &rows {
        if let Some(k) = &r.skip {
            *skips.entry(k.clone()).or_default() += 1;
        }
    }
    rep.skip_reasons = skips.into_iter().collect();
    rep.skip_reasons.sort_by_key(|r| std::cmp::Reverse(r.1));

    // lifetimes of gross-positive episodes per route key
    let mut by_key: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in &pr {
        by_key.entry(&r.key).or_default().push(r);
    }
    let (mut lower, mut upper, mut gaps) = (Vec::new(), Vec::new(), Vec::new());
    for rs in by_key.values() {
        for w in rs.windows(2) {
            gaps.push((w[1].ts - w[0].ts) / 1_000);
        }
        let mut i = 0;
        while i < rs.len() {
            if rs[i].gross > 0 {
                let start = rs[i].ts;
                let mut j = i;
                while j + 1 < rs.len() && rs[j + 1].gross > 0 {
                    j += 1;
                }
                lower.push((rs[j].ts - start) / 1_000);
                if j + 1 < rs.len() {
                    upper.push((rs[j + 1].ts - start) / 1_000);
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }
    rep.lifetime_gross_positive = Lifetime {
        episodes: lower.len(),
        median_lower_ms: median(&mut lower),
        median_upper_ms: median(&mut upper),
        median_sampling_gap_ms: median(&mut gaps),
    };

    // per strategy
    let mut strat: BTreeMap<String, StrategyStats> = BTreeMap::new();
    for r in &rows {
        let e = strat
            .entry(r.strategy.clone())
            .or_insert_with(|| StrategyStats { strategy: r.strategy.clone(), ..Default::default() });
        e.scanned += 1;
    }
    for (name, e) in strat.iter_mut() {
        let rs: Vec<&&Row> = pr.iter().filter(|r| &r.strategy == name).collect();
        e.priced = rs.len() as i64;
        e.positive_gross = rs.iter().filter(|r| r.gross > 0).count() as i64;
        e.positive_net = rs.iter().filter(|r| r.net > 0).count() as i64;
        e.executable = ex.iter().filter(|r| &r.strategy == name).count() as i64;
        let mut g: Vec<i64> = rs.iter().map(|r| r.gross_edge).collect();
        let mut n: Vec<i64> = rs.iter().map(|r| r.net_edge).collect();
        e.best_net_edge_bps = bps(n.iter().max().copied());
        e.median_gross_edge_bps = bps(median(&mut g));
        e.median_net_edge_bps = bps(median(&mut n));
        e.sum_expected_net_lamports = ex.iter().filter(|r| &r.strategy == name).map(|r| r.net).sum();
        let (sims, ok): (i64, i64) = c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(s.ok),0) FROM simulations s JOIN opportunities o
             ON o.session_id = s.session_id AND o.id = s.opportunity_id WHERE s.session_id = ?1 AND o.strategy = ?2",
            params![session, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        e.simulations = sims;
        e.sim_ok = ok;
    }
    rep.strategies = strat.into_values().collect();

    // simulations
    let (sims, fails): (i64, i64) = c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(1 - ok),0) FROM simulations WHERE session_id = ?1",
        params![session],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    rep.simulations = sims;
    rep.sim_failures = fails;
    rep.sim_failure_rate = (sims > 0).then(|| fails as f64 / sims as f64);
    let mut st = c.prepare(
        "SELECT COALESCE(failure_class,'?'), COUNT(*) FROM simulations WHERE session_id = ?1 AND ok = 0
         GROUP BY 1 ORDER BY 2 DESC",
    )?;
    rep.sim_failure_classes =
        st.query_map(params![session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let mut st = c.prepare(
        "SELECT plan || '/' || fidelity, COUNT(*), COALESCE(SUM(1 - ok),0) FROM simulations WHERE session_id = ?1 GROUP BY 1 ORDER BY 2 DESC",
    )?;
    rep.sim_by_plan =
        st.query_map(params![session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    let col = |sql: &str| -> Result<Vec<i64>, StoreError> {
        let mut st = c.prepare(sql)?;
        Ok(st.query_map(params![session], |r| r.get::<_, i64>(0))?.collect::<Result<_, _>>()?)
    };
    rep.median_sim_latency_ms = median(&mut col("SELECT latency_ms FROM simulations WHERE session_id = ?1")?);
    rep.median_cu = median(&mut col("SELECT units FROM simulations WHERE session_id = ?1 AND ok = 1")?);
    let mut landing: Vec<i64> = rows.iter().filter(|r| r.sim_net.is_some()).map(|r| r.landing).collect();
    if landing.is_empty() {
        landing = pr.iter().map(|r| r.landing).collect();
    }
    rep.expected_landing_cost_lamports = median(&mut landing);
    let mut diff: Vec<i64> = rows.iter().filter_map(|r| r.sim_net.map(|s| s - r.net)).collect();
    rep.median_sim_minus_model_lamports = median(&mut diff);

    // trades / errors
    let (pn, pu, fills): (i64, i64, i64) = c.query_row(
        "SELECT COALESCE(SUM(net),0), COALESCE(SUM(net_usd_micros),0), COUNT(*) FROM trades WHERE session_id = ?1 AND paper = 1",
        params![session],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    rep.paper_net_lamports = pn;
    rep.paper_net_usd_micros = pu;
    rep.paper_fills = fills;
    rep.errors = c.query_row("SELECT COUNT(*) FROM errors WHERE session_id = ?1", params![session], |r| r.get(0))?;
    rep.rate_limited_429 = store.kind_count(session, "rate_limited")?;
    Ok(rep)
}

fn sol(l: i64) -> String {
    format!("{} SOL", format_atoms(l as i128, 9, 6))
}

fn obps(v: Option<f64>) -> String {
    v.map(|b| format!("{b:+.2} bp")).unwrap_or_else(|| "—".into())
}

fn oms(v: Option<i64>) -> String {
    v.map(|m| fmt_duration_us(m * 1_000)).unwrap_or_else(|| "—".into())
}

/// Plain-text report (what `mobius-searcher --report SESSION` prints).
pub fn render(r: &Report) -> String {
    let mut o = String::new();
    let line = |o: &mut String, k: &str, v: String| o.push_str(&format!("  {k:<34} {v}\n"));
    o.push_str(&format!("SESSION {}  ·  {}  ·  {}\n\n", r.session_id, r.mode, fmt_duration_us(r.duration_ms * 1_000)));
    o.push_str("OPPORTUNITIES\n");
    line(&mut o, "scanned (all evaluations)", r.scanned.to_string());
    line(&mut o, "priced (full route quoted)", r.priced.to_string());
    line(
        &mut o,
        "gross-positive",
        format!("{} ({:.1}%)", r.positive_gross, 100.0 * r.positive_gross as f64 / r.priced.max(1) as f64),
    );
    line(
        &mut o,
        "net-positive (after all costs)",
        format!("{} ({:.1}%)", r.positive_net, 100.0 * r.positive_net as f64 / r.priced.max(1) as f64),
    );
    line(&mut o, "executable (sim + risk passed)", r.executable.to_string());
    line(&mut o, "median gross edge", obps(r.median_gross_edge_bps));
    line(&mut o, "p90 / max gross edge", format!("{} / {}", obps(r.p90_gross_edge_bps), obps(r.max_gross_edge_bps)));
    line(&mut o, "median net edge", obps(r.median_net_edge_bps));
    let lt = &r.lifetime_gross_positive;
    line(
        &mut o,
        "gross-positive episodes",
        format!(
            "{}  lifetime median {}–{} (sampling gap {})",
            lt.episodes,
            oms(lt.median_lower_ms),
            oms(lt.median_upper_ms),
            oms(lt.median_sampling_gap_ms)
        ),
    );
    line(&mut o, "median quote latency (all legs)", oms(r.median_quote_latency_ms));
    o.push_str("\nPNL (lamports; paper = simulated, no landing risk applied)\n");
    line(&mut o, "gross expected, all priced", sol(r.gross_expected_pnl_all));
    line(&mut o, "net expected, all priced", sol(r.net_expected_pnl_all));
    line(&mut o, "gross expected, executable", sol(r.gross_expected_pnl_executable));
    line(&mut o, "net expected, executable", sol(r.net_expected_pnl_executable));
    line(&mut o, "paper fills / net", format!("{} / {}", r.paper_fills, sol(r.paper_net_lamports)));
    o.push_str("\nSIMULATION\n");
    line(&mut o, "simulations", r.simulations.to_string());
    line(
        &mut o,
        "failure rate",
        r.sim_failure_rate
            .map(|f| format!("{:.1}% ({} failed)", f * 100.0, r.sim_failures))
            .unwrap_or_else(|| "—".into()),
    );
    for (c, n) in &r.sim_failure_classes {
        line(&mut o, &format!("  · {c}"), n.to_string());
    }
    for (p, n, f) in &r.sim_by_plan {
        line(
            &mut o,
            &format!("  {p}"),
            format!("{n} sims · {f} failed ({:.1}%)", 100.0 * *f as f64 / (*n).max(1) as f64),
        );
    }
    line(&mut o, "median sim latency", oms(r.median_sim_latency_ms));
    line(&mut o, "median CU consumed", r.median_cu.map(|c| c.to_string()).unwrap_or_else(|| "—".into()));
    line(
        &mut o,
        "median sim − model net",
        r.median_sim_minus_model_lamports.map(|d| format!("{d:+} lamports")).unwrap_or_else(|| "—".into()),
    );
    line(
        &mut o,
        "expected landing cost (median)",
        r.expected_landing_cost_lamports
            .map(|l| format!("{l} lamports (base+priority+tip)"))
            .unwrap_or_else(|| "—".into()),
    );
    line(
        &mut o,
        "median Jito tip (policy)",
        r.median_tip_lamports.map(|l| format!("{l} lamports")).unwrap_or_else(|| "—".into()),
    );
    o.push_str("\nSKIP REASONS\n");
    for (k, n) in &r.skip_reasons {
        line(&mut o, k, n.to_string());
    }
    o.push_str("\nSTRATEGIES\n");
    o.push_str(&format!(
        "  {:<12} {:>7} {:>7} {:>6} {:>6} {:>5} {:>11} {:>11} {:>11} {:>9}\n",
        "strategy", "scanned", "priced", "gross+", "net+", "exec", "med gross", "med net", "best net", "sim ok"
    ));
    for s in &r.strategies {
        o.push_str(&format!(
            "  {:<12} {:>7} {:>7} {:>6} {:>6} {:>5} {:>11} {:>11} {:>11} {:>9}\n",
            s.strategy,
            s.scanned,
            s.priced,
            s.positive_gross,
            s.positive_net,
            s.executable,
            obps(s.median_gross_edge_bps),
            obps(s.median_net_edge_bps),
            obps(s.best_net_edge_bps),
            format!("{}/{}", s.sim_ok, s.simulations)
        ));
    }
    o.push_str("\nSYSTEM\n");
    line(&mut o, "errors", r.errors.to_string());
    line(&mut o, "429 / rate-limited events", r.rate_limited_429.to_string());
    line(&mut o, "dropped storage events", r.dropped_events.to_string());
    o
}
