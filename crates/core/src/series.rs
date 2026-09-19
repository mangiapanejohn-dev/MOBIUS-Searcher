//! Display-side time series (f64 values are fine here: this is chart data, not
//! accounting) and OHLC bucketing from real samples.

use crate::time::Ts;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct TimeSeries {
    points: VecDeque<(Ts, f64)>,
    cap: usize,
}

impl Default for TimeSeries {
    fn default() -> Self {
        Self::with_capacity(50_000)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ohlc {
    pub start: Ts,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub samples: u32,
}

impl TimeSeries {
    pub fn with_capacity(cap: usize) -> Self {
        Self { points: VecDeque::with_capacity(cap.min(4096)), cap: cap.max(1) }
    }

    /// Append; out-of-order points are inserted in place (replay safety).
    pub fn push(&mut self, ts: Ts, v: f64) {
        if !v.is_finite() {
            return;
        }
        if self.points.len() == self.cap {
            self.points.pop_front();
        }
        match self.points.back() {
            Some((last, _)) if *last > ts => {
                let idx = self.points.partition_point(|(t, _)| *t <= ts);
                self.points.insert(idx, (ts, v));
            }
            _ => self.points.push_back((ts, v)),
        }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn last(&self) -> Option<(Ts, f64)> {
        self.points.back().copied()
    }

    pub fn first(&self) -> Option<(Ts, f64)> {
        self.points.front().copied()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &(Ts, f64)> {
        self.points.iter()
    }

    /// Points with `t0 <= ts <= t1`.
    pub fn range(&self, t0: Ts, t1: Ts) -> impl Iterator<Item = &(Ts, f64)> {
        let a = self.points.partition_point(|(t, _)| *t < t0);
        let b = self.points.partition_point(|(t, _)| *t <= t1);
        self.points.range(a..b)
    }

    /// Last value at or before `ts` (step interpolation).
    pub fn value_at(&self, ts: Ts) -> Option<(Ts, f64)> {
        let idx = self.points.partition_point(|(t, _)| *t <= ts);
        if idx == 0 { None } else { self.points.get(idx - 1).copied() }
    }

    /// The sample closest in time to `ts`.
    pub fn nearest(&self, ts: Ts) -> Option<(Ts, f64)> {
        let idx = self.points.partition_point(|(t, _)| *t < ts);
        let after = self.points.get(idx).copied();
        let before = if idx > 0 { self.points.get(idx - 1).copied() } else { None };
        match (before, after) {
            (Some(b), Some(a)) => Some(if (ts.0 - b.0.0) <= (a.0.0 - ts.0) { b } else { a }),
            (b, a) => b.or(a),
        }
    }

    pub fn min_max(&self, t0: Ts, t1: Ts) -> Option<(f64, f64)> {
        self.range(t0, t1).fold(None, |acc, (_, v)| match acc {
            None => Some((*v, *v)),
            Some((lo, hi)) => Some((lo.min(*v), hi.max(*v))),
        })
    }

    /// OHLC buckets of width `bucket_us` aligned to the epoch, within [t0, t1].
    pub fn ohlc(&self, t0: Ts, t1: Ts, bucket_us: i64) -> Vec<Ohlc> {
        let mut out: Vec<Ohlc> = Vec::new();
        if bucket_us <= 0 {
            return out;
        }
        for (t, v) in self.range(t0, t1) {
            let start = Ts(t.0.div_euclid(bucket_us) * bucket_us);
            match out.last_mut() {
                Some(c) if c.start == start => {
                    c.high = c.high.max(*v);
                    c.low = c.low.min(*v);
                    c.close = *v;
                    c.samples += 1;
                }
                _ => out.push(Ohlc { start, open: *v, high: *v, low: *v, close: *v, samples: 1 }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_ordering_and_cap() {
        let mut s = TimeSeries::with_capacity(3);
        s.push(Ts(10), 1.0);
        s.push(Ts(30), 3.0);
        s.push(Ts(20), 2.0);
        assert_eq!(s.iter().map(|p| p.0.0).collect::<Vec<_>>(), vec![10, 20, 30]);
        s.push(Ts(40), 4.0);
        assert_eq!(s.len(), 3);
        assert_eq!(s.first(), Some((Ts(20), 2.0)));
        s.push(Ts(50), f64::NAN);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn lookups() {
        let mut s = TimeSeries::default();
        for i in 0..10 {
            s.push(Ts(i * 100), i as f64);
        }
        assert_eq!(s.value_at(Ts(250)), Some((Ts(200), 2.0)));
        assert_eq!(s.value_at(Ts(-1)), None);
        assert_eq!(s.nearest(Ts(260)), Some((Ts(300), 3.0)));
        assert_eq!(s.nearest(Ts(240)), Some((Ts(200), 2.0)));
        assert_eq!(s.range(Ts(200), Ts(400)).count(), 3);
        assert_eq!(s.min_max(Ts(200), Ts(400)), Some((2.0, 4.0)));
    }

    #[test]
    fn ohlc_buckets() {
        let mut s = TimeSeries::default();
        for (t, v) in [(0, 5.0), (10, 7.0), (20, 4.0), (30, 6.0), (100, 1.0), (150, 2.0)] {
            s.push(Ts(t), v);
        }
        let c = s.ohlc(Ts(0), Ts(1000), 100);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].open, c[0].high, c[0].low, c[0].close, c[0].samples), (5.0, 7.0, 4.0, 6.0, 4));
        assert_eq!((c[1].open, c[1].close, c[1].start), (1.0, 2.0, Ts(100)));
    }
}
