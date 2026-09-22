//! Wiring: clients, feeds, pipeline tasks, recorder, hub, command handling.

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use searcher_core::config::{Config, OracleSourceKind, SchedulerKind};
use searcher_core::event::{Command, LogLevel, SessionInfo};
use searcher_core::model::{Mode, ServiceId};
use searcher_core::units::format_atoms;
use searcher_core::{Address, Event, Ts};
use searcher_execution::live::LiveParams;
use searcher_execution::{Pipeline, PipelineConfig, RealBackend, RuntimeView, Wallet};
use searcher_jito::{JitoClient, SendPermit, TipPolicy};
use searcher_jupiter::{ApiKey, JupiterClient};
use searcher_market::{ChainState, RpcClient, feed};
use searcher_risk::{KillSwitch, RiskEngine, RiskLimits};
use searcher_strategy::{CrossDex, FastPairs, RoundTrip, Scheduler, Strategy, Triangular};
use searcher_telemetry::{EventBus, Limiter, LimiterConfig, Telemetry, WindowLimiter};
use searcher_tui::ViewModel;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub struct Running {
    pub session_id: String,
    pub telemetry: Arc<Telemetry>,
    pub probe: Arc<searcher_execution::Probe>,
    pub vm: Arc<RwLock<ViewModel>>,
    pub commands: mpsc::UnboundedSender<Command>,
    pub shutdown_tx: watch::Sender<bool>,
    pub bus: Arc<EventBus>,
    pub recorder: std::thread::JoinHandle<searcher_storage::RecorderStats>,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
    pub db_path: PathBuf,
}

fn limits_for_display(c: &Config) -> Vec<(String, String)> {
    let r = &c.risk;
    let sol = |l: u64| format!("{} SOL", format_atoms(l as i128, 9, 4));
    vec![
        ("max_trade_size".into(), sol(r.max_trade_lamports)),
        ("max_trade_pct_of_equity".into(), format!("{}%", r.max_trade_pct_of_equity_bps as f64 / 100.0)),
        ("max_daily_loss".into(), format!("${}", r.max_daily_loss_usd)),
        ("max_consecutive_failures".into(), r.max_consecutive_failures.to_string()),
        ("max_slippage".into(), format!("{} bp", r.max_slippage_bps)),
        ("max_quote_age_ms".into(), r.max_quote_age_ms.to_string()),
        ("max_simulation_age_ms".into(), r.max_simulation_age_ms.to_string()),
        ("max_priority_fee".into(), format!("{} lamports", r.max_priority_fee_lamports)),
        ("max_jito_tip".into(), format!("{} lamports", r.max_jito_tip_lamports)),
        ("min_wallet_sol_for_fees".into(), sol(r.min_wallet_sol_for_fees_lamports)),
        ("max_open_execution".into(), r.max_open_executions.to_string()),
        ("max_slot_lag".into(), format!("{} slots", r.max_slot_lag)),
        (
            "min_profit".into(),
            format!(
                "{} lamports · {} bp · ${}",
                c.profit.min_profit_lamports, c.profit.min_profit_bps, c.profit.min_profit_usd
            ),
        ),
        ("live_enabled".into(), c.execution.live_enabled.to_string()),
    ]
}

pub(crate) fn build_strategies(c: &Config) -> Result<Vec<Box<dyn Strategy>>> {
    let tokens = c.tokens();
    let tok = |s: &str| tokens.get(s).cloned().with_context(|| format!("unknown token {s}"));
    let fast = FastPairs(c.jupiter.fast_mode_pairs.clone());
    let mut v: Vec<Box<dyn Strategy>> = Vec::new();
    for s in c.strategies.round_trip.iter().filter(|s| s.enabled) {
        v.push(Box::new(RoundTrip {
            base: tok(&s.base)?,
            quote: tok(&s.quote)?,
            amount: s.amount_lamports,
            weight: s.weight,
            fast: fast.clone(),
            max_accounts: s.max_accounts,
        }));
    }
    for s in c.strategies.cross_dex.iter().filter(|s| s.enabled) {
        v.push(Box::new(CrossDex::new(
            tok(&s.base)?,
            tok(&s.quote)?,
            s.amount_lamports,
            s.weight,
            s.dexes.clone(),
            fast.clone(),
            s.max_accounts,
        )));
    }
    for s in c.strategies.triangular.iter().filter(|s| s.enabled) {
        let cycle = s.cycle.iter().map(|t| tok(t)).collect::<Result<Vec<_>>>()?;
        v.push(Box::new(Triangular {
            cycle,
            amount: s.amount_lamports,
            weight: s.weight,
            fast: fast.clone(),
            max_accounts: s.max_accounts,
        }));
    }
    Ok(v)
}

/// `[storage]` as the recorder's retention policy.
pub fn retention(cfg: &Config) -> searcher_storage::Retention {
    let s = &cfg.storage;
    searcher_storage::Retention {
        keep_days: s.retention_days,
        keep_trading_days: s.keep_trading_days,
        max_db_mb: s.max_db_mb,
        prune_every_min: s.prune_interval_min,
    }
}

pub async fn start(cfg: Config, db_path: PathBuf) -> Result<Running> {
    start_with(cfg, db_path, None).await
}

/// `user_config`: where operator threshold changes are saved (None: they
/// last for this session only).
pub async fn start_with(cfg: Config, db_path: PathBuf, user_config: Option<PathBuf>) -> Result<Running> {
    let mode = cfg.general.mode;
    let session_id = searcher_storage::new_session_id();
    let telemetry = Arc::new(Telemetry::new());

    // Channels: UI (drop on overflow) and storage (drop + count).
    let (ui_tx, mut ui_rx) = mpsc::channel::<Event>(8_192);
    let (st_tx, st_rx) = std::sync::mpsc::sync_channel::<Event>(65_536);
    let bus = Arc::new(EventBus::new(Some(ui_tx), Some(st_tx)));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Secrets come only from the environment; config holds env var *names*.
    let jup_key = std::env::var(&cfg.jupiter.api_key_env).ok().and_then(ApiKey::new);
    let rpc_url = cfg.rpc.resolved_url();
    let ws_url = cfg.rpc.resolved_ws_url();
    let jito_uuid = std::env::var(&cfg.jito.uuid_env).ok();

    let event_mode = cfg.scheduler.kind == SchedulerKind::Event;
    let jupiter = Arc::new(if event_mode {
        // Gateway semantics: N requests per sliding window (learned at runtime).
        let sc = &cfg.scheduler;
        JupiterClient::with_limiter(
            &cfg.jupiter.base_url,
            jup_key,
            Limiter::Window(WindowLimiter::new(
                "jupiter.window",
                sc.window_capacity,
                Duration::from_millis(sc.window_ms + sc.window_margin_ms),
                sc.window_safety,
            )),
            Duration::from_millis(cfg.jupiter.timeout_ms),
            telemetry.clone(),
        )?
    } else {
        let mut jl = LimiterConfig::new(cfg.jupiter.general_rps, cfg.jupiter.general_burst);
        jl.max_backoff = Duration::from_secs(60);
        JupiterClient::new(
            &cfg.jupiter.base_url,
            jup_key,
            jl,
            Duration::from_millis(cfg.jupiter.timeout_ms),
            telemetry.clone(),
        )?
    });
    let rpc = Arc::new(RpcClient::new(
        &rpc_url,
        LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
        cfg.rpc.simulate_rps,
        Duration::from_millis(cfg.rpc.timeout_ms),
        telemetry.clone(),
    )?);
    let jito = Arc::new(JitoClient::new(
        &cfg.jito.block_engine_url,
        &cfg.jito.tip_floor_url,
        jito_uuid,
        cfg.jito.rps,
        telemetry.clone(),
    )?);

    let chain = Arc::new(ChainState::default());
    let view = Arc::new(RuntimeView::default());
    let kill = Arc::new(KillSwitch::default());
    let risk = Arc::new(RiskEngine::new(RiskLimits::from_config(&cfg.risk).map_err(anyhow::Error::msg)?, kill.clone()));

    // Wallet: only in modes that send. PAPER never reads a private key.
    let mut taker = cfg.simulation_taker();
    let live = if mode.sends_transactions() {
        let Some(permit) = SendPermit::check(mode, cfg.execution.live_enabled) else {
            bail!("mode {} requires execution.live_enabled = true", mode.label());
        };
        let path = cfg.wallet.keypair_path.clone().context("wallet.keypair_path required")?;
        let expected: Option<Address> =
            cfg.wallet.pubkey.as_deref().map(|p| p.parse()).transpose().map_err(|e| anyhow::anyhow!("{e}"))?;
        let wallet = Wallet::load(std::path::Path::new(&path), expected.as_ref()).context("loading hot wallet")?;
        // In sending modes the signer is always the taker (quotes, simulation, fee payer).
        taker = Some(wallet.pubkey());
        Some((permit, wallet, RealBackend { rpc: rpc.clone(), jito: jito.clone(), chain: chain.clone() }))
    } else {
        None
    };

    let taker_label = match (&cfg.paper.simulation_taker, taker) {
        (Some(_), Some(t)) if mode == Mode::Paper => Some(format!("sim taker {} (shadow)", t.short())),
        (_, Some(t)) => Some(format!("wallet {}", t.short())),
        _ => Some("no taker · simulation off".into()),
    };
    let session = SessionInfo {
        session_id: session_id.clone(),
        started_at: Ts::now(),
        mode,
        version: env!("CARGO_PKG_VERSION").into(),
        config_summary: cfg.summary(),
        taker: taker_label,
        paper_equity_lamports: (mode == Mode::Paper).then_some(cfg.paper.equity_lamports),
        limits: limits_for_display(&cfg),
    };

    // Recorder thread.
    let drops = bus.dropped_store_counter();
    let recorder = searcher_storage::spawn_recorder_with(
        db_path.clone(),
        session.clone(),
        st_rx,
        telemetry.clone(),
        Arc::new(move || drops.load(std::sync::atomic::Ordering::Relaxed)),
        retention(&cfg),
    )?;
    bus.emit(Event::Session(session));

    // View model hub.
    let vm = Arc::new(RwLock::new(ViewModel::new(false)));
    let mut tasks = Vec::new();
    {
        let vm = vm.clone();
        let bus = bus.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(e) = ui_rx.recv().await {
                let mut g = vm.write();
                g.apply(&e);
                // drain what is already queued under one lock
                while let Ok(e) = ui_rx.try_recv() {
                    g.apply(&e);
                }
                g.ui_dropped = bus.dropped_ui();
            }
        }));
    }

    let emit: feed::Emit = {
        let bus = bus.clone();
        Arc::new(move |e| bus.emit(e))
    };

    // Latency probes (scheduler-independent) + Tokio scheduling lag.
    let tokens = cfg.tokens();
    let probe = Arc::new(searcher_execution::Probe::new(
        telemetry.latency.clone(),
        cfg.feeds.pools.iter().map(|p| p.dex.clone()),
        (tokens.sol().mint, tokens.get("USDC").map(|t| t.mint).unwrap_or_default()),
    ));
    tasks.push(tokio::spawn(searcher_telemetry::latency::run_runtime_lag_probe(
        telemetry.latency.clone(),
        shutdown_rx.clone(),
    )));
    // HOT ticks: probe (both modes) and, in event mode, the scheduler. The
    // channel is bounded and never waited on: a full queue counts a drop.
    let (tick_tx, tick_rx) = mpsc::channel::<searcher_market::hot::HotTick>(4_096);
    let hot: searcher_market::hot::HotSink = {
        let probe = probe.clone();
        let book = telemetry.latency.clone();
        let view = view.clone();
        let tick_tx = event_mode.then_some(tick_tx);
        Arc::new(move |t| {
            if let searcher_market::hot::HotTick::Pool { dex, mid, received, .. } = &t {
                probe.on_pool(dex, *mid, *received);
                // USD valuation fallback (reference, like Price API v3 was):
                // only when no executable SOL price is fresher than 60 s
                if tick_tx.is_some() && view.sol_price(Ts::now(), 60_000).is_none() {
                    view.set_sol_price(
                        searcher_core::UsdPrice::new((*mid * 1e6).round() as u64),
                        Ts::now(),
                        "on-chain pool mid (reference)",
                    );
                }
            }
            if let Some(tx) = &tick_tx
                && tx.try_send(t).is_err()
            {
                book.count("router.ticks_dropped", 1);
            }
        })
    };

    // Oracle delivery: Hermes stream when configured with a key, otherwise the
    // on-chain Pyth accounts (both produce the same OracleUpdate ticks).
    let hermes_key = std::env::var(&cfg.feeds.pyth_api_key_env).ok().filter(|k| !k.trim().is_empty());
    let use_hermes = cfg.feeds.enabled && cfg.feeds.oracle_source == OracleSourceKind::Hermes && hermes_key.is_some();
    if cfg.feeds.oracle_source == OracleSourceKind::Hermes && hermes_key.is_none() {
        bus.emit(Event::Log {
            ts: Ts::now(),
            level: LogLevel::Warn,
            message: format!(
                "oracle_source = hermes but {} is not set: using on-chain Pyth accounts",
                cfg.feeds.pyth_api_key_env
            ),
        });
    }
    // Chain feeds: one WebSocket for slots + watched pool/oracle accounts.
    let watches = if cfg.feeds.enabled {
        let mut feeds = cfg.feeds.clone();
        if use_hermes {
            feeds.oracles.clear(); // delivered by Hermes instead
        }
        feed::watches_from_config(&feeds, &cfg.tokens()).map_err(anyhow::Error::msg)?
    } else {
        Vec::new()
    };
    if let (true, Some(key)) = (use_hermes, hermes_key) {
        let feeds = cfg
            .feeds
            .oracles
            .iter()
            .filter_map(|o| Some((Arc::from(o.symbol.as_str()), searcher_core::config::parse_feed_id(&o.feed_id)?)))
            .collect();
        tasks.push(tokio::spawn(searcher_market::hermes::run_hermes(
            searcher_market::hermes::HermesConfig {
                base_url: cfg.feeds.hermes_url.clone(),
                api_key: key,
                feeds,
                emit_interval: Duration::from_millis(cfg.feeds.emit_interval_ms),
            },
            hot.clone(),
            emit.clone(),
            shutdown_rx.clone(),
        )));
    }
    let fee_accounts: Vec<Address> =
        watches.iter().filter(|w| matches!(w, feed::Watch::Pool { .. })).map(feed::Watch::address).collect();
    tasks.push(tokio::spawn(feed::run_chain_ws(
        ws_url,
        feed::AccountFeed::new(watches, Duration::from_millis(cfg.feeds.emit_interval_ms))
            .with_latency(telemetry.latency.clone()),
        rpc.clone(),
        chain.clone(),
        telemetry.clone(),
        emit.clone(),
        Some(hot),
        shutdown_rx.clone(),
    )));
    if !fee_accounts.is_empty() {
        tasks.push(tokio::spawn(feed::run_network_poller(
            rpc.clone(),
            fee_accounts,
            emit.clone(),
            Duration::from_millis(cfg.feeds.network_poll_ms),
            shutdown_rx.clone(),
        )));
    }
    tasks.push(tokio::spawn(feed::run_block_height_poller(
        rpc.clone(),
        chain.clone(),
        emit.clone(),
        Duration::from_secs(3),
        shutdown_rx.clone(),
    )));

    // Jito tip stream (push); the REST tip floor below is the fallback.
    let tip_stream_at = Arc::new(std::sync::atomic::AtomicI64::new(0));
    if cfg.feeds.enabled && !cfg.feeds.tip_stream_url.is_empty() {
        let view = view.clone();
        let bus = bus.clone();
        let at = tip_stream_at.clone();
        tasks.push(tokio::spawn(feed::run_text_stream(
            cfg.feeds.tip_stream_url.clone(),
            Duration::from_secs(120),
            move |text| {
                let now = Ts::now();
                if let Ok(v) = serde_json::from_str(text)
                    && let Ok(tf) = searcher_jito::client::parse_tip_floor(&v, now)
                {
                    at.store(now.0, std::sync::atomic::Ordering::Relaxed);
                    view.set_tip_floor(tf.clone());
                    bus.emit(Event::TipFloor(tf));
                }
            },
            shutdown_rx.clone(),
        )));
    }

    // Jito: tip accounts (refreshed), tip floor periodically ("tip if sent now").
    {
        let jito = jito.clone();
        let tip_stream_at = tip_stream_at.clone();
        let view = view.clone();
        let bus = bus.clone();
        let every = Duration::from_millis(cfg.jito.tip_floor_refresh_ms.max(5_000));
        let accounts_every = Duration::from_millis(cfg.jito.tip_accounts_refresh_ms.max(60_000));
        let mut sd = shutdown_rx.clone();
        tasks.push(tokio::spawn(async move {
            let mut floor_tick = tokio::time::interval(every);
            let mut accounts_tick = tokio::time::interval(accounts_every);
            let mut have_accounts = false;
            loop {
                tokio::select! {
                    _ = sd.changed() => return,
                    _ = accounts_tick.tick() => match jito.get_tip_accounts().await {
                        Ok(v) => { view.set_tip_accounts(v); have_accounts = true; }
                        Err(e) if !have_accounts => {
                            bus.emit(Event::Error { ts: Ts::now(), service: "jito".into(), message: format!("getTipAccounts: {e} (using verified fallback list)") });
                            view.set_tip_accounts(searcher_jito::KNOWN_TIP_ACCOUNTS.iter().filter_map(|a| a.parse().ok()).collect());
                        }
                        Err(_) => {} // keep the last good list
                    },
                    _ = floor_tick.tick() => {
                        // the stream publishes about every 30 s; poll only when it is quiet
                        let stream_age = Ts::now().0 - tip_stream_at.load(std::sync::atomic::Ordering::Relaxed);
                        if stream_age < 45_000_000 {
                            continue;
                        }
                        if let Ok(tf) = jito.tip_floor().await {
                            view.set_tip_floor(tf.clone());
                            bus.emit(Event::TipFloor(tf));
                        }
                    }
                }
            }
        }));
    }

    // Reference price (Price API v3; non-executable, labelled as such). In
    // event mode it would spend the scarce Jupiter budget on a reference the
    // on-chain pool mids already give, so it is off there.
    if cfg.jupiter.price_refresh_ms > 0 && !event_mode {
        let jupiter = jupiter.clone();
        let view = view.clone();
        let bus = bus.clone();
        let sol = cfg.tokens().sol().mint;
        let every = Duration::from_millis(cfg.jupiter.price_refresh_ms.max(10_000));
        let mut sd = shutdown_rx.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tokio::select! {
                    _ = sd.changed() => return,
                    _ = tick.tick() => {
                        if let Ok(p) = jupiter.prices(&[sol]).await && let Some(px) = p.get(&sol) {
                            let now = Ts::now();
                            if view.sol_price(now, 60_000).is_none() {
                                view.set_sol_price(*px, now, "jupiter price v3");
                            }
                            bus.emit(Event::Sample(searcher_core::MarketSample {
                                ts: now, pair: "SOL/USD".into(), price: *px,
                                side: searcher_core::SampleSide::Reference, source: "jupiter price v3 (reference)".into(), size_atoms: 0,
                            }));
                        }
                    }
                }
            }
        }));
    }

    // Health + equity snapshots.
    {
        let telemetry = telemetry.clone();
        let bus = bus.clone();
        let view = view.clone();
        let rpc = rpc.clone();
        let wallet_pk: Option<Address> =
            if mode.sends_transactions() { taker } else { cfg.wallet.pubkey.as_deref().and_then(|p| p.parse().ok()) };
        let paper_equity = cfg.paper.equity_lamports;
        let usdc_mint = cfg.tokens().get("USDC").map(|t| t.mint).unwrap_or_default();
        let max_trade = cfg.risk.max_trade_lamports;
        let tolerance = cfg.jupiter.tolerance_bps(cfg.risk.max_slippage_bps);
        let mut last_low: Option<std::time::Instant> = None;
        let mut sd = shutdown_rx.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut n: u64 = 0;
            loop {
                tokio::select! {
                    _ = sd.changed() => return,
                    _ = tick.tick() => {
                        n += 1;
                        let now = Ts::now();
                        for s in ServiceId::ALL {
                            bus.emit(Event::Health { ts: now, service: s, snapshot: telemetry.snapshot(s, now) });
                        }
                        if n % 5 == 1 {
                            match (mode, wallet_pk) {
                                (Mode::Paper, _) => {
                                    let l = (paper_equity as i64 + view.paper_net()).max(0) as u64;
                                    bus.emit(Event::Equity { ts: now, lamports: l, source: "paper notional".into() });
                                }
                                (_, Some(pk)) => {
                                    if let Ok(b) = rpc.get_balance(&pk).await {
                                        view.set_wallet_lamports(b, now);
                                        bus.emit(Event::Equity { ts: now, lamports: b, source: "wallet SOL".into() });
                                        // inventory ledger: every minute
                                        if n % 30 == 1 {
                                            let usdc = rpc.token_balance(&pk, &usdc_mint).await.ok();
                                            let px = view.sol_price(now, 60_000);
                                            bus.emit(Event::Inventory {
                                                ts: now,
                                                sol_lamports: b,
                                                usdc_atoms: usdc,
                                                sol_usd_micros: px.map(|p| p.micros_per_token),
                                            });
                                            // the next leg's input is fixed: USDC covers first-leg shortfalls
                                            let k = usdc.zip(px).and_then(|(u, p)| {
                                                searcher_core::costs::inventory_coverage(u, max_trade, p.f64(), tolerance)
                                            });
                                            if let Some(k) = k.filter(|k| *k < searcher_core::costs::INVENTORY_MIN_COVERAGE)
                                                && last_low.is_none_or(|t: std::time::Instant| t.elapsed() > Duration::from_secs(600))
                                            {
                                                last_low = Some(std::time::Instant::now());
                                                bus.emit(Event::Log {
                                                    ts: now,
                                                    level: LogLevel::Warn,
                                                    message: format!(
                                                        "USDC inventory ${:.2} covers only ~{k:.0} worst-case leg shortfalls: add USDC or trades will be refused (INVENTORY_LOW)",
                                                        usdc.unwrap_or(0) as f64 / 1e6
                                                    ),
                                                });
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }));
    }

    // Token-account rent as the chain charges it now (it changes: 2,039,280
    // lamports until rent was lowered, 1,488,440 on 2026-09-22).
    let mut cost_params = cfg.profit.cost_params();
    match rpc.minimum_balance_for_rent_exemption(165).await {
        Ok(r) => cost_params.token_account_rent = r,
        Err(e) => bus.emit(Event::Log {
            ts: Ts::now(),
            level: LogLevel::Warn,
            message: format!(
                "token account rent not read from the chain ({e}); using {} lamports",
                cost_params.token_account_rent
            ),
        }),
    }

    // Pipeline.
    let pcfg = PipelineConfig {
        mode,
        live_enabled: cfg.execution.live_enabled,
        taker,
        slippage: cfg.jupiter.slippage_spec().map_err(anyhow::Error::msg)?,
        cu_price_percentile: cfg.jupiter.compute_unit_price_percentile.clone(),
        blockhash_slots_to_expiry: cfg.jupiter.blockhash_slots_to_expiry,
        for_jito_bundle: cfg.jupiter.for_jito_bundle,
        cost_params,
        guards: cfg.profit.guards().map_err(anyhow::Error::msg)?,
        protect_min_out: cfg.profit.protect_min_out,
        prefer_single_tx: cfg.execution.prefer_single_tx,
        max_quote_age_ms: cfg.risk.max_quote_age_ms,
        simulate_unprofitable: true,
        confirm_timeout: Duration::from_millis(cfg.execution.confirm_timeout_ms),
        dont_front: cfg.jito.dont_front,
        paper_equity_lamports: cfg.paper.equity_lamports,
        live: LiveParams {
            min_wallet_lamports: cfg.risk.min_wallet_sol_for_fees_lamports,
            blockhash_margin: 10,
            poll: Duration::from_millis(cfg.execution.bundle_status_poll_ms),
            timeout: Duration::from_millis(cfg.execution.bundle_timeout_ms),
        },
    };
    let pipeline = Arc::new(Pipeline::new(
        pcfg,
        cfg.tokens(),
        jupiter.clone(),
        rpc.clone(),
        risk.clone(),
        chain.clone(),
        bus.clone(),
        view.clone(),
        TipPolicy::new(cfg.jito.tip_policy.clone()),
        live,
        telemetry.clone(),
        probe.clone(),
    ));
    let scheduler = Scheduler::new(build_strategies(&cfg)?);
    if scheduler.is_empty() {
        bail!("no enabled strategies");
    }
    bus.emit(Event::Log {
        ts: Ts::now(),
        level: LogLevel::Info,
        message: format!("strategies: {}", scheduler.names().join(" · ")),
    });
    let (sim_tx, sim_rx) = mpsc::channel(2);
    if event_mode {
        let sc = &cfg.scheduler;
        let rcfg = searcher_execution::router::RouterConfig {
            max_in_flight: sc.max_in_flight,
            reserve: sc.reserve,
            change_bp: sc.change_bp,
            leg_ttl: Duration::from_millis(sc.leg_ttl_ms),
            unobservable_ttl: Duration::from_millis(sc.unobservable_ttl_ms),
            floor: Duration::from_secs(sc.floor_s),
        };
        let tokens = cfg.tokens();
        let (sol, usdc) = (tokens.sol().mint, tokens.get("USDC").map(|t| t.mint).unwrap_or_default());
        let oracle_symbols: std::collections::HashSet<String> =
            cfg.feeds.oracles.iter().map(|o| o.symbol.clone()).collect();
        let (p1, p2, probe2) = (pipeline.clone(), pipeline.clone(), probe.clone());
        let core = searcher_execution::router::Core::new(
            rcfg,
            scheduler.universe(),
            |l| probe2.observes(&l.input, &l.output, &l.dex_filter),
            |m| {
                // tokens the pools do not price are watched through their oracle
                if *m == sol || *m == usdc {
                    return None;
                }
                let sym = format!("{}/USD", tokens.by_mint(m)?.symbol);
                oracle_symbols.contains(&sym).then(|| Arc::from(sym.as_str()))
            },
            Arc::new(move || p1.new_id()),
            Arc::new(move |spec, amount| p2.request_for(spec, amount)),
            telemetry.latency.clone(),
        );
        bus.emit(Event::Log {
            ts: Ts::now(),
            level: LogLevel::Info,
            message: format!(
                "scheduler: event-driven · {} routes · Jupiter window {}/{} s (learned at runtime)",
                core.routes(),
                sc.window_capacity,
                sc.window_ms / 1000
            ),
        });
        tasks.push(tokio::spawn(searcher_execution::router::run(
            core,
            pipeline.clone(),
            jupiter.clone(),
            tick_rx,
            sim_tx,
            shutdown_rx.clone(),
        )));
    } else {
        drop(tick_rx);
        tasks.push(tokio::spawn(pipeline.clone().run_scanner(scheduler, sim_tx, shutdown_rx.clone())));
    }
    tasks.push(tokio::spawn(pipeline.clone().run_simulator(sim_rx, shutdown_rx.clone())));

    // Operator commands (TUI → engine).
    bus.emit(Event::Thresholds {
        ts: Ts::now(),
        values: searcher_core::thresholds::values(&cfg),
        loss_possible: searcher_core::thresholds::loss_possible(&cfg),
    });
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
    {
        let bus = bus.clone();
        let kill = kill.clone();
        let pipeline = pipeline.clone();
        let risk = risk.clone();
        let shutdown_tx = shutdown_tx.clone();
        let mut current = cfg.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(c) = cmd_rx.recv().await {
                match c {
                    Command::SetThresholds { changes, allow_loss } => {
                        let log = |level, message: String| bus.emit(Event::Log { ts: Ts::now(), level, message });
                        match set_thresholds(&current, &changes, allow_loss, &pipeline, &risk, user_config.as_deref()) {
                            Ok((next, lines)) => {
                                for l in lines {
                                    log(LogLevel::Warn, l);
                                }
                                bus.emit(Event::Thresholds {
                                    ts: Ts::now(),
                                    values: searcher_core::thresholds::values(&next),
                                    loss_possible: searcher_core::thresholds::loss_possible(&next),
                                });
                                current = next;
                            }
                            Err(e) => log(LogLevel::Warn, format!("threshold change refused: {e}")),
                        }
                    }
                    Command::KillSwitch { engage, reason } => {
                        let changed = if engage { kill.engage(&reason) } else { kill.release() };
                        if changed {
                            bus.emit(Event::KillSwitch { ts: Ts::now(), engaged: engage, reason });
                        }
                    }
                    Command::Confirm { opportunity, approve } => {
                        pipeline.confirms.resolve(opportunity, approve);
                    }
                    Command::Shutdown => {
                        let _ = shutdown_tx.send(true);
                    }
                }
            }
        }));
    }

    Ok(Running { session_id, telemetry, probe, vm, commands: cmd_tx, shutdown_tx, bus, recorder, tasks, db_path })
}

/// Validate and apply an operator threshold change: live (pipeline, risk
/// engine), then in the user's config file. Returns the new configuration
/// and the log lines describing the change.
fn set_thresholds(
    current: &Config,
    changes: &[(String, String)],
    allow_loss: bool,
    pipeline: &Pipeline,
    risk: &RiskEngine,
    user_config: Option<&std::path::Path>,
) -> Result<(Config, Vec<String>)> {
    let change = searcher_core::thresholds::apply(current, changes).map_err(anyhow::Error::msg)?;
    if change.opens_loss && !allow_loss {
        bail!("these settings let a landed trade lose money; type ALLOW LOSS to confirm");
    }
    let c = &change.config;
    let live = pipeline.thresholds();
    let mut cost_params = c.profit.cost_params();
    cost_params.token_account_rent = live.cost_params.token_account_rent; // read from the chain at start
    pipeline.set_thresholds(searcher_execution::pipeline::Thresholds {
        guards: c.profit.guards().map_err(anyhow::Error::msg)?,
        cost_params,
        protect_min_out: c.profit.protect_min_out,
        slippage: c.jupiter.slippage_spec().map_err(anyhow::Error::msg)?,
    });
    risk.set_limits(RiskLimits::from_config(&c.risk).map_err(anyhow::Error::msg)?);
    let mut lines: Vec<String> =
        change.diff.iter().map(|(k, old, new)| format!("threshold {k}: {old} → {new} (operator)")).collect();
    if change.opens_loss {
        lines.push("ALLOW LOSS acknowledged: landed trades can now lose money".into());
    }
    match user_config {
        Some(path) => match crate::thresholds_file::write(path, &change.values) {
            Ok(bak) => lines.push(format!(
                "saved to {}{}",
                path.display(),
                bak.map(|b| format!(" (previous file: {})", b.display())).unwrap_or_default()
            )),
            Err(e) => lines.push(format!("NOT saved ({e:#}); the change lasts for this session only")),
        },
        None => lines.push("not saved (no user config file); the change lasts for this session only".into()),
    }
    Ok((change.config, lines))
}

impl Running {
    /// Stop tasks, flush the recorder, close the session.
    pub async fn stop(self) -> searcher_storage::RecorderStats {
        let _ = self.shutdown_tx.send(true);
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
        drop(self.bus); // last storage sender → recorder drains and ends the session
        tokio::task::spawn_blocking(move || self.recorder.join().unwrap_or_default()).await.unwrap_or_default()
    }
}
