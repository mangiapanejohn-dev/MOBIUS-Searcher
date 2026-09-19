//! Service health, latency tracking and rate-limit scheduling.

pub mod bus;
pub mod health;
pub mod latency;
pub mod proxy;
pub mod ratelimit;

pub use bus::EventBus;
pub use health::Telemetry;
pub use latency::{LatencyBook, LatencyReport};
pub use ratelimit::{Limiter, LimiterConfig, LimiterState, RateLimiter, WindowLimiter, backoff_delay};

use std::time::Instant;

/// Measures one request's wall latency.
pub struct Stopwatch(Instant);

impl Stopwatch {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn ms(&self) -> u32 {
        self.0.elapsed().as_millis().min(u32::MAX as u128) as u32
    }
}
