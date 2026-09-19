//! Per-service health: counters, latency ring, error rate, rate-limit state.

use parking_lot::Mutex;
use searcher_core::model::{ServiceId, ServiceSnapshot, ServiceState};
use searcher_core::{Ppm, Ts};
use std::collections::VecDeque;

const RING: usize = 120;

#[derive(Debug, Default)]
struct Stats {
    requests: u64,
    errors: u64,
    rate_limited: u64,
    consecutive_errors: u32,
    recent: VecDeque<(Ts, u32, bool)>,
    last_success: Option<Ts>,
    last_error: Option<(Ts, String)>,
    backoff_until: Option<Ts>,
    quota_remaining: Option<i64>,
    disabled: bool,
    connected: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Telemetry {
    services: [Mutex<Stats>; 6],
    /// Stage latencies for benchmarks (see `latency`).
    pub latency: std::sync::Arc<crate::latency::LatencyBook>,
}

fn idx(s: ServiceId) -> usize {
    match s {
        ServiceId::Jupiter => 0,
        ServiceId::Rpc => 1,
        ServiceId::WebSocket => 2,
        ServiceId::Jito => 3,
        ServiceId::Db => 4,
        ServiceId::MarketFeed => 5,
    }
}

impl Telemetry {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, s: ServiceId, f: impl FnOnce(&mut Stats) -> R) -> R {
        f(&mut self.services[idx(s)].lock())
    }

    fn push_recent(st: &mut Stats, ts: Ts, latency_ms: u32, ok: bool) {
        if st.recent.len() == RING {
            st.recent.pop_front();
        }
        st.recent.push_back((ts, latency_ms, ok));
    }

    pub fn record_ok(&self, s: ServiceId, latency_ms: u32) {
        let now = Ts::now();
        self.with(s, |st| {
            st.requests += 1;
            st.consecutive_errors = 0;
            st.last_success = Some(now);
            Self::push_recent(st, now, latency_ms, true);
        });
    }

    pub fn record_err(&self, s: ServiceId, latency_ms: Option<u32>, msg: impl Into<String>) {
        let now = Ts::now();
        let msg = msg.into();
        self.with(s, |st| {
            st.requests += 1;
            st.errors += 1;
            st.consecutive_errors += 1;
            st.last_error = Some((now, msg));
            Self::push_recent(st, now, latency_ms.unwrap_or(0), false);
        });
    }

    pub fn record_rate_limited(&self, s: ServiceId, backoff_ms: u64) {
        let now = Ts::now();
        self.with(s, |st| {
            st.requests += 1;
            st.rate_limited += 1;
            st.backoff_until = Some(now.plus_ms(backoff_ms as i64));
            st.last_error = Some((now, format!("429 rate limited; backing off {backoff_ms} ms")));
            Self::push_recent(st, now, 0, false);
        });
    }

    pub fn set_quota(&self, s: ServiceId, remaining: i64) {
        self.with(s, |st| st.quota_remaining = Some(remaining));
    }

    pub fn set_disabled(&self, s: ServiceId, disabled: bool) {
        self.with(s, |st| st.disabled = disabled);
    }

    /// For connection-oriented services (WebSocket).
    pub fn set_connected(&self, s: ServiceId, connected: bool) {
        self.with(s, |st| st.connected = Some(connected));
    }

    pub fn snapshot(&self, s: ServiceId, now: Ts) -> ServiceSnapshot {
        self.with(s, |st| {
            let window: Vec<_> = st.recent.iter().filter(|(t, _, _)| t.age_ms(now) <= 60_000).collect();
            let n = window.len() as i128;
            let errs = window.iter().filter(|(_, _, ok)| !ok).count() as i128;
            let error_rate = Ppm::ratio(errs, n).unwrap_or(Ppm::ZERO);
            let mut lats: Vec<u32> = window.iter().filter(|(_, _, ok)| *ok).map(|(_, l, _)| *l).collect();
            lats.sort_unstable();
            let p50 = lats.get(lats.len() / 2).copied();
            let backoff_until = st.backoff_until.filter(|u| *u > now);
            let state = if st.disabled {
                ServiceState::Disabled
            } else if backoff_until.is_some() {
                ServiceState::RateLimited
            } else if st.connected == Some(false) || st.consecutive_errors >= 3 {
                ServiceState::Down
            } else if n >= 3 && error_rate > Ppm(200_000) {
                ServiceState::Degraded
            } else if st.connected == Some(true) || st.last_success.is_some_and(|t| t.age_ms(now) <= 120_000) {
                ServiceState::Ok
            } else {
                ServiceState::Idle
            };
            ServiceSnapshot {
                service: Some(s),
                state,
                last_latency_ms: st.recent.iter().rev().find(|(_, _, ok)| *ok).map(|(_, l, _)| *l),
                p50_latency_ms: p50,
                requests: st.requests,
                errors: st.errors,
                rate_limited: st.rate_limited,
                error_rate,
                backoff_until,
                last_success: st.last_success,
                last_error: st.last_error.as_ref().map(|(t, m)| format!("{} {m}", t.hms())),
                quota_remaining: st.quota_remaining,
                recent_latency: st.recent.iter().filter(|(_, _, ok)| *ok).map(|(_, l, _)| *l).collect(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states() {
        let t = Telemetry::new();
        let now = Ts::now();
        assert_eq!(t.snapshot(ServiceId::Jupiter, now).state, ServiceState::Idle);
        t.record_ok(ServiceId::Jupiter, 40);
        t.record_ok(ServiceId::Jupiter, 20);
        let s = t.snapshot(ServiceId::Jupiter, Ts::now());
        assert_eq!(s.state, ServiceState::Ok);
        assert_eq!(s.requests, 2);
        assert_eq!(s.p50_latency_ms, Some(40));
        t.record_rate_limited(ServiceId::Jupiter, 5_000);
        let s = t.snapshot(ServiceId::Jupiter, Ts::now());
        assert_eq!(s.state, ServiceState::RateLimited);
        assert_eq!(s.rate_limited, 1);
        for _ in 0..3 {
            t.record_err(ServiceId::Rpc, None, "boom");
        }
        assert_eq!(t.snapshot(ServiceId::Rpc, Ts::now()).state, ServiceState::Down);
        t.set_disabled(ServiceId::Jito, true);
        assert_eq!(t.snapshot(ServiceId::Jito, Ts::now()).state, ServiceState::Disabled);
    }
}
