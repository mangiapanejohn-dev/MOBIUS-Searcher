//! One shared timeline for every graph (winproc-tui model): timeframe span,
//! follow-live vs. frozen right edge, a cursor on real sample timestamps,
//! and A/B markers.

use searcher_core::Ts;
use searcher_core::series::TimeSeries;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Timeframe {
    M1,
    M5,
    M15,
    H1,
    Session,
}

impl Timeframe {
    pub const ALL: [Timeframe; 5] = [Timeframe::M1, Timeframe::M5, Timeframe::M15, Timeframe::H1, Timeframe::Session];

    pub fn label(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::H1 => "1h",
            Timeframe::Session => "session",
        }
    }

    pub fn span_us(self) -> Option<i64> {
        match self {
            Timeframe::M1 => Some(60_000_000),
            Timeframe::M5 => Some(300_000_000),
            Timeframe::M15 => Some(900_000_000),
            Timeframe::H1 => Some(3_600_000_000),
            Timeframe::Session => None,
        }
    }

    pub fn wider(self) -> Timeframe {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1).min(Self::ALL.len() - 1)]
    }

    pub fn narrower(self) -> Timeframe {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[i.saturating_sub(1)]
    }
}

#[derive(Clone, Debug)]
pub struct Timeline {
    pub tf: Timeframe,
    pub follow: bool,
    /// Right edge while not following.
    pub right: Ts,
    /// Selected sample time (always a real sample timestamp of the active graph).
    pub cursor: Option<Ts>,
    pub a: Option<Ts>,
    pub b: Option<Ts>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self { tf: Timeframe::M5, follow: true, right: Ts(0), cursor: None, a: None, b: None }
    }
}

impl Timeline {
    /// Visible window `[t0, t1]` given session bounds.
    pub fn window(&self, first: Ts, latest: Ts) -> (Ts, Ts) {
        match self.tf.span_us() {
            None => {
                let t1 = if self.follow { latest } else { self.right.min(latest) };
                (first, t1.max(first.plus_ms(1_000)))
            }
            Some(span) => {
                let t1 = if self.follow { latest } else { self.right };
                (Ts(t1.0 - span), t1)
            }
        }
    }

    fn span_now(&self, first: Ts, latest: Ts) -> i64 {
        let (a, b) = self.window(first, latest);
        (b.0 - a.0).max(1)
    }

    /// Cursor time: explicit cursor, else the latest sample.
    pub fn cursor_ts(&self, series: Option<&TimeSeries>) -> Option<Ts> {
        self.cursor.or_else(|| series.and_then(|s| s.last()).map(|p| p.0))
    }

    /// Move the cursor `steps` samples along `series` (negative = older).
    pub fn step(&mut self, series: &TimeSeries, steps: i64, first: Ts, latest: Ts) {
        if series.is_empty() {
            return;
        }
        let pts: Vec<Ts> = series.iter().map(|p| p.0).collect();
        let cur = self.cursor.unwrap_or(*pts.last().unwrap());
        let idx = pts.partition_point(|t| *t < cur).min(pts.len() - 1) as i64;
        let ni = (idx + steps).clamp(0, pts.len() as i64 - 1) as usize;
        let t = pts[ni];
        if ni == pts.len() - 1 && steps > 0 && self.follow {
            self.cursor = None;
            return;
        }
        let span = self.span_now(first, latest);
        if self.follow {
            self.right = latest;
        }
        self.follow = false;
        self.cursor = Some(t);
        if self.tf.span_us().is_some() {
            if t > self.right {
                self.right = t;
            } else if t.0 < self.right.0 - span {
                self.right = Ts(t.0 + span / 8);
            }
        }
    }

    pub fn home(&mut self, series: &TimeSeries, first: Ts, latest: Ts) {
        if let Some((t, _)) = series.first() {
            let span = self.span_now(first, latest);
            self.follow = false;
            self.cursor = Some(t);
            self.right = Ts(t.0 + span);
        }
    }

    /// End: back to live.
    pub fn end(&mut self) {
        self.follow = true;
        self.cursor = None;
    }

    /// Pan the window by 1/8 span (winproc-style); leaves follow mode.
    pub fn pan(&mut self, dir: i64, first: Ts, latest: Ts) {
        let span = self.span_now(first, latest);
        if self.follow {
            self.right = latest;
        }
        self.follow = false;
        self.right = Ts((self.right.0 + dir * span / 8).clamp(first.0 + span / 4, latest.0));
        if self.right >= latest {
            self.follow = true;
        }
    }

    pub fn mark_a(&mut self, t: Option<Ts>) {
        if t.is_some() {
            self.a = t;
        }
    }

    pub fn mark_b(&mut self, t: Option<Ts>) {
        if t.is_some() {
            self.b = t;
        }
    }

    pub fn clear_marks(&mut self) {
        self.a = None;
        self.b = None;
    }

    /// (earlier, later) of A/B if both set.
    pub fn ab(&self) -> Option<(Ts, Ts)> {
        match (self.a, self.b) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> TimeSeries {
        let mut s = TimeSeries::default();
        for i in 0..100 {
            s.push(Ts(i * 1_000_000), i as f64);
        }
        s
    }

    #[test]
    fn window_follow_and_frozen() {
        let mut t = Timeline::default();
        assert_eq!(t.window(Ts(0), Ts(400_000_000)), (Ts(100_000_000), Ts(400_000_000)));
        t.tf = Timeframe::Session;
        assert_eq!(t.window(Ts(5), Ts(400_000_000)), (Ts(5), Ts(400_000_000)));
    }

    #[test]
    fn cursor_moves_on_real_samples_and_freezes_window() {
        let s = series();
        let mut t = Timeline { tf: Timeframe::M1, ..Default::default() };
        let latest = Ts(99_000_000);
        t.step(&s, -3, Ts(0), latest);
        assert_eq!(t.cursor, Some(Ts(96_000_000)));
        assert!(!t.follow);
        // new data must not scroll the frozen window
        let w1 = t.window(Ts(0), latest);
        let w2 = t.window(Ts(0), Ts(200_000_000));
        assert_eq!(w1, w2);
        // walking far left shifts the window to keep the cursor visible
        t.step(&s, -80, Ts(0), latest);
        let (a, b) = t.window(Ts(0), latest);
        let c = t.cursor.unwrap();
        assert!(a <= c && c <= b, "{a:?} {c:?} {b:?}");
        t.end();
        assert!(t.follow && t.cursor.is_none());
    }

    #[test]
    fn ab_marks() {
        let mut t = Timeline::default();
        t.mark_a(Some(Ts(1)));
        assert!(t.ab().is_none());
        t.mark_b(Some(Ts(5)));
        assert_eq!(t.ab(), Some((Ts(1), Ts(5))));
        t.mark_b(None);
        assert_eq!(t.b, Some(Ts(5)), "marking without a cursor is a no-op");
        t.clear_marks();
        assert!(t.a.is_none() && t.b.is_none());
    }

    #[test]
    fn timeframe_ladder() {
        assert_eq!(Timeframe::M1.narrower(), Timeframe::M1);
        assert_eq!(Timeframe::H1.wider(), Timeframe::Session);
        assert_eq!(Timeframe::Session.wider(), Timeframe::Session);
    }
}
