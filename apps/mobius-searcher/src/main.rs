//! `mobius-searcher` — MØBIUS-Searcher, a Solana/Jupiter arbitrage searcher. PAPER by default.
//!
//!   mobius                         run with TUI (config layers: see `--print-config`)
//!   mobius-searcher --doctor                check config, secrets and every endpoint
//!   mobius-searcher --headless --duration 3600
//!   mobius-searcher --list-sessions
//!   mobius-searcher --replay <SESSION>      same TUI over a recorded session
//!   mobius-searcher --report <SESSION>      paper-run statistics
//!   mobius-searcher --replay <SESSION> --snapshot 120x40,80x24 --out shots/
//!   mobius-searcher --research [--duration N]   measurements only (research.sqlite)
//!   mobius-searcher --research-report [RUN|latest|all]

use mobius_searcher::{budget, canary, doctor, engine, envfile, research, setup};

use anyhow::{Context, Result, bail};
use clap::Parser;
use parking_lot::RwLock;
use searcher_core::config::{self, ColorMode, Config, GlyphMode, Layered};
use searcher_core::model::Mode;
use searcher_storage::Store;
use searcher_tui::{App, TuiOptions, ViewModel, default_graphs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "mobius-searcher",
    version,
    about = "MØBIUS-Searcher — Solana/Jupiter arbitrage searcher (PAPER by default)"
)]
struct Cli {
    /// Run the setup wizard (first use, or review and change settings) and exit.
    #[arg(long, alias = "init")]
    setup: bool,
    #[command(flatten)]
    setup_opts: setup::SetupOpts,
    /// Skip the automatic first-use wizard for this launch.
    #[arg(long)]
    skip_setup: bool,
    /// Your config file, layered over config/mobius.toml (default:
    /// ~/.config/mobius/config.toml, or $MOBIUS_CONFIG).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the effective configuration with the layer each value comes from.
    #[arg(long)]
    print_config: bool,
    /// Check the configuration, secrets (names only) and every endpoint, then exit.
    #[arg(long)]
    doctor: bool,
    /// Override mode: paper | confirm | live (confirm/live still require execution.live_enabled).
    #[arg(long)]
    mode: Option<String>,
    /// SQLite database path (default: <data dir>/mobius.sqlite; data dir =
    /// general.data_dir, else ~/.local/share/mobius).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Run without the TUI (status lines on stdout).
    #[arg(long)]
    headless: bool,
    /// Stop after N seconds (soak runs).
    #[arg(long)]
    duration: Option<u64>,
    /// Replay a recorded session in the TUI.
    #[arg(long, value_name = "SESSION_ID")]
    replay: Option<String>,
    /// Print the paper-run report of a session (`latest` = most recent).
    #[arg(long, value_name = "SESSION_ID")]
    report: Option<String>,
    /// Validate a private key file offline and print its public wallet address.
    #[arg(long, value_name = "PATH")]
    check_wallet: Option<PathBuf>,
    /// One real trade through the LIVE path to prove it end to end: CONFIRM
    /// (approve with y), one SOL→USDC→SOL route, loss ≤ canary.max_loss_lamports
    /// (also on chain), then an account-by-account reconciliation.
    #[arg(long)]
    canary: bool,
    /// Research mode: size ladder, cross-chain spreads and DEX lag, recorded to
    /// <data dir>/research.sqlite. Holds the Jupiter budget; signs and sends nothing.
    #[arg(long)]
    research: bool,
    /// Summarise research runs: a run id, `latest` or `all` (default: all).
    #[arg(long, value_name = "RUN", num_args = 0..=1, default_missing_value = "all")]
    research_report: Option<String>,
    /// Price MARKET (e.g. WETH/USDC, SOL/USDT) on every enabled venue that lists it, then exit.
    #[arg(long, value_name = "MARKET")]
    quote: Option<String>,
    /// Base amount for --quote.
    #[arg(long, default_value_t = 1.0)]
    size: f64,
    /// Report as JSON.
    #[arg(long)]
    json: bool,
    #[arg(long)]
    list_sessions: bool,
    /// Apply the [storage] retention policy to the database now, then exit.
    #[arg(long)]
    prune: bool,
    /// Print database size, sessions and retention state, then exit.
    #[arg(long)]
    db_info: bool,
    /// Render frames of a replayed session: e.g. `120x40,80x24`.
    #[arg(long)]
    snapshot: Option<String>,
    /// Pages to snapshot (digits 1-8). Empty: one frame after `--keys`, named by `--shot-name`.
    #[arg(long, default_value = "12345678")]
    pages: String,
    #[arg(long, default_value = "view")]
    shot_name: String,
    #[arg(long, default_value = "snapshots")]
    out: PathBuf,
    /// ASCII glyphs only.
    #[arg(long)]
    ascii: bool,
    /// Disable colors.
    #[arg(long)]
    no_color: bool,
    /// Do not capture the mouse (keeps plain drag-to-select text).
    #[arg(long)]
    no_mouse: bool,
    /// Override the Jupiter request scheduler: event | round_robin (A/B benchmarks).
    #[arg(long)]
    scheduler: Option<String>,
    /// Glyph set: auto | unicode | ascii (overrides config).
    #[arg(long)]
    glyphs: Option<String>,
    /// Color depth: auto | truecolor | ansi256 | none (overrides config).
    #[arg(long)]
    color: Option<String>,
    /// Key script applied at TUI start, e.g. `4a<left*30>b` (replay/snapshots).
    #[arg(long, default_value = "")]
    keys: String,
}

/// The user layer: `--config`, else `$MOBIUS_CONFIG`, else ~/.config/mobius/config.toml.
fn user_config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(config::user_config_path)
}

fn load_config(cli: &Cli) -> Result<Layered> {
    let user = user_config_path(cli);
    if cli.config.is_some() && !user.exists() {
        bail!("--config {}: no such file", user.display());
    }
    let mut layered = config::load_layered(std::path::Path::new(config::REPO_CONFIG_PATH), &user)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cfg = &mut layered.config;
    if let Some(m) = &cli.mode {
        cfg.general.mode = match m.as_str() {
            "paper" => Mode::Paper,
            "confirm" => Mode::Confirm,
            "live" => Mode::Live,
            other => bail!("unknown mode `{other}`"),
        };
        cfg.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(g) = &cli.glyphs {
        cfg.ui.glyphs = match g.as_str() {
            "auto" => GlyphMode::Auto,
            "unicode" => GlyphMode::Unicode,
            "ascii" => GlyphMode::Ascii,
            other => bail!("unknown glyph mode `{other}`"),
        };
    }
    if let Some(c) = &cli.color {
        cfg.ui.color = match c.as_str() {
            "auto" => ColorMode::Auto,
            "truecolor" => ColorMode::Truecolor,
            "ansi256" => ColorMode::Ansi256,
            "none" => ColorMode::None,
            other => bail!("unknown color mode `{other}`"),
        };
    }
    if cli.ascii {
        cfg.ui.glyphs = GlyphMode::Ascii;
    }
    if cli.no_color {
        cfg.ui.color = ColorMode::None;
    }
    if cli.no_mouse {
        cfg.ui.mouse = false;
    }
    if let Some(k) = &cli.scheduler {
        cfg.scheduler.kind = match k.as_str() {
            "event" => searcher_core::config::SchedulerKind::Event,
            "round_robin" | "round-robin" => searcher_core::config::SchedulerKind::RoundRobin,
            other => anyhow::bail!("--scheduler must be event or round_robin, got {other}"),
        };
    }
    Ok(layered)
}

/// Every effective value with its layer. URLs show only scheme and host;
/// the keypair path is not printed.
fn print_config(l: &Layered, env_files: &[(PathBuf, Vec<String>)]) {
    println!("# layers (later wins): default → repo → user");
    for (layer, path) in &l.files {
        println!("#   {:<5} {}", layer.label(), path.display());
    }
    println!("# secrets are read from: process env → {}", {
        let f: Vec<String> = env_files.iter().map(|(p, _)| p.display().to_string()).collect();
        f.join(" → ")
    });
    let mut section = String::new();
    let mut entries = l.entries();
    // a table's own keys before its sub-tables, so the output stays valid TOML
    entries.sort_by(|a, b| a.0.rsplit_once('.').map(|x| x.0).cmp(&b.0.rsplit_once('.').map(|x| x.0)));
    for (key, value, layer) in entries {
        let (sec, leaf) = key.rsplit_once('.').unwrap_or(("", key.as_str()));
        if sec != section {
            println!("\n[{sec}]");
            section = sec.to_string();
        }
        let shown = match &value {
            toml::Value::String(v) if leaf.ends_with("url") => format!("\"{}\"", config::display_url(v)),
            _ if leaf == "keypair_path" => "\"(set)\"".into(),
            v => v.to_string(),
        };
        let mark = if layer == config::Layer::Default { String::new() } else { format!("  # {}", layer.label()) };
        println!("{leaf} = {shown}{mark}");
    }
}

/// TUI options; `replay` picks start graphs from what the session recorded.
fn tui_opts(cfg: &Config, replay: Option<&ViewModel>) -> TuiOptions {
    TuiOptions {
        glyphs: cfg.ui.glyphs,
        color: cfg.ui.color,
        fps: cfg.ui.fps,
        max_graphs: cfg.ui.max_graphs,
        graphs: default_graphs(replay, cfg.feeds.enabled),
        mouse: cfg.ui.mouse,
        // the Markets page reads the first enabled OKX venue
        okx: cfg
            .venues
            .values()
            .find(|v| v.enabled && matches!(v.kind, searcher_core::config::VenueKind::Okx) && !v.markets.is_empty())
            .map(|v| searcher_tui::cex::OkxSource {
                rest_url: v.rest_url.clone(),
                markets: v.markets.clone(),
                watchlist: v.watchlist.clone(),
            }),
    }
}

fn resolve_session(store: &Store, id: &str) -> Result<String> {
    if id == "latest" {
        return store.list_sessions()?.first().map(|s| s.id.clone()).context("no sessions recorded");
    }
    Ok(id.to_string())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = user_config_path(&cli);
    let env_path = config::user_env_path();
    let marker_path = config::setup_marker_path();
    let ordinary_tui_run = !cli.headless
        && cli.duration.is_none()
        && cli.replay.is_none()
        && cli.report.is_none()
        && cli.check_wallet.is_none()
        && !cli.print_config
        && !cli.doctor
        && cli.quote.is_none()
        && !cli.research
        && !cli.canary
        && cli.research_report.is_none()
        && !cli.list_sessions
        && !cli.prune
        && !cli.db_info
        && cli.snapshot.is_none();
    let auto_setup = !cli.skip_setup && setup::should_auto_run(&marker_path, &config_path, ordinary_tui_run);
    if cli.setup || auto_setup {
        let outcome = match setup::run(&config_path, &env_path, &marker_path, cli.setup, &cli.setup_opts) {
            Ok(outcome) => outcome,
            Err(error) if setup::is_cancelled(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        if cli.setup || !outcome.start_now {
            return Ok(());
        }
    }
    // secrets: process env, then the user's .env, then ./.env (first one wins)
    let env_files: Vec<(PathBuf, Vec<String>)> =
        envfile::sources().into_iter().map(|p| (p.clone(), envfile::load(&p))).collect();
    let layered = load_config(&cli)?;
    let cfg = layered.config.clone();
    searcher_telemetry::proxy::configure(
        searcher_telemetry::proxy::ProxySetting::parse(&cfg.network.proxy).map_err(anyhow::Error::msg)?,
    );
    if cli.print_config {
        print_config(&layered, &env_files);
        return Ok(());
    }
    if let Some(market) = &cli.quote {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return rt.block_on(mobius_searcher::quote::run(&cfg, market, cli.size));
    }
    if cli.doctor {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let ok = rt.block_on(doctor::run(&layered, &env_files, cli.json));
        std::process::exit(if ok { 0 } else { 1 });
    }
    if let Some(which) = &cli.research_report {
        let store = searcher_storage::ResearchStore::open(&cfg.data_dir().join("research.sqlite"))?;
        let all: Vec<String> = store.runs()?.into_iter().map(|r| r.id).collect();
        let runs = match which.as_str() {
            "all" => all,
            "latest" => all.into_iter().take(1).collect(),
            id => vec![id.to_string()],
        };
        if runs.is_empty() {
            bail!("no research runs recorded yet (start one with --research)");
        }
        let r = research::report::build(&store, &runs)?;
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            print!("{}", research::report::render(&r));
        }
        return Ok(());
    }
    if cli.research {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return rt.block_on(research::run(cfg, cli.duration));
    }
    let db_path = cli.db.clone().unwrap_or_else(|| cfg.data_dir().join("mobius.sqlite"));

    if let Some(path) = &cli.check_wallet {
        let expected = cfg
            .wallet
            .pubkey
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|e| anyhow::anyhow!("configured wallet.pubkey: {e}"))?;
        let wallet = searcher_execution::Wallet::load(path, expected.as_ref()).context("checking wallet")?;
        println!("wallet OK: {}", wallet.pubkey());
        return Ok(());
    }

    if cli.db_info {
        println!("{}", Store::open(&db_path)?.db_info(&db_path)?);
        return Ok(());
    }
    if cli.prune {
        let mut store = Store::open(&db_path)?;
        let report = store.prune(&engine::retention(&cfg), searcher_core::Ts::now(), None)?;
        println!("{report}");
        if !store.db_info(&db_path)?.incremental_vacuum {
            // older file layout: space only comes back with a full VACUUM
            // (needs about as much free disk as the database)
            store.vacuum()?;
        }
        return Ok(());
    }
    if cli.list_sessions {
        let store = Store::open(&db_path)?;
        println!("{:<24} {:<8} {:>9} {:>8} {:>7}  STARTED → ENDED", "SESSION", "MODE", "EVENTS", "OPPS", "DROPPED");
        for s in store.list_sessions()? {
            println!(
                "{:<24} {:<8} {:>9} {:>8} {:>7}  {} → {}",
                s.id,
                s.mode,
                s.events,
                s.opportunities,
                s.dropped,
                s.started_at.format("%Y-%m-%d %H:%M:%S"),
                s.ended_at.map(|t| t.format("%H:%M:%S")).unwrap_or_else(|| "(open)".into())
            );
        }
        return Ok(());
    }
    if let Some(id) = &cli.report {
        let store = Store::open(&db_path)?;
        let id = resolve_session(&store, id)?;
        let r = searcher_storage::build_report(&store, &id)?;
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            print!("{}", searcher_storage::render_report(&r));
        }
        return Ok(());
    }
    if let Some(id) = &cli.replay {
        return replay(&cli, &cfg, &db_path, id);
    }

    for (path, names) in &env_files {
        if !names.is_empty() {
            eprintln!("loaded from {}: {}", path.display(), names.join(", ")); // names only, never values
        }
    }
    let cfg = if cli.canary {
        if cli.headless {
            bail!("--canary needs the TUI: sending is approved with a keypress (y)");
        }
        let c = canary::prepare(&cfg)?;
        println!(
            "CANARY · one SOL→USDC→SOL round trip of {} SOL through the LIVE path\n  \
             each candidate waits for y (n declines) · loss bound {} lamports (also the on-chain min-out)\n  \
             after the first landed trade the session ends and the transaction is reconciled",
            c.strategies.round_trip[0].amount_lamports as f64 / 1e9,
            c.canary.max_loss_lamports
        );
        c
    } else {
        cfg
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(cli, cfg, db_path))
}

fn replay(cli: &Cli, cfg: &Config, db_path: &std::path::Path, id: &str) -> Result<()> {
    let store = Store::open(db_path)?;
    let id = resolve_session(&store, id)?;
    let events = store.load_events(&id)?;
    let mut vm = ViewModel::new(true);
    for e in &events {
        vm.apply(e);
    }
    if let Some(sizes) = &cli.snapshot {
        std::fs::create_dir_all(&cli.out)?;
        for size in sizes.split(',') {
            let (w, h) = size.split_once('x').context("size must be WxH")?;
            let (w, h): (u16, u16) = (w.trim().parse()?, h.trim().parse()?);
            let mut app = App::new(&tui_opts(cfg, Some(&vm)));
            for k in searcher_tui::parse_keys(&cli.keys).map_err(anyhow::Error::msg)? {
                app.on_key(k, &vm);
            }
            let mut frames: Vec<(String, ratatui::buffer::Buffer)> = Vec::new();
            if cli.pages.is_empty() {
                frames.push((format!("{}-{w}x{h}", cli.shot_name), searcher_tui::snapshot(&mut app, &vm, w, h)));
            }
            for p in cli.pages.chars() {
                app.on_key(ratatui_key(p), &vm);
                frames.push((format!("{id}-p{p}-{w}x{h}"), searcher_tui::snapshot(&mut app, &vm, w, h)));
            }
            for (stem, buf) in frames {
                std::fs::write(cli.out.join(format!("{stem}.txt")), searcher_tui::buffer_text(&buf))?;
                std::fs::write(cli.out.join(format!("{stem}.html")), searcher_tui::buffer_html(&buf, &stem))?;
            }
        }
        println!("wrote snapshots of {} events to {}", events.len(), cli.out.display());
        return Ok(());
    }
    let keys = searcher_tui::parse_keys(&cli.keys).map_err(anyhow::Error::msg)?;
    let opts = tui_opts(cfg, Some(&vm));
    let vm = Arc::new(RwLock::new(vm));
    let stop = Arc::new(AtomicBool::new(false));
    searcher_tui::run(vm, Box::new(|_| {}), opts, stop, keys)?;
    Ok(())
}

fn ratatui_key(c: char) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char(c),
        ratatui::crossterm::event::KeyModifiers::NONE,
    )
}

async fn run(cli: Cli, cfg: Config, db_path: PathBuf) -> Result<()> {
    let mode = cfg.general.mode;
    let is_canary = cli.canary;
    let canary_cfg = cfg.clone();
    let _budget = budget::acquire(&cfg.data_dir(), &format!("{} session", mode.label()))?;
    let opts = tui_opts(&cfg, None);
    let user_config = Some(user_config_path(&cli));
    let running = engine::start_with(cfg, db_path, user_config).await?;
    let session = running.session_id.clone();
    let mut shutdown_rx = running.shutdown_tx.subscribe();

    let stop = Arc::new(AtomicBool::new(false));
    let (tui_done_tx, mut tui_done_rx) = tokio::sync::oneshot::channel::<String>();
    let mut headless = cli.headless;
    let tui_thread = if !cli.headless {
        let vm = running.vm.clone();
        let cmds = running.commands.clone();
        let stop = stop.clone();
        Some(std::thread::Builder::new().name("tui".into()).spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                searcher_tui::run(vm, Box::new(move |c| drop(cmds.send(c))), opts, stop, vec![])
            }));
            let msg = match res {
                Ok(Ok(())) => "ok".to_string(),
                Ok(Err(e)) => format!("TUI error: {e}"),
                Err(_) => "TUI panicked".to_string(),
            };
            let _ = tui_done_tx.send(msg);
        })?)
    } else {
        drop(tui_done_tx);
        println!("session {session} · mode {} · headless · Ctrl-C to stop", mode.label());
        None
    };

    let deadline = cli.duration.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut status_tick = tokio::time::interval(Duration::from_secs(15));
    let mut canary_tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = canary_tick.tick(), if is_canary => {
                // the canary ends with its first sent trade (landed or failed)
                let done = running.vm.read().executions.values().any(|x| {
                    matches!(x.state, searcher_core::model::ExecState::Landed { .. } | searcher_core::model::ExecState::Failed { .. })
                });
                if done {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = shutdown_rx.changed() => break,
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
            r = &mut tui_done_rx, if !headless => {
                match r.as_deref() {
                    Ok("ok") | Err(_) => break,
                    Ok(msg) => {
                        // TUI failure must not stop the searcher.
                        eprintln!("{msg}; the searcher keeps running headless (Ctrl-C to stop)");
                        headless = true;
                    }
                }
            }
            _ = status_tick.tick(), if headless => print_status(&running.vm.read()),
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(t) = tui_thread {
        let _ = tokio::task::spawn_blocking(move || t.join()).await;
    }
    let db = running.db_path.clone();
    let landed = if is_canary { canary_landed(&running.vm.read()) } else { None };
    running.probe.finish();
    let latency = running.telemetry.latency.report();
    let stats = running.stop().await;
    println!(
        "session {session} recorded: {} events in {} batches ({} write errors) → {}",
        stats.written,
        stats.batches,
        stats.errors,
        db.display()
    );
    let store = Store::open(&db)?;
    if let Ok(r) = searcher_storage::build_report(&store, &session) {
        print!("\n{}", searcher_storage::render_report(&r));
    }
    if let Some((opp, sig, tip)) = landed {
        canary_report(&canary_cfg, &opp, &sig, tip, db.parent().unwrap_or(std::path::Path::new("."))).await?;
    } else if is_canary {
        println!("\ncanary: no trade landed in this session (nothing to reconcile)");
    }
    // Stage latencies (monotonic clocks) → stdout + data/bench/<session>.json
    print!("\n{}", latency.render());
    let dir = db.parent().unwrap_or(std::path::Path::new(".")).join("bench");
    if std::fs::create_dir_all(&dir).is_ok() {
        let path = dir.join(format!("{session}.json"));
        if let Ok(json) = serde_json::to_string_pretty(&latency) {
            let _ = std::fs::write(&path, json);
            println!("latency report → {}", path.display());
        }
    }
    Ok(())
}

/// The canary's landed trade: (opportunity, first signature, tip).
fn canary_landed(vm: &ViewModel) -> Option<(searcher_core::Opportunity, String, u64)> {
    let x = vm.executions.values().find(|x| matches!(x.state, searcher_core::model::ExecState::Landed { .. }))?;
    Some((vm.opps.get(&x.opportunity)?.clone(), x.signatures.first()?.clone(), x.tip_lamports))
}

/// Fetch the landed transaction (retrying until the node has it), reconcile,
/// print and keep the report under `<data dir>/canary/`.
async fn canary_report(
    cfg: &Config,
    opp: &searcher_core::Opportunity,
    sig: &str,
    tip: u64,
    data: &std::path::Path,
) -> Result<()> {
    let telemetry = Arc::new(searcher_telemetry::Telemetry::new());
    let rpc = searcher_market::RpcClient::new(
        &cfg.rpc.resolved_url(),
        searcher_telemetry::LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
        cfg.rpc.simulate_rps,
        Duration::from_millis(cfg.rpc.timeout_ms),
        telemetry,
    )?;
    let taker = match cfg.wallet.pubkey.as_deref() {
        Some(p) => p.parse().map_err(|e| anyhow::anyhow!("wallet.pubkey: {e}"))?,
        None => {
            let path = cfg.wallet.keypair_path.clone().context("wallet.keypair_path")?;
            searcher_execution::Wallet::load(std::path::Path::new(&path), None)?.pubkey()
        }
    };
    let usdc = cfg.tokens().get("USDC").map(|t| t.mint).context("USDC token")?;
    let mut tx = None;
    for _ in 0..30 {
        if let Some(t) = rpc.get_transaction(sig).await? {
            tx = Some(t);
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let tx = tx.with_context(|| format!("transaction {sig} not available from the RPC after 60 s; reconcile later"))?;
    let r = canary::reconcile(opp, &taker, &usdc, tip, cfg.canary.max_loss_lamports, sig, &tx);
    let text = canary::render(&r);
    print!("\n{text}");
    let dir = data.join("canary");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{sig}.txt")), &text)?;
    std::fs::write(dir.join(format!("{sig}.json")), serde_json::to_string_pretty(&r)?)?;
    println!("report → {}", dir.join(format!("{sig}.txt")).display());
    Ok(())
}

fn print_status(vm: &ViewModel) {
    let jup = vm.health.get(&searcher_core::ServiceId::Jupiter).map(|h| h.state.label()).unwrap_or("-");
    println!(
        "{}  opps {:>5}  executed {:>3}  sims {:>4} ({} failed)  pnl(sim) {}  jupiter {}  429s {}  slot {}",
        searcher_core::Ts::now().hms(),
        vm.total_opps,
        vm.executed,
        vm.sims,
        vm.sim_failed,
        vm.simulated,
        jup,
        vm.rate_limited,
        vm.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into())
    );
}
