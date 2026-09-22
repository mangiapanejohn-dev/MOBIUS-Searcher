//! The single event type that flows from the engine to the state hub (UI),
//! persistence and replay. Replay re-feeds persisted events through the exact
//! same path, so what you see in replay is what the live UI saw.

use crate::metrics::MetricId;
use crate::model::{
    ExecutionAttempt, MarketSample, Mode, Opportunity, OpportunityId, RiskDecision, ServiceId, ServiceSnapshot,
    SimulationResult, TradeResult,
};
use crate::time::Ts;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub started_at: Ts,
    pub mode: Mode,
    pub version: String,
    /// Redacted config summary (no secrets).
    pub config_summary: String,
    pub taker: Option<String>,
    pub paper_equity_lamports: Option<u64>,
    /// Configured risk limits as (name, value) for display.
    #[serde(default)]
    pub limits: Vec<(String, String)>,
}

/// Pipeline stage lines for the event stream (`build`, `simulation`, ...).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Opportunity,
    Build,
    Protect,
    Simulation,
    Risk,
    Paper,
    Confirm,
    Submit,
    Landed,
    Skip,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Opportunity => "opportunity",
            Stage::Build => "build",
            Stage::Protect => "min-out",
            Stage::Simulation => "simulation",
            Stage::Risk => "risk",
            Stage::Paper => "paper",
            Stage::Confirm => "confirm",
            Stage::Submit => "submit",
            Stage::Landed => "landed",
            Stage::Skip => "skip",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageEvent {
    pub ts: Ts,
    pub opportunity: OpportunityId,
    pub stage: Stage,
    pub ok: bool,
    /// Primary text column (route, verdict, reason code).
    pub subject: String,
    /// Secondary value column (edge, latency, PnL).
    pub value: String,
    /// Longer detail shown on Enter.
    pub detail: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Network conditions polled over RPC. Fields are `None` when that part of
/// the poll has not succeeded yet (fees and TPS are polled separately).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkStats {
    pub ts: Ts,
    /// Transactions/second in the latest performance sample (votes included).
    pub tps: Option<f64>,
    pub non_vote_tps: Option<f64>,
    /// Priority fee percentiles (µlamports/CU) over recent slots that locked
    /// the watched pool accounts.
    pub fee_p50: Option<u64>,
    pub fee_p75: Option<u64>,
    pub fee_p90: Option<u64>,
    pub fee_slots: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TipFloor {
    pub ts: Ts,
    pub p25: u64,
    pub p50: u64,
    pub p75: u64,
    pub p95: u64,
    pub p99: u64,
    pub ema_p50: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub enum Event {
    Session(SessionInfo),
    Slot {
        ts: Ts,
        slot: u64,
    },
    BlockHeight {
        ts: Ts,
        height: u64,
    },
    Sample(MarketSample),
    Metric {
        ts: Ts,
        metric: MetricId,
        value: f64,
    },
    Opportunity(Box<Opportunity>),
    Stage(StageEvent),
    Simulation(Box<SimulationResult>),
    Risk(RiskDecision),
    Execution(ExecutionAttempt),
    Trade(TradeResult),
    Health {
        ts: Ts,
        service: ServiceId,
        snapshot: ServiceSnapshot,
    },
    RateLimited {
        ts: Ts,
        service: ServiceId,
        backoff_ms: u64,
        attempt: u32,
    },
    Error {
        ts: Ts,
        service: String,
        message: String,
    },
    KillSwitch {
        ts: Ts,
        engaged: bool,
        reason: String,
    },
    TipFloor(TipFloor),
    Network(NetworkStats),
    Equity {
        ts: Ts,
        lamports: u64,
        source: String,
    },
    /// Wallet inventory (sending modes): SOL and USDC balances and the SOL
    /// price, so trade PnL and revaluation can be told apart.
    Inventory {
        ts: Ts,
        sol_lamports: u64,
        usdc_atoms: Option<u64>,
        sol_usd_micros: Option<u64>,
    },
    Log {
        ts: Ts,
        level: LogLevel,
        message: String,
    },
    /// Provider response for one leg (lookup-table arrays reduced to counts),
    /// persisted as part of the opportunity snapshot. Not used by the UI.
    RawQuote {
        ts: Ts,
        opportunity: OpportunityId,
        leg: u8,
        body: String,
    },
}

impl Event {
    pub fn ts(&self) -> Ts {
        match self {
            Event::Session(s) => s.started_at,
            Event::Slot { ts, .. }
            | Event::BlockHeight { ts, .. }
            | Event::Metric { ts, .. }
            | Event::Health { ts, .. }
            | Event::RateLimited { ts, .. }
            | Event::Error { ts, .. }
            | Event::KillSwitch { ts, .. }
            | Event::Equity { ts, .. }
            | Event::Inventory { ts, .. }
            | Event::Log { ts, .. }
            | Event::RawQuote { ts, .. } => *ts,
            Event::Sample(s) => s.ts,
            Event::Opportunity(o) => o.updated_at,
            Event::Stage(s) => s.ts,
            Event::Simulation(s) => s.simulated_at,
            Event::Risk(r) => r.checked_at,
            Event::Execution(e) => e.updated_at,
            Event::Trade(t) => t.exit_ts,
            Event::TipFloor(t) => t.ts,
            Event::Network(n) => n.ts,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Event::Session(_) => "session",
            Event::Slot { .. } => "slot",
            Event::BlockHeight { .. } => "block_height",
            Event::Sample(_) => "sample",
            Event::Metric { .. } => "metric",
            Event::Opportunity(_) => "opportunity",
            Event::Stage(_) => "stage",
            Event::Simulation(_) => "simulation",
            Event::Risk(_) => "risk",
            Event::Execution(_) => "execution",
            Event::Trade(_) => "trade",
            Event::Health { .. } => "health",
            Event::RateLimited { .. } => "rate_limited",
            Event::Error { .. } => "error",
            Event::KillSwitch { .. } => "kill_switch",
            Event::TipFloor(_) => "tip_floor",
            Event::Network(_) => "network",
            Event::Equity { .. } => "equity",
            Event::Inventory { .. } => "inventory",
            Event::Log { .. } => "log",
            Event::RawQuote { .. } => "raw_quote",
        }
    }

    /// Events that must reach persistence even under pressure (everything else
    /// may be sampled/dropped by the UI path, never by storage).
    pub fn is_high_frequency(&self) -> bool {
        matches!(self, Event::Slot { .. } | Event::BlockHeight { .. } | Event::Health { .. })
    }
}

/// Operator commands from the UI to the engine (the only UI → engine path).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "cmd")]
pub enum Command {
    KillSwitch { engage: bool, reason: String },
    Confirm { opportunity: OpportunityId, approve: bool },
    Shutdown,
}
