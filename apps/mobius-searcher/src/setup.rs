//! First-run setup. Research mode (PAPER) is the recommended path; assisted
//! trading prepares CONFIRM only behind a typed unlock phrase; advanced setup
//! exposes every value. Nothing is written before the final review, and a
//! newly generated bot key stays in memory until then.

use crate::doctor::Check;
use crate::i18n::{self, Lang, tr};
use crate::setup_ui::{self, MenuItem, TextPrompt, Validator};
use anyhow::{Context, Result, bail};
use searcher_core::config::{
    self, ColorMode, Config, CrossDexConfig, GlyphMode, OracleSourceKind, RoundTripConfig, SchedulerKind,
    TipPolicyKind, TriangularConfig,
};
use searcher_core::model::Mode;
use searcher_execution::{GeneratedWallet, Wallet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

const SETUP_VERSION: u32 = 6;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SetupPath {
    Research,
    Assisted,
    Advanced,
}

impl SetupPath {
    fn label(self) -> &'static str {
        match self {
            SetupPath::Research => "Research mode",
            SetupPath::Assisted => "Assisted trading",
            SetupPath::Advanced => "Advanced setup",
        }
    }

    /// (step title, what it decides) in order; the last one is always review.
    fn steps(self) -> &'static [(&'static str, &'static str)] {
        match self {
            SetupPath::Research => &[
                ("Bot wallet", "create one, connect one, watch one or skip"),
                ("Network", "public endpoints or your own providers"),
                ("Strategies", "which opportunities to scan"),
                ("Safety", "route size and stop limits, derived for you"),
                ("Review & save", "nothing is written before this"),
            ],
            SetupPath::Assisted => &[
                ("Bot wallet", "a dedicated signing wallet"),
                ("Network", "your own providers or public endpoints"),
                ("Strategies", "which opportunities to scan"),
                ("Safety", "route size and stop limits, derived for you"),
                ("Permission", "unlock CONFIRM with a typed phrase"),
                ("Review & save", "nothing is written before this"),
            ],
            SetupPath::Advanced => &[
                ("Mode & wallet", "PAPER, CONFIRM or LIVE and the signer"),
                ("Credentials", "API keys and private endpoints"),
                ("Network limits", "endpoints, rates and timeouts"),
                ("Strategies", "routes, quote tokens and amounts"),
                ("Profit guards", "minimum edge after every cost"),
                ("Risk limits", "sizes, fees and stop conditions"),
                ("Jito tips", "tip policy and bounds"),
                ("Feeds & scheduler", "on-chain feeds and quote pacing"),
                ("Interface & execution", "display, storage and timeouts"),
                ("Review & save", "nothing is written before this"),
            ],
        }
    }

    /// Announces step `n` (1-based) of this path.
    fn step(self, n: usize, details: &[&str]) {
        let steps = self.steps();
        step(n, steps.len(), steps[n - 1].0, details);
    }
}

#[derive(Debug)]
struct PendingWallet {
    path: PathBuf,
    wallet: GeneratedWallet,
}

#[derive(Debug, Default)]
struct EnvAnswers {
    jupiter_key: Option<Option<String>>,
    rpc_url: Option<Option<String>>,
    ws_url: Option<Option<String>>,
    jito_uuid: Option<Option<String>>,
    pyth_key: Option<Option<String>>,
}

/// Which credentials `.env` (or the process environment) already holds.
#[derive(Copy, Clone, Debug, Default)]
struct Present {
    jupiter: bool,
    rpc: bool,
    ws: bool,
    jito: bool,
    pyth: bool,
}

pub struct Outcome {
    pub start_now: bool,
}

/// Answers for `--setup --yes`: nothing is asked, for servers, containers and
/// scripts. Secrets are deliberately not flags (they would end up in shell
/// history); pass them as environment variables when MØBIUS runs.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct SetupOpts {
    /// With --setup: take every answer from the flags below, ask nothing.
    /// A first use becomes a PAPER research setup; an existing setup keeps
    /// everything no flag changes.
    #[arg(long, requires = "setup")]
    pub yes: bool,
    /// Bot wallet: new | keep | none | watch:<ADDRESS> | <keypair file>.
    #[arg(long, value_name = "WALLET", requires = "yes")]
    pub wallet: Option<String>,
    /// Which opportunities to scan (default: core on first use, keep otherwise).
    #[arg(long, value_enum, requires = "yes")]
    pub strategies: Option<ScopeArg>,
    /// Starting risk policy (default: guarded on first use, keep otherwise).
    #[arg(long, value_enum, requires = "yes")]
    pub safety: Option<SafetyArg>,
    /// Setup language (default: MOBIUS_LANG, else the system locale).
    #[arg(long, value_enum)]
    pub lang: Option<Lang>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeArg {
    Keep,
    /// Round-trips and cross-DEX price gaps.
    Core,
    /// Also triangular cycles.
    All,
    RoundTrip,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafetyArg {
    Keep,
    /// Routes up to 1% of equity, pause after 3 failures, $2 daily stop.
    Guarded,
    /// Routes up to 2.5% of equity, pause after 5 failures, $5 daily stop.
    Balanced,
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    setup_ui::is_cancelled(error)
}

/// A normal interactive TUI launch gets onboarding once: only on a first
/// use, when there is neither a finished wizard nor a user config. Automation,
/// reports, replay and headless runs never stop for questions.
pub fn should_auto_run(marker: &Path, config: &Path, interactive_run: bool) -> bool {
    interactive_run && io::stdin().is_terminal() && io::stdout().is_terminal() && !marker.exists() && !config.exists()
}

/// The wallet the user already has, offered as "keep" wherever a wallet is chosen.
#[derive(Clone, Debug)]
struct CurrentWallet {
    pubkey: String,
    keypair_path: Option<String>,
}

/// What every step may read but none changes.
struct Ctx {
    /// Credentials already stored somewhere (env files or process env).
    present: Present,
    /// The wallet in the settings setup started from.
    current_wallet: Option<CurrentWallet>,
    /// New bot keypairs go here: `wallets/` next to the user's config file.
    wallet_dir: PathBuf,
}

pub fn run(
    config_path: &Path,
    env_path: &Path,
    marker_path: &Path,
    explicit: bool,
    opts: &SetupOpts,
) -> Result<Outcome> {
    i18n::set(i18n::detect(opts.lang));
    if opts.yes {
        return run_unattended(config_path, env_path, marker_path, opts);
    }
    // The effective settings come first: a file that does not load must be
    // fixed by hand, never silently replaced.
    let layered = load_current(config_path)?;
    let existing = config_path.exists();
    let session = setup_ui::Session::start(existing)?;
    if !session.visual() {
        println!("\nMØBIUS-Searcher setup");
        println!("─────────────────────");
        println!("Nothing is written until you confirm the review at the end.");
    }

    let below = layered.below_user;
    let mut cfg = layered.config;
    let current_wallet =
        cfg.wallet.pubkey.clone().map(|pubkey| CurrentWallet { pubkey, keypair_path: cfg.wallet.keypair_path.clone() });
    let jupiter_env = cfg.jupiter.api_key_env.clone();
    let rpc_env = cfg.rpc.url_env.clone();
    let ws_env = cfg.rpc.ws_url_env.clone();
    let jito_env = cfg.jito.uuid_env.clone();
    let pyth_env = cfg.feeds.pyth_api_key_env.clone();
    let present = detect_present(&cfg, env_path);
    let wallet_dir = wallet_dir(config_path);
    let ctx = Ctx { present, current_wallet, wallet_dir };

    let mut env_answers = EnvAnswers::default();
    let mut pending_wallet = None;
    let mut first_use_path = None;
    if existing {
        note("Current setup", &review_rows(&cfg, &env_answers, present, None, config_path, env_path))?;
        let action = menu(
            "What would you like to do?",
            &[
                ("Keep this setup", "Nothing is changed.", Some("recommended")),
                ("Change some settings", "Pick a part to change; everything else stays exactly as it is.", None),
                ("Start over", "First-use setup from the shared defaults; this file is backed up first.", None),
            ],
            0,
        )?;
        let changed = match action {
            0 => false,
            1 => edit_sections(&mut cfg, &mut env_answers, &mut pending_wallet, &ctx)?,
            _ => {
                cfg = below.clone();
                first_use_path = Some(first_use(&mut cfg, &mut env_answers, &mut pending_wallet, &ctx)?);
                true
            }
        };
        if !changed {
            finish("No changes", "start: mobius-searcher")?;
            return Ok(Outcome { start_now: !explicit });
        }
    } else {
        first_use_path = Some(first_use(&mut cfg, &mut env_answers, &mut pending_wallet, &ctx)?);
    }
    cfg.validate().map_err(anyhow::Error::msg)?;

    match first_use_path {
        Some(path) => {
            path.step(path.steps().len(), &["Check everything once more. Esc still leaves without writing anything."])
        }
        None => {
            step(1, 1, "Review & save", &["Check everything once more. Esc still leaves without writing anything."])
        }
    }
    note("Review", &review_rows(&cfg, &env_answers, present, pending_wallet.as_ref(), config_path, env_path))?;
    if !confirm("Save this setup?", true)? {
        info("Nothing was written.")?;
        return Ok(Outcome { start_now: false });
    }

    let created_wallet = persist_pending_wallet(pending_wallet.as_ref())?;
    if let Err(error) = write_config(config_path, &below, &cfg) {
        if let Some(path) = &created_wallet {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    let secrets = [
        (jupiter_env.as_str(), env_answers.jupiter_key.as_ref()),
        (rpc_env.as_str(), env_answers.rpc_url.as_ref()),
        (ws_env.as_str(), env_answers.ws_url.as_ref()),
        (jito_env.as_str(), env_answers.jito_uuid.as_ref()),
        (pyth_env.as_str(), env_answers.pyth_key.as_ref()),
    ];
    let secrets_changed = secrets.iter().any(|(_, change)| change.is_some());
    if secrets_changed {
        write_env(env_path, &secrets)?;
    }
    write_marker(marker_path)?;
    setup_ui::mark_saved();

    success(&format!("Settings saved to {} (only what differs from the defaults)", display_path(config_path)))?;
    if secrets_changed {
        success(&format!("Secrets saved to {} (0600)", display_path(env_path)))?;
    }
    if let Some(path) = &created_wallet {
        success(&format!("Bot wallet saved to {} (0600)", display_path(path)))?;
        warn("Back up the keypair file before funding it; a lost key cannot be recovered.")?;
    }
    if first_use_path == Some(SetupPath::Assisted) && cfg.general.mode == Mode::Confirm {
        info("Next: fund only the bot wallet, then approve each transaction in CONFIRM mode.")?;
    }

    let mode = cfg.general.mode.label();
    let start_now = !explicit && confirm(&format!("Start MØBIUS in {mode} mode now?"), true)?;
    let hint =
        if start_now { "starting…" } else { "start: mobius-searcher · change later: mobius-searcher --setup" };
    finish("Setup complete", hint)?;
    Ok(Outcome { start_now })
}

fn load_current(config_path: &Path) -> Result<config::Layered> {
    config::load_layered(Path::new(config::REPO_CONFIG_PATH), config_path)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("reading the current settings (fix the file, or move it away to start fresh)")
}

/// New bot keypairs go to `wallets/` next to the user's config file.
fn wallet_dir(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or(Path::new(".")).join("wallets")
}

/// Which credentials are already stored: the setup's own .env, the files the
/// runtime reads, or the process environment.
fn detect_present(cfg: &Config, env_path: &Path) -> Present {
    let env_texts: Vec<String> = std::iter::once(env_path.to_path_buf())
        .chain(crate::envfile::sources())
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    let has = |key: &str| std::env::var_os(key).is_some() || env_texts.iter().any(|text| env_has(text, key));
    Present {
        jupiter: has(&cfg.jupiter.api_key_env),
        rpc: has(&cfg.rpc.url_env),
        ws: has(&cfg.rpc.ws_url_env),
        jito: has(&cfg.jito.uuid_env),
        pyth: has(&cfg.feeds.pyth_api_key_env),
    }
}

/// `--setup --yes`: the same building blocks as the wizard, driven by flags.
/// Errors leave every file as it was.
fn run_unattended(config_path: &Path, env_path: &Path, marker_path: &Path, opts: &SetupOpts) -> Result<Outcome> {
    let layered = load_current(config_path)?;
    let existing = config_path.exists();
    let below = layered.below_user;
    let mut cfg = layered.config;
    let start = snapshot(&cfg);
    if !existing {
        // a first use without questions is always a research (PAPER) setup
        cfg.general.mode = Mode::Paper;
        cfg.execution.live_enabled = false;
    }
    let sends = cfg.general.mode.sends_transactions();
    let mut pending = None;
    match opts.wallet.as_deref().map(str::trim) {
        None | Some("keep") => {}
        Some("new") => {
            let path = next_wallet_path(&wallet_dir(config_path));
            let wallet = GeneratedWallet::new();
            cfg.wallet.pubkey = Some(wallet.pubkey().to_string());
            cfg.wallet.keypair_path = Some(path.display().to_string());
            pending = Some(PendingWallet { path, wallet });
        }
        Some("none") => {
            if sends {
                bail!("--wallet none: {} mode needs a signing wallet", cfg.general.mode.label());
            }
            cfg.wallet.pubkey = None;
            cfg.wallet.keypair_path = None;
        }
        Some(other) if other.starts_with("watch:") => {
            if sends {
                bail!("--wallet {other}: {} mode needs a signing wallet", cfg.general.mode.label());
            }
            let address: searcher_core::Address = other["watch:".len()..]
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("--wallet {other}: not a Solana address: {e}"))?;
            cfg.wallet.pubkey = Some(address.to_string());
            cfg.wallet.keypair_path = None;
        }
        Some(path) => {
            let path = expand_home_path(path);
            let wallet = Wallet::load(&path, None).with_context(|| format!("--wallet {}", path.display()))?;
            cfg.wallet.pubkey = Some(wallet.pubkey().to_string());
            cfg.wallet.keypair_path = Some(path.display().to_string());
        }
    }
    let scope = opts.strategies.unwrap_or(if existing { ScopeArg::Keep } else { ScopeArg::Core });
    match scope {
        ScopeArg::Keep => {}
        ScopeArg::Core => apply_strategy_scope(&mut cfg, 0),
        ScopeArg::All => apply_strategy_scope(&mut cfg, 1),
        ScopeArg::RoundTrip => apply_strategy_scope(&mut cfg, 2),
    }
    let safety = opts.safety.unwrap_or(if existing { SafetyArg::Keep } else { SafetyArg::Guarded });
    match safety {
        SafetyArg::Keep => {}
        SafetyArg::Guarded => apply_safety_policy(&mut cfg, 0),
        SafetyArg::Balanced => apply_safety_policy(&mut cfg, 1),
    }
    cfg.validate().map_err(anyhow::Error::msg)?;

    if existing && snapshot(&cfg) == start && pending.is_none() {
        info(&format!("No changes: {} already has these settings.", display_path(config_path)))?;
        return Ok(Outcome { start_now: false });
    }
    let present = detect_present(&cfg, env_path);
    note("Setup", &review_rows(&cfg, &EnvAnswers::default(), present, pending.as_ref(), config_path, env_path))?;
    let created_wallet = persist_pending_wallet(pending.as_ref())?;
    if let Err(error) = write_config(config_path, &below, &cfg) {
        if let Some(path) = &created_wallet {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    write_marker(marker_path)?;
    success(&format!("Settings saved to {}", display_path(config_path)))?;
    if let Some(path) = &created_wallet {
        success(&format!("Bot wallet saved to {} (0600)", display_path(path)))?;
        warn("Back up the keypair file before funding it; a lost key cannot be recovered.")?;
    }
    Ok(Outcome { start_now: false })
}

/// The first-use flow: pick a goal, see its steps, walk them.
fn first_use(
    cfg: &mut Config,
    env: &mut EnvAnswers,
    pending_wallet: &mut Option<PendingWallet>,
    ctx: &Ctx,
) -> Result<SetupPath> {
    let path = match menu(
        "What should MØBIUS be ready to do?",
        &[
            (
                "Research mode",
                "Watch the market and simulate every route. Sending transactions stays locked.",
                Some("recommended"),
            ),
            ("Assisted trading", "A dedicated bot wallet; every transaction waits for your approval (CONFIRM).", None),
            ("Advanced setup", "Set every provider, strategy, limit and execution option yourself.", None),
        ],
        0,
    )? {
        0 => SetupPath::Research,
        1 => SetupPath::Assisted,
        _ => SetupPath::Advanced,
    };
    let steps = path.steps();
    let keys: Vec<String> =
        steps.iter().enumerate().map(|(i, (title, _))| format!("{:>2}  {}", i + 1, tr(title))).collect();
    let plan: Vec<(&str, String)> =
        keys.iter().zip(steps).map(|(key, (_, what))| (key.as_str(), (*what).to_string())).collect();
    outline(&format!("{} · {} steps", path.label(), steps.len()), &plan)?;

    match path {
        SetupPath::Research | SetupPath::Assisted => configure_guided(cfg, env, pending_wallet, path, ctx)?,
        SetupPath::Advanced => {
            // "keep the current values" should mean the recommended ones
            apply_strategy_scope(cfg, 0);
            apply_safety_policy(cfg, 0);
            configure_advanced(cfg, env, pending_wallet, ctx)?;
        }
    }
    Ok(path)
}

fn review_rows(
    cfg: &Config,
    env: &EnvAnswers,
    present: Present,
    pending: Option<&PendingWallet>,
    config_path: &Path,
    env_path: &Path,
) -> Vec<(&'static str, String)> {
    let mode = mode_summary(cfg);
    let wallet = cfg.wallet.pubkey.clone().unwrap_or_else(|| "none".into());
    let signer = match (&cfg.wallet.keypair_path, pending) {
        (Some(_), Some(p)) => format!("new · {}", display_path(&p.path)),
        (Some(path), None) => format!("{} · connected", display_path(Path::new(path))),
        (None, _) if cfg.wallet.pubkey.is_some() => "none · watch only".into(),
        (None, _) => format!("none · virtual PAPER equity {} SOL", format_sol(cfg.paper.equity_lamports)),
    };
    vec![
        ("Mode", mode),
        ("Wallet", wallet),
        ("Signer", signer),
        ("Network", network_summary(present, env)),
        ("Venues", venues_summary(cfg)),
        ("Strategies", strategies_summary(cfg)),
        ("Route size", route_summary(cfg)),
        (
            "Stops",
            format!(
                "${} daily loss · {} failures in a row · {} bps slippage",
                cfg.risk.max_daily_loss_usd, cfg.risk.max_consecutive_failures, cfg.risk.max_slippage_bps
            ),
        ),
        (
            "Profit guard",
            format!("≥ ${} and ≥ {} bps after all costs", cfg.profit.min_profit_usd, cfg.profit.min_profit_bps),
        ),
        ("Files", format!("{} · {} (0600)", display_path(config_path), display_path(env_path))),
    ]
}

fn mode_summary(cfg: &Config) -> String {
    match (cfg.general.mode, cfg.execution.live_enabled) {
        (Mode::Paper, false) => "PAPER · simulation only, sending locked".into(),
        (Mode::Paper, true) => "PAPER · sending unlocked for --mode confirm / live".into(),
        (mode, _) => format!("{} · transaction submission unlocked", mode.label()),
    }
}

fn network_summary(present: Present, env: &EnvAnswers) -> String {
    let jupiter = state(present.jupiter, &env.jupiter_key, "Jupiter API key", "Jupiter keyless");
    let rpc = state(present.rpc, &env.rpc_url, "private RPC", "public RPC");
    format!("{jupiter} · {rpc}")
}

fn venues_summary(cfg: &Config) -> String {
    let on: Vec<String> = cfg
        .venues
        .iter()
        .filter(|(_, v)| v.enabled)
        .map(|(name, v)| match v.markets.first() {
            Some(market) => format!("{name} {market}"),
            None => name.clone(),
        })
        .collect();
    if on.is_empty() { "none".into() } else { format!("{} · market data only", on.join(", ")) }
}

fn strategies_summary(cfg: &Config) -> String {
    let mut on = Vec::new();
    if cfg.strategies.round_trip.iter().any(|s| s.enabled) {
        on.push("round-trip");
    }
    if cfg.strategies.cross_dex.iter().any(|s| s.enabled) {
        on.push("cross-DEX");
    }
    if cfg.strategies.triangular.iter().any(|s| s.enabled) {
        on.push("triangular");
    }
    if on.is_empty() {
        return "none enabled".into();
    }
    let separator = if i18n::current() == Lang::Zh { "、" } else { ", " };
    on.iter().map(|name| tr(name).into_owned()).collect::<Vec<_>>().join(separator)
}

fn route_summary(cfg: &Config) -> String {
    format!(
        "{} SOL · at most {}% of equity",
        format_sol(cfg.risk.max_trade_lamports),
        cfg.risk.max_trade_pct_of_equity_bps as f64 / 100.0
    )
}

fn short_key(key: &str) -> String {
    if key.chars().count() <= 12 {
        return key.to_string();
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}

fn snapshot(cfg: &Config) -> Option<toml::Value> {
    toml::Value::try_from(cfg).ok()
}

/// `openclaw configure`-style editing of an existing setup: pick one part,
/// change it, come back; everything not picked stays exactly as it was.
/// Returns whether anything changed.
fn edit_sections(
    cfg: &mut Config,
    env: &mut EnvAnswers,
    pending_wallet: &mut Option<PendingWallet>,
    ctx: &Ctx,
) -> Result<bool> {
    let present = ctx.present;
    let start = snapshot(cfg);
    loop {
        let secrets_changed = env.jupiter_key.is_some()
            || env.rpc_url.is_some()
            || env.ws_url.is_some()
            || env.jito_uuid.is_some()
            || env.pyth_key.is_some();
        let changed = snapshot(cfg) != start || pending_wallet.is_some() || secrets_changed;
        let wallet = match (&cfg.wallet.pubkey, &cfg.wallet.keypair_path) {
            (Some(key), Some(_)) => format!("{} · signing keypair", short_key(key)),
            (Some(key), None) => format!("{} · watch only", short_key(key)),
            _ => "none · virtual PAPER equity".into(),
        };
        let network = network_summary(present, env);
        let venues = venues_summary(cfg);
        let strategies = strategies_summary(cfg);
        let safety = format!("{} · ${} daily stop", route_summary(cfg), cfg.risk.max_daily_loss_usd);
        let mode = mode_summary(cfg);
        let done = if changed {
            ("Review and save", "See every change before anything is written.")
        } else {
            ("Nothing to change", "Leave without writing anything.")
        };
        let pick = menu(
            "Which part do you want to change?",
            &[
                ("Wallet", &wallet, None),
                ("Network & API keys", &network, None),
                ("Markets & venues", &venues, None),
                ("Strategies", &strategies, None),
                ("Safety limits", &safety, None),
                ("Mode & transaction permission", &mode, None),
                ("Advanced", "Profit guards, risk details, Jito, feeds, interface.", None),
                (done.0, done.1, None),
            ],
            if changed { 7 } else { 0 },
        )?;
        match pick {
            0 => {
                let current = cfg
                    .wallet
                    .pubkey
                    .clone()
                    .map(|pubkey| CurrentWallet { pubkey, keypair_path: cfg.wallet.keypair_path.clone() });
                let signer = cfg.general.mode.sends_transactions();
                configure_wallet(cfg, pending_wallet, signer, current.as_ref(), ctx)?;
            }
            1 => {
                let proxy = proxy_summary(cfg);
                let update = menu(
                    "Network & API keys",
                    &[
                        ("Keep current", &network, Some("current")),
                        (
                            "Update keys and endpoints",
                            "Hidden input; Enter keeps each stored value, '-' clears it.",
                            None,
                        ),
                        ("Change the proxy", &proxy, None),
                        ("Test the connection", "Solana RPC, WebSocket, Jupiter, Jito and venues, from here.", None),
                    ],
                    0,
                )?;
                if update == 1 {
                    let answers = configure_credentials(present, true)?;
                    let keep = |new: Option<Option<String>>, old: &mut Option<Option<String>>| {
                        if new.is_some() {
                            *old = new;
                        }
                    };
                    keep(answers.jupiter_key, &mut env.jupiter_key);
                    keep(answers.rpc_url, &mut env.rpc_url);
                    keep(answers.ws_url, &mut env.ws_url);
                    keep(answers.jito_uuid, &mut env.jito_uuid);
                    keep(answers.pyth_key, &mut env.pyth_key);
                }
                if update == 2 {
                    choose_proxy(cfg)?;
                }
                if update > 0 {
                    network_check(cfg, env, present)?;
                }
            }
            2 => edit_venues(cfg)?,
            3 => {
                let scope = menu(
                    "Which opportunities should MØBIUS scan?",
                    &[
                        ("Keep current", &strategies, Some("current")),
                        ("Core routes", "SOL round-trips and price gaps between DEXes.", None),
                        ("Everything", "Also triangular cycles; needs noticeably more API requests.", None),
                        ("Round-trips only", "The fewest requests; a light first look.", None),
                    ],
                    0,
                )?;
                if let Some(scope) = scope.checked_sub(1) {
                    apply_strategy_scope(cfg, scope);
                }
            }
            4 => {
                let policy = menu(
                    "Safety limits",
                    &[
                        ("Keep current", &safety, Some("current")),
                        ("Guarded", "Routes up to 1% of equity · pause after 3 failures · $2 daily loss stop", None),
                        ("Balanced", "Routes up to 2.5% of equity · pause after 5 failures · $5 daily loss stop", None),
                    ],
                    0,
                )?;
                if let Some(policy) = policy.checked_sub(1) {
                    apply_safety_policy(cfg, policy);
                    success(&format!("Routes now start at {}", route_summary(cfg)))?;
                }
            }
            5 => edit_mode(cfg, pending_wallet, ctx)?,
            6 => {
                for (prompt, section) in [
                    ("Adjust API endpoints and rate limits?", 0),
                    ("Adjust the profit guards?", 1),
                    ("Adjust the risk limits?", 2),
                    ("Adjust the Jito tip policy?", 3),
                    ("Adjust feeds and the quote scheduler?", 4),
                    ("Adjust the interface and execution settings?", 5),
                ] {
                    if customize(prompt)? {
                        match section {
                            0 => configure_network(cfg)?,
                            1 => configure_profit(cfg)?,
                            2 => configure_risk(cfg)?,
                            3 => configure_jito(cfg)?,
                            4 => configure_feeds_and_scheduler(cfg, env, present.pyth)?,
                            _ => configure_ui_and_execution(cfg)?,
                        }
                    }
                }
            }
            _ => return Ok(changed),
        }
    }
}

fn edit_venues(cfg: &mut Config) -> Result<()> {
    if cfg.venues.is_empty() {
        return info("No venues are configured.");
    }
    let names: Vec<String> = cfg.venues.keys().cloned().collect();
    let details: Vec<String> = cfg
        .venues
        .values()
        .map(|v| {
            let markets = if v.markets.is_empty() { "no markets".to_string() } else { v.markets.join(", ") };
            format!("{} · {:?} · {markets}", if v.enabled { "on" } else { "off" }, v.kind)
        })
        .collect();
    let mut items: Vec<(&str, &str, Option<&str>)> =
        names.iter().zip(&details).map(|(name, detail)| (name.as_str(), detail.as_str(), None)).collect();
    items.push(("Back", "Keep the venues as they are.", None));
    let pick = menu("Which venue do you want to change?", &items, items.len() - 1)?;
    if let Some(name) = names.get(pick)
        && let Some(venue) = cfg.venues.get_mut(name)
    {
        venue.enabled = confirm(&format!("Use {name} market data?"), venue.enabled)?;
        info("Trading on venues stays off until their order connectors exist.")?;
    }
    Ok(())
}

fn edit_mode(cfg: &mut Config, pending_wallet: &mut Option<PendingWallet>, ctx: &Ctx) -> Result<()> {
    let modes = [Mode::Paper, Mode::Confirm, Mode::Live];
    let now = modes.iter().position(|m| *m == cfg.general.mode).unwrap_or(0);
    let badge = |i: usize| (i == now).then_some("current");
    let picked = modes[menu(
        "Operating mode",
        &[
            ("PAPER", "Simulate only; nothing is ever sent.", badge(0)),
            ("CONFIRM", "Send only the transactions you approve one by one.", badge(1)),
            ("LIVE", "Send automatically within the risk limits.", badge(2)),
        ],
        now,
    )?];
    if picked == cfg.general.mode {
        return Ok(());
    }
    if picked == Mode::Paper {
        cfg.general.mode = Mode::Paper;
        cfg.execution.live_enabled = false;
        return success("PAPER: sending is locked again");
    }
    cfg.general.mode = picked;
    if cfg.wallet.keypair_path.is_none() {
        warn(&format!("{} needs a signing wallet.", picked.label()))?;
        configure_wallet(cfg, pending_wallet, true, None, ctx)?;
    }
    let phrase = format!("ENABLE {}", picked.label());
    let check = |value: &str| -> std::result::Result<(), String> {
        if value == phrase { Ok(()) } else { Err(format!("Type exactly: {phrase}")) }
    };
    let typed = ask(&TextPrompt {
        label: &format!("Type {phrase} to unlock transaction submission"),
        placeholder: &phrase,
        empty_answer: "",
        hidden: false,
        validate: Some(&check),
    })?;
    let path = cfg.wallet.keypair_path.clone().context("transaction mode requires a signing wallet")?;
    cfg.paper.simulation_taker = None;
    unlock_transaction_mode(cfg, path, typed.trim())
}

fn configure_guided(
    cfg: &mut Config,
    env: &mut EnvAnswers,
    pending_wallet: &mut Option<PendingWallet>,
    path: SetupPath,
    ctx: &Ctx,
) -> Result<()> {
    let present = ctx.present;
    let assisted = path == SetupPath::Assisted;
    cfg.general.mode = if assisted { Mode::Confirm } else { Mode::Paper };
    cfg.execution.live_enabled = false;

    path.step(
        1,
        &[
            "Use a wallet that exists only for the bot, never your main wallet.",
            "A new key stays in memory until you save, and is never shown or logged.",
        ],
    );
    configure_wallet(cfg, pending_wallet, assisted, ctx.current_wallet.as_ref(), ctx)?;

    path.step(
        2,
        if assisted {
            &["Assisted trading works best with your own RPC. Public endpoints are fine for a first test."]
        } else {
            &["Public endpoints are enough to start; they are rate limited, so scans run slower."]
        },
    );
    let public = MenuItem {
        title: "Public endpoints",
        description: "Keyless: the built-in mainnet RPC and Jupiter access.",
        badge: (!assisted).then_some("recommended"),
    };
    let own = MenuItem {
        title: "My own providers",
        description: "Add a Jupiter API key and a private RPC. They are stored only in .env.",
        badge: assisted.then_some("recommended"),
    };
    let items = if assisted { [own, public] } else { [public, own] };
    let chosen = menu_items("How should MØBIUS reach Solana and Jupiter?", &items, 0)?;
    if (chosen == 0) == assisted {
        *env = configure_credentials(present, false)?;
    } else if present.jupiter || present.rpc {
        info("Credentials already in .env stay in use.")?;
    }
    network_check(cfg, env, present)?;

    path.step(3, &["Route sizes come from the safety policy in the next step."]);
    let scope = menu(
        "Which opportunities should MØBIUS scan?",
        &[
            ("Core routes", "SOL round-trips and price gaps between DEXes.", Some("recommended")),
            ("Everything", "Also triangular cycles; needs noticeably more API requests.", None),
            ("Round-trips only", "The fewest requests; a light first look.", None),
        ],
        0,
    )?;
    apply_strategy_scope(cfg, scope);

    path.step(
        4,
        &[
            "MØBIUS sizes every route for you; there is no amount to pick.",
            "Guards reject bigger routes and pause on repeated failures or daily losses.",
        ],
    );
    let policy = menu(
        "How cautiously should the bot start?",
        &[
            ("Guarded", "Routes up to 1% of equity · pause after 3 failures · $2 daily loss stop", Some("recommended")),
            ("Balanced", "Routes up to 2.5% of equity · pause after 5 failures · $5 daily loss stop", None),
        ],
        0,
    )?;
    apply_safety_policy(cfg, policy);
    success(&format!(
        "Routes start at {} SOL; wider slippage than {} bps is rejected",
        format_sol(cfg.risk.max_trade_lamports),
        cfg.risk.max_slippage_bps
    ))?;

    if assisted {
        path.step(
            5,
            &[
                "CONFIRM shows every transaction and sends nothing without your approval.",
                "The kill switch and every risk guard stay active.",
            ],
        );
        let unlock = menu(
            "Allow MØBIUS to submit transactions you approve?",
            &[
                ("Unlock CONFIRM", "You will type a short phrase to confirm.", Some("recommended")),
                ("Stay in PAPER for now", "Keep the wallet; switch later with --setup.", None),
            ],
            0,
        )?;
        if unlock == 0 {
            let keypair = cfg.wallet.keypair_path.clone().context("assisted trading requires a signing wallet")?;
            let check = |value: &str| -> std::result::Result<(), String> {
                if value == "ENABLE CONFIRM" { Ok(()) } else { Err("Type exactly: ENABLE CONFIRM".into()) }
            };
            let phrase = ask(&TextPrompt {
                label: "Type ENABLE CONFIRM to unlock approved transactions",
                placeholder: "ENABLE CONFIRM",
                empty_answer: "",
                hidden: false,
                validate: Some(&check),
            })?;
            unlock_transaction_mode(cfg, keypair, phrase.trim())?;
            success("CONFIRM unlocked: every transaction still needs your approval")?;
        } else {
            cfg.general.mode = Mode::Paper;
            cfg.execution.live_enabled = false;
            info("Staying in PAPER; the bot wallet is kept for later.")?;
        }
    }
    Ok(())
}

fn configure_wallet(
    cfg: &mut Config,
    pending_wallet: &mut Option<PendingWallet>,
    require_signer: bool,
    current: Option<&CurrentWallet>,
    ctx: &Ctx,
) -> Result<()> {
    let keep = current.filter(|c| !require_signer || c.keypair_path.is_some());
    let keep_title = keep.map(|c| format!("Keep {}", short_key(&c.pubkey)));
    let keep_detail = keep.map(|c| match &c.keypair_path {
        Some(path) => format!("Signing keypair {}", display_path(Path::new(path))),
        None => "Watch only; no signing access.".to_string(),
    });
    let mut items: Vec<(&str, &str, Option<&str>)> = Vec::new();
    if let (Some(title), Some(detail)) = (&keep_title, &keep_detail) {
        items.push((title, detail, Some("current")));
    }
    items.push((
        "Create a new bot wallet",
        "A fresh Solana keypair, saved privately (0600) when you save.",
        keep.is_none().then_some("recommended"),
    ));
    items.push(("Use an existing keypair file", "A Solana CLI keypair JSON that you control.", None));
    if !require_signer {
        items.push(("Watch an address only", "Follow its balances; no signing access.", None));
        items.push(("No wallet for now", "Simulate with virtual PAPER equity.", None));
    }
    let picked = menu("How should the bot wallet be prepared?", &items, 0)?;
    let Some(selection) = picked.checked_sub(usize::from(keep.is_some())) else {
        let kept = keep.expect("the keep option is listed only with a current wallet");
        cfg.wallet.pubkey = Some(kept.pubkey.clone());
        cfg.wallet.keypair_path = kept.keypair_path.clone();
        success(&format!("Keeping wallet {}", kept.pubkey))?;
        return wallet_status(cfg, false);
    };

    *pending_wallet = None;
    match selection {
        0 => {
            let path = next_wallet_path(&ctx.wallet_dir);
            let wallet = GeneratedWallet::new();
            cfg.wallet.pubkey = Some(wallet.pubkey().to_string());
            cfg.wallet.keypair_path = Some(path.display().to_string());
            success(&format!("New bot wallet {}", wallet.pubkey()))?;
            info(&format!("Written to {} when you save.", display_path(&path)))?;
            *pending_wallet = Some(PendingWallet { path, wallet });
        }
        1 => {
            let existing = cfg
                .wallet
                .keypair_path
                .clone()
                .or_else(|| current.and_then(|c| c.keypair_path.clone()))
                .unwrap_or_default();
            let check = |value: &str| -> std::result::Result<(), String> {
                let value = if value.is_empty() { existing.as_str() } else { value };
                if value.is_empty() {
                    return Err("Enter the path of a keypair file".into());
                }
                Wallet::load(&expand_home_path(value), None).map(|_| ()).map_err(|e| e.to_string())
            };
            let placeholder =
                if existing.is_empty() { "~/.config/solana/bot.json".to_string() } else { existing.clone() };
            let entered = ask(&TextPrompt {
                label: "Keypair file",
                placeholder: &placeholder,
                empty_answer: &existing,
                hidden: false,
                validate: Some(&check),
            })?;
            let value = if entered.trim().is_empty() { existing.as_str() } else { entered.trim() };
            let path = expand_home_path(value);
            let wallet =
                Wallet::load(&path, None).with_context(|| format!("checking bot wallet {}", path.display()))?;
            cfg.wallet.pubkey = Some(wallet.pubkey().to_string());
            cfg.wallet.keypair_path = Some(path.display().to_string());
            success(&format!("Using bot wallet {}", wallet.pubkey()))?;
        }
        2 => {
            let check = |value: &str| -> std::result::Result<(), String> {
                value.parse::<searcher_core::Address>().map(|_| ()).map_err(|e| format!("Not a Solana address: {e}"))
            };
            let value = ask(&TextPrompt {
                label: "Wallet address to watch",
                placeholder: "base58 public key",
                empty_answer: "",
                hidden: false,
                validate: Some(&check),
            })?;
            let address: searcher_core::Address =
                value.trim().parse().map_err(|error| anyhow::anyhow!("wallet address: {error}"))?;
            cfg.wallet.pubkey = Some(address.to_string());
            cfg.wallet.keypair_path = None;
            success(&format!("Watching {address}; signing stays disabled"))?;
        }
        _ => {
            cfg.wallet.pubkey = None;
            cfg.wallet.keypair_path = None;
            success(&format!("Virtual PAPER equity: {} SOL", format_sol(cfg.paper.equity_lamports)))?;
        }
    }
    let fresh = pending_wallet.is_some();
    wallet_status(cfg, fresh)
}

/// Balance of an existing wallet, and a funding QR code when this setup will
/// send transactions from a wallet that cannot yet pay for them.
fn wallet_status(cfg: &Config, fresh: bool) -> Result<()> {
    let Some(address) = cfg.wallet.pubkey.clone() else { return Ok(()) };
    let sends = cfg.general.mode.sends_transactions() && cfg.wallet.keypair_path.is_some();
    let reserve = cfg.risk.min_wallet_sol_for_fees_lamports;
    let balance = if fresh {
        Some(0)
    } else {
        let balance = setup_ui::with_spinner("Reading the wallet balance…", || wallet_balance(cfg, &address));
        match balance {
            Some(b) => info(&format!("Balance {} SOL", format_sol(b)))?,
            None => info("Balance unavailable right now; `mobius-searcher --doctor` shows it later.")?,
        }
        balance
    };
    if sends && balance.is_some_and(|b| b < reserve) {
        let caption = format!(
            "Fund it before sending: at least {} SOL for fees, plus what it may trade. Scan with a Solana wallet \
             app or copy the address.",
            format_sol(reserve)
        );
        qr(&format!("solana:{address}"), &address, &caption)?;
    }
    Ok(())
}

/// `getBalance` through the configured RPC; `None` when it cannot answer.
fn wallet_balance(cfg: &Config, address: &str) -> Option<u64> {
    #[cfg(test)]
    if script::active() {
        return script::balance();
    }
    let address: searcher_core::Address = address.parse().ok()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    runtime.block_on(async {
        let rpc = searcher_market::RpcClient::new(
            &cfg.rpc.resolved_url(),
            searcher_telemetry::LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
            cfg.rpc.simulate_rps,
            std::time::Duration::from_millis(cfg.rpc.timeout_ms),
            std::sync::Arc::new(searcher_telemetry::Telemetry::new()),
        )
        .ok()?;
        rpc.get_balance(&address).await.ok()
    })
}

fn apply_strategy_scope(cfg: &mut Config, scope: usize) {
    ensure_strategy_slots(cfg);
    cfg.strategies.round_trip[0].enabled = true;
    cfg.strategies.cross_dex[0].enabled = scope != 2;
    cfg.strategies.triangular[0].enabled = scope == 1;
}

fn apply_safety_policy(cfg: &mut Config, policy: usize) {
    let (equity_bps, daily_loss, failures, slippage_bps) =
        if policy == 0 { (100_i64, "2", 3, 50) } else { (250_i64, "5", 5, 100) };
    cfg.risk.max_trade_pct_of_equity_bps = equity_bps;
    cfg.risk.max_daily_loss_usd = daily_loss.into();
    cfg.risk.max_consecutive_failures = failures;
    cfg.risk.max_slippage_bps = slippage_bps;
    cfg.risk.max_open_executions = 1;
    cfg.profit.protect_min_out = true;
    let amount = cfg.paper.equity_lamports.saturating_mul(equity_bps as u64) / 10_000;
    apply_trade_size(cfg, amount.max(1_000_000));
}

fn proxy_summary(cfg: &Config) -> String {
    match cfg.network.proxy.trim() {
        "" | "auto" => format!("Automatic · {}", auto_proxy_now()),
        "none" | "direct" => "No proxy · always connect directly".into(),
        url => format!("HTTP proxy {url}"),
    }
}

/// What `auto` resolves to on this machine right now.
fn auto_proxy_now() -> String {
    let env = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k, v)));
    match (env, searcher_telemetry::proxy::system_proxy(true)) {
        (Some((k, v)), _) => format!("${k} {}", config::display_url(&v)),
        (None, Some((host, port))) => format!("system proxy {host}:{port}"),
        (None, None) => "no proxy found, connects directly".into(),
    }
}

fn choose_proxy(cfg: &mut Config) -> Result<()> {
    let now = match cfg.network.proxy.trim() {
        "" | "auto" => 0,
        "none" | "direct" => 1,
        _ => 2,
    };
    let badge = |i: usize| (i == now).then_some("current");
    let auto = format!("Environment variables, else the system settings. Now: {}.", auto_proxy_now());
    let pick = menu(
        "How should MØBIUS connect to the internet?",
        &[
            ("Detect the proxy automatically", &auto, badge(0)),
            ("Connect directly", "Ignore every proxy setting.", badge(1)),
            ("Use this HTTP proxy", "An http://host:port proxy for every connection.", badge(2)),
        ],
        now,
    )?;
    cfg.network.proxy = match pick {
        0 => "auto".into(),
        1 => "none".into(),
        _ => {
            let current = if now == 2 { cfg.network.proxy.clone() } else { String::new() };
            let check = |v: &str| -> std::result::Result<(), String> {
                if v.is_empty() && !current.is_empty() {
                    return Ok(());
                }
                if !v.starts_with("http://") {
                    return Err("Enter it as http://host:port".into());
                }
                searcher_telemetry::proxy::ProxySetting::parse(v).map(|_| ())
            };
            let placeholder = if current.is_empty() { "http://127.0.0.1:7890".to_string() } else { current.clone() };
            let value = ask(&TextPrompt {
                label: "HTTP proxy",
                placeholder: &placeholder,
                empty_answer: &current,
                hidden: false,
                validate: Some(&check),
            })?;
            if value.trim().is_empty() { current } else { value.trim().trim_end_matches('/').to_string() }
        }
    };
    Ok(())
}

/// Tests the settings as they would be saved, then offers to fix what failed.
/// A failure never blocks saving: the user may simply be offline right now.
fn network_check(cfg: &mut Config, env: &mut EnvAnswers, present: Present) -> Result<()> {
    loop {
        let checks = setup_ui::with_spinner("Testing connections from this machine…", || probe(cfg, env))?;
        for c in &checks {
            check_line(c.ok, c.optional, &c.name, &format!("{} · {}", c.target, c.detail))?;
        }
        let failed: Vec<&str> = checks.iter().filter(|c| !c.ok && !c.optional).map(|c| c.name.as_str()).collect();
        if failed.is_empty() {
            return Ok(());
        }
        let proxy = proxy_summary(cfg);
        let pick = menu(
            &format!("{} did not answer. What now?", failed.join(", ")),
            &[
                ("Try another proxy setting", &proxy, None),
                ("Re-enter keys and endpoints", "Hidden input; Enter keeps each stored value.", None),
                ("Test again", "After fixing something outside MØBIUS.", None),
                ("Continue anyway", "Save as is; `mobius-searcher --doctor` tests it again later.", None),
            ],
            0,
        )?;
        match pick {
            0 => choose_proxy(cfg)?,
            1 => {
                let answers = configure_credentials(present, false)?;
                for (new, old) in [
                    (answers.jupiter_key, &mut env.jupiter_key),
                    (answers.rpc_url, &mut env.rpc_url),
                    (answers.ws_url, &mut env.ws_url),
                ] {
                    if new.is_some() {
                        *old = new;
                    }
                }
            }
            2 => {}
            _ => return warn("Saved without a working connection; `mobius-searcher --doctor` tests it again."),
        }
    }
}

/// Runs `mobius-searcher --doctor --json` as a child process on a temporary
/// copy of the settings. The child gets the just-typed secrets in its own
/// environment only (nothing is written) and a fresh proxy setting, which a
/// running process cannot change.
fn probe(cfg: &Config, env: &EnvAnswers) -> Result<Vec<Check>> {
    #[cfg(test)]
    if let Some(checks) = script::probe() {
        return checks;
    }
    use std::process::{Command, Stdio};
    let mut settings = cfg.clone();
    // streams take seconds to deliver a first message; the RPC, WebSocket,
    // Jupiter, Jito and venue checks cover what setup can fix
    settings.feeds.enabled = false;
    settings.scheduler.kind = SchedulerKind::RoundRobin;
    settings.general.mode = Mode::Paper;
    let dir = std::env::temp_dir().join(format!(
        "mobius-setup-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    fs::create_dir_all(&dir)?;
    let file = dir.join("config.toml");
    let result = (|| -> Result<Vec<Check>> {
        fs::write(&file, toml::to_string(&settings).context("serializing the probe settings")?)?;
        let mut command = Command::new(std::env::current_exe().context("locating mobius-searcher")?);
        command.args(["--doctor", "--json", "--config"]).arg(&file);
        for (name, change) in [
            (&cfg.jupiter.api_key_env, &env.jupiter_key),
            (&cfg.rpc.url_env, &env.rpc_url),
            (&cfg.rpc.ws_url_env, &env.ws_url),
            (&cfg.jito.uuid_env, &env.jito_uuid),
            (&cfg.feeds.pyth_api_key_env, &env.pyth_key),
        ] {
            match change {
                Some(Some(value)) => {
                    command.env(name, value);
                }
                // cleared: an empty value hides the stored one from the child
                Some(None) => {
                    command.env(name, "");
                }
                None => {}
            }
        }
        let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let mut stdout = child.stdout.take().context("child stdout")?;
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            let _ = io::Read::read_to_string(&mut stdout, &mut out);
            out
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        while child.try_wait()?.is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("the connection test did not finish within 45 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let out = reader.join().unwrap_or_default();
        let report: serde_json::Value = out
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .context("the connection test printed no report")?;
        report
            .get("checks")
            .and_then(|c| c.as_array())
            .map(|checks| checks.iter().filter_map(Check::from_json).collect())
            .context("the connection test report has no checks")
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}

fn configure_credentials(present: Present, include_optional: bool) -> Result<EnvAnswers> {
    let keep = |there: bool, otherwise: &'static str| if there { "enter keeps the current value" } else { otherwise };
    let jupiter = secret("Jupiter API key (optional)", keep(present.jupiter, "enter = keyless access"), None)?;
    let https = |v: &str| url_check(v, &["https://", "http://"]);
    let rpc = secret("Private RPC URL (optional)", keep(present.rpc, "enter = public RPC"), Some(&https))?;
    let wss = |v: &str| url_check(v, &["wss://", "ws://"]);
    let ws = secret("Private RPC WebSocket URL (optional)", keep(present.ws, "enter = public WebSocket"), Some(&wss))?;
    let (jito_uuid, pyth_key) = if include_optional {
        let jito = secret("Jito UUID (optional)", keep(present.jito, "enter = none"), None)?;
        let pyth = secret("Pyth / Hermes API key (optional)", keep(present.pyth, "enter = on-chain oracle"), None)?;
        (change(jito), change(pyth))
    } else {
        (None, None)
    };
    Ok(EnvAnswers { jupiter_key: change(jupiter), rpc_url: change(rpc), ws_url: change(ws), jito_uuid, pyth_key })
}

fn url_check(value: &str, schemes: &[&str]) -> std::result::Result<(), String> {
    if value.is_empty() || value == "-" || schemes.iter().any(|s| value.starts_with(s)) {
        Ok(())
    } else {
        Err(format!("Must start with {}  ('-' clears the stored value)", schemes.join(" or ")))
    }
}

/// Hidden input for a credential: empty keeps what is stored, `-` clears it.
fn secret(label: &str, placeholder: &str, validate: Option<Validator<'_>>) -> Result<String> {
    let empty_answer = if placeholder.starts_with("enter keeps") { "kept" } else { "skipped" };
    ask(&TextPrompt {
        label,
        placeholder: &format!("{placeholder} · '-' clears"),
        empty_answer,
        hidden: true,
        validate,
    })
}

fn next_wallet_path(root: &Path) -> PathBuf {
    let first = root.join("bot-keypair.json");
    if !first.exists() {
        return first;
    }
    for index in 2..10_000 {
        let candidate = root.join(format!("bot-keypair-{index}.json"));
        if !candidate.exists() {
            return candidate;
        }
    }
    root.join(format!("bot-keypair-{}.json", std::process::id()))
}

/// `~/…` for paths under the home directory; shorter to read and to copy.
fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

fn expand_home_path(value: &str) -> PathBuf {
    if value == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(relative) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(value)
}

fn persist_pending_wallet(pending: Option<&PendingWallet>) -> Result<Option<PathBuf>> {
    let Some(pending) = pending else { return Ok(None) };
    pending
        .wallet
        .write_new(&pending.path)
        .with_context(|| format!("creating bot wallet {}", pending.path.display()))?;
    Ok(Some(pending.path.clone()))
}

// ------------------------------------------------------------ advanced path

fn configure_advanced(
    cfg: &mut Config,
    env: &mut EnvAnswers,
    pending_wallet: &mut Option<PendingWallet>,
    ctx: &Ctx,
) -> Result<()> {
    let present = ctx.present;
    let path = SetupPath::Advanced;
    path.step(1, &["LIVE and CONFIRM send real transactions and need a signing wallet and a typed phrase."]);
    configure_profile(cfg, pending_wallet, ctx)?;

    path.step(2, &["All optional. Values are hidden while typed and stored only in .env (0600)."]);
    *env = configure_credentials(present, true)?;

    path.step(3, &["Endpoints, request rates and timeouts. The defaults fit Jupiter's free plan."]);
    if customize("Adjust API endpoints and rate limits?")? {
        configure_network(cfg)?;
    }
    network_check(cfg, env, present)?;

    path.step(4, &["Each strategy can be switched on or off; amounts are in SOL."]);
    configure_strategies(cfg)?;

    path.step(5, &["A route must clear these after fees, tips, slippage and a safety buffer."]);
    if customize("Adjust the profit guards?")? {
        configure_profit(cfg)?;
    }

    path.step(6, &["Hard limits checked before any route is simulated or sent."]);
    if customize("Adjust the risk limits?")? {
        configure_risk(cfg)?;
    }

    path.step(7, &["Only used when a bundle is sent through Jito."]);
    if customize("Adjust the Jito tip policy?")? {
        configure_jito(cfg)?;
    }

    path.step(8, &["Pool and oracle feeds let the scheduler re-quote only when something moved."]);
    if customize("Adjust feeds and the quote scheduler?")? {
        configure_feeds_and_scheduler(cfg, env, present.pyth)?;
    }

    path.step(9, &["Glyphs, colours, frame rate, data directory and execution timeouts."]);
    if customize("Adjust the interface and execution settings?")? {
        configure_ui_and_execution(cfg)?;
    }
    Ok(())
}

fn customize(prompt: &str) -> Result<bool> {
    Ok(menu(
        prompt,
        &[
            ("Keep the current values", "Recommended unless you know what to change.", Some("recommended")),
            ("Customize", "Go through each value; Enter keeps the one shown.", None),
        ],
        0,
    )? == 1)
}

fn configure_profile(cfg: &mut Config, pending_wallet: &mut Option<PendingWallet>, ctx: &Ctx) -> Result<()> {
    let default = match cfg.general.mode {
        Mode::Paper => 0,
        Mode::Confirm => 1,
        Mode::Live => 2,
    };
    cfg.general.mode = match menu(
        "Operating mode",
        &[
            ("PAPER", "Simulate only; nothing is ever sent.", Some("recommended")),
            ("CONFIRM", "Send only the transactions you approve one by one.", None),
            ("LIVE", "Send automatically within the risk limits.", None),
        ],
        default,
    )? {
        0 => Mode::Paper,
        1 => Mode::Confirm,
        _ => Mode::Live,
    };

    if cfg.general.mode == Mode::Paper {
        lock_to_paper(cfg);
        configure_wallet(cfg, pending_wallet, false, ctx.current_wallet.as_ref(), ctx)?;
        cfg.paper.equity_lamports = prompt_sol("Virtual PAPER equity", cfg.paper.equity_lamports)?;
        return Ok(());
    }

    cfg.execution.live_enabled = false;
    cfg.paper.simulation_taker = None;
    configure_wallet(cfg, pending_wallet, true, ctx.current_wallet.as_ref(), ctx)?;
    warn("This mode can send real transactions. The key itself is never copied into the config.")?;
    let phrase = format!("ENABLE {}", cfg.general.mode.label());
    let check = |value: &str| -> std::result::Result<(), String> {
        if value == phrase { Ok(()) } else { Err(format!("Type exactly: {phrase}")) }
    };
    let confirmation = ask(&TextPrompt {
        label: &format!("Type {phrase} to unlock transaction submission"),
        placeholder: &phrase,
        empty_answer: "",
        hidden: false,
        validate: Some(&check),
    })?;
    let path = cfg.wallet.keypair_path.clone().context("transaction mode requires a signing wallet")?;
    unlock_transaction_mode(cfg, path, confirmation.trim())
}

fn apply_trade_size(cfg: &mut Config, amount: u64) {
    ensure_strategy_slots(cfg);
    for s in &mut cfg.strategies.round_trip {
        s.amount_lamports = amount;
    }
    for s in &mut cfg.strategies.cross_dex {
        s.amount_lamports = amount;
    }
    for s in &mut cfg.strategies.triangular {
        s.amount_lamports = amount;
    }
    cfg.risk.max_trade_lamports = amount;
}

fn lock_to_paper(cfg: &mut Config) {
    cfg.general.mode = Mode::Paper;
    cfg.execution.live_enabled = false;
    cfg.wallet.keypair_path = None;
}

fn unlock_transaction_mode(cfg: &mut Config, keypair_path: String, confirmation: &str) -> Result<()> {
    if !cfg.general.mode.sends_transactions() {
        bail!("PAPER mode cannot unlock transaction submission");
    }
    let phrase = format!("ENABLE {}", cfg.general.mode.label());
    if confirmation != phrase {
        bail!("transaction submission was not enabled; confirmation did not match");
    }
    if keypair_path.trim().is_empty() {
        bail!("{} mode requires a keypair path", cfg.general.mode.label());
    }
    cfg.wallet.keypair_path = Some(keypair_path);
    cfg.execution.live_enabled = true;
    Ok(())
}

fn configure_network(cfg: &mut Config) -> Result<()> {
    cfg.jupiter.base_url = prompt_text("Jupiter base URL", &cfg.jupiter.base_url)?;
    cfg.jupiter.general_rps = prompt_number("Jupiter requests per second", cfg.jupiter.general_rps)?;
    cfg.jupiter.general_burst = prompt_number("Jupiter burst", cfg.jupiter.general_burst)?;
    cfg.jupiter.timeout_ms = prompt_number("Jupiter timeout (ms)", cfg.jupiter.timeout_ms)?;
    cfg.jupiter.slippage = prompt_text("Jupiter slippage (`rtse` or bps)", &cfg.jupiter.slippage)?;
    cfg.rpc.url = prompt_text("Fallback Solana RPC URL", &cfg.rpc.url)?;
    cfg.rpc.ws_url = prompt_text("Fallback Solana RPC WebSocket URL", &cfg.rpc.ws_url)?;
    cfg.rpc.rps = prompt_number("RPC requests per second", cfg.rpc.rps)?;
    cfg.rpc.burst = prompt_number("RPC burst", cfg.rpc.burst)?;
    cfg.rpc.simulate_rps = prompt_number("Simulations per second", cfg.rpc.simulate_rps)?;
    cfg.rpc.timeout_ms = prompt_number("RPC timeout (ms)", cfg.rpc.timeout_ms)?;
    cfg.jito.block_engine_url = prompt_text("Jito block-engine URL", &cfg.jito.block_engine_url)?;
    cfg.jito.tip_floor_url = prompt_text("Jito tip-floor URL", &cfg.jito.tip_floor_url)?;
    Ok(())
}

fn configure_strategies(cfg: &mut Config) -> Result<()> {
    ensure_strategy_slots(cfg);

    let rt = &mut cfg.strategies.round_trip[0];
    rt.enabled = confirm("Scan SOL → quote → SOL round-trips?", rt.enabled)?;
    if rt.enabled {
        rt.quote = prompt_text("Round-trip quote token", &rt.quote)?;
        rt.amount_lamports = prompt_sol("Round-trip amount", rt.amount_lamports)?;
        rt.weight = prompt_number("Round-trip scheduler weight", rt.weight)?;
    }

    let xd = &mut cfg.strategies.cross_dex[0];
    xd.enabled = confirm("Scan price gaps between DEXes?", xd.enabled)?;
    if xd.enabled {
        xd.quote = prompt_text("Cross-DEX quote token", &xd.quote)?;
        xd.amount_lamports = prompt_sol("Cross-DEX amount", xd.amount_lamports)?;
        xd.dexes = prompt_csv("DEX labels", &xd.dexes)?;
        xd.weight = prompt_number("Cross-DEX scheduler weight", xd.weight)?;
    }

    let tri = &mut cfg.strategies.triangular[0];
    tri.enabled = confirm("Scan triangular cycles?", tri.enabled)?;
    if tri.enabled {
        tri.cycle = prompt_csv("Triangular cycle", &tri.cycle)?;
        tri.amount_lamports = prompt_sol("Triangular amount", tri.amount_lamports)?;
        tri.weight = prompt_number("Triangular scheduler weight", tri.weight)?;
    }
    Ok(())
}

fn ensure_strategy_slots(cfg: &mut Config) {
    if cfg.strategies.round_trip.is_empty() {
        cfg.strategies.round_trip.push(RoundTripConfig::default());
    }
    if cfg.strategies.cross_dex.is_empty() {
        cfg.strategies.cross_dex.push(CrossDexConfig::default());
    }
    if cfg.strategies.triangular.is_empty() {
        cfg.strategies.triangular.push(TriangularConfig::default());
    }
}

fn configure_profit(cfg: &mut Config) -> Result<()> {
    cfg.profit.min_profit_lamports =
        prompt_sol("Minimum absolute profit", cfg.profit.min_profit_lamports.max(0) as u64)?
            .try_into()
            .context("minimum profit is too large")?;
    cfg.profit.min_profit_bps = prompt_number("Minimum profit (bps)", cfg.profit.min_profit_bps)?;
    cfg.profit.min_profit_usd = prompt_text("Minimum profit (USD)", &cfg.profit.min_profit_usd)?;
    cfg.profit.expected_slippage_share_bps =
        prompt_number("Expected slippage cost share (bps)", cfg.profit.expected_slippage_share_bps)?;
    cfg.profit.safety_buffer_lamports = prompt_sol("Additional safety buffer", cfg.profit.safety_buffer_lamports)?;
    cfg.profit.safety_buffer_bps = prompt_number("Safety buffer (bps)", cfg.profit.safety_buffer_bps)?;
    cfg.profit.cu_margin_bps = prompt_number("Compute-unit margin (bps)", cfg.profit.cu_margin_bps)?;
    cfg.profit.max_cu_price_micro = prompt_number("Maximum CU price (micro-lamports)", cfg.profit.max_cu_price_micro)?;
    cfg.profit.protect_min_out = confirm("Protect the final leg with a minimum output?", cfg.profit.protect_min_out)?;
    Ok(())
}

fn configure_risk(cfg: &mut Config) -> Result<()> {
    cfg.risk.max_trade_lamports = prompt_sol("Maximum trade size", cfg.risk.max_trade_lamports)?;
    cfg.risk.max_trade_pct_of_equity_bps =
        prompt_number("Maximum trade share of equity (bps)", cfg.risk.max_trade_pct_of_equity_bps)?;
    cfg.risk.max_daily_loss_usd = prompt_text("Maximum daily loss (USD)", &cfg.risk.max_daily_loss_usd)?;
    cfg.risk.max_consecutive_failures =
        prompt_number("Maximum consecutive failures", cfg.risk.max_consecutive_failures)?;
    cfg.risk.max_slippage_bps = prompt_number("Maximum slippage (bps)", cfg.risk.max_slippage_bps)?;
    cfg.risk.max_quote_age_ms = prompt_number("Maximum quote age (ms)", cfg.risk.max_quote_age_ms)?;
    cfg.risk.max_simulation_age_ms = prompt_number("Maximum simulation age (ms)", cfg.risk.max_simulation_age_ms)?;
    cfg.risk.max_priority_fee_lamports =
        prompt_number("Maximum priority fee (lamports)", cfg.risk.max_priority_fee_lamports)?;
    cfg.risk.max_jito_tip_lamports = prompt_number("Maximum Jito tip (lamports)", cfg.risk.max_jito_tip_lamports)?;
    cfg.risk.min_wallet_sol_for_fees_lamports =
        prompt_sol("SOL reserved for fees and rent", cfg.risk.min_wallet_sol_for_fees_lamports)?;
    cfg.risk.max_open_executions = prompt_number("Maximum open executions", cfg.risk.max_open_executions)?;
    cfg.risk.max_slot_lag = prompt_number("Maximum slot lag", cfg.risk.max_slot_lag)?;
    Ok(())
}

fn configure_jito(cfg: &mut Config) -> Result<()> {
    let default = match cfg.jito.tip_policy.kind {
        TipPolicyKind::Fixed => 0,
        TipPolicyKind::Percentile => 1,
        TipPolicyKind::ProfitShare => 2,
    };
    cfg.jito.tip_policy.kind = match menu(
        "Tip policy",
        &[
            ("Fixed", "The same tip on every bundle.", None),
            ("Percentile", "Follow recently landed tips.", None),
            ("Profit share", "A share of the expected profit.", None),
        ],
        default,
    )? {
        0 => TipPolicyKind::Fixed,
        1 => TipPolicyKind::Percentile,
        _ => TipPolicyKind::ProfitShare,
    };
    if cfg.jito.tip_policy.kind == TipPolicyKind::Fixed {
        cfg.jito.tip_policy.fixed_lamports =
            prompt_number("Fixed Jito tip (lamports)", cfg.jito.tip_policy.fixed_lamports)?;
    } else if cfg.jito.tip_policy.kind == TipPolicyKind::Percentile {
        let values = ["p25", "p50", "p75", "p95", "p99", "ema50"];
        let default = values.iter().position(|v| *v == cfg.jito.tip_policy.percentile).unwrap_or(1);
        cfg.jito.tip_policy.percentile = values[choice("Landed-tip percentile", &values, default)?].into();
    } else {
        cfg.jito.tip_policy.profit_share_bps =
            prompt_number("Tip share of expected profit (bps)", cfg.jito.tip_policy.profit_share_bps)?;
    }
    cfg.jito.tip_policy.min_lamports = prompt_number("Minimum Jito tip (lamports)", cfg.jito.tip_policy.min_lamports)?;
    cfg.jito.tip_policy.max_lamports = prompt_number("Maximum Jito tip (lamports)", cfg.jito.tip_policy.max_lamports)?;
    cfg.jito.dont_front = confirm("Add Jito's dont-front account?", cfg.jito.dont_front)?;
    Ok(())
}

fn configure_feeds_and_scheduler(cfg: &mut Config, env: &EnvAnswers, had_pyth: bool) -> Result<()> {
    cfg.feeds.enabled = confirm("Use on-chain pool and oracle feeds?", cfg.feeds.enabled)?;
    if cfg.feeds.enabled {
        let pyth_available = state(had_pyth, &env.pyth_key, "yes", "no") == "yes";
        let default = if cfg.feeds.oracle_source == OracleSourceKind::Hermes { 1 } else { 0 };
        let source = choice("Oracle source", &["onchain", "hermes"], default)?;
        cfg.feeds.oracle_source = if source == 1 { OracleSourceKind::Hermes } else { OracleSourceKind::Onchain };
        if cfg.feeds.oracle_source == OracleSourceKind::Hermes && !pyth_available {
            warn("No Pyth key is configured; Hermes falls back to on-chain prices.")?;
        }
        cfg.feeds.emit_interval_ms = prompt_number("Minimum feed sample interval (ms)", cfg.feeds.emit_interval_ms)?;
        cfg.feeds.network_poll_ms = prompt_number("Network-stat polling interval (ms)", cfg.feeds.network_poll_ms)?;
    }
    let default = if cfg.scheduler.kind == SchedulerKind::Event { 0 } else { 1 };
    let chosen = choice("Quote scheduler", &["event", "round_robin"], default)?;
    cfg.scheduler.kind =
        if chosen == 0 && cfg.feeds.enabled { SchedulerKind::Event } else { SchedulerKind::RoundRobin };
    cfg.scheduler.max_in_flight = prompt_number("Maximum quote requests in flight", cfg.scheduler.max_in_flight)?;
    cfg.scheduler.floor_s = prompt_number("Full-route refresh floor (seconds)", cfg.scheduler.floor_s)?;
    Ok(())
}

fn configure_ui_and_execution(cfg: &mut Config) -> Result<()> {
    // "" means the per-user default; keep it unless the user types a path
    let shown = display_path(&cfg.data_dir());
    let value = ask(&TextPrompt {
        label: "Data directory",
        placeholder: &format!("enter keeps {shown}"),
        empty_answer: &shown,
        hidden: false,
        validate: None,
    })?;
    if !value.trim().is_empty() {
        cfg.general.data_dir = expand_home_path(value.trim()).display().to_string();
    }
    let glyph_default = match cfg.ui.glyphs {
        GlyphMode::Auto => 0,
        GlyphMode::Unicode => 1,
        GlyphMode::Ascii => 2,
    };
    cfg.ui.glyphs = match choice("Glyph mode", &["auto", "unicode", "ascii"], glyph_default)? {
        1 => GlyphMode::Unicode,
        2 => GlyphMode::Ascii,
        _ => GlyphMode::Auto,
    };
    let color_default = match cfg.ui.color {
        ColorMode::Auto => 0,
        ColorMode::Truecolor => 1,
        ColorMode::Ansi256 => 2,
        ColorMode::None => 3,
    };
    cfg.ui.color = match choice("Color mode", &["auto", "truecolor", "ansi256", "none"], color_default)? {
        1 => ColorMode::Truecolor,
        2 => ColorMode::Ansi256,
        3 => ColorMode::None,
        _ => ColorMode::Auto,
    };
    cfg.ui.fps = prompt_number("TUI frames per second", cfg.ui.fps)?;
    cfg.ui.max_graphs = prompt_number("Maximum graphs", cfg.ui.max_graphs)?;
    cfg.ui.mouse = confirm("Enable mouse controls?", cfg.ui.mouse)?;
    cfg.execution.prefer_single_tx = confirm("Prefer one atomic transaction?", cfg.execution.prefer_single_tx)?;
    cfg.execution.confirm_timeout_ms =
        prompt_number("Manual confirmation timeout (ms)", cfg.execution.confirm_timeout_ms)?;
    cfg.execution.bundle_timeout_ms = prompt_number("Bundle landing timeout (ms)", cfg.execution.bundle_timeout_ms)?;
    Ok(())
}

// ------------------------------------------------- prompts (visual or plain)

fn step(current: usize, total: usize, title: &str, details: &[&str]) {
    #[cfg(test)]
    if script::active() {
        let details: Vec<String> = details.iter().map(|d| tr(d).into_owned()).collect();
        return script::log(format!("step {current}/{total} {} :: {}", tr(title), details.join(" / ")));
    }
    if setup_ui::active() {
        setup_ui::step(current, total, title, details);
    } else {
        let heading = format!("{current}/{total}  {}", tr(title));
        println!("\n{heading}\n{}", "─".repeat(heading.chars().count()));
        for detail in details {
            println!("{}", tr(detail));
        }
    }
}

fn note(title: &str, rows: &[(&str, String)]) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        let body: Vec<String> = rows.iter().map(|(k, v)| format!("{}={}", tr(k), tr(v))).collect();
        script::log(format!("note {}: {}", tr(title), body.join(" | ")));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::note(title, rows);
    }
    println!("\n{}", tr(title));
    let rows: Vec<(String, String)> = rows.iter().map(|(k, v)| (tr(k).into_owned(), tr(v).into_owned())).collect();
    let width = rows.iter().map(|(k, _)| unicode_width::UnicodeWidthStr::width(k.as_str())).max().unwrap_or(0);
    for (key, value) in &rows {
        if key.is_empty() {
            println!("  {value}");
        } else {
            let pad = width - unicode_width::UnicodeWidthStr::width(key.as_str());
            println!("  {key}{}  {value}", " ".repeat(pad));
        }
    }
    Ok(())
}

fn outline(title: &str, rows: &[(&str, String)]) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        return note(title, rows);
    }
    if setup_ui::active() {
        return setup_ui::outline(title, rows);
    }
    note(title, rows)
}

fn qr(data: &str, address: &str, caption: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!("qr {data}"));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::qr(data, address, caption);
    }
    println!("  {}\n  {address}", tr(caption));
    Ok(())
}

fn check_line(ok: bool, optional: bool, name: &str, detail: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!(
            "check {} {name} {detail}",
            if ok {
                "ok"
            } else if optional {
                "warn"
            } else {
                "FAIL"
            }
        ));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::check_line(ok, optional, name, detail);
    }
    println!(
        "  {} {name:<17} {detail}",
        if ok {
            "ok  "
        } else if optional {
            "warn"
        } else {
            "FAIL"
        }
    );
    Ok(())
}

fn success(message: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!("success {}", tr(message)));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::success(message);
    }
    println!("✓ {}", tr(message));
    Ok(())
}

fn info(message: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!("info {}", tr(message)));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::info(message);
    }
    println!("  {}", tr(message));
    Ok(())
}

fn warn(message: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!("warn {}", tr(message)));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::warn(message);
    }
    println!("! {}", tr(message));
    Ok(())
}

fn finish(title: &str, hint: &str) -> Result<()> {
    #[cfg(test)]
    if script::active() {
        script::log(format!("finish {} :: {}", tr(title), tr(hint)));
        return Ok(());
    }
    if setup_ui::active() {
        return setup_ui::finish(title, hint);
    }
    println!("\n{} · {}", tr(title), tr(hint));
    Ok(())
}

fn menu(prompt: &str, items: &[(&str, &str, Option<&str>)], default: usize) -> Result<usize> {
    let items: Vec<MenuItem<'_>> =
        items.iter().map(|(title, description, badge)| MenuItem { title, description, badge: *badge }).collect();
    menu_items(prompt, &items, default)
}

fn menu_items(prompt: &str, items: &[MenuItem<'_>], default: usize) -> Result<usize> {
    #[cfg(test)]
    if let Some(answer) = script::menu(prompt, items, default) {
        return answer;
    }
    if let Some(value) = setup_ui::prompt_menu(prompt, items, default)? {
        return Ok(value);
    }
    println!("\n{}", tr(prompt));
    for (index, item) in items.iter().enumerate() {
        let badge = item.badge.map_or(String::new(), |value| format!(" ({})", tr(value)));
        println!("  {}. {}{badge}", index + 1, tr(item.title));
        if !item.description.is_empty() {
            println!("     {}", tr(item.description));
        }
    }
    loop {
        let value = read_plain(&format!("Choice [{}]: ", default + 1), false)?;
        if value.trim().is_empty() {
            return Ok(default);
        }
        if let Ok(number) = value.trim().parse::<usize>()
            && (1..=items.len()).contains(&number)
        {
            return Ok(number - 1);
        }
        println!("Choose a number from 1 to {}.", items.len());
    }
}

fn choice(prompt: &str, values: &[&str], default: usize) -> Result<usize> {
    let items: Vec<(&str, &str, Option<&str>)> = values.iter().map(|v| (*v, "", None)).collect();
    menu(prompt, &items, default)
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    #[cfg(test)]
    if let Some(answer) = script::confirm(prompt, default) {
        return answer;
    }
    if let Some(value) = setup_ui::prompt_bool(prompt, default)? {
        return Ok(value);
    }
    loop {
        let hint = if default { "Y/n" } else { "y/N" };
        let value = read_plain(&format!("{} [{hint}]: ", tr(prompt)), false)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please enter y or n."),
        }
    }
}

/// One line of text; validation errors re-ask instead of failing.
fn ask(spec: &TextPrompt<'_>) -> Result<String> {
    #[cfg(test)]
    if let Some(answer) = script::text(spec) {
        return answer;
    }
    if let Some(value) = setup_ui::prompt_text(spec)? {
        return Ok(value);
    }
    let hint = if spec.placeholder.is_empty() { String::new() } else { format!(" [{}]", tr(spec.placeholder)) };
    loop {
        let value = read_plain(&format!("{}{hint}: ", tr(spec.label)), spec.hidden)?;
        match spec.validate.map_or(Ok(()), |validate| validate(value.trim())) {
            Ok(()) => return Ok(value),
            Err(reason) => println!("{}", tr(&reason)),
        }
    }
}

fn prompt_text(label: &str, current: &str) -> Result<String> {
    let value = ask(&TextPrompt {
        label,
        placeholder: &format!("enter keeps {current}"),
        empty_answer: current,
        hidden: false,
        validate: None,
    })?;
    Ok(if value.trim().is_empty() { current.to_string() } else { value.trim().to_string() })
}

fn prompt_number<T>(label: &str, current: T) -> Result<T>
where
    T: Copy + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let check = |value: &str| -> std::result::Result<(), String> {
        if value.is_empty() {
            Ok(())
        } else {
            value.parse::<T>().map(|_| ()).map_err(|e| format!("Not a valid number: {e}"))
        }
    };
    let shown = current.to_string();
    let value = ask(&TextPrompt {
        label,
        placeholder: &format!("enter keeps {shown}"),
        empty_answer: &shown,
        hidden: false,
        validate: Some(&check),
    })?;
    if value.trim().is_empty() {
        return Ok(current);
    }
    value.trim().parse::<T>().map_err(|e| anyhow::anyhow!("{label}: {e}"))
}

fn prompt_sol(label: &str, current_lamports: u64) -> Result<u64> {
    let check = |value: &str| -> std::result::Result<(), String> {
        if value.is_empty() { Ok(()) } else { parse_sol_lamports(value).map(|_| ()).map_err(|e| e.to_string()) }
    };
    let shown = format!("{} SOL", format_sol(current_lamports));
    let value = ask(&TextPrompt {
        label: &format!("{label} (SOL)"),
        placeholder: &format!("enter keeps {shown}"),
        empty_answer: &shown,
        hidden: false,
        validate: Some(&check),
    })?;
    if value.trim().is_empty() {
        return Ok(current_lamports);
    }
    parse_sol_lamports(value.trim())
}

fn prompt_csv(label: &str, current: &[String]) -> Result<Vec<String>> {
    let shown = current.join(", ");
    let value = ask(&TextPrompt {
        label: &format!("{label} (comma-separated)"),
        placeholder: &format!("enter keeps {shown}"),
        empty_answer: &shown,
        hidden: false,
        validate: None,
    })?;
    let values = value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).collect::<Vec<_>>();
    Ok(if values.is_empty() { current.to_vec() } else { values })
}

fn read_plain(prompt: &str, hidden: bool) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    if hidden && io::stdin().is_terminal() && io::stdout().is_terminal() {
        return read_hidden();
    }
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(trim_newline(line))
}

fn read_hidden() -> Result<String> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    enable_raw_mode()?;
    let _guard = RawGuard;
    let mut value = String::new();
    loop {
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                print!("\r\n");
                return Ok(value);
            }
            KeyCode::Esc => {
                print!("\r\n");
                return Err(setup_ui::cancelled());
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                print!("\r\n");
                return Err(setup_ui::cancelled());
            }
            KeyCode::Backspace => {
                if value.pop().is_some() {
                    print!("\u{8} \u{8}");
                    io::stdout().flush()?;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                value.push(c);
                print!("•");
                io::stdout().flush()?;
            }
            _ => {}
        }
    }
}

fn change(value: String) -> Option<Option<String>> {
    match value.trim() {
        "" => None,
        "-" => Some(None),
        value => Some(Some(value.to_string())),
    }
}

fn state(had: bool, change: &Option<Option<String>>, present: &str, absent: &str) -> String {
    let on = match change {
        Some(Some(_)) => true,
        Some(None) => false,
        None => had,
    };
    if on { present } else { absent }.to_string()
}

fn parse_sol_lamports(value: &str) -> Result<u64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        bail!("trade size must be a positive decimal SOL amount");
    }
    if frac.len() > 9 {
        bail!("trade size supports at most 9 decimal places");
    }
    let whole: u64 = whole.parse().context("trade size is too large")?;
    let frac: u64 =
        if frac.is_empty() { 0 } else { format!("{frac:0<9}").parse().context("invalid fractional SOL amount")? };
    let lamports =
        whole.checked_mul(1_000_000_000).and_then(|v| v.checked_add(frac)).context("trade size is too large")?;
    if lamports == 0 {
        bail!("trade size must be greater than zero");
    }
    Ok(lamports)
}

fn format_sol(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = lamports % 1_000_000_000;
    if frac == 0 {
        return whole.to_string();
    }
    format!("{whole}.{frac:09}").trim_end_matches('0').to_string()
}

fn env_has(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        line.split_once('=').is_some_and(|(k, v)| k.trim() == key && !v.trim().is_empty())
    })
}

/// Writes only what differs from the shared layers below the user file, then
/// reloads every layer and checks the result is exactly `cfg`; on any
/// mismatch the previous file is put back.
fn write_config(path: &Path, below: &Config, cfg: &Config) -> Result<()> {
    let delta = config::user_delta(below, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let text = format!(
        "# MØBIUS-Searcher: your settings, layered over {}.\n# Only what differs from the shared defaults. Written by `mobius-searcher --setup`.\n\n{delta}",
        config::REPO_CONFIG_PATH
    );
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let previous = backup(path)?;
    atomic_write(path, text.as_bytes())?;
    let reloaded = config::load_layered(Path::new(config::REPO_CONFIG_PATH), path).map_err(|e| anyhow::anyhow!("{e}"));
    let same =
        reloaded.as_ref().is_ok_and(|l| toml::Value::try_from(&l.config).ok() == toml::Value::try_from(cfg).ok());
    if !same {
        match &previous {
            Some(saved) => fs::copy(saved, path).map(drop),
            None => fs::remove_file(path),
        }
        .with_context(|| format!("restoring {}", path.display()))?;
        bail!("the saved settings did not load back identically; {} was left as it was", path.display());
    }
    Ok(())
}

fn write_env(path: &Path, changes: &[(&str, Option<&Option<String>>)]) -> Result<()> {
    let mut text = fs::read_to_string(path).unwrap_or_default();
    for (key, value) in changes {
        let Some(value) = value else { continue };
        text = set_env_line(&text, key, value.as_deref())?;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    atomic_write_with_mode(path, text.as_bytes(), true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_env_line(text: &str, key: &str, value: Option<&str>) -> Result<String> {
    if let Some(v) = value
        && (v.contains(['\n', '\r', '\'']))
    {
        bail!("{key} contains a character that cannot be stored safely in .env");
    }
    let replacement = value.map(|v| format!("{key}='{v}'"));
    let mut out = Vec::new();
    let mut replaced = false;
    for line in text.lines() {
        let candidate = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        if candidate.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            if !replaced && let Some(line) = &replacement {
                out.push(line.clone());
            }
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced && let Some(line) = replacement {
        out.push(line);
    }
    Ok(if out.is_empty() { String::new() } else { format!("{}\n", out.join("\n")) })
}

fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut candidate = PathBuf::from(format!("{}.bak", path.display()));
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}.bak.{n}", path.display()));
        n += 1;
    }
    fs::copy(path, &candidate).with_context(|| format!("backing up {} to {}", path.display(), candidate.display()))?;
    info(&format!("Previous settings backed up to {}", display_path(&candidate)))?;
    Ok(Some(candidate))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_with_mode(path, contents, false)
}

fn atomic_write_with_mode(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    if let Err(e) = (|| -> io::Result<()> {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })() {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn write_marker(path: &Path) -> Result<()> {
    atomic_write(path, format!("setup_version={SETUP_VERSION}\n").as_bytes())
}

fn trim_newline(mut s: String) -> String {
    while matches!(s.chars().last(), Some('\n' | '\r')) {
        s.pop();
    }
    s
}

/// Scripted answers for driving whole setup flows in tests: every prompt
/// takes the next answer (menus match an option by the start of its title),
/// every line of output is logged instead of printed.
#[cfg(test)]
mod script {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[derive(Clone, Debug)]
    pub enum A {
        /// Pick the first option whose title starts with this.
        Pick(&'static str),
        /// Accept the highlighted default (menu or yes/no).
        Default,
        Yes,
        No,
        /// Type this and press Enter ("" = keep the shown value).
        Text(&'static str),
        /// What the next connection test reports.
        Checks(Vec<Check>),
        Esc,
    }

    struct Script {
        answers: VecDeque<A>,
        log: Vec<String>,
    }

    thread_local! {
        static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
        static BALANCE: RefCell<Option<u64>> = const { RefCell::new(None) };
    }

    /// What `getBalance` answers in this test (default: unavailable).
    pub fn set_balance(lamports: Option<u64>) {
        BALANCE.with(|b| *b.borrow_mut() = lamports);
    }

    pub fn balance() -> Option<u64> {
        BALANCE.with(|b| *b.borrow())
    }

    pub fn install(answers: Vec<A>) {
        SCRIPT.with(|s| *s.borrow_mut() = Some(Script { answers: answers.into(), log: Vec::new() }));
    }

    /// Ends the script; every answer must have been used.
    pub fn take() -> Vec<String> {
        let script = SCRIPT.with(|s| s.borrow_mut().take()).expect("a script was installed");
        assert!(script.answers.is_empty(), "unused answers {:?}\nlog:\n{}", script.answers, script.log.join("\n"));
        script.log
    }

    pub fn active() -> bool {
        SCRIPT.with(|s| s.borrow().is_some())
    }

    pub fn log(line: String) {
        SCRIPT.with(|s| {
            if let Some(script) = s.borrow_mut().as_mut() {
                script.log.push(line);
            }
        });
    }

    fn next(prompt: &str) -> A {
        SCRIPT.with(|s| {
            let mut s = s.borrow_mut();
            let script = s.as_mut().expect("script active");
            script
                .answers
                .pop_front()
                .unwrap_or_else(|| panic!("script ran out of answers at {prompt:?}\nlog:\n{}", script.log.join("\n")))
        })
    }

    pub fn menu(prompt: &str, items: &[MenuItem<'_>], default: usize) -> Option<Result<usize>> {
        if !active() {
            return None;
        }
        let pick = match next(prompt) {
            A::Esc => return Some(Err(setup_ui::cancelled())),
            A::Default => default,
            A::Pick(start) => items.iter().position(|i| i.title.starts_with(start)).unwrap_or_else(|| {
                let titles: Vec<&str> = items.iter().map(|i| i.title).collect();
                panic!("{prompt:?} has no option starting with {start:?}: {titles:?}")
            }),
            other => panic!("{prompt:?} is a menu, the script has {other:?}"),
        };
        let shown: Vec<String> = items
            .iter()
            .map(|i| format!("{} {} {}", tr(i.title), tr(i.description), i.badge.map(tr).unwrap_or_default()))
            .collect();
        log(format!("menu {} -> {} [{}]", tr(prompt), tr(items[pick].title), shown.join(" | ")));
        Some(Ok(pick))
    }

    pub fn confirm(prompt: &str, default: bool) -> Option<Result<bool>> {
        if !active() {
            return None;
        }
        let yes = match next(prompt) {
            A::Esc => return Some(Err(setup_ui::cancelled())),
            A::Default => default,
            A::Yes => true,
            A::No => false,
            other => panic!("{prompt:?} is yes/no, the script has {other:?}"),
        };
        log(format!("confirm {} -> {yes}", tr(prompt)));
        Some(Ok(yes))
    }

    pub fn probe() -> Option<Result<Vec<Check>>> {
        if !active() {
            return None;
        }
        match next("connection test") {
            A::Checks(checks) => {
                log(format!("probe {} checks", checks.len()));
                Some(Ok(checks))
            }
            other => panic!("a connection test runs here, the script has {other:?}"),
        }
    }

    pub fn text(spec: &TextPrompt<'_>) -> Option<Result<String>> {
        if !active() {
            return None;
        }
        loop {
            let value = match next(spec.label) {
                A::Esc => return Some(Err(setup_ui::cancelled())),
                A::Default => "",
                A::Text(value) => value,
                other => panic!("{:?} is a text prompt, the script has {other:?}", spec.label),
            };
            match spec.validate.map_or(Ok(()), |validate| validate(value.trim())) {
                Ok(()) => {
                    let shown = if spec.hidden && !value.is_empty() { "<hidden>" } else { value };
                    log(format!("text {} [{}] -> {shown}", tr(spec.label), tr(spec.placeholder)));
                    return Some(Ok(value.to_string()));
                }
                Err(reason) => log(format!("rejected {} -> {}", tr(spec.label), tr(&reason))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sol_amounts_are_exact() {
        assert_eq!(parse_sol_lamports("0.01").unwrap(), 10_000_000);
        assert_eq!(parse_sol_lamports("1.000000001").unwrap(), 1_000_000_001);
        assert_eq!(format_sol(10_000_000), "0.01");
        assert!(parse_sol_lamports("0").is_err());
        assert!(parse_sol_lamports(".1").is_err());
        assert!(parse_sol_lamports("0.0000000001").is_err());
    }

    #[test]
    fn env_updates_preserve_unrelated_lines_and_never_duplicate() {
        let text = "# local\nOTHER=x\nexport JUPITER_API_KEY=old\n";
        let updated = set_env_line(text, "JUPITER_API_KEY", Some("new=key")).unwrap();
        assert!(updated.contains("# local\nOTHER=x\n"));
        assert_eq!(updated.matches("JUPITER_API_KEY").count(), 1);
        assert!(updated.contains("JUPITER_API_KEY='new=key'"));
        let removed = set_env_line(&updated, "JUPITER_API_KEY", None).unwrap();
        assert!(!removed.contains("JUPITER_API_KEY"));
    }

    #[test]
    fn recommended_profile_forces_paper_and_clears_private_key() {
        let mut cfg = Config::default();
        cfg.general.mode = Mode::Live;
        cfg.execution.live_enabled = true;
        cfg.wallet.keypair_path = Some("/secret.json".into());
        lock_to_paper(&mut cfg);
        assert_eq!(cfg.general.mode, Mode::Paper);
        assert!(!cfg.execution.live_enabled);
        assert!(cfg.wallet.keypair_path.is_none());
    }

    #[test]
    fn live_unlock_requires_the_exact_phrase_and_keypair_path() {
        let mut cfg = Config::default();
        cfg.general.mode = Mode::Live;
        assert!(unlock_transaction_mode(&mut cfg, "/wallet.json".into(), "yes").is_err());
        assert!(!cfg.execution.live_enabled);
        assert!(unlock_transaction_mode(&mut cfg, String::new(), "ENABLE LIVE").is_err());
        unlock_transaction_mode(&mut cfg, "/wallet.json".into(), "ENABLE LIVE").unwrap();
        assert!(cfg.execution.live_enabled);
        assert_eq!(cfg.wallet.keypair_path.as_deref(), Some("/wallet.json"));
    }

    #[test]
    fn saving_writes_only_the_delta_and_reloads_identically() {
        let dir = std::env::temp_dir().join(format!("mobius-setup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let user = dir.join("config.toml");
        let below = config::load_layered(Path::new(config::REPO_CONFIG_PATH), &user).unwrap().below_user;
        let mut cfg = below.clone();
        cfg.wallet.pubkey = Some("So11111111111111111111111111111111111111112".into());
        cfg.execution.live_enabled = true;
        cfg.risk.max_trade_pct_of_equity_bps = 1_000;
        write_config(&user, &below, &cfg).unwrap();
        let text = fs::read_to_string(&user).unwrap();
        assert!(text.contains("live_enabled = true") && text.contains("So1111"), "{text}");
        assert!(!text.contains("[jupiter]"), "unchanged sections are not copied: {text}");
        let reloaded = config::load_layered(Path::new(config::REPO_CONFIG_PATH), &user).unwrap().config;
        assert_eq!(snapshot(&reloaded), snapshot(&cfg));
        // a second save backs the first one up instead of losing it
        cfg.risk.max_trade_pct_of_equity_bps = 500;
        write_config(&user, &below, &cfg).unwrap();
        assert!(
            fs::read_to_string(dir.join("config.toml.bak")).unwrap().contains("max_trade_pct_of_equity_bps = 1000")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn onboarding_never_runs_by_itself_for_an_existing_setup() {
        let dir = std::env::temp_dir().join(format!("mobius-setup-auto-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (marker, user) = (dir.join("setup-complete"), dir.join("config.toml"));
        assert!(!should_auto_run(&marker, &user, false), "automation never stops for questions");
        fs::write(&user, "[execution]\nlive_enabled = true\n").unwrap();
        assert!(!should_auto_run(&marker, &user, true), "a user config means setup already happened");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn guided_policies_derive_limits_without_asking_for_a_sol_amount() {
        let mut cfg = Config::default();
        apply_strategy_scope(&mut cfg, 0);
        apply_safety_policy(&mut cfg, 0);
        assert!(cfg.strategies.round_trip[0].enabled);
        assert!(cfg.strategies.cross_dex[0].enabled);
        assert!(cfg.strategies.triangular.iter().all(|strategy| !strategy.enabled));
        assert_eq!(cfg.risk.max_trade_pct_of_equity_bps, 100);
        assert_eq!(cfg.risk.max_trade_lamports, 10_000_000);
        assert_eq!(cfg.risk.max_consecutive_failures, 3);
        assert_eq!(cfg.risk.max_open_executions, 1);
        assert!(cfg.profit.protect_min_out);
        cfg.validate().unwrap();
    }

    // ---------------------------------------------------------- whole flows

    use super::script::A::{self, *};
    use crate::doctor::Check;

    fn check(name: &str, ok: bool) -> Check {
        Check { ok, optional: false, name: name.into(), target: "https://x".into(), detail: "d".into() }
    }

    /// A connection test where everything answers.
    fn ok() -> A {
        Checks(vec![check("Solana RPC", true), check("Solana WebSocket", true), check("Jupiter", true)])
    }

    /// A connection test where Jupiter does not answer.
    fn jupiter_down() -> A {
        Checks(vec![check("Solana RPC", true), check("Solana WebSocket", true), check("Jupiter", false)])
    }

    /// A throwaway user directory: config, secrets, marker and wallets all
    /// live here, never in the real ~/.config.
    struct Sandbox {
        dir: PathBuf,
        lang: Lang,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mobius-flow-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir, lang: Lang::En }
        }
        fn chinese(mut self) -> Self {
            self.lang = Lang::Zh;
            self
        }
        fn config(&self) -> PathBuf {
            self.dir.join("config.toml")
        }
        fn env(&self) -> PathBuf {
            self.dir.join(".env")
        }
        fn marker(&self) -> PathBuf {
            self.dir.join("setup-complete")
        }
        fn with_config(self, text: &str) -> Self {
            fs::write(self.config(), text).unwrap();
            self
        }
        fn run(&self, answers: Vec<A>) -> (Result<Outcome>, Vec<String>) {
            script::install(answers);
            let opts = SetupOpts { lang: Some(self.lang), ..SetupOpts::default() };
            let outcome = run(&self.config(), &self.env(), &self.marker(), true, &opts);
            i18n::set(Lang::En);
            (outcome, script::take())
        }
        fn unattended(&self, args: &[&str]) -> (Result<Outcome>, Vec<String>) {
            use clap::Parser;
            #[derive(Parser)]
            struct Cli {
                #[arg(long)]
                setup: bool,
                #[command(flatten)]
                opts: SetupOpts,
            }
            let mut cli = Cli::try_parse_from(["mobius-searcher", "--setup", "--yes"].iter().chain(args)).unwrap();
            cli.opts.lang.get_or_insert(self.lang);
            script::install(Vec::new());
            let outcome = run(&self.config(), &self.env(), &self.marker(), true, &cli.opts);
            i18n::set(Lang::En);
            (outcome, script::take())
        }
        fn effective(&self) -> Config {
            config::load_layered(Path::new(config::REPO_CONFIG_PATH), &self.config()).unwrap().config
        }
        fn files(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut stack = vec![self.dir.clone()];
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(dir).unwrap().flatten() {
                    if entry.path().is_dir() {
                        stack.push(entry.path());
                    } else {
                        out.push(entry.path().strip_prefix(&self.dir).unwrap().display().to_string());
                    }
                }
            }
            out.sort();
            out
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn mode_bits(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn research_first_use_creates_a_wallet_and_saves_only_a_delta() {
        let sb = Sandbox::new("research");
        let (outcome, log) =
            sb.run(vec![Pick("Research"), Pick("Create"), Pick("Public"), ok(), Pick("Core"), Pick("Guarded"), Yes]);
        assert!(!outcome.unwrap().start_now, "--setup never starts the engine");
        let cfg = sb.effective();
        assert_eq!(cfg.general.mode, Mode::Paper);
        assert!(!cfg.execution.live_enabled);
        let keypair = PathBuf::from(cfg.wallet.keypair_path.clone().unwrap());
        assert_eq!(keypair, sb.dir.join("wallets/bot-keypair.json"), "wallets live next to the config");
        assert_eq!(mode_bits(&keypair), 0o600);
        let wallet = Wallet::load(&keypair, None).unwrap();
        assert_eq!(cfg.wallet.pubkey.as_deref(), Some(wallet.pubkey().to_string().as_str()));
        assert_eq!(cfg.risk.max_trade_lamports, 10_000_000);
        assert!(cfg.strategies.cross_dex[0].enabled && !cfg.strategies.triangular[0].enabled);
        let text = fs::read_to_string(sb.config()).unwrap();
        assert!(text.contains("[wallet]") && !text.contains("[jupiter]"), "only the delta: {text}");
        assert!(sb.marker().exists());
        assert!(!sb.env().exists(), "no secrets were entered, so no .env");
        assert!(log.iter().any(|l| l.starts_with("warn Back up")), "{log:#?}");
    }

    #[test]
    fn cancelling_or_declining_writes_nothing() {
        for answers in [
            vec![Esc],
            vec![Pick("Research"), Esc],
            vec![Pick("Research"), Pick("Create"), Pick("Public"), ok(), Pick("Core"), Esc],
            vec![
                Pick("Assisted"),
                Pick("Create"),
                Pick("Public"),
                ok(),
                Pick("Core"),
                Pick("Guarded"),
                Pick("Unlock"),
                Esc,
            ],
        ] {
            let sb = Sandbox::new("cancel");
            let (outcome, _) = sb.run(answers.clone());
            assert!(is_cancelled(&outcome.err().expect("cancelled")), "{answers:?}");
            assert!(sb.files().is_empty(), "{answers:?} wrote {:?}", sb.files());
        }
        let sb = Sandbox::new("decline");
        let (outcome, log) =
            sb.run(vec![Pick("Research"), Pick("Create"), Pick("Public"), ok(), Pick("Core"), Pick("Guarded"), No]);
        assert!(!outcome.unwrap().start_now);
        assert!(sb.files().is_empty(), "declined save wrote {:?}", sb.files());
        assert!(log.iter().any(|l| l == "info Nothing was written."), "{log:#?}");
    }

    #[test]
    fn assisted_unlocks_confirm_only_with_the_exact_phrase() {
        let sb = Sandbox::new("assisted");
        let (outcome, log) = sb.run(vec![
            Pick("Assisted"),
            Pick("Create"),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Pick("Unlock"),
            Text("enable confirm"),
            Text("ENABLE CONFIRM"),
            Yes,
        ]);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("rejected Type ENABLE CONFIRM")), "{log:#?}");
        let cfg = sb.effective();
        assert_eq!(cfg.general.mode, Mode::Confirm);
        assert!(cfg.execution.live_enabled);
        assert!(Path::new(cfg.wallet.keypair_path.as_deref().unwrap()).exists());
    }

    #[test]
    fn assisted_can_stay_in_paper_and_keep_the_new_wallet() {
        let sb = Sandbox::new("assisted-paper");
        let answers = vec![
            Pick("Assisted"),
            Pick("Create"),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Pick("Stay"),
            Yes,
        ];
        sb.run(answers).0.unwrap();
        let cfg = sb.effective();
        assert_eq!(cfg.general.mode, Mode::Paper);
        assert!(!cfg.execution.live_enabled);
        assert!(cfg.wallet.keypair_path.is_some(), "the wallet is kept for later");
    }

    #[test]
    fn an_existing_keypair_or_a_watched_address_is_checked_before_it_is_used() {
        let sb = Sandbox::new("existing-keypair");
        let keypair = sb.dir.join("mine.json");
        let generated = GeneratedWallet::new();
        generated.write_new(&keypair).unwrap();
        let leaked = keypair.display().to_string().leak();
        let (outcome, log) = sb.run(vec![
            Pick("Research"),
            Pick("Use an existing"),
            Text("/no/such/keypair.json"),
            Text(leaked),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ]);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("rejected Keypair file")), "{log:#?}");
        let cfg = sb.effective();
        assert_eq!(cfg.wallet.pubkey, Some(generated.pubkey().to_string()));
        assert_eq!(cfg.wallet.keypair_path.as_deref(), Some(leaked as &str));

        let sb = Sandbox::new("watch");
        let address = "So11111111111111111111111111111111111111112";
        let answers = vec![
            Pick("Research"),
            Pick("Watch"),
            Text("not-an-address"),
            Text(address),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ];
        let (outcome, log) = sb.run(answers);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("rejected Wallet address")), "{log:#?}");
        let cfg = sb.effective();
        assert_eq!(cfg.wallet.pubkey.as_deref(), Some(address));
        assert!(cfg.wallet.keypair_path.is_none(), "watch only");
    }

    #[test]
    fn advanced_first_use_keeps_the_recommended_values_on_enter() {
        let sb = Sandbox::new("advanced");
        let mut answers = vec![Pick("Advanced"), Pick("PAPER"), Pick("No wallet"), Text("2.5")];
        answers.extend([Default, Default, Default, Default, Default]); // five optional secrets
        answers.extend([Pick("Keep"), ok()]); // network limits, then the connection test
        answers.extend([Default, Default, Default, Default]); // round-trip: on, quote, amount, weight
        answers.extend([Default, Default, Default, Default, Default]); // cross-DEX: on, quote, amount, dexes, weight
        answers.push(Default); // triangular stays off
        answers.extend([Pick("Keep"), Pick("Keep"), Pick("Keep"), Pick("Keep"), Pick("Keep"), Yes]);
        sb.run(answers).0.unwrap();
        let cfg = sb.effective();
        assert_eq!(cfg.paper.equity_lamports, 2_500_000_000);
        assert_eq!(cfg.risk.max_trade_pct_of_equity_bps, 100, "guarded until customised");
        assert!(cfg.wallet.pubkey.is_none());
        assert!(!cfg.strategies.triangular[0].enabled);
    }

    const EXISTING: &str = "[execution]\nlive_enabled = true\n\n[risk]\nmax_trade_pct_of_equity_bps = 1000\n\n\
                            [wallet]\npubkey = \"So11111111111111111111111111111111111111112\"\n\
                            keypair_path = \"/somewhere/hot-wallet.json\"\n";

    #[test]
    fn keeping_an_existing_setup_writes_nothing() {
        let sb = Sandbox::new("keep").with_config(EXISTING);
        let (outcome, log) = sb.run(vec![Pick("Keep")]);
        outcome.unwrap();
        assert_eq!(fs::read_to_string(sb.config()).unwrap(), EXISTING);
        assert_eq!(sb.files(), vec!["config.toml".to_string()]);
        assert!(log[0].contains("Wallet=So1111") && log[0].contains("hot-wallet.json"), "shown first: {}", log[0]);
    }

    #[test]
    fn changing_one_part_leaves_everything_else_untouched() {
        let sb = Sandbox::new("edit").with_config(EXISTING);
        let before = sb.effective();
        let answers = vec![Pick("Change"), Pick("Markets"), Pick("okx"), No, Pick("Review"), Yes];
        sb.run(answers).0.unwrap();
        let mut after = sb.effective();
        assert!(!after.venues["okx"].enabled);
        after.venues.get_mut("okx").unwrap().enabled = true;
        assert_eq!(snapshot(&after), snapshot(&before), "only the venue switch changed");
        assert_eq!(fs::read_to_string(sb.dir.join("config.toml.bak")).unwrap(), EXISTING);
        assert!(!sb.env().exists());
    }

    #[test]
    fn nothing_to_change_in_the_hub_writes_nothing() {
        let sb = Sandbox::new("hub-noop").with_config(EXISTING);
        let answers = vec![Pick("Change"), Pick("Wallet"), Pick("Keep"), Pick("Safety"), Pick("Keep"), Pick("Nothing")];
        sb.run(answers).0.unwrap();
        assert_eq!(fs::read_to_string(sb.config()).unwrap(), EXISTING);
        assert_eq!(sb.files(), vec!["config.toml".to_string()]);
    }

    #[test]
    fn switching_to_live_needs_the_exact_phrase() {
        let sb = Sandbox::new("to-live").with_config(EXISTING);
        let (outcome, log) = sb.run(vec![
            Pick("Change"),
            Pick("Mode"),
            Pick("LIVE"),
            Text("ENABLE CONFIRM"),
            Text("ENABLE LIVE"),
            Pick("Review"),
            Yes,
        ]);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("rejected Type ENABLE LIVE")), "{log:#?}");
        let cfg = sb.effective();
        assert_eq!(cfg.general.mode, Mode::Live);
        assert!(cfg.execution.live_enabled);
        assert_eq!(cfg.wallet.keypair_path.as_deref(), Some("/somewhere/hot-wallet.json"));
    }

    #[test]
    fn starting_over_offers_to_keep_the_current_wallet() {
        let sb = Sandbox::new("start-over").with_config(EXISTING);
        let answers = vec![
            Pick("Start over"),
            Pick("Research"),
            Pick("Keep"),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ];
        sb.run(answers).0.unwrap();
        let cfg = sb.effective();
        assert_eq!(cfg.wallet.pubkey.as_deref(), Some("So11111111111111111111111111111111111111112"));
        assert_eq!(cfg.wallet.keypair_path.as_deref(), Some("/somewhere/hot-wallet.json"));
        assert_eq!(cfg.general.mode, Mode::Paper);
        assert!(!cfg.execution.live_enabled, "research mode locks sending");
        assert!(sb.dir.join("config.toml.bak").exists());
    }

    #[test]
    fn secrets_go_only_to_the_private_env_file_and_never_to_the_screen() {
        let sb = Sandbox::new("secrets");
        let (outcome, log) = sb.run(vec![
            Pick("Research"),
            Pick("Create"),
            Pick("My own"),
            Text("jup-secret-123456"),
            Text("ftp://rpc.example"),
            Text("https://rpc.example/rpc-secret-789"),
            Default,
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ]);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("rejected Private RPC URL")), "{log:#?}");
        let all = log.join("\n");
        assert!(!all.contains("jup-secret") && !all.contains("rpc-secret"), "a secret reached the output:\n{all}");
        let cfg = sb.effective();
        let env = fs::read_to_string(sb.env()).unwrap();
        assert!(env.contains(&format!("{}='jup-secret-123456'", cfg.jupiter.api_key_env)), "{env}");
        assert!(env.contains("rpc-secret-789"));
        assert_eq!(mode_bits(&sb.env()), 0o600);
        let config_text = fs::read_to_string(sb.config()).unwrap();
        assert!(
            !config_text.contains("jup-secret") && !config_text.contains("rpc-secret"),
            "secrets never go into the config: {config_text}"
        );
    }

    // ------------------------------------------------------ --setup --yes

    #[test]
    fn yes_flags_parse_only_together_with_setup() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[arg(long)]
            setup: bool,
            #[command(flatten)]
            opts: SetupOpts,
        }
        assert!(Cli::try_parse_from(["m", "--yes"]).is_err(), "--yes needs --setup");
        assert!(Cli::try_parse_from(["m", "--setup", "--wallet", "new"]).is_err(), "--wallet needs --yes");
        assert!(Cli::try_parse_from(["m", "--setup", "--yes", "--safety", "reckless"]).is_err());
        let cli = Cli::try_parse_from(["m", "--setup", "--yes", "--strategies", "round-trip", "--safety", "balanced"])
            .unwrap();
        assert_eq!((cli.opts.strategies, cli.opts.safety), (Some(ScopeArg::RoundTrip), Some(SafetyArg::Balanced)));
    }

    #[test]
    fn unattended_first_use_is_a_research_setup_without_a_wallet() {
        let sb = Sandbox::new("yes-first");
        let (outcome, log) = sb.unattended(&[]);
        assert!(!outcome.unwrap().start_now);
        let cfg = sb.effective();
        assert_eq!(cfg.general.mode, Mode::Paper);
        assert!(cfg.wallet.pubkey.is_none() && cfg.wallet.keypair_path.is_none(), "no wallet unless asked");
        assert_eq!(cfg.risk.max_trade_pct_of_equity_bps, 100);
        assert!(cfg.strategies.cross_dex[0].enabled && !cfg.strategies.triangular[0].enabled);
        assert_eq!(sb.files(), vec!["config.toml".to_string(), "setup-complete".to_string()]);
        assert!(log.iter().any(|l| l.starts_with("note Setup:")), "{log:#?}");
    }

    #[test]
    fn unattended_new_wallet_is_created_private() {
        let sb = Sandbox::new("yes-wallet");
        sb.unattended(&["--wallet", "new", "--strategies", "all"]).0.unwrap();
        let cfg = sb.effective();
        let keypair = PathBuf::from(cfg.wallet.keypair_path.unwrap());
        assert_eq!(mode_bits(&keypair), 0o600);
        assert!(cfg.strategies.triangular[0].enabled);
    }

    #[test]
    fn unattended_on_an_existing_setup_changes_only_what_is_asked() {
        let sb = Sandbox::new("yes-noop").with_config(EXISTING);
        let (outcome, log) = sb.unattended(&[]);
        outcome.unwrap();
        assert_eq!(fs::read_to_string(sb.config()).unwrap(), EXISTING);
        assert_eq!(sb.files(), vec!["config.toml".to_string()]);
        assert!(log.iter().any(|l| l.starts_with("info No changes")), "{log:#?}");

        let sb = Sandbox::new("yes-safety").with_config(EXISTING);
        let before = sb.effective();
        sb.unattended(&["--safety", "balanced"]).0.unwrap();
        let after = sb.effective();
        assert_eq!(after.risk.max_trade_pct_of_equity_bps, 250);
        assert_eq!(after.wallet.pubkey, before.wallet.pubkey);
        assert_eq!(after.execution.live_enabled, before.execution.live_enabled);
        assert_eq!(strategies_summary(&after), strategies_summary(&before), "strategy choice kept");
        assert_eq!(format!("{:?}", after.venues), format!("{:?}", before.venues));
    }

    #[test]
    fn unattended_errors_leave_every_file_alone() {
        let live = format!("{EXISTING}\n[general]\nmode = \"live\"\n");
        for args in [
            vec!["--wallet", "none"],
            vec!["--wallet", "watch:So11111111111111111111111111111111111111112"],
            vec!["--wallet", "/no/such/keypair.json"],
        ] {
            let sb = Sandbox::new("yes-err").with_config(&live);
            let (outcome, _) = sb.unattended(&args);
            assert!(outcome.is_err(), "{args:?} should fail");
            assert_eq!(fs::read_to_string(sb.config()).unwrap(), live, "{args:?}");
            assert_eq!(sb.files(), vec!["config.toml".to_string()], "{args:?}");
        }
        let sb = Sandbox::new("yes-bad-watch");
        assert!(sb.unattended(&["--wallet", "watch:nope"]).0.is_err());
        assert!(sb.files().is_empty());
    }

    // ------------------------------------------------ connection test (phase 3)

    #[test]
    fn a_failed_connection_offers_another_proxy_and_tests_again() {
        let sb = Sandbox::new("proxy");
        let (outcome, log) = sb.run(vec![
            Pick("Research"),
            Pick("Create"),
            Pick("Public"),
            jupiter_down(),
            Pick("Try another proxy"),
            Pick("Use this HTTP proxy"),
            Text("socks5://127.0.0.1:7897"),
            Text("http://127.0.0.1:7897"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ]);
        outcome.unwrap();
        assert!(log.iter().any(|l| l == "check FAIL Jupiter https://x · d"), "{log:#?}");
        assert!(log.iter().any(|l| l.starts_with("menu Jupiter did not answer")), "{log:#?}");
        assert!(log.iter().any(|l| l.starts_with("rejected HTTP proxy")), "{log:#?}");
        assert_eq!(sb.effective().network.proxy, "http://127.0.0.1:7897");
    }

    #[test]
    fn continuing_without_a_connection_still_saves_and_says_so() {
        let sb = Sandbox::new("offline");
        let answers = vec![
            Pick("Research"),
            Pick("No wallet"),
            Pick("Public"),
            jupiter_down(),
            Pick("Continue"),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ];
        let (outcome, log) = sb.run(answers);
        outcome.unwrap();
        assert!(log.iter().any(|l| l.starts_with("warn Saved without a working connection")), "{log:#?}");
        assert!(sb.config().exists());
    }

    #[test]
    fn the_hub_can_test_the_connection_without_changing_anything() {
        let sb = Sandbox::new("hub-test").with_config(EXISTING);
        let answers = vec![Pick("Change"), Pick("Network"), Pick("Test"), ok(), Pick("Nothing")];
        let (outcome, log) = sb.run(answers);
        outcome.unwrap();
        assert!(log.iter().any(|l| l == "probe 3 checks"), "{log:#?}");
        assert_eq!(fs::read_to_string(sb.config()).unwrap(), EXISTING);
    }

    #[test]
    fn doctor_checks_survive_the_json_round_trip() {
        let c = Check {
            ok: false,
            optional: true,
            name: "Jito tip stream".into(),
            target: "wss://…".into(),
            detail: "x".into(),
        };
        assert_eq!(Check::from_json(&c.to_json()), Some(c));
        assert_eq!(Check::from_json(&serde_json::json!({"ok": true})), None);
    }

    // ------------------------------------------------ wallet status (phase 4)

    #[test]
    fn a_new_signing_wallet_gets_a_funding_qr_code() {
        let sb = Sandbox::new("qr");
        let answers = vec![
            Pick("Assisted"),
            Pick("Create"),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Pick("Stay"),
            Yes,
        ];
        let (outcome, log) = sb.run(answers);
        outcome.unwrap();
        let pubkey = sb.effective().wallet.pubkey.unwrap();
        assert!(log.iter().any(|l| *l == format!("qr solana:{pubkey}")), "{log:#?}");
    }

    #[test]
    fn research_wallets_show_their_balance_but_need_no_funding() {
        let sb = Sandbox::new("balance");
        let keypair = sb.dir.join("mine.json");
        GeneratedWallet::new().write_new(&keypair).unwrap();
        script::set_balance(Some(500_000_000));
        let path = keypair.display().to_string().leak();
        let answers = vec![
            Pick("Research"),
            Pick("Use an existing"),
            Text(path),
            Pick("Public"),
            ok(),
            Pick("Core"),
            Pick("Guarded"),
            Yes,
        ];
        let (outcome, log) = sb.run(answers);
        script::set_balance(None);
        outcome.unwrap();
        assert!(log.iter().any(|l| l == "info Balance 0.5 SOL"), "{log:#?}");
        assert!(!log.iter().any(|l| l.starts_with("qr ")), "PAPER needs no funding: {log:#?}");
    }

    #[test]
    fn a_live_wallet_below_the_fee_reserve_is_asked_to_be_funded() {
        let live = format!("{EXISTING}\n[general]\nmode = \"live\"\n");
        let sb = Sandbox::new("underfunded").with_config(&live);
        script::set_balance(Some(1_000));
        let (outcome, log) = sb.run(vec![Pick("Change"), Pick("Wallet"), Pick("Keep"), Pick("Nothing")]);
        script::set_balance(None);
        outcome.unwrap();
        assert!(log.iter().any(|l| l == "info Balance 0.000001 SOL"), "{log:#?}");
        assert!(log.iter().any(|l| l == "qr solana:So11111111111111111111111111111111111111112"), "{log:#?}");
        assert_eq!(fs::read_to_string(sb.config()).unwrap(), live, "looking at the wallet changes nothing");
    }

    // ------------------------------------------------------ 中文 (phase 5)

    /// English left in a Chinese log line: two English words in a row that
    /// are not names, units or commands. Paths, URLs, numbers and the log's
    /// own prefix are ignored.
    fn untranslated(line: &str) -> Option<String> {
        const KEEP: &[&str] = &[
            "mobius", "searcher", "setup", "doctor", "mode", "lang", "confirm", "live", "paper", "env", "rtse", "bps",
            "sol", "micro", "lamports", "bundle", "slot", "base", "dont", "front", "block", "engine", "tip", "floor",
            "ctrl", "esc", "key", "okx", "true", "false", "enable", "http", "https",
        ];
        let words: Vec<&str> = line
            .split_whitespace()
            .skip(1) // menu / note / text / step …
            .filter(|w| !w.contains(['/', '.', '$', ':', '=', '-', '_']) && !w.chars().any(|c| c.is_ascii_digit()))
            .collect();
        // capitalised words are names (Solana RPC, Jupiter API, Solana CLI)
        let english: Vec<&str> = words.iter().flat_map(|w| w.split(|c: char| !c.is_ascii_alphabetic())).collect();
        english
            .windows(2)
            .find(|pair| {
                pair.iter().all(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_lowercase()) && !KEEP.contains(w))
            })
            .map(|pair| pair.join(" "))
    }

    fn assert_all_chinese(log: &[String]) {
        let left: Vec<String> = log.iter().filter_map(|l| untranslated(l).map(|w| format!("{w:?} in {l}"))).collect();
        assert!(left.is_empty(), "untranslated text:\n{}", left.join("\n"));
    }

    #[test]
    fn the_untranslated_check_itself_works() {
        assert!(untranslated("note 核对: 模式=PAPER · 只模拟，发送锁定").is_none());
        assert_eq!(untranslated("info Nothing was written here").as_deref(), Some("was written"));
        assert!(untranslated("text 私钥文件 [/private/var/folders/x.json] -> /tmp/a").is_none());
    }

    #[test]
    fn a_chinese_first_use_is_fully_translated() {
        for answers in [
            vec![Pick("Research"), Pick("Create"), Pick("Public"), ok(), Pick("Core"), Pick("Guarded"), Yes],
            vec![
                Pick("Assisted"),
                Pick("Create"),
                Pick("My own"),
                Text("jup-secret-123456"),
                Default,
                Default,
                jupiter_down(),
                Pick("Try another proxy"),
                Pick("Use this HTTP proxy"),
                Text("socks5://127.0.0.1:1"),
                Text("http://127.0.0.1:7897"),
                ok(),
                Pick("Everything"),
                Pick("Balanced"),
                Pick("Unlock"),
                Text("enable"),
                Text("ENABLE CONFIRM"),
                Yes,
            ],
        ] {
            let sb = Sandbox::new("zh").chinese();
            let (outcome, log) = sb.run(answers);
            outcome.unwrap();
            assert_all_chinese(&log);
            assert!(log.iter().any(|l| l.starts_with("finish 设置完成")), "{log:#?}");
        }
    }

    #[test]
    fn a_chinese_edit_of_an_existing_setup_is_fully_translated() {
        let sb = Sandbox::new("zh-hub").with_config(EXISTING).chinese();
        let (outcome, log) = sb.run(vec![
            Pick("Change"),
            Pick("Network"),
            Pick("Test"),
            ok(),
            Pick("Markets"),
            Pick("okx"),
            No,
            Pick("Safety"),
            Pick("Guarded"),
            Pick("Mode"),
            Pick("PAPER"),
            Pick("Review"),
            Yes,
        ]);
        outcome.unwrap();
        assert_all_chinese(&log);
        // the unlock phrase and product names stay as typed
        assert!(log.iter().any(|l| l.contains("当前设置")), "{log:#?}");
    }

    #[test]
    fn chinese_unattended_output_is_translated_and_english_stays_english() {
        let sb = Sandbox::new("zh-yes").chinese();
        let (outcome, log) = sb.unattended(&["--wallet", "new"]);
        outcome.unwrap();
        assert_all_chinese(&log);
        let sb = Sandbox::new("en-yes");
        let (_, log) = sb.unattended(&[]);
        assert!(log.iter().any(|l| l.starts_with("success Settings saved to")), "{log:#?}");
    }
}
