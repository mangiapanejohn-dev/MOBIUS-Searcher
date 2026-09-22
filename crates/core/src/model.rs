//! Provider-neutral domain model. Adapters (Jupiter, RPC, Jito) convert their
//! wire formats into these types; UI, storage and risk only ever see these.

use crate::address::Address;
use crate::costs::CostBreakdown;
use crate::profit::ProfitEval;
use crate::time::Ts;
use crate::units::{Ppm, UsdMicros, UsdPrice};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Paper,
    Confirm,
    Live,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Paper => "PAPER",
            Mode::Confirm => "CONFIRM",
            Mode::Live => "LIVE",
        }
    }

    /// Whether this mode ever signs and sends transactions.
    pub fn sends_transactions(self) -> bool {
        !matches!(self, Mode::Paper)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyKind {
    RoundTrip,
    CrossDex,
    Triangular,
}

impl StrategyKind {
    pub const ALL: [StrategyKind; 3] = [StrategyKind::RoundTrip, StrategyKind::CrossDex, StrategyKind::Triangular];

    pub fn label(self) -> &'static str {
        match self {
            StrategyKind::RoundTrip => "round-trip",
            StrategyKind::CrossDex => "cross-dex",
            StrategyKind::Triangular => "triangular",
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    #[default]
    Normal,
    Fast,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "dexes")]
pub enum DexFilter {
    #[default]
    Any,
    Only(Vec<String>),
    Exclude(Vec<String>),
}

impl DexFilter {
    pub fn describe(&self) -> String {
        match self {
            DexFilter::Any => "any".into(),
            DexFilter::Only(d) => d.join(","),
            DexFilter::Exclude(d) => format!("¬{}", d.join(",¬")),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "bps")]
pub enum SlippageSpec {
    #[default]
    Rtse,
    Fixed(u16),
}

/// One AMM hop inside a leg, straight from the provider's route plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hop {
    pub amm_key: Address,
    pub label: String,
    pub input_mint: Address,
    pub output_mint: Address,
    pub in_amount: u64,
    pub out_amount: u64,
    /// Share of the leg input routed through this hop (10_000 = 100%).
    pub bps: u16,
}

/// One swap leg (one Jupiter `/build` call).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub index: u8,
    pub input_mint: Address,
    pub output_mint: Address,
    pub in_amount: u64,
    pub out_amount: u64,
    /// Minimum output after slippage (`otherAmountThreshold`).
    pub min_out: u64,
    pub slippage_bps: u16,
    pub slippage_spec: SlippageSpec,
    pub price_impact: Ppm,
    pub hops: Vec<Hop>,
    pub mode: RoutingMode,
    pub dex_filter: DexFilter,
    pub quoted_at: Ts,
    pub latency_ms: u32,
    /// Compute unit price the provider suggested (micro-lamports / CU).
    pub cu_price_micro: Option<u64>,
    pub last_valid_block_height: u64,
    /// Provider request id (e.g. `x-api-gateway-request-id`) for support/debug.
    pub request_id: Option<String>,
}

impl Leg {
    /// Unique DEX labels used by this leg, in route order.
    pub fn dex_labels(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for h in &self.hops {
            if !out.contains(&h.label) {
                out.push(h.label.clone());
            }
        }
        out
    }

    /// Slippage tolerance of this leg as a ratio of quoted output.
    pub fn tolerance(&self) -> Ppm {
        Ppm::ratio(self.out_amount.saturating_sub(self.min_out) as i128, self.out_amount as i128).unwrap_or(Ppm::ZERO)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub legs: Vec<Leg>,
}

impl Route {
    /// `Raydium → Orca`, `Raydium+Orca → Meteora DLMM` (DEXes per leg).
    pub fn dex_path(&self) -> String {
        self.legs.iter().map(|l| l.dex_labels().join("+")).collect::<Vec<_>>().join(" → ")
    }

    /// Mints visited, first input to last output.
    pub fn mint_path(&self) -> Vec<Address> {
        let mut v = Vec::with_capacity(self.legs.len() + 1);
        if let Some(first) = self.legs.first() {
            v.push(first.input_mint);
        }
        v.extend(self.legs.iter().map(|l| l.output_mint));
        v
    }

    pub fn oldest_quote(&self) -> Option<Ts> {
        self.legs.iter().map(|l| l.quoted_at).min()
    }

    pub fn total_latency_ms(&self) -> u32 {
        self.legs.iter().map(|l| l.latency_ms).sum()
    }

    pub fn is_closed_cycle(&self) -> bool {
        let p = self.mint_path();
        p.len() >= 2 && p.first() == p.last()
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpportunityId(pub u64);

impl fmt::Display for OpportunityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:05}", self.0)
    }
}

/// Why an opportunity was not (or could not be) executed. Never hidden in the UI.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SkipReason {
    EdgeTooSmall,
    StaleQuote,
    SimFailed,
    Slippage,
    TipTooHigh,
    RiskLimit,
    NoRoute,
    BuildFailed,
    RateLimited,
    TxTooLarge,
    PriceUnavailable,
    KillSwitch,
    SimUnavailable,
    Declined,
    Expired,
}

impl SkipReason {
    pub fn code(self) -> &'static str {
        match self {
            SkipReason::EdgeTooSmall => "EDGE_TOO_SMALL",
            SkipReason::StaleQuote => "STALE_QUOTE",
            SkipReason::SimFailed => "SIM_FAILED",
            SkipReason::Slippage => "SLIPPAGE",
            SkipReason::TipTooHigh => "TIP_TOO_HIGH",
            SkipReason::RiskLimit => "RISK_LIMIT",
            SkipReason::NoRoute => "NO_ROUTE",
            SkipReason::BuildFailed => "BUILD_FAILED",
            SkipReason::RateLimited => "RATE_LIMITED",
            SkipReason::TxTooLarge => "TX_TOO_LARGE",
            SkipReason::PriceUnavailable => "PRICE_UNAVAILABLE",
            SkipReason::KillSwitch => "KILL_SWITCH",
            SkipReason::SimUnavailable => "SIM_UNAVAILABLE",
            SkipReason::Declined => "DECLINED",
            SkipReason::Expired => "EXPIRED",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum OppStatus {
    /// Priced from real quotes, not yet simulated.
    Quoted,
    Skipped(SkipReason),
    /// Passed simulation and risk; eligible to execute in the current mode.
    Executable,
    AwaitingConfirm,
    Submitted,
    PaperFilled,
    Landed,
    Failed,
}

impl OppStatus {
    pub fn skip(&self) -> Option<SkipReason> {
        match self {
            OppStatus::Skipped(r) => Some(*r),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, OppStatus::Skipped(_) | OppStatus::PaperFilled | OppStatus::Landed | OppStatus::Failed)
    }
}

/// A fully-priced candidate cycle with its cost model and pipeline state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: OpportunityId,
    /// Stable key for "the same opportunity over time" (strategy + mint path +
    /// dex constraints). Used for lifetime/episode statistics.
    pub key: String,
    pub strategy: StrategyKind,
    pub label: String,
    pub detected_at: Ts,
    pub slot: Option<u64>,
    pub base_mint: Address,
    pub input: u64,
    pub gross_output: u64,
    pub route: Route,
    pub costs: CostBreakdown,
    pub eval: ProfitEval,
    pub status: OppStatus,
    pub updated_at: Ts,
    /// SOL/USD used for USD figures at evaluation time.
    pub sol_price: Option<UsdPrice>,
    pub simulation: Option<SimulationResult>,
    pub risk: Option<RiskDecision>,
    /// The profit guard that failed, when one did (EDGE_TOO_SMALL covers three).
    #[serde(default)]
    pub guard: Option<crate::profit::GuardFailure>,
}

impl Opportunity {
    pub fn quote_age_ms(&self, now: Ts) -> u64 {
        self.route.oldest_quote().map(|t| t.age_ms(now)).unwrap_or(u64::MAX)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    /// All legs composed into one v0 transaction (atomic by construction).
    SingleTx,
    /// One transaction per leg, sent as a Jito bundle.
    Bundle,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimFidelity {
    /// The whole cycle was simulated in one transaction against current state.
    Exact,
    /// Each transaction simulated independently; intermediate balances assumed.
    PerTx,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimFailureClass {
    InsufficientFunds,
    SlippageExceeded,
    AccountNotFound,
    BlockhashNotFound,
    ComputeExceeded,
    TooManyAccounts,
    TxTooLarge,
    ProgramError,
    RpcError,
    /// Per-tx simulation of a bundle: a later transaction needs the output of
    /// an earlier one, which independent simulation cannot provide. An
    /// artifact of simulation fidelity, not evidence the bundle would fail.
    DependsOnPriorTx,
    Unknown,
}

impl SimFailureClass {
    pub fn label(self) -> &'static str {
        match self {
            SimFailureClass::InsufficientFunds => "insufficient funds",
            SimFailureClass::SlippageExceeded => "slippage exceeded",
            SimFailureClass::AccountNotFound => "account not found",
            SimFailureClass::BlockhashNotFound => "blockhash not found",
            SimFailureClass::ComputeExceeded => "compute exceeded",
            SimFailureClass::TooManyAccounts => "too many accounts",
            SimFailureClass::TxTooLarge => "tx too large",
            SimFailureClass::ProgramError => "program error",
            SimFailureClass::RpcError => "rpc error",
            SimFailureClass::DependsOnPriorTx => "needs prior tx output (per-tx sim)",
            SimFailureClass::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimFailure {
    pub class: SimFailureClass,
    pub tx_index: u8,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxSim {
    pub index: u8,
    pub ok: bool,
    pub units_consumed: u32,
    /// CU limit to request when sending (`units_consumed` × margin).
    pub cu_limit: u32,
    pub cu_price_micro: u64,
    /// Fee the node reported for this transaction, if any.
    pub fee: Option<u64>,
    pub size_bytes: u32,
    pub accounts: u16,
    pub logs: Vec<String>,
    pub err: Option<String>,
    /// Taker lamports before/after, when the node reports balances.
    pub taker_lamports: Option<(u64, u64)>,
    /// What each Jupiter route instruction returned (its output amount,
    /// `Program return`), in order: the executed amounts, not the quoted ones.
    #[serde(default)]
    pub leg_outputs: Vec<u64>,
    /// Accounts this transaction leaves created (no lamports before, some
    /// after) with the lamports now held in them — the deposit the payer funds.
    #[serde(default)]
    pub created: Vec<(Address, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub opportunity: OpportunityId,
    pub plan: PlanKind,
    pub fidelity: SimFidelity,
    pub ok: bool,
    pub failure: Option<SimFailure>,
    pub txs: Vec<TxSim>,
    pub latency_ms: u32,
    pub simulated_at: Ts,
    pub context_slot: Option<u64>,
}

impl SimulationResult {
    pub fn units_consumed(&self) -> u32 {
        self.txs.iter().map(|t| t.units_consumed).sum()
    }

    pub fn cu_limit(&self) -> u32 {
        self.txs.iter().map(|t| t.cu_limit).sum()
    }

    /// Net lamport change of the taker across a single exact simulation.
    pub fn taker_delta(&self) -> Option<i64> {
        if self.fidelity != SimFidelity::Exact || self.txs.len() != 1 {
            return None;
        }
        self.txs[0].taker_lamports.map(|(pre, post)| post as i64 - pre as i64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "limit")]
pub enum RiskViolation {
    KillSwitch,
    TradeSize {
        input: u64,
        max: u64,
    },
    TradePctOfEquity {
        pct_ppm: i64,
        max_ppm: i64,
    },
    DailyLoss {
        loss_usd_micros: i64,
        max_usd_micros: i64,
    },
    ConsecutiveFailures {
        count: u32,
        max: u32,
    },
    Slippage {
        bps: u16,
        max_bps: u16,
    },
    QuoteAge {
        age_ms: u64,
        max_ms: u64,
    },
    SimulationAge {
        age_ms: u64,
        max_ms: u64,
    },
    SimulationMissing,
    PriorityFee {
        lamports: u64,
        max: u64,
    },
    JitoTip {
        lamports: u64,
        max: u64,
    },
    WalletSolForFees {
        lamports: u64,
        min: u64,
    },
    OpenExecutions {
        open: u32,
        max: u32,
    },
    SlotStale {
        age_slots: u64,
        max_slots: u64,
    },
    NegativeExpectedPnl {
        lamports: i64,
    },
    /// Multi-tx bundle simulated per transaction (not exact); refused for sending.
    InexactSimulation,
    SimulationFailed,
}

impl RiskViolation {
    pub fn code(&self) -> &'static str {
        match self {
            RiskViolation::KillSwitch => "kill_switch",
            RiskViolation::TradeSize { .. } => "max_trade_size",
            RiskViolation::TradePctOfEquity { .. } => "max_trade_pct_of_equity",
            RiskViolation::DailyLoss { .. } => "max_daily_loss",
            RiskViolation::ConsecutiveFailures { .. } => "max_consecutive_failures",
            RiskViolation::Slippage { .. } => "max_slippage",
            RiskViolation::QuoteAge { .. } => "max_quote_age_ms",
            RiskViolation::SimulationAge { .. } => "max_simulation_age_ms",
            RiskViolation::SimulationMissing => "simulation_required",
            RiskViolation::PriorityFee { .. } => "max_priority_fee",
            RiskViolation::JitoTip { .. } => "max_jito_tip",
            RiskViolation::WalletSolForFees { .. } => "min_wallet_sol_for_fees",
            RiskViolation::OpenExecutions { .. } => "max_open_execution",
            RiskViolation::SlotStale { .. } => "stale_slot",
            RiskViolation::NegativeExpectedPnl { .. } => "negative_expected_pnl",
            RiskViolation::InexactSimulation => "exact_simulation_required",
            RiskViolation::SimulationFailed => "simulation_failed",
        }
    }

    /// The skip reason this violation surfaces as in the opportunity table.
    pub fn skip_reason(&self) -> SkipReason {
        match self {
            RiskViolation::KillSwitch => SkipReason::KillSwitch,
            RiskViolation::Slippage { .. } => SkipReason::Slippage,
            RiskViolation::QuoteAge { .. } | RiskViolation::SimulationAge { .. } | RiskViolation::SlotStale { .. } => {
                SkipReason::StaleQuote
            }
            RiskViolation::JitoTip { .. } => SkipReason::TipTooHigh,
            RiskViolation::NegativeExpectedPnl { .. } => SkipReason::EdgeTooSmall,
            RiskViolation::SimulationMissing => SkipReason::SimUnavailable,
            RiskViolation::SimulationFailed => SkipReason::SimFailed,
            _ => SkipReason::RiskLimit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskDecision {
    pub opportunity: OpportunityId,
    pub approved: bool,
    pub violations: Vec<RiskViolation>,
    pub checked_at: Ts,
}

impl RiskDecision {
    pub fn primary_skip(&self) -> Option<SkipReason> {
        self.violations.first().map(|v| v.skip_reason())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum ExecState {
    /// PAPER: accepted as a simulated fill; nothing signed or sent.
    PaperFilled,
    AwaitingConfirm,
    Declined,
    Submitted,
    Pending,
    Landed {
        slot: u64,
    },
    Failed {
        reason: String,
    },
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    pub opportunity: OpportunityId,
    pub mode: Mode,
    pub plan: PlanKind,
    pub state: ExecState,
    pub bundle_id: Option<String>,
    pub signatures: Vec<String>,
    pub tip_lamports: u64,
    pub created_at: Ts,
    pub updated_at: Ts,
    /// Submission → landed/failed.
    pub latency_ms: Option<u32>,
}

/// Outcome of an executed (or paper-executed) opportunity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeResult {
    pub opportunity: OpportunityId,
    pub strategy: StrategyKind,
    pub label: String,
    pub mode: Mode,
    pub paper: bool,
    pub entry_ts: Ts,
    pub exit_ts: Ts,
    pub input: u64,
    pub output: u64,
    pub fees_lamports: u64,
    pub tip_lamports: u64,
    pub expected_net: i64,
    /// PAPER: simulation-verified net (no landing risk applied). LIVE: on-chain.
    pub net: i64,
    pub net_usd: Option<UsdMicros>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSide {
    /// Executable price when selling the base asset (base → quote).
    Sell,
    /// Executable price when buying the base asset (quote → base).
    Buy,
    /// Reference price from a price API (not executable).
    Reference,
    /// Pool mid price decoded from on-chain pool state (not executable).
    Mid,
    /// Oracle price from an on-chain Pyth price account (not executable).
    Oracle,
}

/// A price observation. `source` names exactly where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSample {
    pub ts: Ts,
    pub pair: String,
    pub price: UsdPrice,
    pub side: SampleSide,
    pub source: String,
    /// Trade size the price was observed at (base atoms), 0 for reference.
    pub size_atoms: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceId {
    Jupiter,
    Rpc,
    WebSocket,
    Jito,
    Db,
    MarketFeed,
}

impl ServiceId {
    pub const ALL: [ServiceId; 6] = [
        ServiceId::Jupiter,
        ServiceId::Rpc,
        ServiceId::WebSocket,
        ServiceId::Jito,
        ServiceId::Db,
        ServiceId::MarketFeed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ServiceId::Jupiter => "Jupiter API",
            ServiceId::Rpc => "RPC",
            ServiceId::WebSocket => "WebSocket",
            ServiceId::Jito => "Jito",
            ServiceId::Db => "DB",
            ServiceId::MarketFeed => "Market feed",
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    #[default]
    Idle,
    Ok,
    Degraded,
    RateLimited,
    Down,
    Disabled,
}

impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            ServiceState::Idle => "idle",
            ServiceState::Ok => "ok",
            ServiceState::Degraded => "degraded",
            ServiceState::RateLimited => "429 backoff",
            ServiceState::Down => "down",
            ServiceState::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub service: Option<ServiceId>,
    pub state: ServiceState,
    pub last_latency_ms: Option<u32>,
    pub p50_latency_ms: Option<u32>,
    pub requests: u64,
    pub errors: u64,
    pub rate_limited: u64,
    /// Errors / requests over the recent window, ppm.
    pub error_rate: Ppm,
    pub backoff_until: Option<Ts>,
    pub last_success: Option<Ts>,
    pub last_error: Option<String>,
    /// Remaining requests in the provider's window, when reported.
    pub quota_remaining: Option<i64>,
    /// Recent latencies (ms), oldest first, for sparklines.
    pub recent_latency: Vec<u32>,
}
