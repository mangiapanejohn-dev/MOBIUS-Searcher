//! HOT market data handed to the scheduler as it arrives (unthrottled), with
//! the source's own time/slot and our monotonic receipt time, so consumers can
//! tell "the data is old" from "we received it late".

use searcher_core::Ts;
use std::sync::Arc;
use std::time::Instant;

/// Where an oracle price came from. Consumers never depend on the source;
/// switching Pyth delivery (on-chain account ↔ Hermes stream) swaps the adapter.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OracleSource {
    /// Pyth `PriceUpdateV2` account over `accountSubscribe`.
    OnChainPyth,
    /// Pyth Hermes SSE stream (needs an API key).
    Hermes,
}

impl OracleSource {
    pub fn label(self) -> &'static str {
        match self {
            OracleSource::OnChainPyth => "Pyth",
            OracleSource::Hermes => "Pyth Hermes",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OracleUpdate {
    pub source: OracleSource,
    /// e.g. `SOL/USD`
    pub symbol: Arc<str>,
    pub price: f64,
    pub conf: f64,
    /// Publisher timestamp (unix seconds): when the price was true.
    pub publish_time: i64,
    /// Slot the update was posted/observed at, when known.
    pub slot: Option<u64>,
    pub received: Instant,
    pub received_ts: Ts,
}

impl OracleUpdate {
    /// Age of the price itself when we received it.
    pub fn age_at_receipt_ms(&self) -> i64 {
        self.received_ts.0 / 1_000 - self.publish_time * 1_000
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HotTick {
    /// Decoded pool mid (not executable).
    Pool {
        watch: usize,
        dex: Arc<str>,
        pair: Arc<str>,
        mid: f64,
        /// Slot of the account state (notification context).
        slot: Option<u64>,
        /// Chain head we had seen when it arrived.
        head_slot: Option<u64>,
        received: Instant,
        received_ts: Ts,
    },
    Oracle(OracleUpdate),
}

/// Non-blocking consumer of hot ticks (must never block the feed).
pub type HotSink = Arc<dyn Fn(HotTick) + Send + Sync>;
