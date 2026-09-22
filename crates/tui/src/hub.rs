//! View model ("state hub"). Built only from `Event`s, so live and replay are
//! the same code path. Bounded everywhere: old data is evicted, never grows
//! without limit.

use searcher_core::event::{LogLevel, NetworkStats, SessionInfo, StageEvent, TipFloor};
use searcher_core::metrics::MetricId;
use searcher_core::model::*;
use searcher_core::series::TimeSeries;
use searcher_core::{Event, Ts, UsdMicros, UsdPrice};
use std::collections::{BTreeMap, HashMap, VecDeque};

const MAX_OPPS: usize = 4_000;
const MAX_STREAM: usize = 3_000;
const MAX_LOGS: usize = 2_000;
const MAX_MARKERS: usize = 4_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    Opportunity,
    Execution,
    Entry,
    Exit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub ts: Ts,
    pub kind: MarkerKind,
    pub opportunity: OpportunityId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    pub ts: Ts,
    pub level: LogLevel,
    pub source: String,
    pub message: String,
}

/// Latest executable price per (pair, side, source) for the Markets page.
#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    pub pair: String,
    pub side: SampleSide,
    pub source: String,
    pub price: UsdPrice,
    pub first: UsdPrice,
    pub ts: Ts,
    pub samples: u64,
    pub size_atoms: u64,
}

#[derive(Debug, Default)]
pub struct ViewModel {
    pub revision: u64,
    pub session: Option<SessionInfo>,
    pub replay: bool,
    pub slot: Option<u64>,
    pub slot_ts: Option<Ts>,
    pub block_height: Option<u64>,
    pub first_ts: Option<Ts>,
    pub last_ts: Option<Ts>,
    pub opps: BTreeMap<OpportunityId, Opportunity>,
    pub total_opps: u64,
    pub executed: u64,
    pub skip_counts: BTreeMap<SkipReason, u64>,
    pub strategy_counts: BTreeMap<StrategyKind, (u64, u64)>, // (evaluated, gross-positive)
    pub stream: VecDeque<StageEvent>,
    pub logs: VecDeque<LogLine>,
    pub series: HashMap<MetricId, TimeSeries>,
    pub markers: VecDeque<Marker>,
    pub trades: VecDeque<TradeResult>,
    pub executions: BTreeMap<OpportunityId, ExecutionAttempt>,
    pub health: BTreeMap<ServiceId, ServiceSnapshot>,
    pub tip_floor: Option<TipFloor>,
    pub network: Option<NetworkStats>,
    pub kill: Option<(bool, String, Ts)>,
    pub realized: UsdMicros,
    pub simulated: UsdMicros,
    pub realized_lamports: i64,
    pub simulated_lamports: i64,
    pub equity_lamports: Option<(u64, String)>,
    pub sol_price: Option<UsdPrice>,
    pub quotes: Vec<Quote>,
    pub price_samples: HashMap<(String, String), TimeSeries>,
    pub rate_limited: u64,
    pub errors: u64,
    pub pending_confirm: Vec<OpportunityId>,
    pub ui_dropped: u64,
    pub risk_checked: u64,
    pub risk_approved: u64,
    pub risk_violations: BTreeMap<String, u64>,
    pub sims: u64,
    pub sim_failed: u64,
    pub sim_fail_classes: BTreeMap<String, u64>,
}

impl ViewModel {
    pub fn new(replay: bool) -> Self {
        Self { replay, ..Default::default() }
    }

    pub fn series(&self, m: MetricId) -> Option<&TimeSeries> {
        self.series.get(&m)
    }

    fn push_metric(&mut self, m: MetricId, ts: Ts, v: f64) {
        self.series.entry(m).or_insert_with(|| TimeSeries::with_capacity(40_000)).push(ts, v);
    }

    fn log(&mut self, ts: Ts, level: LogLevel, source: &str, message: String) {
        if self.logs.len() == MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(LogLine { ts, level, source: source.into(), message });
    }

    fn marker(&mut self, ts: Ts, kind: MarkerKind, id: OpportunityId) {
        if self.markers.len() == MAX_MARKERS {
            self.markers.pop_front();
        }
        self.markers.push_back(Marker { ts, kind, opportunity: id });
    }

    /// Session PnL in USD (simulated + realized).
    pub fn session_pnl(&self) -> UsdMicros {
        self.realized.saturating_add(self.simulated)
    }

    pub fn equity_usd(&self) -> Option<UsdMicros> {
        let (l, _) = self.equity_lamports.as_ref()?;
        self.sol_price?.value(*l as i128, 9)
    }

    /// Opportunities newest first.
    pub fn opps_newest(&self) -> impl Iterator<Item = &Opportunity> {
        self.opps.values().rev()
    }

    pub fn apply(&mut self, e: &Event) {
        self.revision += 1;
        let ts = e.ts();
        if !matches!(e, Event::Session(_)) {
            if self.first_ts.is_none_or(|f| ts < f) {
                self.first_ts = Some(ts);
            }
            if self.last_ts.is_none_or(|l| ts > l) {
                self.last_ts = Some(ts);
            }
        }
        match e {
            Event::Session(s) => {
                self.session = Some(s.clone());
                if let Some(l) = s.paper_equity_lamports
                    && self.equity_lamports.is_none()
                {
                    self.equity_lamports = Some((l, "paper notional".into()));
                }
                self.log(
                    s.started_at,
                    LogLevel::Info,
                    "session",
                    format!("{} started · {}", s.session_id, s.config_summary),
                );
            }
            Event::Slot { ts, slot } => {
                self.slot = Some(*slot);
                self.slot_ts = Some(*ts);
            }
            Event::BlockHeight { height, .. } => self.block_height = Some(*height),
            Event::Sample(s) => {
                // USD valuation uses SOL prices only (other pairs share the stream).
                let sol = s.pair.starts_with("SOL/");
                if sol && ((s.side == SampleSide::Sell && s.source.contains("best route")) || self.sol_price.is_none())
                {
                    self.sol_price = Some(s.price);
                }
                let side = format!("{:?}", s.side).to_lowercase();
                match self.quotes.iter_mut().find(|q| q.pair == s.pair && q.side == s.side && q.source == s.source) {
                    Some(q) => {
                        q.price = s.price;
                        q.ts = s.ts;
                        q.samples += 1;
                        q.size_atoms = s.size_atoms;
                    }
                    None => self.quotes.push(Quote {
                        pair: s.pair.clone(),
                        side: s.side,
                        source: s.source.clone(),
                        price: s.price,
                        first: s.price,
                        ts: s.ts,
                        samples: 1,
                        size_atoms: s.size_atoms,
                    }),
                }
                self.price_samples
                    .entry((format!("{} {side}", s.pair), s.source.clone()))
                    .or_insert_with(|| TimeSeries::with_capacity(20_000))
                    .push(s.ts, s.price.f64());
            }
            Event::Metric { ts, metric, value } => self.push_metric(*metric, *ts, *value),
            Event::Opportunity(o) => {
                let is_new = !self.opps.contains_key(&o.id);
                if is_new {
                    self.total_opps += 1;
                    let c = self.strategy_counts.entry(o.strategy).or_default();
                    c.0 += 1;
                    if o.eval.gross_pnl > 0 {
                        c.1 += 1;
                        self.marker(o.detected_at, MarkerKind::Opportunity, o.id);
                    }
                }
                let prev_skip = self.opps.get(&o.id).and_then(|p| p.status.skip());
                if prev_skip != o.status.skip() {
                    if let Some(p) = prev_skip
                        && let Some(n) = self.skip_counts.get_mut(&p)
                    {
                        *n = n.saturating_sub(1);
                    }
                    if let Some(r) = o.status.skip() {
                        *self.skip_counts.entry(r).or_default() += 1;
                    }
                }
                match o.status {
                    OppStatus::AwaitingConfirm => {
                        if !self.pending_confirm.contains(&o.id) {
                            self.pending_confirm.push(o.id);
                        }
                    }
                    _ => self.pending_confirm.retain(|id| *id != o.id),
                }
                self.opps.insert(o.id, (**o).clone());
                while self.opps.len() > MAX_OPPS {
                    self.opps.pop_first();
                }
            }
            Event::Stage(s) => {
                if self.stream.len() == MAX_STREAM {
                    self.stream.pop_front();
                }
                self.stream.push_back(s.clone());
            }
            Event::Simulation(sim) => {
                self.sims += 1;
                if !sim.ok {
                    self.sim_failed += 1;
                    let c = sim.failure.as_ref().map(|f| f.class.label()).unwrap_or("unknown");
                    *self.sim_fail_classes.entry(c.to_string()).or_default() += 1;
                }
            }
            Event::Risk(r) => {
                self.risk_checked += 1;
                if r.approved {
                    self.risk_approved += 1;
                }
                for v in &r.violations {
                    *self.risk_violations.entry(v.code().to_string()).or_default() += 1;
                }
            }
            Event::RawQuote { .. } => {}
            Event::Execution(x) => {
                if matches!(x.state, ExecState::PaperFilled | ExecState::Landed { .. }) {
                    self.executed += 1;
                    self.marker(x.updated_at, MarkerKind::Execution, x.opportunity);
                }
                self.executions.insert(x.opportunity, x.clone());
            }
            Event::Trade(t) => {
                let usd = t.net_usd.unwrap_or_default();
                if t.paper {
                    self.simulated = self.simulated.saturating_add(usd);
                    self.simulated_lamports += t.net;
                } else {
                    self.realized = self.realized.saturating_add(usd);
                    self.realized_lamports += t.net;
                }
                self.marker(t.entry_ts, MarkerKind::Entry, t.opportunity);
                self.marker(t.exit_ts, MarkerKind::Exit, t.opportunity);
                if self.trades.len() == 2_000 {
                    self.trades.pop_front();
                }
                self.trades.push_back(t.clone());
                if let Some((l, src)) = self.equity_lamports.clone()
                    && src == "paper notional"
                {
                    let nl = (l as i64 + t.net).max(0) as u64;
                    self.equity_lamports = Some((nl, src));
                }
                if let Some(eq) = self.equity_usd() {
                    self.push_metric(MetricId::Equity, t.exit_ts, eq.f64());
                }
            }
            Event::Health { service, snapshot, .. } => {
                self.health.insert(*service, snapshot.clone());
            }
            Event::RateLimited { ts, service, backoff_ms, .. } => {
                self.rate_limited += 1;
                self.log(
                    *ts,
                    LogLevel::Warn,
                    service.label(),
                    format!("429 rate limited · backing off {backoff_ms} ms"),
                );
            }
            Event::Error { ts, service, message } => {
                self.errors += 1;
                self.log(*ts, LogLevel::Error, service, message.clone());
            }
            Event::KillSwitch { ts, engaged, reason } => {
                self.kill = Some((*engaged, reason.clone(), *ts));
                let msg =
                    if *engaged { format!("KILL SWITCH ENGAGED · {reason}") } else { "kill switch released".into() };
                self.log(*ts, LogLevel::Warn, "risk", msg);
            }
            Event::TipFloor(t) => self.tip_floor = Some(t.clone()),
            Event::Network(n) => self.network = Some(n.clone()),
            Event::Equity { ts, lamports, source } => {
                self.equity_lamports = Some((*lamports, source.clone()));
                if let Some(eq) = self.equity_usd() {
                    self.push_metric(MetricId::Equity, *ts, eq.f64());
                }
            }
            Event::Log { ts, level, message } => self.log(*ts, *level, "engine", message.clone()),
        }
    }

    pub fn kill_engaged(&self) -> bool {
        self.kill.as_ref().is_some_and(|k| k.0)
    }

    pub fn mode(&self) -> Mode {
        self.session.as_ref().map(|s| s.mode).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::costs::CostBreakdown;
    use searcher_core::profit::ProfitEval;
    use searcher_core::{Address, Ppm};

    pub fn opp(id: u64, gross: i64, status: OppStatus) -> Opportunity {
        Opportunity {
            id: OpportunityId(id),
            key: "k".into(),
            strategy: StrategyKind::CrossDex,
            label: "A → B".into(),
            detected_at: Ts(id as i64 * 1_000_000),
            slot: None,
            base_mint: Address([1; 32]),
            input: 1_000_000_000,
            gross_output: (1_000_000_000 + gross) as u64,
            route: Route::default(),
            costs: CostBreakdown::default(),
            eval: ProfitEval { gross_pnl: gross, gross_edge: Ppm(gross / 1000), ..Default::default() },
            status,
            updated_at: Ts(id as i64 * 1_000_000),
            sol_price: None,
            simulation: None,
            risk: None,
            guard: None,
        }
    }

    #[test]
    fn counts_skips_and_updates_without_double_counting() {
        let mut vm = ViewModel::new(false);
        vm.apply(&Event::Opportunity(Box::new(opp(1, 5, OppStatus::Skipped(SkipReason::EdgeTooSmall)))));
        vm.apply(&Event::Opportunity(Box::new(opp(2, -5, OppStatus::Quoted))));
        vm.apply(&Event::Opportunity(Box::new(opp(2, -5, OppStatus::Skipped(SkipReason::SimFailed)))));
        vm.apply(&Event::Opportunity(Box::new(opp(1, 5, OppStatus::Skipped(SkipReason::EdgeTooSmall)))));
        assert_eq!(vm.total_opps, 2);
        assert_eq!(vm.skip_counts[&SkipReason::EdgeTooSmall], 1);
        assert_eq!(vm.skip_counts[&SkipReason::SimFailed], 1);
        assert_eq!(vm.strategy_counts[&StrategyKind::CrossDex], (2, 1));
        assert_eq!(vm.markers.len(), 1, "gross-positive opportunities get a marker");
        assert_eq!(vm.opps_newest().next().unwrap().id, OpportunityId(2));
    }

    #[test]
    fn paper_trades_are_simulated_pnl_not_realized() {
        let mut vm = ViewModel::new(false);
        vm.apply(&Event::Equity { ts: Ts(1), lamports: 1_000_000_000, source: "paper notional".into() });
        vm.apply(&Event::Metric { ts: Ts(2), metric: MetricId::Price, value: 105.0 });
        vm.sol_price = Some(UsdPrice::new(105_000_000));
        vm.apply(&Event::Trade(TradeResult {
            opportunity: OpportunityId(1),
            strategy: StrategyKind::CrossDex,
            label: "x".into(),
            mode: Mode::Paper,
            paper: true,
            entry_ts: Ts(3),
            exit_ts: Ts(4),
            input: 1,
            output: 1,
            fees_lamports: 0,
            tip_lamports: 0,
            expected_net: 10_000,
            net: 10_000,
            net_usd: Some(UsdMicros(1_050)),
        }));
        assert_eq!(vm.simulated, UsdMicros(1_050));
        assert_eq!(vm.realized, UsdMicros::ZERO);
        assert_eq!(vm.equity_lamports.as_ref().unwrap().0, 1_000_010_000);
        assert_eq!(vm.markers.iter().filter(|m| m.kind == MarkerKind::Entry).count(), 1);
        assert!(vm.series(MetricId::Equity).is_some());
    }

    #[test]
    fn only_sol_pairs_set_the_usd_price() {
        let mut vm = ViewModel::new(false);
        let sample = |pair: &str, micros: u64, side| {
            Event::Sample(MarketSample {
                ts: Ts(1),
                pair: pair.into(),
                price: UsdPrice::new(micros),
                side,
                source: "Pyth".into(),
                size_atoms: 0,
            })
        };
        vm.apply(&sample("JUP/USD", 274_206, SampleSide::Oracle));
        assert_eq!(vm.sol_price, None, "a JUP price never values SOL equity");
        vm.apply(&sample("SOL/USD", 112_290_000, SampleSide::Oracle));
        assert_eq!(vm.sol_price, Some(UsdPrice::new(112_290_000)));
        assert_eq!(vm.quotes.len(), 2, "both feeds are listed");
    }

    #[test]
    fn bounded_collections() {
        let mut vm = ViewModel::new(false);
        for i in 0..(MAX_OPPS as u64 + 50) {
            vm.apply(&Event::Opportunity(Box::new(opp(i + 1, -1, OppStatus::Quoted))));
        }
        assert_eq!(vm.opps.len(), MAX_OPPS);
        assert_eq!(vm.total_opps, MAX_OPPS as u64 + 50);
    }
}
