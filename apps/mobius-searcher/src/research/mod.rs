//! `--research`: three measurements that decide what to build next, recorded
//! to `<data_dir>/research.sqlite`. Nothing is signed or sent.
//!
//! * [`ladder`]: the configured routes quoted at several sizes — does a bigger
//!   trade change the edge?
//! * [`xchain`]: the same asset on Solana and on EVM chains — is there a
//!   spread after fees and gas?
//! * [`lag`]: on-chain pool prices against the CEX price — do DEX prices lag,
//!   and is the gap executable?
//!
//! Jupiter is the scarce resource. Every quote goes through one [`Gate`] in
//! (lag entries first, then lag exits, then the rest), and lower classes
//! leave rate-limit slots free for entries.

pub mod ladder;
pub mod lag;
pub mod report;
pub mod xchain;

use crate::budget::{self, KeepAwake};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use searcher_core::config::Config;
use searcher_core::model::SlippageSpec;
use searcher_core::{Address, Ts};
use searcher_jupiter::{ApiKey, BuildRequest, BuiltLeg, JupiterClient};
use searcher_storage::ResearchStore;
use searcher_telemetry::{Limiter, Telemetry, WindowLimiter};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Time-sensitive (a lag episode is open).
    High,
    /// A lag exit: timed, but it must never delay an entry.
    Exit,
    Normal,
}

struct Job {
    req: BuildRequest,
    reply: oneshot::Sender<Result<BuiltLeg, String>>,
    queued: std::time::Instant,
}

#[derive(Default)]
pub struct GateStats {
    pub requests: AtomicI64,
    pub errors: AtomicI64,
    pub rate_limited: AtomicI64,
}

/// Window slots a class of request must leave free when it starts: exits
/// never take the last slot and the rest never take the last two, so a lag
/// entry (which has to be quoted while its gap is still open) finds a slot
/// at once even when the window is otherwise full.
pub const RESERVE: [u32; 3] = [0, 1, 2];

impl Priority {
    fn class(self) -> usize {
        match self {
            Priority::High => 0,
            Priority::Exit => 1,
            Priority::Normal => 2,
        }
    }
}

/// Paces Jupiter requests: the highest-priority waiting request starts as
/// soon as the rate-limit window has a slot for it (see [`RESERVE`]); a
/// request that has its slot is sent at once, without waiting for the ones
/// still in flight.
#[derive(Clone)]
pub struct Gate {
    tx: [mpsc::Sender<Job>; 3],
    pub stats: Arc<GateStats>,
}

impl Gate {
    pub fn spawn(jup: Arc<JupiterClient>, mut shutdown: watch::Receiver<bool>) -> (Gate, tokio::task::JoinHandle<()>) {
        let (hi, mut hi_rx) = mpsc::channel::<Job>(64);
        let (exit, mut exit_rx) = mpsc::channel::<Job>(256);
        let (lo, mut lo_rx) = mpsc::channel::<Job>(64);
        let stats = Arc::new(GateStats::default());
        let s = stats.clone();
        let task = tokio::spawn(async move {
            let mut queues: [std::collections::VecDeque<Job>; 3] = Default::default();
            loop {
                while let Ok(j) = hi_rx.try_recv() {
                    queues[0].push_back(j);
                }
                while let Ok(j) = exit_rx.try_recv() {
                    queues[1].push_back(j);
                }
                while let Ok(j) = lo_rx.try_recv() {
                    queues[2].push_back(j);
                }
                // the highest class with work; until it has a slot, wait for
                // the slot or for new work (which may outrank it)
                let wait = match queues.iter().position(|q| !q.is_empty()) {
                    None => None,
                    Some(c) => {
                        // literal slots (the limiter's own reserve scales with the learned capacity)
                        let slot = match jup.limiter().window() {
                            Some(w) => {
                                let now = std::time::Instant::now();
                                if w.available(now, 0) > RESERVE[c] {
                                    w.try_acquire(now, 0)
                                } else {
                                    Err(w.next_slot_n(now, 0, RESERVE[c] + 1).saturating_duration_since(now))
                                }
                            }
                            None => Ok(()), // token bucket: JupiterClient::build waits itself
                        };
                        match slot {
                            Ok(()) => {
                                let job = queues[c].pop_front().expect("non-empty");
                                let (jup, s, windowed) = (jup.clone(), s.clone(), jup.limiter().window().is_some());
                                tokio::spawn(async move {
                                    s.requests.fetch_add(1, Ordering::Relaxed);
                                    let r = if windowed {
                                        jup.build_with_slot(&job.req, 0, job.queued.elapsed()).await
                                    } else {
                                        jup.build(&job.req, 0).await
                                    };
                                    let r = r.map_err(|e| {
                                        s.errors.fetch_add(1, Ordering::Relaxed);
                                        if e.is_rate_limited() {
                                            s.rate_limited.fetch_add(1, Ordering::Relaxed);
                                        }
                                        e.to_string()
                                    });
                                    let _ = job.reply.send(r);
                                });
                                continue;
                            }
                            Err(d) => Some(d),
                        }
                    }
                };
                let sleep = tokio::time::sleep(wait.unwrap_or(Duration::from_secs(3600)));
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return,
                    Some(j) = hi_rx.recv() => queues[0].push_back(j),
                    Some(j) = exit_rx.recv() => queues[1].push_back(j),
                    Some(j) = lo_rx.recv() => queues[2].push_back(j),
                    _ = sleep, if wait.is_some() => {}
                    else => return,
                }
            }
        });
        (Gate { tx: [hi, exit, lo], stats }, task)
    }

    pub async fn build(&self, req: BuildRequest, p: Priority) -> Result<BuiltLeg, String> {
        let (reply, rx) = oneshot::channel();
        let job = Job { req, reply, queued: std::time::Instant::now() };
        self.tx[p.class()].send(job).await.map_err(|_| "stopped".to_string())?;
        rx.await.map_err(|_| "stopped".to_string())?
    }
}

/// Shared by the three measurements.
pub struct Ctx {
    pub run: String,
    pub cfg: Config,
    pub store: Mutex<ResearchStore>,
    pub gate: Gate,
    pub taker: Address,
    pub slippage: SlippageSpec,
    /// Latest SOL/USD from the CEX feeds (lag), used to value Solana fees.
    pub sol_usd: Mutex<Option<f64>>,
    /// Latest ETH/USD (cross-chain ETH quotes), used to value EVM gas.
    pub eth_usd: Mutex<Option<f64>>,
}

impl Ctx {
    /// A `/build` request with the configured routing parameters.
    pub fn request(
        &self,
        input: Address,
        output: Address,
        amount: u64,
        spec: Option<&searcher_strategy::LegSpec>,
    ) -> BuildRequest {
        let j = &self.cfg.jupiter;
        BuildRequest {
            input_mint: input,
            output_mint: output,
            amount,
            taker: self.taker,
            slippage: self.slippage,
            mode: spec.map(|s| s.mode).unwrap_or_default(),
            dex_filter: spec.map(|s| s.dex_filter.clone()).unwrap_or_default(),
            cu_price_percentile: j.compute_unit_price_percentile.clone(),
            max_accounts: spec.and_then(|s| s.max_accounts).or(j.max_accounts),
            blockhash_slots_to_expiry: j.blockhash_slots_to_expiry,
            for_jito_bundle: j.for_jito_bundle,
        }
    }

    /// Record, logging (not failing) on a database error.
    pub fn record(&self, what: &str, f: impl FnOnce(&ResearchStore) -> Result<(), searcher_storage::StoreError>) {
        if let Err(e) = f(&self.store.lock()) {
            eprintln!("research: writing {what}: {e}");
        }
    }
}

/// Small deterministic shuffle (order of ladder sizes: spreads market drift
/// over sizes instead of always quoting the largest last).
pub fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut x = seed | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

pub async fn run(cfg: Config, duration: Option<u64>) -> Result<()> {
    let data_dir = cfg.data_dir();
    let _lock = budget::acquire(&data_dir, "--research")?;
    let awake = if cfg.research.keep_awake { KeepAwake::start() } else { KeepAwake::off() };
    let db = data_dir.join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let run_id = searcher_storage::new_session_id();

    let telemetry = Arc::new(Telemetry::new());
    let key = std::env::var(&cfg.jupiter.api_key_env).ok().and_then(ApiKey::new);
    let tier = if key.is_some() { "API key" } else { "keyless" };
    let sc = &cfg.scheduler;
    let jupiter = Arc::new(JupiterClient::with_limiter(
        &cfg.jupiter.base_url,
        key,
        Limiter::Window(WindowLimiter::new(
            "jupiter.window",
            sc.window_capacity,
            Duration::from_millis(sc.window_ms + sc.window_margin_ms),
            sc.window_safety,
        )),
        Duration::from_millis(cfg.jupiter.timeout_ms),
        telemetry.clone(),
    )?);
    // Quotes need a taker; nothing is simulated or signed with it here.
    let taker = cfg
        .simulation_taker()
        .unwrap_or(searcher_core::address::well_known::addr("F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe"));
    let slippage = cfg.jupiter.slippage_spec().map_err(anyhow::Error::msg)?;

    let setup = serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "proxy": crate::doctor::proxy_line(),
        "jupiter": { "base_url": searcher_core::config::display_url(&cfg.jupiter.base_url), "tier": tier },
        "rpc_ws": searcher_core::config::display_url(&cfg.rpc.resolved_ws_url()),
        "keep_awake": awake.active(),
        "research": cfg.research,
        "pools": cfg.feeds.pools.iter().map(|p| format!("{} {}", p.dex, p.address)).collect::<Vec<_>>(),
    });
    store.begin_run(&run_id, Ts::now().0, env!("CARGO_PKG_VERSION"), &setup.to_string())?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (gate, gate_task) = Gate::spawn(jupiter, shutdown_rx.clone());
    let ctx = Arc::new(Ctx {
        run: run_id.clone(),
        cfg: cfg.clone(),
        store: Mutex::new(store),
        gate: gate.clone(),
        taker,
        slippage,
        sol_usd: Mutex::new(None),
        eth_usd: Mutex::new(None),
    });

    let plans = ladder::plans(&cfg)?;
    println!(
        "research {run_id} · {} · Jupiter {tier} · keep awake {} · writing {}",
        crate::doctor::proxy_line(),
        if awake.active() { "on" } else { "off" },
        db.display()
    );
    println!(
        "  ladder: {} routes × {} sizes every {} s · cross-chain: {} assets every {} s · lag: {} pools vs OKX/Binance",
        plans.len(),
        cfg.research.ladder_sizes_lamports.len(),
        cfg.research.ladder_every_s,
        cfg.research.xchain_assets.len(),
        cfg.research.xchain_every_s,
        cfg.feeds.pools.len()
    );
    println!("  Ctrl-C to stop; `--research-report` summarises what was recorded.");

    let mut tasks = vec![gate_task];
    tasks.push(tokio::spawn(ladder::run(ctx.clone(), plans, shutdown_rx.clone())));
    tasks.push(tokio::spawn(xchain::run(ctx.clone(), shutdown_rx.clone())));
    tasks.extend(lag::spawn(ctx.clone(), telemetry.clone(), shutdown_rx.clone())?);

    let started = tokio::time::Instant::now(); // monotonic: does not advance while asleep
    let deadline = duration.map(|s| started + Duration::from_secs(s));
    let mut status = tokio::time::interval(Duration::from_secs(30));
    status.tick().await;
    let progress = |ended: Option<i64>| {
        let s = &gate.stats;
        ctx.record("run", |st| {
            st.update_run(
                &run_id,
                ended,
                started.elapsed().as_secs() as i64,
                s.requests.load(Ordering::Relaxed),
                s.errors.load(Ordering::Relaxed),
                s.rate_limited.load(Ordering::Relaxed),
            )
        });
    };
    // `kill` (SIGTERM) stops as cleanly as Ctrl-C: the run's end is recorded
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let terminated = async move { term.recv().await };
    #[cfg(not(unix))]
    let terminated = std::future::pending::<Option<()>>();
    tokio::pin!(terminated);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut terminated => break,
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
            _ = status.tick() => {
                progress(None);
                let [l, x, t, e] = ctx.store.lock().counts(&run_id).unwrap_or_default();
                let s = &gate.stats;
                println!(
                    "{}  ladder {l:>5}  xchain {x:>5}  lag ticks {t:>6}  episodes {e:>4}  jupiter {} req · {} err · {} × 429  awake {} min",
                    Ts::now().hms(),
                    s.requests.load(Ordering::Relaxed),
                    s.errors.load(Ordering::Relaxed),
                    s.rate_limited.load(Ordering::Relaxed),
                    started.elapsed().as_secs() / 60,
                );
            }
        }
    }
    let _ = shutdown_tx.send(true);
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }
    progress(Some(Ts::now().0));
    let store = ctx.store.lock();
    match report::build(&store, std::slice::from_ref(&run_id)) {
        Ok(r) => print!("\n{}", report::render(&r)),
        Err(e) => eprintln!("report: {e}"),
    }
    Ok(())
}
