//! Token bucket + 429 backoff. There is no "retry loop" here: callers ask for
//! permission, make exactly one request, and report the outcome. After a 429
//! the bucket is drained and the limiter refuses until the backoff elapses.

use parking_lot::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct LimiterConfig {
    pub rps: f64,
    pub burst: u32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl LimiterConfig {
    pub fn new(rps: f64, burst: u32) -> Self {
        Self {
            rps,
            burst: burst.max(1),
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
struct Inner {
    cfg: LimiterConfig,
    tokens: f64,
    last_refill: Instant,
    backoff_until: Option<Instant>,
    attempt: u32,
    total_429: u64,
}

#[derive(Debug)]
pub struct RateLimiter {
    name: &'static str,
    inner: Mutex<Inner>,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LimiterState {
    pub tokens: f64,
    pub backoff_remaining: Option<Duration>,
    pub attempt: u32,
    pub total_429: u64,
}

/// Exponential backoff with "equal jitter": `exp/2 + jitter·exp/2`, where
/// `exp = min(cap, base·2^attempt)`, never less than the server's reset hint.
/// `jitter` ∈ [0, 1).
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration, hint: Option<Duration>, jitter: f64) -> Duration {
    let exp = base.saturating_mul(1u32 << attempt.min(16)).min(cap);
    let half = exp / 2;
    let j = jitter.clamp(0.0, 0.999_999);
    let d = half + half.mul_f64(j);
    match hint {
        Some(h) => d.max(h.min(cap)),
        None => d,
    }
}

impl RateLimiter {
    pub fn new(name: &'static str, cfg: LimiterConfig) -> Self {
        let now = Instant::now();
        Self {
            name,
            inner: Mutex::new(Inner {
                tokens: cfg.burst as f64,
                cfg,
                last_refill: now,
                backoff_until: None,
                attempt: 0,
                total_429: 0,
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Take a token now, or return how long to wait.
    pub fn try_acquire(&self, now: Instant) -> Result<(), Duration> {
        let mut g = self.inner.lock();
        if let Some(until) = g.backoff_until {
            if now < until {
                return Err(until - now);
            }
            g.backoff_until = None;
        }
        let elapsed = now.saturating_duration_since(g.last_refill).as_secs_f64();
        g.tokens = (g.tokens + elapsed * g.cfg.rps).min(g.cfg.burst as f64);
        g.last_refill = now;
        if g.tokens >= 1.0 {
            g.tokens -= 1.0;
            Ok(())
        } else {
            let need = (1.0 - g.tokens) / g.cfg.rps.max(1e-9);
            Err(Duration::from_secs_f64(need))
        }
    }

    /// Wait (asynchronously) until a request is permitted.
    pub async fn acquire(&self) {
        loop {
            match self.try_acquire(Instant::now()) {
                Ok(()) => return,
                Err(d) => tokio::time::sleep(d.max(Duration::from_millis(1))).await,
            }
        }
    }

    /// Would a request be permitted without waiting longer than `max_wait`?
    pub fn wait_estimate(&self, now: Instant) -> Duration {
        let g = self.inner.lock();
        if let Some(until) = g.backoff_until
            && now < until
        {
            return until - now;
        }
        let elapsed = now.saturating_duration_since(g.last_refill).as_secs_f64();
        let tokens = (g.tokens + elapsed * g.cfg.rps).min(g.cfg.burst as f64);
        if tokens >= 1.0 { Duration::ZERO } else { Duration::from_secs_f64((1.0 - tokens) / g.cfg.rps.max(1e-9)) }
    }

    /// Report a 429 (or equivalent). Returns the backoff applied.
    pub fn on_rate_limited(&self, now: Instant, hint: Option<Duration>) -> Duration {
        let mut g = self.inner.lock();
        let d = backoff_delay(g.attempt, g.cfg.base_backoff, g.cfg.max_backoff, hint, rand::random::<f64>());
        g.attempt = g.attempt.saturating_add(1);
        g.total_429 += 1;
        g.tokens = 0.0;
        g.last_refill = now;
        let until = now + d;
        g.backoff_until = Some(g.backoff_until.map_or(until, |u| u.max(until)));
        d
    }

    /// Report a successful (non-429) response: resets the backoff exponent.
    pub fn on_success(&self) {
        self.inner.lock().attempt = 0;
    }

    pub fn state(&self, now: Instant) -> LimiterState {
        let g = self.inner.lock();
        LimiterState {
            tokens: g.tokens,
            backoff_remaining: g.backoff_until.filter(|u| *u > now).map(|u| u - now),
            attempt: g.attempt,
            total_429: g.total_429,
        }
    }
}

/// Sliding-window limiter matching Jupiter's gateway: at most `capacity`
/// requests in any `window`. Unlike a token bucket it lets a quiet period be
/// spent as a burst, which is what an event-driven scheduler needs, without
/// ever exceeding the window. The gateway's own `x-ratelimit-*` report is
/// authoritative (it also counts other clients of the same organisation):
/// capacity is learned from `current + remaining`, and a report of
/// `remaining ≤ 0` blocks until `reset`. `safety` and every `reserve` are
/// given for the configured capacity and scale with the learned one, so a
/// smaller tier (keyless: 5 instead of 10) keeps the same proportions instead
/// of leaving nothing for background work.
#[derive(Debug)]
pub struct WindowLimiter {
    name: &'static str,
    inner: Mutex<WindowInner>,
}

#[derive(Debug)]
struct WindowInner {
    /// Window as we account it: the gateway's window plus a margin for
    /// arrival jitter (the gateway counts arrivals, we count sends).
    window: Duration,
    capacity: u32,
    /// Capacity `safety` and reserves are expressed for.
    configured: u32,
    /// Slots never used (other clients, clock skew).
    safety: u32,
    sent: std::collections::VecDeque<Instant>,
    /// Gateway report: `remaining` as of the reporting request's send time;
    /// our sends after that are subtracted.
    server: Option<(Instant, i64)>,
    /// When the gateway's oldest counted request ages out (its `reset`).
    server_reset: Option<Instant>,
    blocked_until: Option<Instant>,
    attempt: u32,
    total_429: u64,
}

impl WindowLimiter {
    /// `window` should include a margin for request arrival jitter.
    pub fn new(name: &'static str, capacity: u32, window: Duration, safety: u32) -> Self {
        Self {
            name,
            inner: Mutex::new(WindowInner {
                window,
                capacity: capacity.max(1),
                configured: capacity.max(1),
                safety,
                sent: Default::default(),
                server: None,
                server_reset: None,
                blocked_until: None,
                attempt: 0,
                total_429: 0,
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Slots kept free: safety + reserve, scaled from the configured to the
    /// learned capacity (rounded down, but a non-zero safety stays ≥ 1).
    fn headroom(g: &WindowInner, reserve: u32) -> u32 {
        let scale = |x: u32| (x as u64 * g.capacity as u64 / g.configured as u64) as u32;
        let safety = if g.safety > 0 { scale(g.safety).max(1) } else { 0 };
        safety + scale(reserve)
    }

    fn prune(g: &mut WindowInner, now: Instant) {
        while g.sent.front().is_some_and(|t| now.saturating_duration_since(*t) >= g.window) {
            g.sent.pop_front();
        }
    }

    /// Requests that may start now while keeping `reserve` slots unused.
    pub fn available(&self, now: Instant, reserve: u32) -> u32 {
        let mut g = self.inner.lock();
        Self::available_locked(&mut g, now, reserve)
    }

    fn available_locked(g: &mut WindowInner, now: Instant, reserve: u32) -> u32 {
        if g.blocked_until.is_some_and(|u| now < u) {
            return 0;
        }
        Self::prune(g, now);
        let local = g.capacity.saturating_sub(Self::headroom(g, reserve)).saturating_sub(g.sent.len() as u32);
        match g.server {
            // the gateway's count is authoritative while it is fresh
            Some((reference, remaining)) if now.saturating_duration_since(reference) < g.window => {
                let ours_since = g.sent.iter().filter(|t| **t > reference).count() as i64;
                // past its `reset` the oldest request the gateway counted has aged out
                let freed = g.server_reset.is_some_and(|r| now >= r) as i64;
                let server = (remaining + freed - ours_since - Self::headroom(g, reserve) as i64).max(0) as u32;
                local.min(server)
            }
            _ => local,
        }
    }

    /// Which limit is binding right now: `backoff`, `local` (our own window
    /// count) or `gateway` (the last `x-ratelimit-*` report); `none` if free.
    pub fn binding(&self, now: Instant, reserve: u32) -> &'static str {
        let mut g = self.inner.lock();
        if g.blocked_until.is_some_and(|u| now < u) {
            return "backoff";
        }
        Self::prune(&mut g, now);
        let local = g.capacity.saturating_sub(Self::headroom(&g, reserve)).saturating_sub(g.sent.len() as u32);
        if local == 0 {
            return "local";
        }
        if Self::available_locked(&mut g, now, reserve) == 0 { "gateway" } else { "none" }
    }

    /// When the next slot frees (for timers), given `reserve`: our own
    /// oldest send ageing out, or — when the gateway's count is the binding
    /// limit — its `reset`.
    pub fn next_slot(&self, now: Instant, reserve: u32) -> Instant {
        self.next_slot_n(now, reserve, 1)
    }

    /// When `need` slots are free at once (keeping `reserve`). A `need` the
    /// window can never hold answers one window from now (a re-check, not a spin).
    pub fn next_slot_n(&self, now: Instant, reserve: u32, need: u32) -> Instant {
        let mut g = self.inner.lock();
        Self::next_slot_locked(&mut g, now, reserve, need.max(1))
    }

    fn next_slot_locked(g: &mut WindowInner, now: Instant, reserve: u32, need: u32) -> Instant {
        if let Some(u) = g.blocked_until.filter(|u| now < *u) {
            return u;
        }
        if Self::available_locked(g, now, reserve) >= need {
            return now;
        }
        let limit = g.capacity.saturating_sub(Self::headroom(g, reserve));
        let local_free = limit.saturating_sub(g.sent.len() as u32);
        let at = if need > limit {
            None
        } else if local_free >= need {
            // the gateway report binds: a slot frees when its oldest request
            // ages out; once that is past (reset has 1 s resolution) nothing
            // new is known until the report itself expires, so do not spin
            g.server_reset.filter(|r| *r > now).or(g.server.map(|(r, _)| r + g.window))
        } else {
            // the (need − free)-th oldest send has to age out
            g.sent.get((need - local_free - 1) as usize).map(|t| *t + g.window)
        };
        at.unwrap_or(now + g.window).max(now + Duration::from_millis(1))
    }

    /// Take a slot now (keeping `reserve` free), or say how long to wait.
    pub fn try_acquire(&self, now: Instant, reserve: u32) -> Result<(), Duration> {
        let mut g = self.inner.lock();
        if Self::available_locked(&mut g, now, reserve) > 0 {
            g.sent.push_back(now);
            Ok(())
        } else {
            Err(Self::next_slot_locked(&mut g, now, reserve, 1).saturating_duration_since(now))
        }
    }

    /// Wait until a slot is free (no reserve).
    pub async fn acquire(&self) {
        loop {
            match self.try_acquire(Instant::now(), 0) {
                Ok(()) => return,
                Err(d) => tokio::time::sleep(d).await,
            }
        }
    }

    /// Gateway report carried by the response to a request sent at
    /// `reference` (the gateway counted that request and everything before it).
    /// `reset_in` = until the oldest request in its window ages out.
    pub fn on_server_report(&self, reference: Instant, now: Instant, current: i64, remaining: i64, reset_in: Duration) {
        let mut g = self.inner.lock();
        let cap = current + remaining;
        if cap > 0 {
            g.capacity = cap.min(u32::MAX as i64) as u32;
        }
        if g.server.is_none_or(|(r, _)| reference >= r) {
            g.server = Some((reference, remaining));
            g.server_reset = Some(now + reset_in);
        }
        if remaining <= 0 {
            let until = now + reset_in.max(Duration::from_millis(100));
            g.blocked_until = Some(g.blocked_until.map_or(until, |u| u.max(until)));
        }
    }

    /// Report a 429. Blocks until the gateway's reset hint (or a backoff).
    pub fn on_rate_limited(&self, now: Instant, hint: Option<Duration>) -> Duration {
        let mut g = self.inner.lock();
        let d = hint.unwrap_or_else(|| backoff_delay(g.attempt, Duration::from_millis(500), g.window, None, 0.5));
        g.attempt = g.attempt.saturating_add(1);
        g.total_429 += 1;
        let until = now + d.max(Duration::from_millis(100));
        g.blocked_until = Some(g.blocked_until.map_or(until, |u| u.max(until)));
        d
    }

    pub fn on_success(&self) {
        self.inner.lock().attempt = 0;
    }

    /// (capacity, window, in-window sends, total 429s)
    pub fn state(&self, now: Instant) -> (u32, Duration, u32, u64) {
        let mut g = self.inner.lock();
        Self::prune(&mut g, now);
        (g.capacity, g.window, g.sent.len() as u32, g.total_429)
    }
}

/// The limiter a client enforces before every request.
#[derive(Debug)]
pub enum Limiter {
    Bucket(RateLimiter),
    Window(WindowLimiter),
}

impl Limiter {
    pub async fn acquire(&self) {
        match self {
            Limiter::Bucket(b) => b.acquire().await,
            Limiter::Window(w) => w.acquire().await,
        }
    }

    pub fn on_rate_limited(&self, now: Instant, hint: Option<Duration>) -> Duration {
        match self {
            Limiter::Bucket(b) => b.on_rate_limited(now, hint),
            Limiter::Window(w) => w.on_rate_limited(now, hint),
        }
    }

    pub fn on_success(&self) {
        match self {
            Limiter::Bucket(b) => b.on_success(),
            Limiter::Window(w) => w.on_success(),
        }
    }

    pub fn on_server_report(&self, reference: Instant, now: Instant, current: i64, remaining: i64, reset_in: Duration) {
        if let Limiter::Window(w) = self {
            w.on_server_report(reference, now, current, remaining, reset_in);
        }
    }

    pub fn bucket(&self) -> Option<&RateLimiter> {
        match self {
            Limiter::Bucket(b) => Some(b),
            Limiter::Window(_) => None,
        }
    }

    pub fn window(&self) -> Option<&WindowLimiter> {
        match self {
            Limiter::Window(w) => Some(w),
            Limiter::Bucket(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_allows_a_burst_but_never_more_than_capacity_per_window() {
        let w = WindowLimiter::new("t", 10, Duration::from_secs(10), 1);
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // quiet period banked: 9 at once (capacity 10 − safety 1)
        for i in 0..9 {
            assert!(w.try_acquire(ms(i), 0).is_ok(), "burst {i}");
        }
        let wait = w.try_acquire(ms(9), 0).unwrap_err();
        assert!(wait > Duration::from_millis(9_900), "next slot when the first ages out: {wait:?}");
        assert!(w.try_acquire(ms(10_000), 0).is_ok(), "the first send aged out: one slot frees");
        assert!(w.try_acquire(ms(10_000), 0).is_err(), "only one");
        // any 10 s window holds ≤ 9 of our sends
        let (cap, win, used, _) = w.state(ms(10_000));
        assert_eq!((cap, win, used), (10, Duration::from_secs(10), 9));
    }

    #[test]
    fn reserve_is_kept_for_urgent_work() {
        let w = WindowLimiter::new("t", 10, Duration::from_secs(10), 1);
        let t0 = Instant::now();
        for _ in 0..6 {
            assert!(w.try_acquire(t0, 3).is_ok());
        }
        assert!(w.try_acquire(t0, 3).is_err(), "background stops at capacity − safety − reserve");
        assert_eq!(w.available(t0, 0), 3, "urgent work still has the reserve");
    }

    #[test]
    fn a_gateway_bound_wait_ends_at_the_gateway_reset() {
        let w = WindowLimiter::new("t", 10, Duration::from_secs(10), 1);
        let t0 = Instant::now();
        // we sent nothing, but the gateway counts 9 of 10 (another client)
        w.on_server_report(t0, t0, 9, 1, Duration::from_millis(2_500));
        assert_eq!(w.available(t0, 0), 0);
        assert_eq!(w.binding(t0, 0), "gateway");
        let at = w.next_slot(t0, 0);
        assert!(at >= t0 + Duration::from_millis(2_400) && at <= t0 + Duration::from_millis(2_600), "{:?}", at - t0);
        assert_eq!(w.try_acquire(t0, 0).unwrap_err(), at - t0);
    }

    #[test]
    fn gateway_report_is_authoritative_and_blocks_at_zero() {
        let w = WindowLimiter::new("t", 60, Duration::from_secs(10), 1);
        let t0 = Instant::now();
        // gateway (response to a request sent 1 ms earlier): another client used 8 of 10
        let sent = t0 - Duration::from_millis(1);
        w.on_server_report(sent, t0, 8, 2, Duration::from_secs(3));
        assert_eq!(w.state(t0).0, 10, "capacity learned from current + remaining");
        assert_eq!(w.available(t0, 0), 1, "remaining 2 − safety 1");
        assert!(w.try_acquire(t0, 0).is_ok());
        assert!(w.try_acquire(t0, 0).is_err());
        w.on_server_report(t0, t0, 10, 0, Duration::from_secs(3));
        assert_eq!(w.available(t0 + Duration::from_secs(2), 0), 0, "blocked until reset");
        assert!(w.next_slot(t0, 0) >= t0 + Duration::from_secs(3));
        let d = w.on_rate_limited(t0, Some(Duration::from_secs(4)));
        assert_eq!(d, Duration::from_secs(4));
        assert_eq!(w.state(t0).3, 1);
    }

    #[test]
    fn a_gateway_reset_in_the_past_frees_one_slot_and_never_means_retry_now() {
        let w = WindowLimiter::new("t", 5, Duration::from_secs(11), 1);
        let t0 = Instant::now();
        // reset rounded to the current second: already due
        w.on_server_report(t0, t0, 4, 1, Duration::ZERO);
        let now = t0 + Duration::from_millis(5);
        // remaining 1 + 1 freed − safety 1
        assert_eq!(w.available(now, 0), 1);
        w.try_acquire(now, 0).unwrap();
        assert_eq!(w.available(now, 0), 0);
        let at = w.next_slot(now, 0);
        assert!(at >= t0 + Duration::from_secs(10), "waits for the report to expire, not 1 ms: {:?}", at - now);
    }

    #[test]
    fn a_smaller_learned_window_keeps_room_for_background_work() {
        // configured for the keyed tier (10, safety 2, reserve 3); the key is
        // missing and the gateway reports the keyless tier (5)
        let w = WindowLimiter::new("t", 10, Duration::from_secs(11), 2);
        let t0 = Instant::now();
        assert_eq!(w.available(t0, 3), 5, "10 − 2 − 3 before any report");
        w.on_server_report(t0, t0, 1, 4, Duration::from_secs(10));
        w.try_acquire(t0, 0).unwrap();
        // scaled: safety 1, reserve 1 → background may use 3, urgent 4
        assert_eq!(w.available(t0, 3), 2, "5 − 1 − 1 − 1 sent");
        assert_eq!(w.available(t0, 0), 3);
    }

    #[test]
    fn next_slot_n_waits_for_enough_slots_at_once() {
        let w = WindowLimiter::new("t", 5, Duration::from_secs(10), 1);
        let t0 = Instant::now();
        for i in 0..3 {
            w.try_acquire(t0 + Duration::from_secs(i), 0).unwrap();
        }
        let now = t0 + Duration::from_secs(3);
        assert_eq!(w.available(now, 0), 1);
        assert_eq!(w.next_slot_n(now, 0, 1), now);
        // 3 at once: the two oldest sends (t0, t0+1 s) must age out
        assert_eq!(w.next_slot_n(now, 0, 3), t0 + Duration::from_secs(11));
        // more than the window can ever hold: a re-check one window later, never "now"
        assert_eq!(w.next_slot_n(now, 0, 9), now + Duration::from_secs(10));
    }

    #[test]
    fn bucket_refills_at_rate() {
        let rl = RateLimiter::new("t", LimiterConfig::new(2.0, 2));
        let t0 = Instant::now();
        assert!(rl.try_acquire(t0).is_ok());
        assert!(rl.try_acquire(t0).is_ok());
        let wait = rl.try_acquire(t0).unwrap_err();
        assert!((wait.as_secs_f64() - 0.5).abs() < 0.01, "{wait:?}");
        assert!(rl.try_acquire(t0 + Duration::from_millis(510)).is_ok());
        assert!(rl.try_acquire(t0 + Duration::from_millis(520)).is_err());
    }

    #[test]
    fn burst_is_capped() {
        let rl = RateLimiter::new("t", LimiterConfig::new(10.0, 3));
        let t0 = Instant::now() + Duration::from_secs(100); // long idle
        for _ in 0..3 {
            assert!(rl.try_acquire(t0).is_ok());
        }
        assert!(rl.try_acquire(t0).is_err());
    }

    #[test]
    fn backoff_grows_and_respects_cap_and_hint() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_delay(0, base, cap, None, 0.0), Duration::from_millis(250));
        assert_eq!(backoff_delay(3, base, cap, None, 0.0), Duration::from_secs(2));
        assert!(backoff_delay(3, base, cap, None, 0.99) < Duration::from_secs(4));
        assert_eq!(backoff_delay(30, base, cap, None, 0.0), Duration::from_secs(30));
        assert_eq!(backoff_delay(0, base, cap, Some(Duration::from_secs(7)), 0.0), Duration::from_secs(7));
        assert_eq!(backoff_delay(0, base, cap, Some(Duration::from_secs(700)), 0.0), cap);
    }

    #[test]
    fn rate_limited_blocks_until_backoff_elapses_then_resets_on_success() {
        let mut cfg = LimiterConfig::new(100.0, 5);
        cfg.base_backoff = Duration::from_secs(2);
        let rl = RateLimiter::new("t", cfg);
        let t0 = Instant::now();
        let d1 = rl.on_rate_limited(t0, None);
        assert!(d1 >= Duration::from_secs(1) && d1 < Duration::from_secs(2));
        assert!(rl.try_acquire(t0 + Duration::from_millis(900)).is_err());
        assert_eq!(rl.state(t0).attempt, 1);
        let d2 = rl.on_rate_limited(t0, None);
        assert!(d2 >= Duration::from_secs(2), "second backoff grows: {d2:?}");
        // after backoff the bucket starts empty and refills at rps
        let after = t0 + Duration::from_secs(5);
        assert!(rl.try_acquire(after).is_ok());
        rl.on_success();
        assert_eq!(rl.state(after).attempt, 0);
        assert_eq!(rl.state(after).total_429, 2);
    }
}
