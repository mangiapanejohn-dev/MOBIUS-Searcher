//! `--research-report`: what the research runs recorded, as whole
//! distributions. Every table shows how many samples there were next to how
//! many were positive; nothing is filtered to the good tail.

use searcher_storage::ResearchStore;
use searcher_storage::StoreError;
use searcher_storage::research::{Episode, LadderRow, LagTick, XchainRow};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;

/// Fixed on-chain cost of one Solana transaction used for "after fixed
/// costs": base fee (1 signature) + the minimum Jito tip. The provider's
/// priority-fee estimate is added per sample when it was recorded.
const FIXED_LAMPORTS: i64 = 5_000 + 1_000;

#[derive(Serialize, Default, Clone, Debug)]
pub struct Dist {
    pub n: usize,
    pub positive: usize,
    pub p5: Option<f64>,
    pub p25: Option<f64>,
    pub p50: Option<f64>,
    pub p75: Option<f64>,
    pub p95: Option<f64>,
    pub max: Option<f64>,
}

impl Dist {
    pub fn of(mut v: Vec<f64>) -> Dist {
        v.retain(|x| x.is_finite());
        v.sort_by(|a, b| a.total_cmp(b));
        let q = |p: f64| -> Option<f64> {
            if v.is_empty() {
                return None;
            }
            let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
            Some(v[rank.min(v.len()) - 1])
        };
        Dist {
            n: v.len(),
            positive: v.iter().filter(|x| **x > 0.0).count(),
            p5: q(5.0),
            p25: q(25.0),
            p50: q(50.0),
            p75: q(75.0),
            p95: q(95.0),
            max: v.last().copied(),
        }
    }
}

#[derive(Serialize, Default, Debug)]
pub struct Report {
    pub runs: Vec<String>,
    pub awake_s: i64,
    pub jupiter_requests: i64,
    pub jupiter_errors: i64,
    pub jupiter_429: i64,
    pub setup: Vec<String>,
    /// size (lamports) → (gross bp, after fixed costs bp, errors)
    pub ladder: BTreeMap<u64, (Dist, Dist, usize)>,
    pub ladder_best: Option<String>,
    /// "asset venue $N" → (A bp, B bp, errors, median skew ms)
    pub xchain: BTreeMap<String, (Dist, Dist, usize, Option<f64>)>,
    /// dex → (|gap| bp, share of samples over the trigger, median pool age ms)
    pub lag_gap: BTreeMap<String, (Dist, f64, Option<f64>)>,
    /// "kind dex" → episodes
    pub lag_episodes: BTreeMap<String, EpisodeStats>,
    pub trigger_bps: Option<f64>,
}

#[derive(Serialize, Default, Debug)]
pub struct EpisodeStats {
    pub n: usize,
    pub quoted: usize,
    pub errors: usize,
    /// executable price vs CEX mid (bp, positive = better than fair)
    pub exec_vs_mid: Dist,
    /// vs the CEX side you would unwind on
    pub exec_vs_touch: Dist,
    /// after the fixed transaction cost at the quoted size
    pub exec_after_fixed: Dist,
    pub duration_s: Dist,
    pub quote_ms: Dist,
    /// markout horizon (s) → mean bp the CEX moved toward the pool / the pool toward the CEX
    pub markouts: BTreeMap<u32, (f64, f64, usize)>,
}

pub fn build(store: &ResearchStore, runs: &[String]) -> Result<Report, StoreError> {
    let mut r = Report { runs: runs.to_vec(), ..Default::default() };
    for id in runs {
        if let Some(run) = store.run(id)? {
            r.awake_s += run.awake_s;
            r.jupiter_requests += run.jupiter_requests;
            r.jupiter_errors += run.jupiter_errors;
            r.jupiter_429 += run.jupiter_429;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&run.setup) {
                let s = format!(
                    "{} · {} {} · proxy {} · Jupiter {} · keep awake {}",
                    id,
                    v["os"].as_str().unwrap_or("?"),
                    v["arch"].as_str().unwrap_or("?"),
                    v["proxy"].as_str().unwrap_or("?"),
                    v["jupiter"]["tier"].as_str().unwrap_or("?"),
                    v["keep_awake"]
                );
                r.trigger_bps = r.trigger_bps.or(v["research"]["lag_trigger_bps"].as_f64());
                r.setup.push(s);
            }
        }
    }
    ladder(&mut r, &store.ladder(runs)?);
    xchain(&mut r, &store.xchain(runs)?);
    lag(&mut r, &store.lag_ticks(runs)?, &store.episodes(runs)?);
    Ok(r)
}

fn bps(num: i64, den: u64) -> f64 {
    num as f64 / den as f64 * 1e4
}

fn ladder(r: &mut Report, rows: &[LadderRow]) {
    let mut by: BTreeMap<u64, (Vec<f64>, Vec<f64>, usize)> = BTreeMap::new();
    let mut best: Option<(f64, &LadderRow)> = None;
    for row in rows {
        let e = by.entry(row.size).or_default();
        let Some(g) = row.gross else {
            e.2 += 1;
            continue;
        };
        e.0.push(bps(g, row.size));
        let net = g - FIXED_LAMPORTS - row.priority_est.unwrap_or(0) as i64;
        let net_bps = bps(net, row.size);
        e.1.push(net_bps);
        if best.is_none_or(|(b, _)| net_bps > b) {
            best = Some((net_bps, row));
        }
    }
    r.ladder = by.into_iter().map(|(k, (g, n, err))| (k, (Dist::of(g), Dist::of(n), err))).collect();
    r.ladder_best = best.map(|(b, row)| {
        format!(
            "{:+.2} bp after fixed costs: {} at {} SOL ({} | {}), gross {:+} lamports",
            b,
            row.route_label,
            row.size as f64 / 1e9,
            row.dexes1.as_deref().unwrap_or("-"),
            row.dexes2.as_deref().unwrap_or("-"),
            row.gross.unwrap_or(0)
        )
    });
}

#[derive(Default)]
struct XchainAcc {
    a: Vec<f64>,
    b: Vec<f64>,
    errors: usize,
    skew: Vec<f64>,
}

fn xchain(r: &mut Report, rows: &[XchainRow]) {
    let mut by: BTreeMap<String, XchainAcc> = BTreeMap::new();
    for row in rows {
        let e = by.entry(format!("{} {} ${}", row.asset, row.venue, row.notional_usd)).or_default();
        match (row.a_bps, row.b_bps) {
            (Some(a), Some(b)) => {
                e.a.push(a);
                e.b.push(b);
                if let Some(s) = row.skew_ms {
                    e.skew.push(s as f64);
                }
            }
            _ => e.errors += 1,
        }
    }
    r.xchain =
        by.into_iter().map(|(k, x)| (k, (Dist::of(x.a), Dist::of(x.b), x.errors, Dist::of(x.skew).p50))).collect();
}

fn lag(r: &mut Report, ticks: &[LagTick], eps: &[Episode]) {
    let trigger = r.trigger_bps.unwrap_or(f64::INFINITY);
    let mut by: BTreeMap<String, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for t in ticks {
        let e = by.entry(t.dex.clone()).or_default();
        e.0.push(t.gap_bps.abs());
        e.1.push(t.pool_age_ms as f64);
    }
    r.lag_gap = by
        .into_iter()
        .map(|(k, (g, age))| {
            let over = g.iter().filter(|x| **x >= trigger).count() as f64 / g.len().max(1) as f64;
            (k, (Dist::of(g), over, Dist::of(age).p50))
        })
        .collect();

    let mut groups: BTreeMap<String, Vec<&Episode>> = BTreeMap::new();
    for e in eps {
        groups.entry(format!("{} {}", e.kind, e.dex)).or_default().push(e);
        groups.entry(format!("{} (all)", e.kind)).or_default().push(e);
    }
    for (k, v) in groups {
        let mut s = EpisodeStats { n: v.len(), ..Default::default() };
        let (mut mid, mut touch, mut after, mut dur, mut qms) = (vec![], vec![], vec![], vec![], vec![]);
        let mut marks: BTreeMap<u32, (f64, f64, usize)> = BTreeMap::new();
        for e in &v {
            if e.confirm_err.is_some() {
                s.errors += 1;
            }
            if let Some(g) = e.exec_gap_bps {
                s.quoted += 1;
                mid.push(g);
                let size = e.size.unwrap_or(0).max(1);
                after.push(g - bps(FIXED_LAMPORTS, size));
            }
            if let Some(g) = e.exec_gap_touch_bps {
                touch.push(g);
            }
            if let Some(ms) = e.confirm_ms {
                qms.push(ms as f64);
            }
            if let Some(end) = e.end_ts
                && e.kind == "trigger"
            {
                dur.push((end - e.start_ts) as f64 / 1e6);
            }
            // markouts: did the CEX move toward the pool, or the pool toward the CEX?
            let pool0 = e.cex_mid * (1.0 + e.start_gap_bps / 1e4);
            let dir = (pool0 - e.cex_mid).signum();
            if let Some(m) = e.markouts.as_deref().and_then(|m| serde_json::from_str::<Vec<serde_json::Value>>(m).ok())
            {
                for x in m {
                    let (Some(sec), Some(c)) = (x["s"].as_u64(), x["cex_mid"].as_f64()) else { continue };
                    let cex_move = dir * (c - e.cex_mid) / e.cex_mid * 1e4;
                    let pool_move = x["pool_mid"].as_f64().map(|p| -dir * (p - pool0) / pool0 * 1e4).unwrap_or(0.0);
                    let t = marks.entry(sec as u32).or_default();
                    t.0 += cex_move;
                    t.1 += pool_move;
                    t.2 += 1;
                }
            }
        }
        s.exec_vs_mid = Dist::of(mid);
        s.exec_vs_touch = Dist::of(touch);
        s.exec_after_fixed = Dist::of(after);
        s.duration_s = Dist::of(dur);
        s.quote_ms = Dist::of(qms);
        s.markouts = marks.into_iter().map(|(k, (c, p, n))| (k, (c / n as f64, p / n as f64, n))).collect();
        r.lag_episodes.insert(k, s);
    }
}

fn f(v: Option<f64>) -> String {
    v.map(|x| format!("{x:+.2}")).unwrap_or_else(|| "-".into())
}

fn dist_cols(d: &Dist) -> String {
    format!(
        "{:>6} {:>6}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
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

const HEAD: &str = "     n   >0          p5      p25      p50      p75      p95      max";

pub fn render(r: &Report) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "RESEARCH REPORT · {} run(s) · {:.1} h awake", r.runs.len(), r.awake_s as f64 / 3600.0);
    for s in &r.setup {
        let _ = writeln!(o, "  {s}");
    }
    let _ = writeln!(
        o,
        "  Jupiter: {} requests · {} errors · {} rate-limited (429)\n",
        r.jupiter_requests, r.jupiter_errors, r.jupiter_429
    );

    let _ = writeln!(o, "1. SIZE LADDER — quoted round trips (bp of input)");
    let _ =
        writeln!(o, "   gross = final output − input; after fixed = − base fee − min tip − provider priority estimate");
    let _ = writeln!(o, "   size SOL           {HEAD}");
    for (size, (g, n, err)) in &r.ladder {
        let _ = writeln!(o, "   {:>8} gross      {}   errors {err}", *size as f64 / 1e9, dist_cols(g));
        let _ = writeln!(o, "   {:>8} after fixed{}", "", dist_cols(n));
    }
    if let Some(b) = &r.ladder_best {
        let _ = writeln!(o, "   best sample: {b}");
    }
    let _ =
        writeln!(o, "   Quotes are not fills: the audit measured executed outputs 1.16 bp below quotes on average.\n");

    let _ =
        writeln!(o, "2. CROSS-CHAIN — same asset, Solana vs EVM chain (bp of cost, after swap fees, Solana fees, gas)");
    let _ = writeln!(o, "   A = buy on Solana, sell on the chain · B = buy on the chain, sell on Solana");
    let _ = writeln!(o, "   Not included: bridging to rebalance inventory, price risk between the two legs.");
    let _ = writeln!(o, "   market                  {HEAD}");
    for (k, (a, b, err, skew)) in &r.xchain {
        let _ = writeln!(o, "   {k:<18} A {}   errors {err}", dist_cols(a));
        let _ = writeln!(
            o,
            "   {:<18} B {}   quotes span p50 {} ms",
            "",
            dist_cols(b),
            skew.map(|s| format!("{s:.0}")).unwrap_or("-".into())
        );
    }
    let _ = writeln!(o, "   EVM prices are Uniswap v3 only (other DEXes on those chains are not asked).\n");

    let _ = writeln!(o, "3. DEX LAG vs CEX (SOL/USDC)");
    let _ = writeln!(
        o,
        "   |pool mid − CEX mid| every 5 s (bp; mids are not executable) · trigger {} bp",
        r.trigger_bps.map(|t| format!("{t}")).unwrap_or("-".into())
    );
    let _ = writeln!(o, "   dex                     {HEAD}   ≥trigger  pool age p50");
    for (dex, (d, over, age)) in &r.lag_gap {
        let _ = writeln!(
            o,
            "   {dex:<20}    {}   {:>6.1} %  {} ms",
            dist_cols(d),
            over * 100.0,
            age.map(|a| format!("{a:.0}")).unwrap_or("-".into())
        );
    }
    let _ =
        writeln!(o, "\n   Episodes: one Jupiter quote on that DEX (0.1 SOL by default), compared with the CEX price.");
    let _ =
        writeln!(o, "   control = the same quote at a random time: the trigger is useful only if it beats control.");
    for (k, s) in &r.lag_episodes {
        let _ = writeln!(o, "   {k}: {} episodes · {} quoted · {} quote errors", s.n, s.quoted, s.errors);
        let _ = writeln!(o, "      vs CEX mid     {}", dist_cols(&s.exec_vs_mid));
        let _ = writeln!(o, "      vs CEX touch   {}", dist_cols(&s.exec_vs_touch));
        let _ = writeln!(o, "      − fixed cost   {}", dist_cols(&s.exec_after_fixed));
        if s.duration_s.n > 0 {
            let _ = writeln!(o, "      duration s     {}", dist_cols(&s.duration_s));
        }
        for (sec, (c, p, n)) in &s.markouts {
            let _ = writeln!(
                o,
                "      +{sec:>2} s: CEX moved {c:+.2} bp toward the pool, pool moved {p:+.2} bp toward the CEX (n {n})"
            );
        }
    }
    let _ = writeln!(
        o,
        "\n   Pool mids arrive every few seconds on public RPC (see pool age); durations have that resolution."
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_count_everything() {
        let d = Dist::of(vec![-3.0, -1.0, 0.0, 2.0, f64::NAN]);
        assert_eq!((d.n, d.positive), (4, 1));
        assert_eq!(d.p50, Some(-1.0));
        assert_eq!(d.max, Some(2.0));
        assert_eq!(Dist::of(vec![]).p50, None);
    }

    #[test]
    fn report_over_an_empty_run_renders() {
        let s = ResearchStore::open_in_memory().unwrap();
        s.begin_run("r", 0, "0.2.0", r#"{"os":"macos","research":{"lag_trigger_bps":4.0}}"#).unwrap();
        s.insert_ladder("r", &LadderRow { size: 100_000_000, gross: Some(-20_000), ..Default::default() }).unwrap();
        let r = build(&s, &["r".to_string()]).unwrap();
        let text = render(&r);
        assert!(text.contains("SIZE LADDER"), "{text}");
        let (g, _, _) = &r.ladder[&100_000_000];
        assert_eq!(g.p50, Some(-2.0));
        assert_eq!(r.trigger_bps, Some(4.0));
    }
}
