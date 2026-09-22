//! Risk engine and kill switch. The engine is the last gate before anything
//! is executed (paper or live); it never talks to the network.

use parking_lot::Mutex;
use searcher_core::config::RiskConfig;
use searcher_core::model::{Mode, Opportunity, RiskDecision, RiskViolation, SimFidelity};
use searcher_core::units::parse_decimal;
use searcher_core::{Ts, UsdMicros, UsdPrice};
use std::sync::atomic::{AtomicBool, Ordering};

/// Stops new trades. Never kills the process or touches recorded data.
#[derive(Debug, Default)]
pub struct KillSwitch {
    engaged: AtomicBool,
    reason: Mutex<String>,
}

impl KillSwitch {
    pub fn engage(&self, reason: &str) -> bool {
        *self.reason.lock() = reason.to_string();
        !self.engaged.swap(true, Ordering::SeqCst)
    }

    pub fn release(&self) -> bool {
        self.engaged.swap(false, Ordering::SeqCst)
    }

    pub fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::SeqCst)
    }

    pub fn reason(&self) -> String {
        self.reason.lock().clone()
    }
}

#[derive(Clone, Debug)]
pub struct RiskLimits {
    pub max_trade_lamports: u64,
    pub max_trade_pct_of_equity_ppm: i64,
    pub max_daily_loss: UsdMicros,
    pub max_consecutive_failures: u32,
    pub max_slippage_bps: u16,
    pub max_quote_age_ms: u64,
    pub max_simulation_age_ms: u64,
    pub max_priority_fee_lamports: u64,
    pub max_jito_tip_lamports: u64,
    pub min_wallet_sol_for_fees_lamports: u64,
    pub max_open_executions: u32,
    pub max_slot_lag: u64,
}

impl RiskLimits {
    pub fn from_config(c: &RiskConfig) -> Result<Self, String> {
        Ok(Self {
            max_trade_lamports: c.max_trade_lamports,
            max_trade_pct_of_equity_ppm: c.max_trade_pct_of_equity_bps * 100,
            max_daily_loss: UsdMicros(parse_decimal(&c.max_daily_loss_usd, 6)? as i64),
            max_consecutive_failures: c.max_consecutive_failures,
            max_slippage_bps: c.max_slippage_bps,
            max_quote_age_ms: c.max_quote_age_ms,
            max_simulation_age_ms: c.max_simulation_age_ms,
            max_priority_fee_lamports: c.max_priority_fee_lamports,
            max_jito_tip_lamports: c.max_jito_tip_lamports,
            min_wallet_sol_for_fees_lamports: c.min_wallet_sol_for_fees_lamports,
            max_open_executions: c.max_open_executions,
            max_slot_lag: c.max_slot_lag,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RiskState {
    /// UTC day number the daily PnL belongs to.
    pub day: i64,
    pub daily_pnl: UsdMicros,
    pub consecutive_failures: u32,
    pub open_executions: u32,
}

/// Live facts the engine needs, gathered by the caller.
#[derive(Clone, Debug)]
pub struct RiskContext {
    pub now: Ts,
    pub mode: Mode,
    pub equity_lamports: u64,
    /// Fee-payer SOL balance if known. Unknown is a violation when sending.
    pub wallet_lamports: Option<u64>,
    pub sol_price: Option<UsdPrice>,
    pub latest_slot: Option<u64>,
}

pub struct RiskEngine {
    limits: parking_lot::RwLock<RiskLimits>,
    state: Mutex<RiskState>,
    kill: std::sync::Arc<KillSwitch>,
}

fn utc_day(ts: Ts) -> i64 {
    ts.micros().div_euclid(86_400_000_000)
}

impl RiskEngine {
    pub fn new(limits: RiskLimits, kill: std::sync::Arc<KillSwitch>) -> Self {
        Self { limits: parking_lot::RwLock::new(limits), state: Mutex::new(RiskState::default()), kill }
    }

    pub fn limits(&self) -> RiskLimits {
        self.limits.read().clone()
    }

    /// Replace the limits (operator change while running).
    pub fn set_limits(&self, limits: RiskLimits) {
        *self.limits.write() = limits;
    }

    pub fn kill_switch(&self) -> &std::sync::Arc<KillSwitch> {
        &self.kill
    }

    pub fn state(&self, now: Ts) -> RiskState {
        let mut s = self.state.lock();
        Self::roll_day(&mut s, now);
        s.clone()
    }

    fn roll_day(s: &mut RiskState, now: Ts) {
        let d = utc_day(now);
        if s.day != d {
            s.day = d;
            s.daily_pnl = UsdMicros::ZERO;
        }
    }

    /// Evaluate an opportunity that has been priced (and ideally simulated).
    pub fn evaluate(&self, o: &Opportunity, ctx: &RiskContext) -> RiskDecision {
        let l = &self.limits();
        let mut v = Vec::new();
        let st = self.state(ctx.now);

        if self.kill.is_engaged() {
            v.push(RiskViolation::KillSwitch);
        }
        if o.input > l.max_trade_lamports {
            v.push(RiskViolation::TradeSize { input: o.input, max: l.max_trade_lamports });
        }
        if ctx.equity_lamports > 0 {
            let pct = (o.input as i128 * 1_000_000 / ctx.equity_lamports as i128) as i64;
            if pct > l.max_trade_pct_of_equity_ppm {
                v.push(RiskViolation::TradePctOfEquity { pct_ppm: pct, max_ppm: l.max_trade_pct_of_equity_ppm });
            }
        }
        if -st.daily_pnl.0 >= l.max_daily_loss.0 {
            v.push(RiskViolation::DailyLoss { loss_usd_micros: -st.daily_pnl.0, max_usd_micros: l.max_daily_loss.0 });
        }
        if st.consecutive_failures >= l.max_consecutive_failures {
            v.push(RiskViolation::ConsecutiveFailures {
                count: st.consecutive_failures,
                max: l.max_consecutive_failures,
            });
        }
        if let Some(worst) = o.route.legs.iter().map(|x| x.slippage_bps).max()
            && worst > l.max_slippage_bps
        {
            v.push(RiskViolation::Slippage { bps: worst, max_bps: l.max_slippage_bps });
        }
        let qa = o.quote_age_ms(ctx.now);
        if qa > l.max_quote_age_ms {
            v.push(RiskViolation::QuoteAge { age_ms: qa, max_ms: l.max_quote_age_ms });
        }
        match &o.simulation {
            None => v.push(RiskViolation::SimulationMissing),
            Some(sim) => {
                if !sim.ok {
                    v.push(RiskViolation::SimulationFailed);
                }
                let age = sim.simulated_at.age_ms(ctx.now);
                if age > l.max_simulation_age_ms {
                    v.push(RiskViolation::SimulationAge { age_ms: age, max_ms: l.max_simulation_age_ms });
                }
                if sim.fidelity != SimFidelity::Exact && ctx.mode.sends_transactions() {
                    v.push(RiskViolation::InexactSimulation);
                }
                if let (Some(latest), Some(at)) = (ctx.latest_slot, sim.context_slot)
                    && latest.saturating_sub(at) > l.max_slot_lag
                {
                    v.push(RiskViolation::SlotStale { age_slots: latest - at, max_slots: l.max_slot_lag });
                }
            }
        }
        if o.costs.priority_fee > l.max_priority_fee_lamports {
            v.push(RiskViolation::PriorityFee { lamports: o.costs.priority_fee, max: l.max_priority_fee_lamports });
        }
        if o.costs.jito_tip > l.max_jito_tip_lamports {
            v.push(RiskViolation::JitoTip { lamports: o.costs.jito_tip, max: l.max_jito_tip_lamports });
        }
        match ctx.wallet_lamports {
            Some(w) if w < l.min_wallet_sol_for_fees_lamports => {
                v.push(RiskViolation::WalletSolForFees { lamports: w, min: l.min_wallet_sol_for_fees_lamports })
            }
            None if ctx.mode.sends_transactions() => {
                v.push(RiskViolation::WalletSolForFees { lamports: 0, min: l.min_wallet_sol_for_fees_lamports })
            }
            _ => {}
        }
        if st.open_executions >= l.max_open_executions {
            v.push(RiskViolation::OpenExecutions { open: st.open_executions, max: l.max_open_executions });
        }
        let net = o.eval.simulated_net.map_or(o.eval.expected_net, |s| s.min(o.eval.expected_net));
        if net <= 0 {
            v.push(RiskViolation::NegativeExpectedPnl { lamports: net });
        }
        RiskDecision { opportunity: o.id, approved: v.is_empty(), violations: v, checked_at: ctx.now }
    }

    pub fn open_execution(&self) {
        self.state.lock().open_executions += 1;
    }

    /// Record an execution outcome (paper or live).
    pub fn close_execution(&self, now: Ts, success: bool, net_usd: UsdMicros) {
        let mut s = self.state.lock();
        Self::roll_day(&mut s, now);
        s.open_executions = s.open_executions.saturating_sub(1);
        s.daily_pnl = s.daily_pnl.saturating_add(net_usd);
        if success {
            s.consecutive_failures = 0;
        } else {
            s.consecutive_failures += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::costs::CostBreakdown;
    use searcher_core::model::*;
    use searcher_core::profit::ProfitEval;
    use searcher_core::{Address, Ppm};
    use std::sync::Arc;

    fn limits() -> RiskLimits {
        RiskLimits::from_config(&RiskConfig::default()).unwrap()
    }

    fn leg(slip: u16, quoted_at: Ts) -> Leg {
        Leg {
            index: 0,
            input_mint: Address([1; 32]),
            output_mint: Address([1; 32]),
            in_amount: 1,
            out_amount: 1,
            min_out: 1,
            slippage_bps: slip,
            slippage_spec: SlippageSpec::Rtse,
            price_impact: Ppm::ZERO,
            hops: vec![],
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            quoted_at,
            latency_ms: 0,
            cu_price_micro: None,
            last_valid_block_height: 0,
            request_id: None,
        }
    }

    fn sim(at: Ts, ok: bool, fidelity: SimFidelity, slot: u64) -> SimulationResult {
        SimulationResult {
            opportunity: OpportunityId(1),
            plan: PlanKind::SingleTx,
            fidelity,
            ok,
            failure: None,
            txs: vec![],
            latency_ms: 10,
            simulated_at: at,
            context_slot: Some(slot),
        }
    }

    fn good(now: Ts) -> Opportunity {
        Opportunity {
            id: OpportunityId(1),
            key: "k".into(),
            strategy: StrategyKind::CrossDex,
            label: "A → B".into(),
            detected_at: now,
            slot: Some(100),
            base_mint: Address([1; 32]),
            input: 100_000_000,
            gross_output: 100_200_000,
            route: Route { legs: vec![leg(20, now), leg(20, now)] },
            costs: CostBreakdown { priority_fee: 1_000, jito_tip: 10_000, ..Default::default() },
            eval: ProfitEval { expected_net: 50_000, simulated_net: Some(40_000), ..Default::default() },
            status: OppStatus::Quoted,
            updated_at: now,
            sol_price: Some(UsdPrice::new(100_000_000)),
            simulation: Some(sim(now, true, SimFidelity::Exact, 100)),
            risk: None,
            guard: None,
        }
    }

    fn ctx(now: Ts, mode: Mode) -> RiskContext {
        RiskContext {
            now,
            mode,
            equity_lamports: 1_000_000_000,
            wallet_lamports: Some(1_000_000_000),
            sol_price: Some(UsdPrice::new(100_000_000)),
            latest_slot: Some(101),
        }
    }

    fn codes(d: &RiskDecision) -> Vec<&'static str> {
        d.violations.iter().map(|v| v.code()).collect()
    }

    #[test]
    fn clean_opportunity_passes() {
        let now = Ts::from_secs(1_000_000);
        let e = RiskEngine::new(limits(), Arc::new(KillSwitch::default()));
        let d = e.evaluate(&good(now), &ctx(now, Mode::Paper));
        assert!(d.approved, "{:?}", d.violations);
    }

    #[test]
    fn every_limit_trips() {
        let now = Ts::from_secs(1_000_000);
        let kill = Arc::new(KillSwitch::default());
        let e = RiskEngine::new(limits(), kill.clone());
        let mut o = good(now);
        o.input = 5_000_000_000;
        o.route.legs[1].slippage_bps = 500;
        o.route.legs[0].quoted_at = now.plus_ms(-10_000);
        o.costs.priority_fee = 1_000_000;
        o.costs.jito_tip = 1_000_000;
        o.eval.simulated_net = Some(-1);
        o.simulation = Some(sim(now.plus_ms(-10_000), false, SimFidelity::PerTx, 10));
        kill.engage("test");
        let mut c = ctx(now, Mode::Live);
        c.wallet_lamports = Some(1);
        for _ in 0..5 {
            e.close_execution(now, false, UsdMicros(-2_000_000));
        }
        e.open_execution();
        let d = e.evaluate(&o, &c);
        assert!(!d.approved);
        for code in [
            "kill_switch",
            "max_trade_size",
            "max_trade_pct_of_equity",
            "max_daily_loss",
            "max_consecutive_failures",
            "max_slippage",
            "max_quote_age_ms",
            "simulation_failed",
            "max_simulation_age_ms",
            "exact_simulation_required",
            "stale_slot",
            "max_priority_fee",
            "max_jito_tip",
            "min_wallet_sol_for_fees",
            "max_open_execution",
            "negative_expected_pnl",
        ] {
            assert!(codes(&d).contains(&code), "missing {code}: {:?}", codes(&d));
        }
        assert_eq!(d.primary_skip(), Some(SkipReason::KillSwitch));
    }

    #[test]
    fn missing_simulation_and_unknown_wallet_fail_closed_when_sending() {
        let now = Ts::from_secs(1_000_000);
        let e = RiskEngine::new(limits(), Arc::new(KillSwitch::default()));
        let mut o = good(now);
        o.simulation = None;
        let mut c = ctx(now, Mode::Live);
        c.wallet_lamports = None;
        let d = e.evaluate(&o, &c);
        assert!(codes(&d).contains(&"simulation_required"));
        assert!(codes(&d).contains(&"min_wallet_sol_for_fees"));
        // PAPER does not require a wallet balance
        let mut c = ctx(now, Mode::Paper);
        c.wallet_lamports = None;
        o.simulation = Some(sim(now, true, SimFidelity::PerTx, 100));
        assert!(e.evaluate(&o, &c).approved, "per-tx fidelity is allowed in PAPER");
    }

    #[test]
    fn daily_loss_resets_next_utc_day_and_kill_switch_releases() {
        let day1 = Ts::from_secs(86_400 * 20_000 + 10);
        let kill = Arc::new(KillSwitch::default());
        let e = RiskEngine::new(limits(), kill.clone());
        e.open_execution();
        e.close_execution(day1, false, UsdMicros(-6_000_000));
        assert!(!e.evaluate(&good(day1), &ctx(day1, Mode::Paper)).approved);
        let day2 = Ts::from_secs(86_400 * 20_001 + 10);
        assert_eq!(e.state(day2).daily_pnl, UsdMicros::ZERO);
        assert!(kill.engage("k"));
        assert!(!kill.engage("k"), "second engage is a no-op");
        assert!(kill.is_engaged());
        assert!(kill.release());
        assert!(!kill.is_engaged());
    }
}
