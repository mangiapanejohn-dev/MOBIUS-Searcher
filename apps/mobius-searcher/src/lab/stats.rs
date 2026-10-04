//! What an experiment did, said in full: the return and the worst fall, how
//! much of the time it simply held SOL and what that alone would have earned,
//! every trade's average with its uncertainty, the costs, each month.

use super::rules::{Account, Costs};
use serde::Serialize;

/// An experiment at the close of one bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub ts: i64,
    pub close: f64,
    pub equity: f64,
    pub sol_value: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Stats {
    pub name: String,
    pub bars: usize,
    /// Final value over the capital, less one: open lots at the last close, less the cost of selling them.
    pub ret: f64,
    /// Deepest fall of the equity from an earlier high.
    pub max_fall: f64,
    /// Average share of the equity held in SOL.
    pub exposure: f64,
    /// A portfolio that always held that share in SOL and never traded: what
    /// the market alone gave for the same exposure.
    pub same_exposure: f64,
    /// `ret` less `same_exposure`: what the rule added or took.
    pub excess: f64,
    /// The same difference day by day, basis points a day: mean and its 95 %
    /// interval from resampling days. An interval around zero: over these
    /// bars the rule cannot be told from holding that much SOL.
    pub excess_daily_bps: Option<(f64, f64, f64)>,
    pub trades: usize,
    pub open_lots: usize,
    /// What the lots still held would bring now, after the cost of selling,
    /// over what they cost. The trade columns count closed trades only.
    pub open_result: Option<f64>,
    /// Share of the trades that came back with more than they cost.
    pub won: Option<f64>,
    /// Net of a trade over what it paid in, basis points: mean and median.
    pub trade_mean_bps: Option<f64>,
    pub trade_median_bps: Option<f64>,
    /// 95 % interval of the mean, resampling whole days (trades of a day move together).
    pub trade_ci_bps: Option<(f64, f64)>,
    pub costs_usd: f64,
    /// Costs paid over the capital.
    pub costs_share: f64,
    /// USD traded over the capital.
    pub turnover: f64,
    /// Return within each UTC month the experiment saw.
    pub months: Vec<(String, f64)>,
    /// First and second half of the bars.
    pub halves: Option<(f64, f64)>,
    pub frozen: bool,
}

/// Year, month, day (UTC) of a time in ms.
pub fn ymd(ms: i64) -> (i64, u32, u32) {
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = ((doy - (153 * mp + 2) / 5 + 1) as u32, (if mp < 10 { mp + 3 } else { mp - 9 }) as u32);
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub fn date(ms: i64) -> String {
    let (y, m, d) = ymd(ms);
    format!("{y}-{m:02}-{d:02}")
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// The same numbers on every run: the interval is part of a report, not a draw.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

pub fn stats(name: &str, capital: f64, costs: &Costs, points: &[Point], acct: &Account) -> Stats {
    let n = points.len();
    let last = points.last().copied().unwrap_or(Point { ts: 0, close: 0.0, equity: capital, sol_value: 0.0 });
    // what is still held is worth its price less the cost of selling it
    let exit: f64 = acct.lots.iter().map(|l| costs.of(l.sol * last.close, last.close)).sum();
    let end = last.equity - exit;
    let (mut peak, mut max_fall) = (capital, 0.0f64);
    for p in points {
        peak = peak.max(p.equity);
        max_fall = max_fall.min(p.equity / peak - 1.0);
    }
    let exposure = if n > 0 { points.iter().map(|p| p.sol_value / p.equity).sum::<f64>() / n as f64 } else { 0.0 };
    let same_exposure =
        points.windows(2).map(|w| 1.0 + exposure * (w[1].close / w[0].close - 1.0)).product::<f64>() - 1.0;
    let ret = end / capital - 1.0;
    // the rule against the same exposure, one UTC day at a time
    let mut days_eq: Vec<(f64, f64)> = Vec::new(); // equity and close at each day's last bar
    for (i, p) in points.iter().enumerate() {
        if points.get(i + 1).is_none_or(|q| q.ts.div_euclid(86_400_000) != p.ts.div_euclid(86_400_000)) {
            days_eq.push((p.equity, p.close));
        }
    }
    let daily: Vec<f64> =
        days_eq.windows(2).map(|w| (w[1].0 / w[0].0 - 1.0) - exposure * (w[1].1 / w[0].1 - 1.0)).collect();
    let excess_daily_bps = (daily.len() >= 20).then(|| {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut means: Vec<f64> = (0..2_000)
            .map(|_| (0..daily.len()).map(|_| daily[rng.below(daily.len())]).sum::<f64>() / daily.len() as f64)
            .collect();
        means.sort_by(|a, b| a.total_cmp(b));
        let mean = daily.iter().sum::<f64>() / daily.len() as f64;
        (mean * 1e4, percentile(&means, 0.025) * 1e4, percentile(&means, 0.975) * 1e4)
    });
    let held: f64 = acct.lots.iter().map(|l| l.usd).sum();
    let open_result = (held > 0.0).then(|| (acct.sol() * last.close - exit) / held - 1.0);

    let mut bps: Vec<f64> = acct.trades.iter().map(|t| t.net / t.usd * 1e4).collect();
    let trades = bps.len();
    let mean = (trades > 0).then(|| bps.iter().sum::<f64>() / trades as f64);
    let won = (trades > 0).then(|| bps.iter().filter(|b| **b > 0.0).count() as f64 / trades as f64);
    // the mean's interval: days drawn again with all their trades
    let mut days: Vec<(i64, f64, f64)> = Vec::new();
    for (t, b) in acct.trades.iter().zip(&bps) {
        let day = t.closed.div_euclid(86_400_000);
        match days.last_mut() {
            Some(d) if d.0 == day => {
                d.1 += b;
                d.2 += 1.0;
            }
            _ => days.push((day, *b, 1.0)),
        }
    }
    let ci = (trades >= 20 && days.len() >= 5).then(|| {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut means: Vec<f64> = (0..2_000)
            .map(|_| {
                let (mut s, mut k) = (0.0, 0.0);
                for _ in 0..days.len() {
                    let d = days[rng.below(days.len())];
                    s += d.1;
                    k += d.2;
                }
                s / k
            })
            .collect();
        means.sort_by(|a, b| a.total_cmp(b));
        (percentile(&means, 0.025), percentile(&means, 0.975))
    });
    bps.sort_by(|a, b| a.total_cmp(b));
    let median = (trades > 0).then(|| percentile(&bps, 0.5));

    let mut months: Vec<(String, f64)> = Vec::new();
    let mut start = capital;
    for (i, p) in points.iter().enumerate() {
        let (y, m, _) = ymd(p.ts);
        let last_of_month = points.get(i + 1).is_none_or(|q| {
            let (qy, qm, _) = ymd(q.ts);
            (qy, qm) != (y, m)
        });
        if last_of_month {
            months.push((format!("{y}-{m:02}"), p.equity / start - 1.0));
            start = p.equity;
        }
    }
    let halves = (n >= 4).then(|| {
        let mid = points[n / 2].equity;
        (mid / capital - 1.0, last.equity / mid - 1.0)
    });
    Stats {
        name: name.to_string(),
        bars: n,
        ret,
        max_fall,
        exposure,
        same_exposure,
        excess: ret - same_exposure,
        excess_daily_bps,
        trades,
        open_lots: acct.lots.len(),
        open_result,
        won,
        trade_mean_bps: mean,
        trade_median_bps: median,
        trade_ci_bps: ci,
        costs_usd: acct.costs_paid,
        costs_share: acct.costs_paid / capital,
        turnover: acct.turnover / capital,
        months,
        halves,
        frozen: acct.frozen,
    }
}

fn pct(x: f64) -> String {
    format!("{:+.1} %", x * 100.0)
}

/// The table of a set of experiments over the same bars.
pub fn render(title: &str, all: &[Stats]) -> String {
    let mut out = format!("{title}\n\n");
    let w = all.iter().map(|s| s.name.chars().count()).max().unwrap_or(4).max(10);
    out += &format!(
        "{:<w$}  {:>9}  {:>9}  {:>6}  {:>9}  {:>9}  {:<26}  {:>6}  {:>5}  {:<30}  {:>8}  {:>11}\n",
        "experiment",
        "return",
        "max fall",
        "in SOL",
        "same SOL",
        "rule adds",
        "… a day [95 %]",
        "trades",
        "won",
        "net a closed trade [95 %]",
        "costs",
        "worst month"
    );
    for s in all {
        let trade = match (s.trade_mean_bps, s.trade_ci_bps) {
            (Some(m), Some((lo, hi))) => format!("{m:+.2} bp [{lo:+.2}, {hi:+.2}]"),
            (Some(m), None) => format!("{m:+.2} bp [too few trades]"),
            _ => "—".into(),
        };
        let worst = s.months.iter().map(|m| m.1).fold(f64::NAN, f64::min);
        let daily = match s.excess_daily_bps {
            Some((m, lo, hi)) => format!("{m:+.1} bp [{lo:+.1}, {hi:+.1}]"),
            None => "too few days".into(),
        };
        out += &format!(
            "{:<w$}  {:>9}  {:>9}  {:>5.0}%  {:>9}  {:>9}  {:<26}  {:>6}  {:>5}  {:<30}  {:>8}  {:>11}\n",
            s.name,
            pct(s.ret),
            pct(s.max_fall),
            s.exposure * 100.0,
            pct(s.same_exposure),
            pct(s.excess),
            daily,
            s.trades,
            s.won.map_or("—".into(), |x| format!("{:.0}%", x * 100.0)),
            trade,
            pct(-s.costs_share),
            if worst.is_nan() { "—".into() } else { pct(worst) },
        );
    }
    out += "\n";
    for s in all {
        let months: Vec<String> = s.months.iter().map(|m| format!("{:+.0}", m.1 * 100.0)).collect();
        out += &format!("{:<w$}  by month, %: {}", s.name, months.join(" "));
        if let Some((a, b)) = s.halves {
            out += &format!("  ·  first half {}, second half {}", pct(a), pct(b));
        }
        if let Some(r) = s.open_result {
            out += &format!("  ·  {} lot(s) still held, worth {} of what they cost", s.open_lots, pct(r));
        }
        if s.frozen {
            out += "  ·  FROZEN by its total-loss stop";
        }
        out += "\n";
    }
    out += "\nHow to read it. `in SOL` is the average share of the account held in SOL; `same SOL` is what a\n\
            portfolio always holding that share, never trading, returned over the same bars; `rule adds` is the\n\
            difference: what the rule did beyond carrying that much SOL, and `… a day` is that difference per\n\
            day with its uncertainty. An interval that includes zero means these bars do not tell the rule\n\
            from simply holding that much SOL; one path of one coin proves little either way.\n\
            `net a closed trade` is after every cost, over what the trade paid in. It leaves out what is still\n\
            held: a grid's closed trades all win, its losses sit in the lots it has not sold.\n\
            Fills in a backtest are an assumption (the next bar's open); nothing here was sent anywhere.\n";
    if all.len() > 1 {
        out += &format!(
            "{} experiments share these bars: the best of several looks better than it is, and none of the\n\
             intervals above knows that the others were tried.\n",
            all.len()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::rules::Trade;

    #[test]
    fn civil_dates_from_milliseconds() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_791_072_000_000), "2026-10-04");
        assert_eq!(ymd(951_782_400_000), (2000, 2, 29));
        assert_eq!(ymd(-86_400_000), (1969, 12, 31));
    }

    fn points(equity: &[f64], sol_share: f64, closes: &[f64]) -> Vec<Point> {
        equity
            .iter()
            .zip(closes)
            .enumerate()
            .map(|(i, (e, c))| Point {
                ts: i as i64 * 86_400_000 * 20,
                close: *c,
                equity: *e,
                sol_value: e * sol_share,
            })
            .collect()
    }

    #[test]
    fn return_fall_and_the_same_exposure_benchmark() {
        // half in SOL throughout while SOL goes 100 → 80 → 120: the benchmark is half of each move, compounded
        let p = points(&[100.0, 90.0, 110.0], 0.5, &[100.0, 80.0, 120.0]);
        let s = stats("x", 100.0, &Costs::default(), &p, &Account::new(100.0));
        assert!((s.ret - 0.10).abs() < 1e-12);
        assert!((s.max_fall + 0.10).abs() < 1e-12);
        assert!((s.exposure - 0.5).abs() < 1e-12);
        assert!((s.same_exposure - (0.9 * 1.25 - 1.0)).abs() < 1e-12, "{}", s.same_exposure);
        assert!((s.excess - (0.10 - 0.125)).abs() < 1e-12, "it did worse than just holding half: {}", s.excess);
        // three points 20 days apart: January, January, February
        assert_eq!(s.months.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(), ["1970-01", "1970-02"]);
        assert!((s.months[0].1 + 0.10).abs() < 1e-12 && (s.months[1].1 - (110.0 / 90.0 - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn what_is_still_held_is_marked_less_the_cost_of_selling_it() {
        let mut a = Account::new(0.0);
        a.lots.push(crate::lab::rules::Lot { price: 100.0, sol: 1.0, usd: 100.0, opened: 0 });
        let p = [Point { ts: 0, close: 100.0, equity: 100.0, sol_value: 100.0 }];
        let costs = Costs { route_bps: 10.0, fixed_fee_lamports: 0.0 };
        let s = stats("x", 100.0, &costs, &p, &a);
        assert!((s.ret + 0.001).abs() < 1e-12, "{}", s.ret);
        assert_eq!(s.open_lots, 1);
        assert!((s.open_result.unwrap() + 0.001).abs() < 1e-12, "the lot would bring its cost less the selling cost");
    }

    #[test]
    fn the_interval_of_the_trade_mean_needs_enough_trades_and_days() {
        let trade = |day: i64, net: f64| Trade { opened: 0, closed: day * 86_400_000, usd: 100.0, net };
        let mut a = Account::new(100.0);
        a.trades = (0..10).map(|d| trade(d, 0.01)).collect();
        let none = stats("x", 100.0, &Costs::default(), &[], &a);
        assert_eq!(none.trade_ci_bps, None, "ten trades: no interval");
        assert!((none.trade_mean_bps.unwrap() - 1.0).abs() < 1e-9);
        // 40 trades over 20 days, +3 bp and −1 bp on alternate days: mean +1 bp, interval around it
        a.trades = (0..40).map(|i| trade(i / 2, if (i / 2) % 2 == 0 { 0.03 } else { -0.01 })).collect();
        let s = stats("x", 100.0, &Costs::default(), &[], &a);
        let (lo, hi) = s.trade_ci_bps.unwrap();
        assert!(lo < 1.0 && hi > 1.0 && lo > -1.0 && hi < 3.0, "{lo} {hi}");
        assert_eq!(s.won, Some(0.5));
        // the same report every time
        assert_eq!(s, stats("x", 100.0, &Costs::default(), &[], &a));
    }

    #[test]
    fn the_daily_difference_from_the_same_exposure_has_an_interval() {
        // 40 days, always half in SOL, SOL flat: the account gains 10 bp on even days and loses 10 bp on odd ones
        let mut eq = 100.0;
        let p: Vec<Point> = (0..40)
            .map(|d| {
                eq *= if d % 2 == 0 { 1.001 } else { 0.999 };
                Point { ts: d * 86_400_000, close: 100.0, equity: eq, sol_value: eq * 0.5 }
            })
            .collect();
        let s = stats("x", 100.0, &Costs::default(), &p, &Account::new(100.0));
        let (mean, lo, hi) = s.excess_daily_bps.unwrap();
        assert!(mean.abs() < 0.6 && lo < 0.0 && hi > 0.0, "{mean} [{lo}, {hi}]: nothing to tell from holding");
        // a steady 5 bp a day over the same exposure is told apart
        let p: Vec<Point> = (0..40)
            .map(|d| Point {
                ts: d * 86_400_000,
                close: 100.0,
                equity: 100.0 * 1.0005f64.powi(d as i32),
                sol_value: 50.0,
            })
            .collect();
        let (mean, lo, _) = stats("x", 100.0, &Costs::default(), &p, &Account::new(100.0)).excess_daily_bps.unwrap();
        assert!((mean - 5.0).abs() < 0.01 && lo > 4.9, "{mean} {lo}");
    }

    #[test]
    fn the_table_names_every_experiment_and_says_when_trades_are_too_few() {
        let mut a = Account::new(100.0);
        a.trades = vec![Trade { opened: 0, closed: 1, usd: 100.0, net: 0.5 }];
        let p = points(&[100.0, 100.5], 0.0, &[100.0, 101.0]);
        let out = render("LAB", &[stats("reversal", 100.0, &Costs::default(), &p, &a)]);
        assert!(out.contains("reversal") && out.contains("too few trades") && out.contains("rule adds"), "{out}");
        assert!(out.contains("too few days"), "two points are not twenty days: {out}");
    }
}
