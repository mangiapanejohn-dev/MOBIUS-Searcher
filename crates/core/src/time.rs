//! Timestamps are unix microseconds (`Ts`). Wall-clock is used for records and
//! the shared UI timeline; `std::time::Instant` is used for latency measurement.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ts(pub i64);

impl Ts {
    pub fn now() -> Ts {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Ts(d.as_micros() as i64)
    }

    pub const fn from_millis(ms: i64) -> Ts {
        Ts(ms * 1_000)
    }

    pub const fn from_secs(s: i64) -> Ts {
        Ts(s * 1_000_000)
    }

    pub fn micros(self) -> i64 {
        self.0
    }

    pub fn millis(self) -> i64 {
        self.0.div_euclid(1_000)
    }

    pub fn secs_f64(self) -> f64 {
        self.0 as f64 / 1e6
    }

    /// Saturating age in milliseconds relative to `now`.
    pub fn age_ms(self, now: Ts) -> u64 {
        (now.0 - self.0).max(0) as u64 / 1_000
    }

    pub fn plus_ms(self, ms: i64) -> Ts {
        Ts(self.0 + ms * 1_000)
    }

    /// Local `HH:MM:SS`.
    pub fn hms(self) -> String {
        self.format("%H:%M:%S")
    }

    /// Local `HH:MM:SS.mmm`.
    pub fn hms_millis(self) -> String {
        self.format("%H:%M:%S%.3f")
    }

    pub fn format(self, fmt: &str) -> String {
        use chrono::TimeZone;
        match chrono::Local.timestamp_micros(self.0) {
            chrono::LocalResult::Single(t) => t.format(fmt).to_string(),
            _ => "--:--:--".into(),
        }
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hms_millis())
    }
}

/// Human duration: `850 µs`, `186 ms`, `42.0 s`, `3m 05s`, `1h 02m`.
pub fn fmt_duration_us(us: i64) -> String {
    let neg = us < 0;
    let us = us.unsigned_abs();
    let s = if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{} ms", us / 1_000)
    } else if us < 60_000_000 {
        format!("{:.1} s", us as f64 / 1e6)
    } else if us < 3_600_000_000 {
        let secs = us / 1_000_000;
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        let mins = us / 60_000_000;
        format!("{}h {:02}m", mins / 60, mins % 60)
    };
    if neg { format!("-{s}") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration_us(186_000), "186 ms");
        assert_eq!(fmt_duration_us(4_200_000), "4.2 s");
        assert_eq!(fmt_duration_us(185_000_000), "3m 05s");
        assert_eq!(fmt_duration_us(3_720_000_000), "1h 02m");
        assert_eq!(fmt_duration_us(-5), "-5 µs");
        assert_eq!(fmt_duration_us(-40_000_000), "-40.0 s");
    }

    #[test]
    fn age() {
        assert_eq!(Ts(1_000_000).age_ms(Ts(1_042_000)), 42);
        assert_eq!(Ts(2_000_000).age_ms(Ts(1_000_000)), 0);
    }
}
