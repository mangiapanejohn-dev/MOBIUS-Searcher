//! Opportunity pipeline:
//!
//! scanner (strategy scheduler → Jupiter `/build` per leg → pricing →
//! min-out protection → assembly) ──bounded──► simulator (simulate →
//! actual CU → CU limit × margin → re-price → risk) → executor.
//!
//! Every evaluated cycle is emitted, including unprofitable and failed ones.

use crate::assemble::{
    AssembledTx, AssemblyParams, ata_create_target, compose_bundle, compose_single, message_account_keys,
    sol_equivalent_delta, wsol_accounts,
};
use crate::live::{LiveBackend, LiveOutcome, LiveParams, execute_live};
use crate::probe::{Observes, Probe, union};
use crate::simulate::{failure_from, tx_sim};
use crate::view::RuntimeView;
use crate::wallet::Wallet;
use base64::Engine;
use parking_lot::Mutex;
use searcher_core::costs::{CostParams, TxShape};
use searcher_core::event::{Stage, StageEvent};
use searcher_core::ix::LegInstructions;
use searcher_core::metrics::MetricId;
use searcher_core::model::*;
use searcher_core::profit::ProfitGuards;
use searcher_core::token::TokenRegistry;
use searcher_core::units::{MAX_COMPUTE_UNITS_PER_TX, priority_fee_lamports};
use searcher_core::{Address, Event, Ppm, Ts, UsdPrice};
use searcher_jito::{JitoClient, SendPermit, TipPolicy};
use searcher_jupiter::{BuildRequest, BuiltLeg, JupiterClient, JupiterError};
use searcher_market::{ChainState, RpcClient};
use searcher_risk::{RiskContext, RiskEngine};
use searcher_strategy::pricing::{
    PricingEnv, PricingInput, TipInfo, intermediate_drift_value, price, protective_slippage_bps,
    reprice_after_simulation, required_final_out,
};
use searcher_strategy::{CandidatePlan, LegSpec, Scheduler};
use searcher_telemetry::EventBus;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// First intermediate leg that delivered less than the next leg's fixed
/// input: `(leg, needed, delivered)` (leg numbers from 1).
fn inventory_short(legs: &[Leg], executed: &[u64]) -> Option<(usize, u64, u64)> {
    executed
        .iter()
        .zip(legs.iter().skip(1))
        .enumerate()
        .find(|(_, (got, next))| **got < next.in_amount)
        .map(|(i, (got, next))| (i + 1, next.in_amount, *got))
}

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub mode: Mode,
    pub live_enabled: bool,
    /// Taker for `/build` and simulation (bot wallet or paper shadow taker).
    pub taker: Option<Address>,
    pub slippage: SlippageSpec,
    pub cu_price_percentile: String,
    pub blockhash_slots_to_expiry: u16,
    pub for_jito_bundle: bool,
    pub cost_params: CostParams,
    pub guards: ProfitGuards,
    pub protect_min_out: bool,
    pub prefer_single_tx: bool,
    pub max_quote_age_ms: u64,
    /// Simulate unprofitable candidates too (budget permitting) so that
    /// simulation statistics exist even when nothing is profitable.
    pub simulate_unprofitable: bool,
    pub confirm_timeout: Duration,
    pub dont_front: bool,
    pub paper_equity_lamports: u64,
    pub live: LiveParams,
}

/// A candidate handed from the scanner to the simulator.
pub struct SimJob {
    pub opp: Opportunity,
    pub legs: Vec<LegInstructions>,
    pub plan: PlanKind,
    pub txs: Vec<AssembledTx>,
    pub sim_cu_limit: u32,
    pub sim_cu_price: u64,
    pub sim_tip: u64,
    pub tip_account: Option<Address>,
    pub atas_to_create: u8,
    pub taker: Address,
}

/// Outcome of pricing a fully quoted route.
pub struct Finished {
    /// Prepared for simulation (None: assembly failed).
    pub job: Option<SimJob>,
    pub gross_bp: f64,
    pub net_bp: f64,
}

#[derive(Default)]
pub struct Confirmations {
    pending: Mutex<HashMap<OpportunityId, oneshot::Sender<bool>>>,
}

impl Confirmations {
    pub fn register(&self, id: OpportunityId) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        rx
    }

    pub fn resolve(&self, id: OpportunityId, approve: bool) -> bool {
        self.pending.lock().remove(&id).map(|tx| tx.send(approve).is_ok()).unwrap_or(false)
    }
}

pub struct RealBackend {
    pub rpc: Arc<RpcClient>,
    pub jito: Arc<JitoClient>,
    pub chain: Arc<ChainState>,
}

impl LiveBackend for RealBackend {
    async fn simulate_signed(&self, tx_b64: String) -> Result<searcher_market::SimulateOutcome, String> {
        self.rpc.simulate_signed(&tx_b64).await.map_err(|e| e.to_string())
    }
    async fn send_bundle(&self, permit: &SendPermit, txs: Vec<String>) -> Result<String, String> {
        self.jito.send_bundle(permit, &txs).await.map_err(|e| e.to_string())
    }
    async fn inflight(&self, id: String) -> Result<Option<searcher_jito::InflightStatus>, String> {
        self.jito
            .inflight_statuses(&[id])
            .await
            .map(|v| v.into_iter().next().map(|(_, s)| s))
            .map_err(|e| e.to_string())
    }
    async fn balance(&self, a: Address) -> Result<u64, String> {
        self.rpc.get_balance(&a).await.map_err(|e| e.to_string())
    }
    fn block_height(&self) -> Option<u64> {
        self.chain.block_height()
    }
    async fn bundle_status(&self, id: String) -> Result<crate::live::BundleConfirmation, String> {
        self.jito
            .bundle_statuses(&[id])
            .await
            .map(|v| v.into_iter().next().map(|s| (s.confirmation_status, s.err)))
            .map_err(|e| e.to_string())
    }
}

pub struct Pipeline {
    pub cfg: PipelineConfig,
    pub tokens: TokenRegistry,
    pub jupiter: Arc<JupiterClient>,
    pub rpc: Arc<RpcClient>,
    pub risk: Arc<RiskEngine>,
    pub chain: Arc<ChainState>,
    pub bus: Arc<EventBus>,
    pub view: Arc<RuntimeView>,
    pub tip_policy: TipPolicy,
    pub confirms: Arc<Confirmations>,
    pub telemetry: Arc<searcher_telemetry::Telemetry>,
    /// Present only in CONFIRM/LIVE with `live_enabled` and a loaded wallet.
    pub live: Option<(SendPermit, Wallet, RealBackend)>,
    /// Scheduler-independent latency probes.
    pub probe: Arc<Probe>,
    /// Guards, cost parameters and min-out protection in effect; the operator
    /// can replace them while running (`cfg` holds the values at start).
    thresholds: parking_lot::RwLock<Arc<Thresholds>>,
    next_id: AtomicU64,
    last_decision: Mutex<HashMap<String, Instant>>,
}

/// The operator-adjustable part of the pipeline configuration.
#[derive(Clone, Debug)]
pub struct Thresholds {
    pub guards: ProfitGuards,
    pub cost_params: CostParams,
    pub protect_min_out: bool,
    pub slippage: SlippageSpec,
}

const SIM_CU_LIMIT: u32 = MAX_COMPUTE_UNITS_PER_TX;
const DONT_FRONT: &str = "jitodontfront111111111111111111111111111111";
const PRICE_MAX_AGE_MS: u64 = 120_000;

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: PipelineConfig,
        tokens: TokenRegistry,
        jupiter: Arc<JupiterClient>,
        rpc: Arc<RpcClient>,
        risk: Arc<RiskEngine>,
        chain: Arc<ChainState>,
        bus: Arc<EventBus>,
        view: Arc<RuntimeView>,
        tip_policy: TipPolicy,
        live: Option<(SendPermit, Wallet, RealBackend)>,
        telemetry: Arc<searcher_telemetry::Telemetry>,
        probe: Arc<Probe>,
    ) -> Self {
        let thresholds = parking_lot::RwLock::new(Arc::new(Thresholds {
            guards: cfg.guards.clone(),
            cost_params: cfg.cost_params.clone(),
            protect_min_out: cfg.protect_min_out,
            slippage: cfg.slippage,
        }));
        Self {
            cfg,
            thresholds,
            tokens,
            jupiter,
            rpc,
            risk,
            chain,
            bus,
            view,
            tip_policy,
            confirms: Arc::new(Confirmations::default()),
            telemetry,
            live,
            probe,
            next_id: AtomicU64::new(1),
            last_decision: Mutex::new(HashMap::new()),
        }
    }

    /// Probe bookkeeping for one received leg; returns what it observes.
    fn observe_leg(&self, req: &BuildRequest, b: &BuiltLeg) -> Observes {
        let o = self.probe.observes(&req.input_mint, &req.output_mint, &req.dex_filter);
        self.probe.on_quote_sent(&req.key(), &o, b.timing.sent);
        self.probe.on_quote_received(&o, b.timing.sent, b.timing.received);
        if let (Some(h), Some(src)) = (self.chain.block_height(), b.timing.source_block_height) {
            self.telemetry.latency.value("quote.source_lag_blocks", h as i64 - src as i64);
        }
        o
    }

    /// Latency of a strategy decision (route fully quoted and priced).
    fn record_decision(&self, key: &str, picked: Instant, built: &[BuiltLeg], observes: &[Observes]) {
        let (Some(first), Some(last)) = (built.first(), built.last()) else { return };
        let decided = Instant::now();
        let lat = &self.telemetry.latency;
        lat.duration_us("decision.first_wait_us", first.timing.sent.saturating_duration_since(picked));
        lat.duration_us("decision.legs_us", last.timing.received.saturating_duration_since(first.timing.sent));
        lat.duration_us("decision.pick_to_decision_us", decided - picked);
        let oldest = built.iter().map(|b| b.timing.sent).min().unwrap_or(first.timing.sent);
        lat.duration_us("decision.quote_age_us", decided - oldest);
        lat.count("decisions", 1);
        self.probe.on_decision(&union(observes), first.timing.sent, decided);
        if let Some(prev) = self.last_decision.lock().insert(key.to_string(), decided) {
            lat.duration_us("route.decision_interval_us", decided - prev);
        }
    }

    fn emit(&self, e: Event) {
        self.bus.emit(e);
    }

    fn stage(
        &self,
        id: OpportunityId,
        stage: Stage,
        ok: bool,
        subject: impl Into<String>,
        value: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.emit(Event::Stage(StageEvent {
            ts: Ts::now(),
            opportunity: id,
            stage,
            ok,
            subject: subject.into(),
            value: value.into(),
            detail: detail.into(),
        }));
    }

    fn metric(&self, m: MetricId, v: f64) {
        self.emit(Event::Metric { ts: Ts::now(), metric: m, value: v });
    }

    fn sol(&self) -> Address {
        self.tokens.sol().mint
    }

    fn tip_for(&self, pre_tip_profit: i64) -> TipInfo {
        let q = self.tip_policy.quote(self.view.tip_floor().as_ref(), pre_tip_profit);
        TipInfo { lamports: q.lamports, capped: q.capped }
    }

    fn equity_lamports(&self) -> u64 {
        match self.cfg.mode {
            Mode::Paper => (self.cfg.paper_equity_lamports as i64 + self.view.paper_net()).max(0) as u64,
            _ => self.view.wallet_lamports(Ts::now(), 120_000).unwrap_or(0),
        }
    }

    /// The `/build` request for `spec` at `amount` with the configured taker
    /// and slippage (what the scanner and the scheduler both send).
    pub fn request_for(&self, spec: &LegSpec, amount: u64) -> BuildRequest {
        self.build_request(spec, amount, self.cfg.taker.unwrap_or(Address([1; 32])), self.thresholds().slippage)
    }

    fn build_request(&self, spec: &LegSpec, amount: u64, taker: Address, slippage: SlippageSpec) -> BuildRequest {
        BuildRequest {
            input_mint: spec.input,
            output_mint: spec.output,
            amount,
            taker,
            slippage,
            mode: spec.mode,
            dex_filter: spec.dex_filter.clone(),
            cu_price_percentile: self.cfg.cu_price_percentile.clone(),
            max_accounts: spec.max_accounts,
            blockhash_slots_to_expiry: self.cfg.blockhash_slots_to_expiry,
            for_jito_bundle: self.cfg.for_jito_bundle,
        }
    }

    /// Executable price samples from each built leg.
    fn on_leg(&self, leg: &Leg) {
        self.telemetry.record_ok(ServiceId::MarketFeed, leg.latency_ms);
        let sol = self.sol();
        let (Some(tin), Some(tout)) = (self.tokens.by_mint(&leg.input_mint), self.tokens.by_mint(&leg.output_mint))
        else {
            return;
        };
        let source = match &leg.dex_filter {
            DexFilter::Any => "jupiter /build best route".to_string(),
            f => format!("jupiter /build {}", f.describe()),
        };
        let now = Ts::now();
        let sample = if leg.input_mint == sol && tout.usd_stable {
            UsdPrice::from_trade(leg.in_amount, tin.decimals, leg.out_amount, tout.decimals)
                .map(|p| (p, SampleSide::Sell, leg.in_amount))
        } else if tin.usd_stable && leg.output_mint == sol {
            UsdPrice::from_trade(leg.out_amount, tout.decimals, leg.in_amount, tin.decimals)
                .map(|p| (p, SampleSide::Buy, leg.out_amount))
        } else {
            None
        };
        if let Some((p, side, size)) = sample {
            self.emit(Event::Sample(MarketSample {
                ts: now,
                pair: format!("SOL/{}", if side == SampleSide::Sell { &tout.symbol } else { &tin.symbol }),
                price: p,
                side,
                source,
                size_atoms: size,
            }));
            if side == SampleSide::Sell && leg.dex_filter == DexFilter::Any {
                self.view.set_sol_price(p, now, "jupiter /build SOL→USDC");
                self.metric(MetricId::Price, p.f64());
            } else if self.view.sol_price(now, PRICE_MAX_AGE_MS).is_none() {
                self.view.set_sol_price(p, now, "jupiter /build (dex-constrained)");
            }
        }
        self.metric(MetricId::JupiterLatency, leg.latency_ms as f64);
    }

    fn pricing_env<'a>(t: &'a Thresholds, tip: &'a dyn Fn(i64) -> TipInfo) -> PricingEnv<'a> {
        PricingEnv { cost_params: &t.cost_params, guards: &t.guards, tip }
    }

    /// Thresholds in effect now.
    pub fn thresholds(&self) -> Arc<Thresholds> {
        self.thresholds.read().clone()
    }

    pub fn set_thresholds(&self, t: Thresholds) {
        *self.thresholds.write() = Arc::new(t);
    }

    // ───────────────────────────── scanner ─────────────────────────────

    pub async fn run_scanner(
        self: Arc<Self>,
        mut scheduler: Scheduler,
        sim_tx: mpsc::Sender<SimJob>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let Some(plan) = scheduler.next_plan() else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            tokio::select! {
                _ = shutdown.changed() => return,
                job = self.evaluate(plan) => {
                    if let Some(job) = job {
                        self.submit(job, &sim_tx).await;
                    }
                }
            }
        }
    }

    /// Build, price, protect and assemble one plan. Emits everything it learns.
    pub async fn evaluate(&self, plan: CandidatePlan) -> Option<SimJob> {
        let picked = Instant::now();
        let id = self.new_id();
        let taker = self.cfg.taker.unwrap_or(Address([1; 32]));
        let mut built: Vec<BuiltLeg> = Vec::with_capacity(plan.legs.len());
        let mut observes: Vec<Observes> = Vec::with_capacity(plan.legs.len());
        let mut amount = plan.amount;
        for (i, spec) in plan.legs.iter().enumerate() {
            let req = self.build_request(spec, amount, taker, self.thresholds().slippage);
            match self.jupiter.build(&req, i as u8).await {
                Ok(b) => {
                    observes.push(self.observe_leg(&req, &b));
                    self.on_leg(&b.leg);
                    self.emit(Event::RawQuote {
                        ts: Ts::now(),
                        opportunity: id,
                        leg: i as u8,
                        body: compact_raw(&b.raw),
                    });
                    amount = b.leg.out_amount;
                    built.push(b);
                }
                Err(e) => {
                    self.on_build_error(id, &plan, built, e);
                    return None;
                }
            }
        }
        self.finish(id, &plan, built, &observes, picked).await.job
    }

    pub fn new_id(&self) -> OpportunityId {
        OpportunityId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Emit what a freshly fetched leg tells (price samples, latency, raw body).
    pub fn record_fetched_leg(&self, id: OpportunityId, index: u8, req: &BuildRequest, b: &BuiltLeg) -> Observes {
        let o = self.observe_leg(req, b);
        self.on_leg(&b.leg);
        self.emit(Event::RawQuote { ts: Ts::now(), opportunity: id, leg: index, body: compact_raw(&b.raw) });
        o
    }

    /// Hand a prepared job to the simulator: a real candidate waits for a
    /// slot (bounded by the quote age budget); an unprofitable sample is
    /// simulated only if the simulator is idle.
    pub async fn submit(&self, job: SimJob, sim_tx: &mpsc::Sender<SimJob>) {
        if job.opp.status == OppStatus::Quoted {
            let _ = tokio::time::timeout(Duration::from_millis(self.cfg.max_quote_age_ms), sim_tx.send(job)).await;
        } else if self.cfg.simulate_unprofitable {
            let _ = sim_tx.try_send(job);
        }
    }

    /// Price a fully quoted route (the strategy decision), then protect and
    /// assemble it. `picked` = when the route was chosen for evaluation.
    pub async fn finish(
        &self,
        id: OpportunityId,
        plan: &CandidatePlan,
        mut built: Vec<BuiltLeg>,
        observes: &[Observes],
        picked: Instant,
    ) -> Finished {
        let latency: u32 = built.iter().map(|b| b.leg.latency_ms).sum();
        let mut opp = self.price_legs(id, plan, built.iter().map(|b| b.leg.clone()).collect(), TxShape::SINGLE, 0);
        self.record_decision(&plan.key, picked, &built, observes);
        let (gross_bp, net_bp) = (opp.eval.gross_edge.bps_f64(), opp.eval.net_edge.bps_f64());
        let job = self.prepare(id, plan, &mut built, &mut opp, latency).await;
        Finished { job, gross_bp, net_bp }
    }

    async fn prepare(
        &self,
        id: OpportunityId,
        plan: &CandidatePlan,
        built: &mut [BuiltLeg],
        opp: &mut Opportunity,
        latency: u32,
    ) -> Option<SimJob> {
        let taker = self.cfg.taker.unwrap_or(Address([1; 32]));
        self.stage(id, Stage::Opportunity, true, &plan.label, opp.eval.net_edge.to_string(), opp.route.dex_path());
        self.stage(
            id,
            Stage::Build,
            true,
            "Jupiter",
            format!("{latency}ms"),
            format!("{} legs · {}", built.len(), plan.strategy.label()),
        );
        self.metric(MetricId::GrossEdge, opp.eval.gross_edge.bps_f64());
        self.metric(MetricId::NetEdge, opp.eval.net_edge.bps_f64());
        if let Some(w) = opp.route.legs.iter().map(|l| l.slippage_bps).max() {
            self.metric(MetricId::Slippage, w as f64);
        }
        self.metric(MetricId::JitoTip, opp.costs.jito_tip as f64);
        if plan.strategy == StrategyKind::RoundTrip && opp.route.legs.len() == 2 {
            // spread between executable sell and buy price at the same size
            let spread = Ppm::ratio(-(opp.eval.gross_pnl as i128), opp.input as i128).unwrap_or(Ppm::ZERO);
            self.metric(MetricId::Spread, spread.bps_f64());
        }

        // On-chain profit floor for real candidates.
        if opp.status == OppStatus::Quoted && self.thresholds().protect_min_out {
            self.protect(plan, opp, built).await;
        }

        // ATA existence (cached) so no-op creates are dropped and rent is costed.
        let targets: Vec<Address> =
            built.iter().flat_map(|b| b.instructions.setup.iter().filter_map(ata_create_target)).collect();
        let unchecked = self.view.unchecked_atas(&targets);
        if !unchecked.is_empty()
            && let Ok(res) = self.rpc.get_accounts_lamports(&unchecked).await
        {
            let pairs: Vec<(Address, bool)> = unchecked.iter().zip(res).map(|(a, l)| (*a, l.is_some())).collect();
            self.view.record_atas(&pairs);
        }
        let existing = self.view.existing_atas();
        let legs: Vec<LegInstructions> = built.iter().map(|b| b.instructions.clone()).collect();
        let leg_refs: Vec<&LegInstructions> = legs.iter().collect();
        let tip_account = self.view.pick_tip_account();
        let cu_price = opp.costs.compute_unit_price_micro;
        let blockhash = legs.iter().max_by_key(|l| l.last_valid_block_height).map(|l| l.blockhash).unwrap_or([0; 32]);
        let dont_front = self.cfg.dont_front.then(|| DONT_FRONT.parse().ok()).flatten();
        let params = AssemblyParams {
            payer: taker,
            cu_limit: SIM_CU_LIMIT,
            cu_price_micro: cu_price,
            tip: tip_account.map(|a| (a, opp.costs.jito_tip)),
            dont_front,
            existing_atas: &existing,
            blockhash,
        };
        let assembled = if self.cfg.prefer_single_tx {
            match compose_single(&leg_refs, &params) {
                Ok(a) => Ok((PlanKind::SingleTx, vec![a])),
                Err(e) if e.is_size_limit() => compose_bundle(&leg_refs, &params, None).map(|v| (PlanKind::Bundle, v)),
                Err(e) => Err(e),
            }
        } else {
            compose_bundle(&leg_refs, &params, None).map(|v| (PlanKind::Bundle, v))
        };
        let (plan_kind, txs) = match assembled {
            Ok(x) => x,
            Err(e) => {
                if opp.status == OppStatus::Quoted {
                    opp.status = OppStatus::Skipped(if e.is_size_limit() {
                        SkipReason::TxTooLarge
                    } else {
                        SkipReason::BuildFailed
                    });
                    self.stage(
                        id,
                        Stage::Skip,
                        false,
                        opp.status.skip().map(|s| s.code()).unwrap_or(""),
                        "",
                        e.to_string(),
                    );
                }
                self.emit_opp(opp);
                return None;
            }
        };
        let atas: u8 = txs.iter().map(|t| t.creates_atas.len() as u8).sum();
        if plan_kind == PlanKind::Bundle || atas > 0 {
            // re-price with the real shape (base fee per tx) and ATA rent
            let shape = if plan_kind == PlanKind::Bundle { TxShape::bundle(txs.len()) } else { TxShape::SINGLE };
            let status_before = opp.status.clone();
            *opp = self.price_legs(id, plan, opp.route.legs.clone(), shape, atas);
            if status_before != OppStatus::Quoted && opp.status == OppStatus::Quoted {
                opp.status = status_before;
            }
        }
        if let Some(r) = opp.status.skip() {
            self.stage(
                id,
                Stage::Skip,
                false,
                r.code(),
                opp.eval.net_edge.to_string(),
                format!("{} · net {} lamports", plan.label, opp.eval.expected_net),
            );
        }
        self.emit_opp(opp);
        Some(SimJob {
            sim_tip: tip_account.map(|_| opp.costs.jito_tip).unwrap_or(0),
            opp: opp.clone(),
            legs,
            plan: plan_kind,
            txs,
            sim_cu_limit: SIM_CU_LIMIT,
            sim_cu_price: cu_price,
            tip_account,
            atas_to_create: atas,
            taker,
        })
    }

    fn price_legs(
        &self,
        id: OpportunityId,
        plan: &CandidatePlan,
        legs: Vec<Leg>,
        shape: TxShape,
        atas: u8,
    ) -> Opportunity {
        let tip = |p: i64| self.tip_for(p);
        let th = self.thresholds();
        let env = Self::pricing_env(&th, &tip);
        let now = Ts::now();
        price(
            PricingInput {
                id,
                plan,
                legs,
                now,
                slot: self.chain.slot(),
                sol_price: self.view.sol_price(now, PRICE_MAX_AGE_MS),
                base_decimals: self.tokens.sol().decimals,
                shape,
                atas_to_create: atas,
            },
            &env,
        )
    }

    async fn protect(&self, plan: &CandidatePlan, opp: &mut Opportunity, built: &mut [BuiltLeg]) {
        let id = opp.id;
        let Some(last) = built.last() else { return };
        let required = required_final_out(opp, &self.thresholds().guards);
        let Some(bps) = protective_slippage_bps(last.leg.out_amount, required) else {
            opp.status = OppStatus::Skipped(SkipReason::Slippage);
            self.stage(
                id,
                Stage::Protect,
                false,
                "cannot protect",
                "",
                format!("required {required} > quoted {}", last.leg.out_amount),
            );
            return;
        };
        if bps >= last.leg.slippage_bps {
            self.stage(
                id,
                Stage::Protect,
                true,
                format!("min-out ok ({}bp)", last.leg.slippage_bps),
                "",
                format!("min_out {} ≥ required {required}", last.leg.min_out),
            );
            return;
        }
        let spec = &plan.legs[plan.legs.len() - 1];
        let req = self.build_request(
            spec,
            last.leg.in_amount,
            self.cfg.taker.unwrap_or(Address([1; 32])),
            SlippageSpec::Fixed(bps),
        );
        match self.jupiter.build(&req, (built.len() - 1) as u8).await {
            Ok(b) if b.leg.min_out >= required => {
                let n = built.len();
                built[n - 1] = b;
                let legs = built.iter().map(|b| b.leg.clone()).collect();
                *opp = self.price_legs(id, plan, legs, TxShape::SINGLE, 0);
                self.stage(
                    id,
                    Stage::Protect,
                    true,
                    format!("slippage {bps}bp"),
                    "",
                    format!("min_out ≥ {required} (input + costs + min profit)"),
                );
            }
            Ok(b) => {
                opp.status = OppStatus::Skipped(SkipReason::Slippage);
                self.stage(
                    id,
                    Stage::Protect,
                    false,
                    "moved",
                    "",
                    format!("re-quote min_out {} < required {required}", b.leg.min_out),
                );
            }
            Err(e) => {
                opp.status = OppStatus::Skipped(if e.is_rate_limited() {
                    SkipReason::RateLimited
                } else {
                    SkipReason::BuildFailed
                });
                self.stage(id, Stage::Protect, false, "re-quote failed", "", e.to_string());
            }
        }
    }

    pub fn on_build_error(&self, id: OpportunityId, plan: &CandidatePlan, built: Vec<BuiltLeg>, e: JupiterError) {
        let reason = match &e {
            JupiterError::RateLimited(d) => {
                self.emit(Event::RateLimited {
                    ts: Ts::now(),
                    service: ServiceId::Jupiter,
                    backoff_ms: d.as_millis() as u64,
                    attempt: 0,
                });
                SkipReason::RateLimited
            }
            JupiterError::NoRoute(_) => SkipReason::NoRoute,
            _ => {
                self.emit(Event::Error { ts: Ts::now(), service: "jupiter".into(), message: e.to_string() });
                SkipReason::BuildFailed
            }
        };
        self.stage(id, Stage::Skip, false, reason.code(), "", format!("{} · leg {} · {e}", plan.label, built.len()));
        if reason != SkipReason::RateLimited {
            // Market fact (no route / failed build): record it as an opportunity row.
            let legs: Vec<Leg> = built.into_iter().map(|b| b.leg).collect();
            let mut opp = self.price_legs(id, plan, legs, TxShape::SINGLE, 0);
            // An incomplete cycle has no final output: never report an edge for it.
            opp.gross_output = 0;
            opp.eval = searcher_core::profit::ProfitEval::default();
            opp.status = OppStatus::Skipped(reason);
            self.emit_opp(&opp);
        }
    }

    fn emit_opp(&self, o: &Opportunity) {
        self.emit(Event::Opportunity(Box::new(o.clone())));
    }

    // ──────────────────────────── simulator ────────────────────────────

    pub async fn run_simulator(self: Arc<Self>, mut rx: mpsc::Receiver<SimJob>, mut shutdown: watch::Receiver<bool>) {
        loop {
            let job = tokio::select! {
                _ = shutdown.changed() => return,
                j = rx.recv() => match j { Some(j) => j, None => return },
            };
            self.simulate_job(job).await;
        }
    }

    pub async fn simulate_job(&self, mut job: SimJob) {
        let now = Ts::now();
        let id = job.opp.id;
        let candidate = job.opp.status == OppStatus::Quoted;
        let age = job.opp.quote_age_ms(now);
        if age > self.cfg.max_quote_age_ms {
            if candidate {
                job.opp.status = OppStatus::Skipped(SkipReason::StaleQuote);
                job.opp.updated_at = now;
                self.stage(
                    id,
                    Stage::Skip,
                    false,
                    "STALE_QUOTE",
                    format!("{age}ms"),
                    "quote aged out before simulation",
                );
                self.emit_opp(&job.opp);
            }
            return;
        }
        if self.cfg.taker.is_none() {
            if candidate {
                job.opp.status = OppStatus::Skipped(SkipReason::SimUnavailable);
                self.stage(
                    id,
                    Stage::Skip,
                    false,
                    "SIM_UNAVAILABLE",
                    "",
                    "no wallet.pubkey / paper.simulation_taker configured",
                );
                self.emit_opp(&job.opp);
            }
            return;
        }

        let (sim, balances) = self.simulate_txs(&job).await;
        self.metric(MetricId::SimLatency, sim.latency_ms as f64);
        if sim.ok {
            self.metric(MetricId::ComputeUnits, sim.units_consumed() as f64);
        }
        let verdict = if sim.ok {
            "pass".to_string()
        } else {
            format!("fail · {}", sim.failure.as_ref().map(|f| f.class.label()).unwrap_or("?"))
        };
        self.stage(
            id,
            Stage::Simulation,
            sim.ok,
            verdict,
            format!("{}ms", sim.latency_ms),
            sim.failure
                .as_ref()
                .map(|f| f.message.clone())
                .unwrap_or_else(|| format!("{} CU · {:?} · {:?}", sim.units_consumed(), sim.plan, sim.fidelity)),
        );

        // Re-price with actual CU (and the SOL-equivalent balance delta).
        if sim.ok {
            let cu_used: Vec<u32> = sim.txs.iter().map(|t| t.units_consumed).collect();
            let shape = if job.plan == PlanKind::Bundle { TxShape::bundle(job.txs.len()) } else { TxShape::SINGLE };
            let tip = |p: i64| self.tip_for(p);
            let th = self.thresholds();
            let env = Self::pricing_env(&th, &tip);
            let status_before = job.opp.status.clone();
            let base_dec = self.tokens.sol().decimals;
            reprice_after_simulation(
                &mut job.opp,
                shape,
                &cu_used,
                None,
                None,
                job.atas_to_create,
                &env,
                base_dec,
                now,
            );
            let exact = (sim.fidelity == SimFidelity::Exact && sim.txs.len() == 1).then(|| &sim.txs[0]);
            // rent of accounts left created: capital, reported and capped, not a trade cost
            let deposits = exact.map(|t| t.created.iter().map(|(_, l)| *l).sum::<u64>());
            // what intermediate legs left in / took from the inventory, valued in base units
            let drift = exact.and_then(|t| intermediate_drift_value(&job.opp.route.legs, &t.leg_outputs));
            if exact.is_some() && drift.is_none() {
                self.stage(id, Stage::Simulation, true, "drift unknown", "", "no executed output per leg in the logs");
            }
            let delta = Self::sol_equivalent(&job, &sim, balances.as_ref()).and_then(|d| {
                // Correct the simulated fee/tip to the final CU limit and tip.
                let sim_prio = priority_fee_lamports(job.sim_cu_limit, job.sim_cu_price) as i64;
                Some(
                    d + drift? + deposits.unwrap_or(0) as i64 + sim_prio - job.opp.costs.priority_fee as i64
                        + job.sim_tip as i64
                        - job.opp.costs.jito_tip as i64,
                )
            });
            reprice_after_simulation(
                &mut job.opp,
                shape,
                &cu_used,
                delta,
                deposits,
                job.atas_to_create,
                &env,
                base_dec,
                now,
            );
            if !candidate {
                // unprofitable sample: keep the original (first) reason
                job.opp.status = status_before;
            }
        } else if candidate {
            let short =
                inventory_short(&job.opp.route.legs, sim.txs.first().map(|t| &t.leg_outputs[..]).unwrap_or(&[]));
            job.opp.status = OppStatus::Skipped(match sim.failure.as_ref().map(|f| f.class) {
                Some(SimFailureClass::RpcError) => SkipReason::SimUnavailable,
                Some(SimFailureClass::SlippageExceeded) => SkipReason::Slippage,
                Some(SimFailureClass::InsufficientFunds) if short.is_some() => SkipReason::Inventory,
                _ => SkipReason::SimFailed,
            });
            if let (Some(SkipReason::Inventory), Some((leg, need, got))) = (job.opp.status.skip(), short) {
                self.stage(
                    id,
                    Stage::Skip,
                    false,
                    "INVENTORY_LOW",
                    format!("{}", need - got),
                    format!(
                        "leg {} needs {need}, leg {leg} delivered {got}: keep at least {} of that token in the wallet",
                        leg + 1,
                        need - got
                    ),
                );
            }
        }
        job.opp.simulation = Some(sim.clone());
        job.opp.updated_at = Ts::now();
        self.emit(Event::Simulation(Box::new(sim)));

        if job.opp.status == OppStatus::Quoted {
            self.decide(job).await;
        } else {
            if candidate && let Some(r) = job.opp.status.skip() {
                self.stage(id, Stage::Skip, false, r.code(), job.opp.eval.net_edge.to_string(), "after simulation");
            }
            self.emit_opp(&job.opp);
        }
    }

    /// SOL-equivalent taker delta from an exact (single-tx) simulation.
    fn sol_equivalent(job: &SimJob, sim: &SimulationResult, balances: Option<&(Vec<u64>, Vec<u64>)>) -> Option<i64> {
        if sim.fidelity != SimFidelity::Exact || job.txs.len() != 1 {
            return None;
        }
        let (pre, post) = balances?;
        let legs: Vec<&LegInstructions> = job.legs.iter().collect();
        let keys = message_account_keys(&job.txs[0].tx, &legs)?;
        sol_equivalent_delta(&keys, &job.taker, &wsol_accounts(&legs), pre, post)
    }

    async fn simulate_txs(&self, job: &SimJob) -> (SimulationResult, Option<(Vec<u64>, Vec<u64>)>) {
        let started = std::time::Instant::now();
        let margin = self.thresholds().cost_params.cu_margin;
        let mut txs = Vec::with_capacity(job.txs.len());
        let mut failure = None;
        let mut context_slot = None;
        let mut balances = None;
        for (i, a) in job.txs.iter().enumerate() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&a.wire);
            match self.rpc.simulate(&b64, &[]).await {
                Ok(out) => {
                    context_slot = context_slot.or(out.context_slot);
                    if job.txs.len() == 1
                        && let (Some(pre), Some(post)) = (&out.pre_balances, &out.post_balances)
                    {
                        balances = Some((pre.clone(), post.clone()));
                    }
                    if failure.is_none() {
                        failure = failure_from(i as u8, &out).map(|mut f| {
                            // Independent simulation of a dependent bundle tx.
                            if job.plan == PlanKind::Bundle
                                && i > 0
                                && matches!(f.class, SimFailureClass::ProgramError | SimFailureClass::InsufficientFunds)
                            {
                                f.class = SimFailureClass::DependsOnPriorTx;
                            }
                            f
                        });
                    }
                    // account keys (with lookup tables) name the accounts the tx creates
                    let keys = (job.txs.len() == 1)
                        .then(|| {
                            let legs: Vec<&LegInstructions> = job.legs.iter().collect();
                            message_account_keys(&a.tx, &legs)
                        })
                        .flatten();
                    txs.push(tx_sim(i as u8, a, &out, margin, keys.as_deref()));
                }
                Err(e) => {
                    failure = Some(SimFailure {
                        class: SimFailureClass::RpcError,
                        tx_index: i as u8,
                        message: e.to_string(),
                    });
                    break;
                }
            }
        }
        let r = SimulationResult {
            opportunity: job.opp.id,
            plan: job.plan,
            fidelity: if job.plan == PlanKind::SingleTx { SimFidelity::Exact } else { SimFidelity::PerTx },
            ok: failure.is_none() && txs.len() == job.txs.len() && txs.iter().all(|t| t.ok),
            failure,
            txs,
            latency_ms: started.elapsed().as_millis() as u32,
            simulated_at: Ts::now(),
            context_slot,
        };
        (r, balances)
    }

    // ───────────────────────────── risk + exec ─────────────────────────

    async fn decide(&self, mut job: SimJob) {
        let id = job.opp.id;
        let now = Ts::now();
        let ctx = RiskContext {
            now,
            mode: self.cfg.mode,
            equity_lamports: self.equity_lamports(),
            wallet_lamports: match self.cfg.mode {
                Mode::Paper => None,
                _ => self.view.wallet_lamports(now, 60_000),
            },
            sol_price: self.view.sol_price(now, PRICE_MAX_AGE_MS),
            latest_slot: self.chain.slot(),
        };
        let d = self.risk.evaluate(&job.opp, &ctx);
        let verdict = if d.approved {
            "pass".to_string()
        } else {
            d.violations.iter().map(|v| v.code()).collect::<Vec<_>>().join(", ")
        };
        self.stage(id, Stage::Risk, d.approved, verdict, "", format!("{:?}", d.violations));
        self.emit(Event::Risk(d.clone()));
        job.opp.risk = Some(d.clone());
        if !d.approved {
            job.opp.status = OppStatus::Skipped(d.primary_skip().unwrap_or(SkipReason::RiskLimit));
            self.stage(
                id,
                Stage::Skip,
                false,
                job.opp.status.skip().map(|r| r.code()).unwrap_or(""),
                job.opp.eval.net_edge.to_string(),
                "risk",
            );
            self.emit_opp(&job.opp);
            return;
        }
        job.opp.status = OppStatus::Executable;
        self.emit_opp(&job.opp);
        match self.cfg.mode {
            Mode::Paper => self.paper_fill(job),
            Mode::Confirm => self.confirm_then_send(job).await,
            Mode::Live => self.send_live(job).await,
        }
    }

    fn net_of(o: &Opportunity) -> i64 {
        o.eval.simulated_net.map_or(o.eval.expected_net, |s| s.min(o.eval.expected_net))
    }

    fn paper_fill(&self, mut job: SimJob) {
        let now = Ts::now();
        let o = &mut job.opp;
        self.risk.open_execution();
        let net = Self::net_of(o);
        let price = self.view.sol_price(now, PRICE_MAX_AGE_MS);
        let net_usd = price.and_then(|p| p.value(net as i128, 9));
        self.risk.close_execution(now, true, net_usd.unwrap_or_default());
        let total = self.view.add_paper_net(net);
        o.status = OppStatus::PaperFilled;
        o.updated_at = now;
        self.emit(Event::Execution(ExecutionAttempt {
            opportunity: o.id,
            mode: Mode::Paper,
            plan: job.plan,
            state: ExecState::PaperFilled,
            bundle_id: None,
            signatures: vec![],
            tip_lamports: o.costs.jito_tip,
            created_at: now,
            updated_at: now,
            latency_ms: None,
        }));
        self.emit(Event::Trade(TradeResult {
            opportunity: o.id,
            strategy: o.strategy,
            label: o.label.clone(),
            mode: Mode::Paper,
            paper: true,
            entry_ts: o.detected_at,
            exit_ts: now,
            input: o.input,
            output: o.gross_output,
            fees_lamports: o.costs.base_fee + o.costs.priority_fee,
            tip_lamports: o.costs.jito_tip,
            expected_net: o.eval.expected_net,
            net,
            net_usd,
        }));
        let usd = net_usd.map(|u| u.to_string()).unwrap_or_else(|| format!("{net} lamports"));
        self.stage(
            o.id,
            Stage::Paper,
            net > 0,
            "simulated fill",
            format!("{}{usd}", if net > 0 { "+" } else { "" }),
            "PAPER: nothing signed or sent",
        );
        if let Some(p) = price {
            self.metric(MetricId::Pnl, p.value(total as i128, 9).map(|u| u.f64()).unwrap_or(0.0));
        }
        self.emit_opp(o);
    }

    async fn confirm_then_send(&self, mut job: SimJob) {
        let id = job.opp.id;
        let rx = self.confirms.register(id);
        job.opp.status = OppStatus::AwaitingConfirm;
        self.emit_opp(&job.opp);
        self.stage(
            id,
            Stage::Confirm,
            true,
            "awaiting operator",
            format!("{}s", self.cfg.confirm_timeout.as_secs()),
            "press y to send, n to decline",
        );
        match tokio::time::timeout(self.cfg.confirm_timeout, rx).await {
            Ok(Ok(true)) => {
                // quote may have aged while waiting
                if job.opp.quote_age_ms(Ts::now()) > self.cfg.max_quote_age_ms {
                    job.opp.status = OppStatus::Skipped(SkipReason::StaleQuote);
                    self.stage(id, Stage::Skip, false, "STALE_QUOTE", "", "approved too late");
                    self.emit_opp(&job.opp);
                    return;
                }
                self.send_live(job).await
            }
            Ok(Ok(false)) => {
                job.opp.status = OppStatus::Skipped(SkipReason::Declined);
                self.stage(id, Stage::Skip, false, "DECLINED", "", "operator declined");
                self.emit_opp(&job.opp);
            }
            _ => {
                self.confirms.resolve(id, false);
                job.opp.status = OppStatus::Skipped(SkipReason::Expired);
                self.stage(id, Stage::Skip, false, "EXPIRED", "", "no confirmation in time");
                self.emit_opp(&job.opp);
            }
        }
    }

    async fn send_live(&self, mut job: SimJob) {
        let id = job.opp.id;
        let Some((permit, wallet, backend)) = self.live.as_ref() else {
            job.opp.status = OppStatus::Skipped(SkipReason::RiskLimit);
            self.stage(
                id,
                Stage::Skip,
                false,
                "LIVE_GATE",
                "",
                "live execution not enabled (execution.live_enabled / wallet)",
            );
            self.emit_opp(&job.opp);
            return;
        };
        // Final transaction: actual CU × margin, final tip, same instructions.
        let legs: Vec<&LegInstructions> = job.legs.iter().collect();
        let existing = self.view.existing_atas();
        let sim = job.opp.simulation.clone();
        let limits: Vec<u32> = sim.as_ref().map(|s| s.txs.iter().map(|t| t.cu_limit).collect()).unwrap_or_default();
        let params = AssemblyParams {
            payer: wallet.pubkey(),
            cu_limit: limits.first().copied().unwrap_or(SIM_CU_LIMIT),
            cu_price_micro: job.opp.costs.compute_unit_price_micro,
            tip: job.tip_account.map(|a| (a, job.opp.costs.jito_tip)),
            dont_front: self.cfg.dont_front.then(|| DONT_FRONT.parse().ok()).flatten(),
            existing_atas: &existing,
            blockhash: job
                .legs
                .iter()
                .max_by_key(|l| l.last_valid_block_height)
                .map(|l| l.blockhash)
                .unwrap_or([0; 32]),
        };
        let txs = match job.plan {
            PlanKind::SingleTx => compose_single(&legs, &params).map(|a| vec![a]),
            PlanKind::Bundle => compose_bundle(&legs, &params, Some(&limits)),
        };
        let txs = match txs {
            Ok(t) => t,
            Err(e) => {
                job.opp.status = OppStatus::Skipped(SkipReason::TxTooLarge);
                self.stage(id, Stage::Skip, false, "TX_TOO_LARGE", "", e.to_string());
                self.emit_opp(&job.opp);
                return;
            }
        };
        let lvbh = job.legs.iter().map(|l| l.last_valid_block_height).min().unwrap_or(0);
        let now = Ts::now();
        self.risk.open_execution();
        job.opp.status = OppStatus::Submitted;
        self.emit_opp(&job.opp);
        self.stage(
            id,
            Stage::Submit,
            true,
            "Jito sendBundle",
            format!("tip {}", job.opp.costs.jito_tip),
            format!("{:?} · {} tx", job.plan, txs.len()),
        );
        let out = execute_live(backend, permit, wallet, txs, lvbh, &self.cfg.live).await;
        let price = self.view.sol_price(Ts::now(), PRICE_MAX_AGE_MS);
        let (state, success, net, bundle_id, sigs, latency) = match &out {
            LiveOutcome::Landed { bundle_id, slot, signatures, realized_lamports, latency_ms } => (
                ExecState::Landed { slot: *slot },
                true,
                realized_lamports.unwrap_or(0),
                Some(bundle_id.clone()),
                signatures.clone(),
                Some(*latency_ms),
            ),
            LiveOutcome::Failed { bundle_id, reason, latency_ms } => (
                ExecState::Failed { reason: reason.clone() },
                false,
                0,
                Some(bundle_id.clone()),
                vec![],
                Some(*latency_ms),
            ),
            LiveOutcome::TimedOut { bundle_id } => {
                (ExecState::Expired, false, 0, Some(bundle_id.clone()), vec![], None)
            }
            LiveOutcome::NotSent(r) => {
                (ExecState::Failed { reason: format!("not sent: {r}") }, false, 0, None, vec![], None)
            }
        };
        let net_usd = price.and_then(|p| p.value(net as i128, 9));
        self.risk.close_execution(Ts::now(), success, net_usd.unwrap_or_default());
        if let Some(l) = latency {
            self.metric(MetricId::BundleLatency, l as f64);
        }
        self.emit(Event::Execution(ExecutionAttempt {
            opportunity: id,
            mode: self.cfg.mode,
            plan: job.plan,
            state: state.clone(),
            bundle_id,
            signatures: sigs,
            tip_lamports: job.opp.costs.jito_tip,
            created_at: now,
            updated_at: Ts::now(),
            latency_ms: latency,
        }));
        job.opp.status = if success { OppStatus::Landed } else { OppStatus::Failed };
        if success {
            self.emit(Event::Trade(TradeResult {
                opportunity: id,
                strategy: job.opp.strategy,
                label: job.opp.label.clone(),
                mode: self.cfg.mode,
                paper: false,
                entry_ts: job.opp.detected_at,
                exit_ts: Ts::now(),
                input: job.opp.input,
                output: job.opp.gross_output,
                fees_lamports: job.opp.costs.base_fee + job.opp.costs.priority_fee,
                tip_lamports: job.opp.costs.jito_tip,
                expected_net: job.opp.eval.expected_net,
                net,
                net_usd,
            }));
        }
        self.stage(
            id,
            Stage::Landed,
            success,
            format!("{state:?}"),
            net_usd.map(|u| u.to_string()).unwrap_or_default(),
            format!("{out:?}"),
        );
        self.emit_opp(&job.opp);
    }
}

/// Provider body with lookup-table address arrays replaced by their lengths
/// (they are 70–80% of the bytes and fully described by the table keys).
pub fn compact_raw(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut v) => {
            if let Some(m) = v.get_mut("addressesByLookupTableAddress").and_then(|m| m.as_object_mut()) {
                for (_, arr) in m.iter_mut() {
                    let n = arr.as_array().map(|a| a.len()).unwrap_or(0);
                    *arr = serde_json::json!({ "len": n });
                }
            }
            v.to_string()
        }
        Err(_) => body.chars().take(4096).collect(),
    }
}
