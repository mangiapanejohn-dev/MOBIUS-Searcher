//! Event-driven quote scheduler.
//!
//! ```text
//! pool / oracle ticks ─► detector (dependency graph: only routes that read
//!                        the changed input are touched)
//!                     ─► priority (continuations › triggered routes by score
//!                        › background calibration)
//!                     ─► global budget (Jupiter's sliding window, learned from
//!                        the gateway; a reserve only urgent work may use)
//!                     ─► /build (identical requests collapsed)
//!                     ─► shared leg cache (event-invalidated, carries timing)
//!                     ─► route jobs ─► pricing / decision (Pipeline::finish)
//! ```
//!
//! A Jupiter request is the scarce resource (Free plan ≈ 1 per second). A
//! route is only quoted when an input it depends on changed, or when it is due
//! for calibration; among changed routes the one most likely to clear its cost
//! per request spent goes first. Nothing here invents prices: pool mids only
//! decide *what to ask*, every decision is priced from Jupiter quotes whose
//! send/receive times travel with them.

use crate::pipeline::Pipeline;
use crate::probe::Observes;
use searcher_core::model::OpportunityId;
use searcher_jupiter::{BuildRequest, BuiltLeg, JupiterClient, JupiterError};
use searcher_market::hot::HotTick;
use searcher_strategy::{CandidatePlan, LegSpec};
use searcher_telemetry::{LatencyBook, WindowLimiter};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Debug)]
pub struct RouterConfig {
    /// Jupiter requests in flight at once.
    pub max_in_flight: usize,
    /// Budget slots background calibration may not use (kept for bursts).
    pub reserve: u32,
    /// A move of a route's input smaller than this is not news (bp).
    pub change_bp: f64,
    /// Longest reuse of a cached leg whose pools are watched (any move ≥
    /// `change_bp` of those pools invalidates it earlier).
    pub leg_ttl: Duration,
    /// Longest reuse of a leg nothing on-chain observes; shortened by volatility.
    pub unobservable_ttl: Duration,
    /// Every route is re-quoted at least this often (model calibration).
    pub floor: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 3,
            reserve: 3,
            change_bp: 0.5,
            leg_ttl: Duration::from_millis(2_500),
            unobservable_ttl: Duration::from_millis(1_500),
            floor: Duration::from_secs(90),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Next leg of a route whose first quote is already aging.
    Continuation,
    /// A route whose input changed.
    Urgent,
    /// Calibration of a route that has not been quoted for `floor`.
    Background,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Continuation => "continuation",
            Class::Urgent => "urgent",
            Class::Background => "background",
        }
    }
}

// Priors until a route has been quoted (bp). Deliberately pessimistic: the
// last observed session had median net edge ≈ −9 bp.
const PRIOR_OFFSET_BP: f64 = -8.0;
const PRIOR_VAR_BP2: f64 = 16.0;
const PRIOR_GROSS_BP: f64 = -5.0;
const PRIOR_COST_BP: f64 = 6.0;
const EWMA: f64 = 0.2;
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
/// Pool mids older than this are not used as signal.
const MID_FRESH: Duration = Duration::from_secs(20);

/// Market inputs a route's prediction was made from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inputs {
    /// Cross-dex routes with both pools watched: (mid sell dex − mid buy dex) / mid, bp.
    pub gap_bp: Option<f64>,
    /// Mean of the fresh watched pool mids.
    pub mean: Option<f64>,
    /// Watched pools the route reads, in `RouteDef::pools` order.
    pub pools: Vec<Option<f64>>,
    /// Oracle prices the route reads, in `RouteDef::oracles` order.
    pub oracles: Vec<Option<f64>>,
}

fn moved_bp(now: Option<f64>, then: Option<f64>) -> f64 {
    match (now, then) {
        (Some(a), Some(b)) if b != 0.0 => ((a / b) - 1.0).abs() * 10_000.0,
        _ => 0.0,
    }
}

struct RouteDef {
    plan: CandidatePlan,
    /// Request key of the first leg (its amount is fixed).
    leg0_key: String,
    obs: Vec<Observes>,
    /// (dex we sell the base on = leg 0, dex we buy it back on = last leg)
    signal: Option<(Arc<str>, Arc<str>)>,
    /// Watched pools any leg reads.
    pools: Vec<Arc<str>>,
    /// Reads the whole watched market (best-route legs, unobservable venues).
    any_pool: bool,
    oracles: Vec<Arc<str>>,
}

#[derive(Default)]
struct RouteState {
    /// (decided at, gross bp, net bp)
    last: Option<(Instant, f64, f64)>,
    /// Inputs at the last decision's dispatch (what "changed" is measured from).
    reference: Option<Inputs>,
    /// EWMA of (gross − gap) and its variance: how Jupiter's executable edge
    /// relates to the on-chain gap (fees, impact) for this route.
    offset: Option<(f64, f64)>,
    samples: u32,
    /// First relevant change since the last decision (receipt time).
    trigger: Option<Instant>,
    job: Option<u64>,
    last_failure: Option<Instant>,
    /// Send times of the quotes the last decision was priced from.
    last_legs: Vec<Instant>,
}

struct Job {
    id: OpportunityId,
    route: usize,
    legs: Vec<BuiltLeg>,
    obs: Vec<Observes>,
    picked: Instant,
    trigger: Option<Instant>,
    class: Class,
    inputs: Inputs,
    /// First leg sent (or reused) at — decisions reference inputs from here.
    dispatched: Instant,
    /// Previous leg received (continuation wait starts here).
    prev_received: Option<Instant>,
}

struct Cached {
    leg: BuiltLeg,
    obs: Observes,
    /// Watched pool mids when it was sent (invalidation reference).
    mids: Vec<(Arc<str>, f64)>,
}

struct InFlight {
    jobs: Vec<u64>,
    obs: Observes,
    mids: Vec<(Arc<str>, f64)>,
    index: u8,
    sent: Instant,
}

/// A watched pool that moved less than this since a quote was sent counts as
/// unchanged for that quote (bp).
const UNCHANGED_BP: f64 = 0.1;

/// What the async shell should do next.
#[derive(Debug)]
pub enum Step {
    /// Send this request (a budget slot is already taken).
    Dispatch { key: String, req: BuildRequest, index: u8, job: OpportunityId, class: Class },
    /// This job has all its legs: price it.
    Finish(u64),
    /// Nothing to send before this instant (budget) — or `None`: wait for events.
    Wait(Option<Instant>),
}

/// A finished job handed to pricing.
pub struct ReadyJob {
    pub id: OpportunityId,
    pub route: usize,
    pub plan: CandidatePlan,
    pub legs: Vec<BuiltLeg>,
    pub obs: Vec<Observes>,
    pub picked: Instant,
    pub trigger: Option<Instant>,
    pub inputs: Inputs,
    pub dispatched: Instant,
}

/// Allocates opportunity ids.
pub type NewId = Arc<dyn Fn() -> OpportunityId + Send + Sync>;
/// Builds the `/build` request for a leg at an amount (pipeline settings).
pub type MakeReq = Arc<dyn Fn(&LegSpec, u64) -> BuildRequest + Send + Sync>;

/// Scheduling state machine: synchronous, no I/O (unit-testable).
pub struct Core {
    cfg: RouterConfig,
    defs: Vec<RouteDef>,
    state: Vec<RouteState>,
    by_pool: HashMap<Arc<str>, Vec<usize>>,
    any_pool: Vec<usize>,
    by_oracle: HashMap<Arc<str>, Vec<usize>>,
    mids: HashMap<Arc<str>, (f64, Instant)>,
    oracles: HashMap<Arc<str>, f64>,
    last_mean: Option<(f64, Instant)>,
    /// EWMA of squared mean-mid changes per second (bp²/s).
    vol_rate: f64,
    triggered: HashSet<usize>,
    jobs: HashMap<u64, Job>,
    next_job: u64,
    continuations: VecDeque<u64>,
    ready: VecDeque<u64>,
    inflight: HashMap<String, InFlight>,
    /// Jobs that need a key currently in flight from *before* their trigger:
    /// they retry (with a fresh request) once it completes.
    blocked_on: HashMap<String, Vec<u64>>,
    cache: HashMap<String, Cached>,
    new_id: NewId,
    make_req: MakeReq,
    book: Arc<LatencyBook>,
}

/// ln Φ(z) for the standard normal CDF, stable far into the left tail.
pub fn ln_phi(z: f64) -> f64 {
    if z < -5.0 {
        let z2 = z * z;
        -z2 / 2.0 - (-z).ln() - 0.5 * (2.0 * std::f64::consts::PI).ln() + (1.0 - 1.0 / z2 + 3.0 / (z2 * z2)).ln()
    } else {
        (0.5 * erfc(-z / std::f64::consts::SQRT_2)).max(1e-300).ln()
    }
}

/// Complementary error function (Numerical Recipes erfcc, |ε| < 1.2e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

impl Core {
    /// `observes(leg)` classifies legs; `oracle_of(mint)` names the oracle for
    /// a non-watched token (e.g. JUP → "JUP/USD").
    pub fn new(
        cfg: RouterConfig,
        plans: Vec<CandidatePlan>,
        observes: impl Fn(&LegSpec) -> Observes,
        oracle_of: impl Fn(&searcher_core::Address) -> Option<Arc<str>>,
        new_id: NewId,
        make_req: MakeReq,
        book: Arc<LatencyBook>,
    ) -> Self {
        let mut defs = Vec::new();
        for plan in plans {
            let obs: Vec<Observes> = plan.legs.iter().map(&observes).collect();
            let dex_of = |o: &Observes| match o {
                Observes::Dexes(d) if d.len() == 1 => Some(d[0].clone()),
                _ => None,
            };
            let signal = match (obs.first().and_then(dex_of), obs.last().and_then(dex_of)) {
                (Some(a), Some(b)) if obs.len() == 2 && a != b => Some((a, b)),
                _ => None,
            };
            let mut pools: Vec<Arc<str>> = Vec::new();
            for o in &obs {
                if let Observes::Dexes(d) = o {
                    for x in d {
                        if !pools.contains(x) {
                            pools.push(x.clone());
                        }
                    }
                }
            }
            // A round trip over the best route both ways has an edge that does
            // not depend on the price level: price moves are no news for it.
            let level_invariant = obs.iter().all(|o| *o == Observes::AllPools);
            let any_pool = !level_invariant && obs.iter().any(|o| !matches!(o, Observes::Dexes(_)));
            let mut oracles: Vec<Arc<str>> = Vec::new();
            for l in &plan.legs {
                for m in [l.input, l.output] {
                    if let Some(o) = oracle_of(&m)
                        && !oracles.contains(&o)
                    {
                        oracles.push(o);
                    }
                }
            }
            let leg0_key = make_req(&plan.legs[0], plan.amount).key();
            defs.push(RouteDef { plan, leg0_key, obs, signal, pools, any_pool, oracles });
        }
        let mut by_pool: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        let mut by_oracle: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        let mut any_pool = Vec::new();
        for (i, d) in defs.iter().enumerate() {
            for p in &d.pools {
                by_pool.entry(p.clone()).or_default().push(i);
            }
            for o in &d.oracles {
                by_oracle.entry(o.clone()).or_default().push(i);
            }
            if d.any_pool {
                any_pool.push(i);
            }
        }
        let state = defs.iter().map(|_| RouteState::default()).collect();
        Self {
            cfg,
            defs,
            state,
            by_pool,
            any_pool,
            by_oracle,
            mids: HashMap::new(),
            oracles: HashMap::new(),
            last_mean: None,
            vol_rate: 0.0,
            triggered: HashSet::new(),
            jobs: HashMap::new(),
            next_job: 1,
            continuations: VecDeque::new(),
            ready: VecDeque::new(),
            inflight: HashMap::new(),
            blocked_on: HashMap::new(),
            cache: HashMap::new(),
            new_id,
            make_req,
            book,
        }
    }

    pub fn routes(&self) -> usize {
        self.defs.len()
    }

    fn fresh_mid(&self, dex: &Arc<str>, now: Instant) -> Option<f64> {
        self.mids.get(dex).filter(|(_, t)| now.saturating_duration_since(*t) < MID_FRESH).map(|(m, _)| *m)
    }

    fn mean(&self, now: Instant) -> Option<f64> {
        let v: Vec<f64> = self
            .mids
            .values()
            .filter(|(_, t)| now.saturating_duration_since(*t) < MID_FRESH)
            .map(|(m, _)| *m)
            .collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }

    fn inputs(&self, r: usize, now: Instant) -> Inputs {
        let d = &self.defs[r];
        let gap_bp = d.signal.as_ref().and_then(|(a, b)| {
            let (ma, mb) = (self.fresh_mid(a, now)?, self.fresh_mid(b, now)?);
            Some((ma - mb) / mb * 10_000.0)
        });
        Inputs {
            gap_bp,
            mean: self.mean(now),
            pools: d.pools.iter().map(|p| self.fresh_mid(p, now)).collect(),
            oracles: d.oracles.iter().map(|o| self.oracles.get(o).copied()).collect(),
        }
    }

    /// Largest relevant move since the route's reference (bp). For cross-dex
    /// routes with both pools watched only the *gap* matters: a parallel move
    /// of both pools does not change the opportunity.
    fn change_bp(&self, r: usize, now: Instant) -> f64 {
        let Some(reference) = &self.state[r].reference else { return f64::INFINITY };
        let cur = self.inputs(r, now);
        if let (Some(g), Some(g0)) = (cur.gap_bp, reference.gap_bp) {
            return (g - g0).abs();
        }
        let mut m: f64 = 0.0;
        for (a, b) in cur.pools.iter().zip(&reference.pools) {
            m = m.max(moved_bp(*a, *b));
        }
        if self.defs[r].any_pool {
            m = m.max(moved_bp(cur.mean, reference.mean));
        }
        for (a, b) in cur.oracles.iter().zip(&reference.oracles) {
            m = m.max(moved_bp(*a, *b));
        }
        m
    }

    fn touch(&mut self, routes: &[usize], at: Instant, now: Instant) {
        for &r in routes {
            if self.state[r].reference.is_none() {
                continue; // never decided: calibration handles it
            }
            let c = self.change_bp(r, now);
            let st = &mut self.state[r];
            if c >= self.cfg.change_bp {
                if st.trigger.is_none() {
                    st.trigger = Some(at);
                }
                self.triggered.insert(r);
            } else if st.trigger.is_some() && st.job.is_none() {
                // reverted before we spent anything on it
                st.trigger = None;
                self.triggered.remove(&r);
                self.book.count("router.cancelled", 1);
            }
        }
    }

    /// Apply one market tick: update state, invalidate cached legs, re-check
    /// only the routes that read this input.
    pub fn on_tick(&mut self, t: &HotTick, now: Instant) {
        let started = Instant::now();
        match t {
            HotTick::Pool { dex, mid, received, .. } => {
                self.mids.insert(dex.clone(), (*mid, *received));
                if let Some(m) = self.mean(now) {
                    if let Some((m0, t0)) = self.last_mean {
                        let dt = received.saturating_duration_since(t0).as_secs_f64().max(0.05);
                        let bp = moved_bp(Some(m), Some(m0));
                        self.vol_rate = (1.0 - EWMA) * self.vol_rate + EWMA * (bp * bp / dt);
                    }
                    self.last_mean = Some((m, *received));
                }
                let dex = dex.clone();
                let change = self.cfg.change_bp;
                let before = self.cache.len();
                self.cache
                    .retain(|_, c| c.mids.iter().all(|(d, m0)| *d != dex || moved_bp(Some(*mid), Some(*m0)) < change));
                self.book.count("router.cache_invalidated", (before - self.cache.len()) as u64);
                let mut routes = self.by_pool.get(&dex).cloned().unwrap_or_default();
                routes.extend(self.any_pool.iter().copied());
                routes.sort_unstable();
                routes.dedup();
                self.touch(&routes, *received, now);
            }
            HotTick::Oracle(o) => {
                self.oracles.insert(o.symbol.clone(), o.price);
                let routes = self.by_oracle.get(&o.symbol).cloned().unwrap_or_default();
                self.touch(&routes, o.received, now);
            }
        }
        self.book.since_us("router.detect_us", started);
    }

    fn ttl(&self, o: &Observes) -> Duration {
        match o {
            Observes::Unobservable => {
                // ~time for a 1 bp move of the watched market, bounded
                let bp_per_s = self.vol_rate.sqrt();
                let t =
                    if bp_per_s > 0.0 { Duration::from_secs_f64(1.0 / bp_per_s) } else { self.cfg.unobservable_ttl };
                t.clamp(Duration::from_millis(300), self.cfg.unobservable_ttl)
            }
            _ => self.cfg.leg_ttl,
        }
    }

    fn cached(&self, key: &str) -> Option<&BuiltLeg> {
        let c = self.cache.get(key)?;
        (c.leg.timing.sent.elapsed() < self.ttl(&c.obs)).then_some(&c.leg)
    }

    fn score(&self, r: usize, now: Instant) -> f64 {
        let st = &self.state[r];
        let target = st.last.map(|(_, g, n)| g - n).unwrap_or(PRIOR_COST_BP);
        let (pred, sigma) = match self.inputs(r, now).gap_bp {
            Some(gap) => {
                let (mean, var) = st.offset.unwrap_or((PRIOR_OFFSET_BP, PRIOR_VAR_BP2));
                let sigma = (var + PRIOR_VAR_BP2 / (1.0 + st.samples as f64)).sqrt().max(0.5);
                (gap + mean, sigma)
            }
            None => {
                let base = st.last.map(|(_, g, _)| g).unwrap_or(PRIOR_GROSS_BP);
                let dt = st.last.map(|(t, _, _)| now.saturating_duration_since(t).as_secs_f64()).unwrap_or(60.0);
                (base, (1.0 + self.vol_rate * dt).sqrt())
            }
        };
        let def = &self.defs[r];
        let reused = self.cached(&def.leg0_key).is_some() || self.inflight.contains_key(&def.leg0_key);
        let requests = (def.plan.legs.len() - usize::from(reused)).max(1) as f64;
        ln_phi((pred - target) / sigma) - requests.ln()
    }

    fn start_job(&mut self, r: usize, class: Class, now: Instant) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        let trigger = self.state[r].trigger;
        let job = Job {
            id: (self.new_id)(),
            route: r,
            legs: Vec::new(),
            obs: Vec::new(),
            picked: now,
            trigger,
            class,
            inputs: self.inputs(r, now),
            dispatched: now,
            prev_received: None,
        };
        self.jobs.insert(id, job);
        self.state[r].job = Some(id);
        self.triggered.remove(&r);
        if let Some(t) = trigger {
            self.book.duration_us("router.trigger_to_pick_us", now.saturating_duration_since(t));
        }
        self.book.count(&format!("router.jobs.{}", class.label()), 1);
        id
    }

    /// Request for the job's next leg.
    fn next_req(&self, job: u64) -> Option<(u8, BuildRequest)> {
        let j = self.jobs.get(&job)?;
        let plan = &self.defs[j.route].plan;
        let i = j.legs.len();
        let spec = plan.legs.get(i)?;
        let amount = if i == 0 { plan.amount } else { j.legs[i - 1].leg.out_amount };
        Some((i as u8, (self.make_req)(spec, amount)))
    }

    /// Advance `job` without spending budget where possible (cache, in-flight
    /// collapsing). Returns the request still to be sent, if any.
    fn advance(&mut self, job: u64, now: Instant) -> Option<(String, u8, BuildRequest)> {
        loop {
            let (index, req) = match self.next_req(job) {
                Some(x) => x,
                None => {
                    self.ready.push_back(job);
                    return None;
                }
            };
            let key = req.key();
            let trigger = self.jobs.get(&job)?.trigger;
            let usable = self.cached(&key).is_some()
                && self
                    .cache
                    .get(&key)
                    .is_some_and(|c| self.reflects(c.leg.timing.sent, &c.obs, &c.mids, trigger, now));
            if usable && let Some(leg) = self.cached(&key).cloned() {
                self.book.count("router.cache_hits", 1);
                self.book.duration_us("router.cache_hit_age_us", leg.timing.sent.elapsed());
                let obs = self.cache[&key].obs.clone();
                let j = self.jobs.get_mut(&job)?;
                if j.legs.is_empty() {
                    j.dispatched = leg.timing.sent;
                }
                j.legs.push(leg);
                j.obs.push(obs);
                j.prev_received = Some(now);
                continue;
            }
            if let Some(f) = self.inflight.get(&key) {
                if self.reflects(f.sent, &f.obs, &f.mids, trigger, now) {
                    self.book.count("router.coalesced", 1);
                    self.inflight.get_mut(&key)?.jobs.push(job);
                } else {
                    // sent before this job's trigger: its answer cannot show the change
                    self.book.count("router.waits_for_stale_inflight", 1);
                    self.blocked_on.entry(key).or_default().push(job);
                }
                return None;
            }
            return Some((key, index, req));
        }
    }

    /// Does a quote sent at `sent` reflect the market this job must see? No
    /// trigger: yes. Otherwise it must be newer than the trigger, or read only
    /// watched pools that have not moved since it was sent.
    fn reflects(
        &self,
        sent: Instant,
        obs: &Observes,
        mids_at_send: &[(Arc<str>, f64)],
        trigger: Option<Instant>,
        now: Instant,
    ) -> bool {
        let Some(t) = trigger else { return true };
        if sent >= t {
            return true;
        }
        let covered = match obs {
            Observes::Unobservable => return false,
            Observes::Dexes(d) => d.len(),
            Observes::AllPools => {
                self.mids.values().filter(|(_, at)| now.saturating_duration_since(*at) < MID_FRESH).count()
            }
        };
        mids_at_send.len() == covered
            && mids_at_send
                .iter()
                .all(|(d, m0)| self.fresh_mid(d, now).is_some_and(|m| moved_bp(Some(m), Some(*m0)) < UNCHANGED_BP))
    }

    /// Next finished job worth pricing. A job whose quotes are exactly the ones
    /// its route was last priced from carries no new information: dropped.
    fn pop_ready(&mut self) -> Option<u64> {
        while let Some(j) = self.ready.pop_front() {
            let Some(job) = self.jobs.get(&j) else { continue };
            let sent: Vec<Instant> = job.legs.iter().map(|l| l.timing.sent).collect();
            let route = job.route;
            if sent == self.state[route].last_legs {
                self.jobs.remove(&j);
                let st = &mut self.state[route];
                st.job = None;
                st.trigger = None;
                self.triggered.remove(&route);
                self.book.count("router.repeat_suppressed", 1);
                continue;
            }
            return Some(j);
        }
        None
    }

    fn mids_for(&self, o: &Observes, now: Instant) -> Vec<(Arc<str>, f64)> {
        let dexes: Vec<Arc<str>> = match o {
            Observes::Dexes(d) => d.clone(),
            Observes::AllPools => self.mids.keys().cloned().collect(),
            Observes::Unobservable => Vec::new(),
        };
        dexes.into_iter().filter_map(|d| self.fresh_mid(&d, now).map(|m| (d, m))).collect()
    }

    fn dispatch(&mut self, job: u64, key: String, index: u8, req: BuildRequest, now: Instant) -> Step {
        let j = &self.jobs[&job];
        let route = j.route;
        let class = j.class;
        let id = j.id;
        let obs = self.defs[route].obs[index as usize].clone();
        let mids = self.mids_for(&obs, now);
        if index == 0
            && let Some(j) = self.jobs.get_mut(&job)
        {
            j.dispatched = now;
            if let Some(t) = j.trigger {
                self.book.duration_us("router.trigger_to_dispatch_us", now.saturating_duration_since(t));
            }
        } else if let Some(prev) = self.jobs.get(&job).and_then(|j| j.prev_received) {
            self.book.duration_us("router.continuation_wait_us", now.saturating_duration_since(prev));
        }
        self.inflight.insert(key.clone(), InFlight { jobs: vec![job], obs, mids, index, sent: now });
        self.book.count(&format!("router.dispatch.{}", class.label()), 1);
        Step::Dispatch { key, req, index, job: id, class }
    }

    /// Budget slots still owed to routes in progress (legs not yet sent).
    fn owed(&self) -> u32 {
        self.jobs
            .iter()
            .map(|(k, j)| {
                let left = (self.defs[j.route].plan.legs.len() - j.legs.len()) as u32;
                let waiting = u32::from(self.inflight.values().any(|f| f.jobs.contains(k)));
                left - waiting.min(left)
            })
            .sum()
    }

    /// Requests a new job on `r` would need (its first leg may be reusable).
    fn requests_for(&self, r: usize) -> u32 {
        let d = &self.defs[r];
        let reused = self.cached(&d.leg0_key).is_some() || self.inflight.contains_key(&d.leg0_key);
        (d.plan.legs.len() - usize::from(reused)) as u32
    }

    /// Next thing to do. Order: finished jobs, continuations, the best
    /// triggered route, calibration. Budget slots are taken here.
    ///
    /// Admission control: a new route starts only when the window can also
    /// pay for its remaining legs *and* for the legs still owed to routes in
    /// progress — a half-quoted route waiting for budget is a stale quote.
    pub fn next(&mut self, now: Instant, budget: &WindowLimiter) -> Step {
        if let Some(j) = self.pop_ready() {
            return Step::Finish(j);
        }
        let full = self.inflight.len() >= self.cfg.max_in_flight;
        // 1. continuations (their earlier legs are aging)
        while let Some(&job) = self.continuations.front() {
            match self.advance(job, now) {
                None => {
                    self.continuations.pop_front();
                    if let Some(j) = self.pop_ready() {
                        return Step::Finish(j);
                    }
                }
                Some(_) if full => return Step::Wait(None),
                Some((key, index, req)) => match budget.try_acquire(now, 0) {
                    Ok(()) => {
                        self.continuations.pop_front();
                        return self.dispatch(job, key, index, req, now);
                    }
                    Err(wait) => {
                        self.book.count(&format!("router.continuation_blocked.{}", budget.binding(now, 0)), 1);
                        return Step::Wait(Some(now + wait));
                    }
                },
            }
        }
        if full {
            return Step::Wait(None);
        }
        // 2. triggered routes, best score first
        let mut best: Option<(f64, usize)> = None;
        for &r in &self.triggered {
            let st = &self.state[r];
            if st.job.is_some()
                || st.last_failure.is_some_and(|t| now.saturating_duration_since(t) < RETRY_AFTER_FAILURE)
            {
                continue;
            }
            let s = self.score(r, now);
            if best.is_none_or(|(b, _)| s > b) {
                best = Some((s, r));
            }
        }
        // 3. calibration: routes never decided or not decided for `floor`
        let (route, class, reserve) = match best {
            Some((_, r)) => (r, Class::Urgent, 0),
            None => match self.calibration_due(now) {
                Some(r) => (r, Class::Background, self.cfg.reserve),
                None => return Step::Wait(self.next_deadline(now)),
            },
        };
        // admission: this route's requests plus what running routes still owe
        let need = self.requests_for(route) + self.owed();
        if need > 0 && budget.available(now, reserve) < need {
            self.book.count("router.budget_blocked", 1);
            return Step::Wait(Some(budget.next_slot_n(now, reserve, need)));
        }
        let job = self.start_job(route, class, now);
        match self.advance(job, now) {
            None => {
                if let Some(j) = self.pop_ready() {
                    return Step::Finish(j);
                }
                Step::Wait(Some(now)) // collapsed onto an in-flight request: look for more work
            }
            Some((key, index, req)) => match budget.try_acquire(now, reserve) {
                Ok(()) => self.dispatch(job, key, index, req, now),
                Err(wait) => {
                    // a cached leg expired between the checks: undo, nothing spent
                    self.abandon(job);
                    self.book.count("router.budget_blocked", 1);
                    Step::Wait(Some(now + wait))
                }
            },
        }
    }

    /// The route longest without a decision, if it is due for calibration.
    fn calibration_due(&self, now: Instant) -> Option<usize> {
        (0..self.defs.len())
            .filter(|&r| {
                let st = &self.state[r];
                st.job.is_none()
                    && st.last_failure.is_none_or(|t| now.saturating_duration_since(t) >= RETRY_AFTER_FAILURE)
                    && st.last.is_none_or(|(t, _, _)| now.saturating_duration_since(t) >= self.cfg.floor)
            })
            .min_by_key(|&r| self.state[r].last.map(|(t, _, _)| t))
    }

    fn abandon(&mut self, job: u64) {
        for v in self.blocked_on.values_mut() {
            v.retain(|x| *x != job);
        }
        if let Some(j) = self.jobs.remove(&job) {
            let st = &mut self.state[j.route];
            st.job = None;
            if st.trigger.is_some() {
                self.triggered.insert(j.route);
            }
        }
        self.continuations.retain(|x| *x != job);
    }

    /// A leg request finished. Successful legs go to every job waiting on
    /// the key (and to the cache); failed jobs are returned for reporting.
    pub fn on_leg(
        &mut self,
        key: &str,
        result: Result<&BuiltLeg, &JupiterError>,
        now: Instant,
    ) -> Vec<(u64, ReadyJob)> {
        let Some(f) = self.inflight.remove(key) else { return Vec::new() };
        // jobs that needed a newer answer for this key retry now
        if let Some(blocked) = self.blocked_on.remove(key) {
            self.continuations.extend(blocked);
        }
        let mut failed = Vec::new();
        match result {
            Ok(leg) => {
                self.cache
                    .insert(key.to_string(), Cached { leg: leg.clone(), obs: f.obs.clone(), mids: f.mids.clone() });
                for job in f.jobs {
                    let Some(j) = self.jobs.get_mut(&job) else { continue };
                    if j.legs.len() != f.index as usize {
                        continue;
                    }
                    j.legs.push(leg.clone());
                    j.obs.push(f.obs.clone());
                    j.prev_received = Some(now);
                    let complete = j.legs.len() == self.defs[j.route].plan.legs.len();
                    if complete {
                        self.ready.push_back(job);
                    } else {
                        self.continuations.push_back(job);
                    }
                }
            }
            Err(e) => {
                for job in f.jobs {
                    if let Some(j) = self.jobs.remove(&job) {
                        let st = &mut self.state[j.route];
                        st.job = None;
                        st.last_failure = Some(now);
                        if e.is_rate_limited() && st.trigger.is_some() {
                            self.triggered.insert(j.route);
                        } else {
                            st.trigger = None;
                        }
                        let plan = self.defs[j.route].plan.clone();
                        failed.push((
                            job,
                            ReadyJob {
                                id: j.id,
                                route: j.route,
                                plan,
                                legs: j.legs,
                                obs: j.obs,
                                picked: j.picked,
                                trigger: j.trigger,
                                inputs: j.inputs,
                                dispatched: j.dispatched,
                            },
                        ));
                    }
                }
            }
        }
        failed
    }

    /// Hand a complete job to pricing.
    pub fn take_ready(&mut self, job: u64) -> Option<ReadyJob> {
        let j = self.jobs.remove(&job)?;
        self.state[j.route].last_legs = j.legs.iter().map(|l| l.timing.sent).collect();
        Some(ReadyJob {
            id: j.id,
            route: j.route,
            plan: self.defs[j.route].plan.clone(),
            legs: j.legs,
            obs: j.obs,
            picked: j.picked,
            trigger: j.trigger,
            inputs: j.inputs,
            dispatched: j.dispatched,
        })
    }

    /// Learn from a priced decision and move the route's reference to the
    /// inputs the job started from. A trigger older than the job's start is
    /// answered by it (its quotes reflect the change); a newer one stays.
    pub fn on_decided(&mut self, route: usize, gross_bp: f64, net_bp: f64, inputs: Inputs, started: Instant) {
        let now = Instant::now();
        let st = &mut self.state[route];
        st.job = None;
        st.last = Some((now, gross_bp, net_bp));
        if let Some(gap) = inputs.gap_bp {
            let sample = gross_bp - gap;
            st.offset = Some(match st.offset {
                None => (sample, PRIOR_VAR_BP2),
                Some((m, v)) => {
                    let m2 = (1.0 - EWMA) * m + EWMA * sample;
                    (m2, (1.0 - EWMA) * v + EWMA * (sample - m) * (sample - m))
                }
            });
            st.samples += 1;
        }
        st.reference = Some(inputs);
        // changes that arrived while the job ran are still news
        match st.trigger {
            Some(t) if t <= started => st.trigger = None,
            Some(_) => {
                self.triggered.insert(route);
            }
            None => {}
        }
        if st.trigger.is_some() {
            return;
        }
        // the market may already differ from the reference
        let c = self.change_bp(route, now);
        if c >= self.cfg.change_bp {
            let st = &mut self.state[route];
            st.trigger = Some(now);
            self.triggered.insert(route);
        }
    }

    /// Earliest calibration deadline (for the idle timer).
    pub fn next_floor(&self, now: Instant) -> Option<Instant> {
        self.next_deadline(now)
    }

    /// When calibration (or a retry after a failure) next becomes due; never
    /// in the past (a due route would have been picked).
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.state
            .iter()
            .filter(|s| s.job.is_none())
            .map(|s| {
                let due = s.last.map(|(t, _, _)| t + self.cfg.floor).unwrap_or(now);
                let retry = s.last_failure.map(|t| t + RETRY_AFTER_FAILURE).unwrap_or(now);
                due.max(retry).max(now + Duration::from_millis(1))
            })
            .min()
    }

    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    pub fn route_label(&self, r: usize) -> &str {
        &self.defs[r].plan.label
    }
}

enum Msg {
    Leg { key: String, result: Box<Result<BuiltLeg, JupiterError>> },
    Decided { route: usize, gross_bp: f64, net_bp: f64, inputs: Inputs, started: Instant },
}

/// Async shell around [`Core`]: runs requests and pricing as tasks so the
/// scheduling loop never waits on the network or on pricing.
pub async fn run(
    mut core: Core,
    pipeline: Arc<Pipeline>,
    jupiter: Arc<JupiterClient>,
    mut ticks: mpsc::Receiver<HotTick>,
    sim_tx: mpsc::Sender<crate::pipeline::SimJob>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(_) = jupiter.limiter().window() else {
        tracing::error!("event scheduler needs a sliding-window Jupiter limiter");
        return;
    };
    let book = pipeline.telemetry.latency.clone();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Msg>();
    let mut next_wake: Option<Instant>;
    loop {
        // act until the budget or the work runs out
        loop {
            let budget = jupiter.limiter().window().expect("checked above");
            let now = Instant::now();
            match core.next(now, budget) {
                Step::Finish(job) => {
                    let Some(r) = core.take_ready(job) else { continue };
                    let (pipeline, sim_tx, done, book) =
                        (pipeline.clone(), sim_tx.clone(), done_tx.clone(), book.clone());
                    tokio::spawn(async move {
                        let f = pipeline.finish(r.id, &r.plan, r.legs, &r.obs, r.picked).await;
                        if let Some(t) = r.trigger {
                            book.since_us("router.trigger_to_decision_us", t);
                        }
                        let _ = done.send(Msg::Decided {
                            route: r.route,
                            gross_bp: f.gross_bp,
                            net_bp: f.net_bp,
                            inputs: r.inputs,
                            started: r.picked,
                        });
                        if let Some(job) = f.job {
                            pipeline.submit(job, &sim_tx).await;
                        }
                    });
                }
                Step::Dispatch { key, req, index, job, .. } => {
                    let (jupiter, pipeline, done) = (jupiter.clone(), pipeline.clone(), done_tx.clone());
                    tokio::spawn(async move {
                        let result = jupiter.build_with_slot(&req, index, Duration::ZERO).await;
                        if let Ok(b) = &result {
                            pipeline.record_fetched_leg(job, index, &req, b);
                        }
                        let _ = done.send(Msg::Leg { key, result: Box::new(result) });
                    });
                }
                Step::Wait(at) => {
                    if at == Some(now) {
                        continue;
                    }
                    // a budget slot, a calibration/retry deadline, or (None) the next event
                    next_wake = at;
                    break;
                }
            }
        }
        let sleep = async {
            match next_wake {
                Some(t) => tokio::time::sleep_until(t.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = shutdown.changed() => return,
            t = ticks.recv() => {
                let Some(t) = t else { return };
                let now = Instant::now();
                core.on_tick(&t, now);
                // drain what already queued (latest state wins; every tick still counts)
                while let Ok(t) = ticks.try_recv() {
                    core.on_tick(&t, now);
                }
            }
            m = done_rx.recv() => {
                let Some(m) = m else { return };
                let now = Instant::now();
                match m {
                    Msg::Leg { key, result } => {
                        let failed = core.on_leg(&key, result.as_ref().as_ref(), now);
                        if let Err(e) = *result {
                            for (_, r) in failed {
                                pipeline.on_build_error(r.id, &r.plan, r.legs, clone_err(&e));
                            }
                        }
                    }
                    Msg::Decided { route, gross_bp, net_bp, inputs, started } => {
                        core.on_decided(route, gross_bp, net_bp, inputs, started);
                    }
                }
            }
            _ = sleep => {}
        }
    }
}

fn clone_err(e: &JupiterError) -> JupiterError {
    match e {
        JupiterError::RateLimited(d) => JupiterError::RateLimited(*d),
        JupiterError::NoRoute(s) => JupiterError::NoRoute(s.clone()),
        JupiterError::BadRequest(s) => JupiterError::BadRequest(s.clone()),
        JupiterError::Auth(c, s) => JupiterError::Auth(*c, s.clone()),
        JupiterError::Http(c, s) => JupiterError::Http(*c, s.clone()),
        JupiterError::Timeout => JupiterError::Timeout,
        JupiterError::Transport(s) => JupiterError::Transport(s.clone()),
        JupiterError::Decode(s) => JupiterError::Decode(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Probe;
    use searcher_core::address::well_known;
    use searcher_core::model::SlippageSpec;
    use searcher_core::token::TokenRegistry;
    use searcher_core::{Address, Ts};
    use searcher_jupiter::QuoteTiming;
    use searcher_strategy::{CrossDex, FastPairs, RoundTrip, Strategy};
    use std::sync::atomic::{AtomicU64, Ordering};

    const SELL: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_raydiumclmm.json");

    fn built(json: &str, input: &str, output: &str, sent: Instant) -> BuiltLeg {
        let r: searcher_jupiter::wire::BuildResponse = serde_json::from_str(json).unwrap();
        let ctx = searcher_jupiter::adapter::LegContext {
            index: 0,
            slippage_spec: SlippageSpec::Rtse,
            mode: searcher_core::RoutingMode::Fast,
            dex_filter: searcher_core::DexFilter::Any,
            quoted_at: Ts(0),
            latency_ms: 0,
            request_id: None,
            expect_input: well_known::addr(input),
            expect_output: well_known::addr(output),
        };
        let (leg, instructions) = searcher_jupiter::adapter::to_leg(&r, ctx).unwrap();
        let timing = QuoteTiming {
            sent,
            sent_ts: Ts(0),
            received: sent,
            received_ts: Ts(0),
            token_wait: Duration::ZERO,
            parse: Duration::ZERO,
            source_block_height: None,
        };
        BuiltLeg { leg, instructions, raw: String::new(), timing }
    }

    fn sell(sent: Instant) -> BuiltLeg {
        built(SELL, well_known::WSOL_MINT, well_known::USDC_MINT, sent)
    }

    fn buy(sent: Instant) -> BuiltLeg {
        built(
            include_str!("../../../fixtures/jupiter_build_usdc_sol_whirlpool.json"),
            well_known::USDC_MINT,
            well_known::WSOL_MINT,
            sent,
        )
    }

    /// Drive one full evaluation of whatever `next` picks: both legs answered
    /// with fixtures sent at `at`, then priced (decided). Returns the route.
    fn evaluate_once(c: &mut Core, b: &WindowLimiter, at: Instant) -> usize {
        let Step::Dispatch { key, index: 0, .. } = c.next(at, b) else { panic!("first leg") };
        c.on_leg(&key, Ok(&sell(at)), at);
        let Step::Dispatch { key, index: 1, .. } = c.next(at, b) else { panic!("second leg") };
        c.on_leg(&key, Ok(&buy(at)), at);
        let Step::Finish(job) = c.next(at, b) else { panic!("priced") };
        let r = c.take_ready(job).unwrap();
        c.on_decided(r.route, -5.0, -12.0, r.inputs, r.picked);
        r.route
    }

    fn core(cfg: RouterConfig) -> Core {
        let t = TokenRegistry::defaults();
        let (sol, usdc) = (t.sol().clone(), t.get("USDC").unwrap().clone());
        let fast = FastPairs(vec!["SOL/USDC".into()]);
        let dexes = vec!["Whirlpool".to_string(), "Raydium CLMM".into(), "HumidiFi".into()];
        let mut plans =
            CrossDex::new(sol.clone(), usdc.clone(), 1_000_000_000, 3, dexes, fast.clone(), Some(30)).plans();
        plans.extend(
            RoundTrip {
                base: sol.clone(),
                quote: usdc.clone(),
                amount: 1_000_000_000,
                weight: 1,
                fast,
                max_accounts: Some(30),
            }
            .plans(),
        );
        let probe = Probe::new(
            Arc::new(LatencyBook::default()),
            ["Whirlpool".to_string(), "Raydium CLMM".into()],
            (sol.mint, usdc.mint),
        );
        let ids = Arc::new(AtomicU64::new(1));
        let make_req = |spec: &LegSpec, amount: u64| BuildRequest {
            input_mint: spec.input,
            output_mint: spec.output,
            amount,
            taker: Address([9; 32]),
            slippage: SlippageSpec::Rtse,
            mode: spec.mode,
            dex_filter: spec.dex_filter.clone(),
            cu_price_percentile: "high".into(),
            max_accounts: spec.max_accounts,
            blockhash_slots_to_expiry: 150,
            for_jito_bundle: true,
        };
        Core::new(
            cfg,
            plans,
            |l| probe.observes(&l.input, &l.output, &l.dex_filter),
            |_| None,
            Arc::new(move || OpportunityId(ids.fetch_add(1, Ordering::Relaxed))),
            Arc::new(make_req),
            Arc::new(LatencyBook::default()),
        )
    }

    fn tick(dex: &str, mid: f64, at: Instant) -> HotTick {
        HotTick::Pool {
            watch: 0,
            dex: Arc::from(dex),
            pair: Arc::from("SOL/USDC"),
            mid,
            slot: None,
            head_slot: None,
            received: at,
            received_ts: Ts(0),
        }
    }

    fn route(c: &Core, label: &str) -> usize {
        (0..c.routes()).find(|r| c.route_label(*r) == label).unwrap_or_else(|| panic!("{label}"))
    }

    /// Market at W = R = 100 and every route decided there.
    fn settled(cfg: RouterConfig) -> (Core, Instant) {
        let mut c = core(cfg);
        let t0 = Instant::now();
        c.on_tick(&tick("Whirlpool", 100.0, t0), t0);
        c.on_tick(&tick("Raydium CLMM", 100.0, t0), t0);
        for r in 0..c.routes() {
            let inputs = c.inputs(r, t0);
            c.on_decided(r, -5.0, -12.0, inputs, t0);
        }
        (c, t0)
    }

    fn budget() -> WindowLimiter {
        WindowLimiter::new("t", 10, Duration::from_secs(10), 1)
    }

    fn triggered(c: &Core) -> Vec<String> {
        let mut v: Vec<String> = c.triggered.iter().map(|r| c.route_label(*r).to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn calibration_then_a_quiet_market_spends_nothing() {
        let mut c = core(RouterConfig { max_in_flight: 100, ..Default::default() });
        let b = budget();
        let t0 = Instant::now();
        c.on_tick(&tick("Whirlpool", 100.0, t0), t0);
        let drain = |c: &mut Core, b: &WindowLimiter| {
            let mut dispatched = Vec::new();
            loop {
                match c.next(t0, b) {
                    Step::Dispatch { class, .. } => dispatched.push(class),
                    Step::Finish(_) => {}
                    Step::Wait(Some(t)) if t == t0 => {}
                    Step::Wait(_) => return dispatched,
                }
            }
        };
        // calibration starts only what the background budget can finish:
        // the slots left above the reserve cover every leg still owed
        let sent = drain(&mut c, &b);
        assert!(!sent.is_empty() && sent.iter().all(|c| *c == Class::Background));
        assert!(b.available(t0, 3) >= c.owed(), "owed {} > available {}", c.owed(), b.available(t0, 3));
        assert!(c.book.counter("router.coalesced") >= 1, "routes sharing a first leg attach to one request");
        assert!(b.available(t0, 0) >= 3, "the reserve stays free for urgent work");

        // with too little budget for a whole route, nothing is started
        let mut c = core(RouterConfig { max_in_flight: 100, ..Default::default() });
        let b = budget();
        for _ in 0..5 {
            b.try_acquire(t0, 0).unwrap();
        }
        assert!(drain(&mut c, &b).is_empty(), "1 slot above the reserve cannot finish a 2-leg route");
        assert_eq!(b.available(t0, 0), 4);

        let (mut c, t0) = settled(RouterConfig::default());
        let b = budget();
        let Step::Wait(Some(wake)) = c.next(t0, &b) else { panic!("nothing changed → no request") };
        assert!(wake >= t0 + Duration::from_secs(89), "sleeps until the next calibration");
        assert_eq!(b.available(t0, 0), 9);
        assert!(c.next_floor(t0).unwrap() >= t0 + Duration::from_secs(89), "next calibration in `floor`");
    }

    #[test]
    fn a_gap_change_triggers_only_the_routes_that_read_it() {
        let (mut c, t0) = settled(RouterConfig::default());
        // Raydium +0.6 bp: the W/R gap moves 0.6 bp, the mean only 0.3 bp
        c.on_tick(&tick("Raydium CLMM", 100.006, t0), t0);
        assert_eq!(
            triggered(&c),
            vec![
                "HumidiFi → Raydium CLMM",
                "Raydium CLMM → HumidiFi",
                "Raydium CLMM → Whirlpool",
                "Whirlpool → Raydium CLMM"
            ]
        );

        // both pools +2 bp: the W/R gap is unchanged → those two routes are not news
        let (mut c, t0) = settled(RouterConfig::default());
        c.on_tick(&tick("Whirlpool", 100.02, t0), t0);
        c.on_tick(&tick("Raydium CLMM", 100.02, t0), t0);
        let t = triggered(&c);
        assert!(!t.contains(&"Whirlpool → Raydium CLMM".to_string()), "{t:?}");
        assert!(!t.contains(&"Raydium CLMM → Whirlpool".to_string()), "{t:?}");
        assert_eq!(t.len(), 4, "the HumidiFi routes read those pools: {t:?}");
        assert!(!t.iter().any(|l| l.contains("SOL→USDC→SOL")), "round trip is level-invariant");
    }

    #[test]
    fn identical_legs_are_collapsed_cached_and_invalidated_by_the_pool() {
        let (mut c, t0) = settled(RouterConfig::default());
        let (wr, wh) = (route(&c, "Whirlpool → Raydium CLMM"), route(&c, "Whirlpool → HumidiFi"));
        let a = c.start_job(wr, Class::Urgent, t0);
        let (key, index, _) = c.advance(a, t0).expect("first leg must be sent");
        assert_eq!(index, 0);
        c.inflight.insert(
            key.clone(),
            InFlight {
                jobs: vec![a],
                obs: c.defs[wr].obs[0].clone(),
                mids: c.mids_for(&c.defs[wr].obs[0], t0),
                index: 0,
                sent: t0,
            },
        );
        let b2 = c.start_job(wh, Class::Urgent, t0);
        assert!(c.advance(b2, t0).is_none(), "same first leg in flight → attached, not sent");
        assert_eq!(c.book.counter("router.coalesced"), 1);
        assert!(c.on_leg(&key, Ok(&sell(t0)), t0).is_empty());
        assert_eq!(c.continuations.len(), 2, "one response advanced both routes");

        // a later job reuses the cached leg while the pool is unchanged
        c.on_decided(wr, -5.0, -12.0, c.inputs(wr, t0), t0);
        let j = c.start_job(wr, Class::Urgent, t0);
        let (_, index, _) = c.advance(j, t0).unwrap();
        assert_eq!(index, 1, "leg 0 came from the cache");
        assert_eq!(c.book.counter("router.cache_hits"), 1);

        // Whirlpool moves 1 bp → that cached leg is gone
        c.on_tick(&tick("Whirlpool", 100.01, t0), t0);
        assert!(c.cached(&key).is_none());
        assert!(c.book.counter("router.cache_invalidated") >= 1);
    }

    #[test]
    fn continuations_go_before_new_routes() {
        let (mut c, t0) = settled(RouterConfig::default());
        let b = budget();
        c.on_tick(&tick("Raydium CLMM", 100.01, t0), t0);
        let Step::Dispatch { key, index: 0, .. } = c.next(t0, &b) else { panic!("urgent route first") };
        c.on_leg(&key, Ok(&sell(t0)), t0);
        // other routes are still triggered, but the started route finishes first
        match c.next(t0, &b) {
            Step::Dispatch { index, .. } => assert_eq!(index, 1),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn a_reverted_change_is_cancelled_without_spending() {
        let (mut c, t0) = settled(RouterConfig::default());
        let b = budget();
        c.on_tick(&tick("Raydium CLMM", 100.01, t0), t0);
        assert!(!c.triggered.is_empty());
        c.on_tick(&tick("Raydium CLMM", 100.0, t0), t0);
        assert!(c.triggered.is_empty(), "{:?}", triggered(&c));
        assert!(c.book.counter("router.cancelled") >= 2);
        assert!(matches!(c.next(t0, &b), Step::Wait(Some(_))), "no request");
        assert_eq!(b.available(t0, 0), 9, "nothing was spent");
    }

    #[test]
    fn admission_keeps_budget_for_started_routes() {
        let (mut c, t0) = settled(RouterConfig { max_in_flight: 100, ..Default::default() });
        let b = budget();
        // big move on both pools: many routes triggered at once
        c.on_tick(&tick("Whirlpool", 100.05, t0), t0);
        c.on_tick(&tick("Raydium CLMM", 100.0, t0), t0);
        let mut starts = 0;
        while let Step::Dispatch { .. } = c.next(t0, &b) {
            starts += 1;
        }
        // whatever was started can be finished from the window
        assert!(starts > 0);
        assert!(b.available(t0, 0) >= c.owed(), "owed {} > available {}", c.owed(), b.available(t0, 0));
    }

    #[test]
    fn a_triggered_route_is_never_repriced_from_quotes_older_than_the_change() {
        let (mut c, t0) = settled(RouterConfig::default());
        let b = WindowLimiter::new("t", 100, Duration::from_secs(10), 1);
        c.on_tick(&tick("Raydium CLMM", 100.01, t0), t0);
        let route = evaluate_once(&mut c, &b, t0);
        let label = c.route_label(route).to_string();
        // opposite 0.3 bp moves: no pool crosses the 0.5 bp invalidation, but
        // the gap moves 0.6 bp — the cached quotes predate that change
        let t1 = t0 + Duration::from_millis(200);
        c.on_tick(&tick("Whirlpool", 100.003, t1), t1);
        c.on_tick(&tick("Raydium CLMM", 100.007, t1), t1);
        assert!(triggered(&c).contains(&label), "{:?}", triggered(&c));
        let mut finishes = 0;
        let mut dispatched = 0;
        for _ in 0..50 {
            match c.next(t1, &b) {
                Step::Finish(_) => finishes += 1,
                Step::Dispatch { .. } => dispatched += 1,
                Step::Wait(_) => break,
            }
        }
        assert_eq!(finishes, 0, "no decision without a quote newer than the change");
        assert!(dispatched >= 1, "the change is answered with a fresh request");
    }

    #[test]
    fn identical_quotes_are_never_priced_twice() {
        let (mut c, t0) = settled(RouterConfig::default());
        let b = WindowLimiter::new("t", 100, Duration::from_secs(10), 1);
        c.on_tick(&tick("Raydium CLMM", 100.01, t0), t0);
        let route = evaluate_once(&mut c, &b, t0);
        // nothing moved: a calibration job on the same route completes from
        // the cache with exactly the quotes it was last priced from
        let j = c.start_job(route, Class::Background, t0);
        assert!(c.advance(j, t0).is_none(), "both legs come from the cache");
        assert_eq!(c.pop_ready(), None, "no new information → not priced again");
        assert_eq!(c.book.counter("router.repeat_suppressed"), 1);
        assert!(c.state[route].job.is_none() && c.state[route].trigger.is_none());
    }

    #[test]
    fn ln_phi_is_monotone_and_finite() {
        assert!((ln_phi(0.0) - 0.5f64.ln()).abs() < 1e-6);
        let mut prev = f64::NEG_INFINITY;
        for i in -400..=50 {
            let v = ln_phi(i as f64 * 0.1);
            assert!(v.is_finite() && v >= prev - 1e-9, "z = {}", i as f64 * 0.1);
            prev = v;
        }
        assert!(ln_phi(3.0) < 0.0 && ln_phi(3.0) > -0.01);
    }
}
