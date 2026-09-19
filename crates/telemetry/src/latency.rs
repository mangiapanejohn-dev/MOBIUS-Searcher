//! Latency book: named distributions measured with monotonic clocks, plus
//! counters. Recording is one short mutex push; each series keeps its first
//! `MAX_SAMPLES` values (a benchmark run stays far below that). Units are in
//! the series name (`…_us`, `…_ms`, `…_slots`, `…_bp`).

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const MAX_SAMPLES: usize = 1_000_000;

#[derive(Debug)]
pub struct LatencyBook {
    series: Mutex<BTreeMap<String, Vec<i64>>>,
    counters: Mutex<BTreeMap<String, u64>>,
    started: Instant,
}

impl Default for LatencyBook {
    fn default() -> Self {
        Self { series: Mutex::new(BTreeMap::new()), counters: Mutex::new(BTreeMap::new()), started: Instant::now() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub count: usize,
    pub mean: f64,
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub max: i64,
    pub min: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LatencyReport {
    pub elapsed_s: f64,
    pub series: BTreeMap<String, Stats>,
    pub counters: BTreeMap<String, u64>,
}

/// Nearest-rank percentile of a sorted slice.
fn pct(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

pub fn stats(values: &[i64]) -> Stats {
    if values.is_empty() {
        return Stats::default();
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    Stats {
        count: v.len(),
        mean: v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64,
        p50: pct(&v, 50.0),
        p95: pct(&v, 95.0),
        p99: pct(&v, 99.0),
        max: *v.last().unwrap_or(&0),
        min: v[0],
    }
}

impl LatencyBook {
    pub fn value(&self, name: &str, v: i64) {
        let mut g = self.series.lock();
        let s = g.entry(name.to_string()).or_default();
        if s.len() < MAX_SAMPLES {
            s.push(v);
        }
    }

    pub fn duration_us(&self, name: &str, d: Duration) {
        self.value(name, d.as_micros().min(i64::MAX as u128) as i64);
    }

    /// Record `now − start` in microseconds.
    pub fn since_us(&self, name: &str, start: Instant) {
        self.duration_us(name, start.elapsed());
    }

    pub fn count(&self, name: &str, n: u64) {
        *self.counters.lock().entry(name.to_string()).or_default() += n;
    }

    pub fn counter(&self, name: &str) -> u64 {
        self.counters.lock().get(name).copied().unwrap_or(0)
    }

    pub fn report(&self) -> LatencyReport {
        let series = self.series.lock().iter().map(|(k, v)| (k.clone(), stats(v))).collect();
        LatencyReport {
            elapsed_s: self.started.elapsed().as_secs_f64(),
            series,
            counters: self.counters.lock().clone(),
        }
    }
}

impl LatencyReport {
    /// Aligned text table; `_us` series are shown in ms.
    pub fn render(&self) -> String {
        let mut out = format!(
            "LATENCY ({:.0} s)\n  {:<44} {:>7} {:>9} {:>9} {:>9} {:>9}\n",
            self.elapsed_s, "series", "n", "p50", "p95", "p99", "max"
        );
        for (k, s) in &self.series {
            let (name, f): (String, fn(i64) -> String) = match k.strip_suffix("_us") {
                Some(base) => (format!("{base}_ms"), |v| format!("{:.1}", v as f64 / 1000.0)),
                None => (k.clone(), |v| v.to_string()),
            };
            out.push_str(&format!(
                "  {:<44} {:>7} {:>9} {:>9} {:>9} {:>9}\n",
                name,
                s.count,
                f(s.p50),
                f(s.p95),
                f(s.p99),
                f(s.max)
            ));
        }
        out.push_str("  counters\n");
        for (k, v) in &self.counters {
            out.push_str(&format!("  {k:<44} {v:>7}  ({:.3}/s)\n", *v as f64 / self.elapsed_s.max(1e-9)));
        }
        out
    }
}

/// Tokio scheduling lag: how late a 50 ms sleep wakes up (the async
/// equivalent of event-loop delay).
pub async fn run_runtime_lag_probe(
    book: std::sync::Arc<LatencyBook>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let period = Duration::from_millis(50);
    loop {
        let t = Instant::now();
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(period) => book.duration_us("runtime.lag_us", t.elapsed().saturating_sub(period)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_report() {
        let b = LatencyBook::default();
        for v in 1..=100 {
            b.value("x_us", v * 1000);
        }
        b.count("req", 5);
        let r = b.report();
        let s = &r.series["x_us"];
        assert_eq!((s.count, s.p50, s.p95, s.p99, s.max, s.min), (100, 50_000, 95_000, 99_000, 100_000, 1000));
        assert!(r.render().contains("x_ms"), "µs series render in ms");
        assert_eq!(b.counter("req"), 5);
        assert_eq!(stats(&[]), Stats::default());
    }
}
