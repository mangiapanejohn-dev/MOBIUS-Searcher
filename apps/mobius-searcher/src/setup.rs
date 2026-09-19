//! First-run setup. Research mode (PAPER) is the recommended path; assisted
//! trading prepares CONFIRM only behind a typed unlock phrase; advanced setup
//! exposes every value. Nothing is written before the final review, and a
//! newly generated bot key stays in memory until then.

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

pub fn run(config_path: &Path, env_path: &Path, marker_path: &Path, explicit: bool) -> Result<Outcome> {
    // The effective settings come first: a file that does not load must be
    // fixed by hand, never silently replaced.
    let layered = config::load_layered(Path::new(config::REPO_CONFIG_PATH), config_path)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("reading the current settings (fix the file, or move it away to start fresh)")?;
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
    let env_texts: Vec<String> = std::iter::once(env_path.to_path_buf())
        .chain(crate::envfile::sources())
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    let has = |key: &str| std::env::var_os(key).is_some() || env_texts.iter().any(|text| env_has(text, key));
    let present = Present {
        jupiter: has(&jupiter_env),
        rpc: has(&rpc_env),
        ws: has(&ws_env),
        jito: has(&jito_env),
        pyth: has(&pyth_env),
    };

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
            1 => edit_sections(&mut cfg, &mut env_answers, &mut pending_wallet, present)?,
            _ => {
                cfg = below.clone();
                first_use_path =
                    Some(first_use(&mut cfg, &mut env_answers, &mut pending_wallet, present, current_wallet.as_ref())?);
                true
            }
        };
        if !changed {
            finish("No changes", "start: mobius-searcher")?;
            return Ok(Outcome { start_now: !explicit });
        }
    } else {
        first_use_path =
            Some(first_use(&mut cfg, &mut env_answers, &mut pending_wallet, present, current_wallet.as_ref())?);
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

/// The first-use flow: pick a goal, see its steps, walk them.
fn first_use(
    cfg: &mut Config,
    env: &mut EnvAnswers,
    pending_wallet: &mut Option<PendingWallet>,
    present: Present,
    current_wallet: Option<&CurrentWallet>,
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
    let keys: Vec<String> = steps.iter().enumerate().map(|(i, (title, _))| format!("{:>2}  {title}", i + 1)).collect();
    let plan: Vec<(&str, String)> =
        keys.iter().zip(steps).map(|(key, (_, what))| (key.as_str(), (*what).to_string())).collect();
    outline(&format!("{} · {} steps", path.label(), steps.len()), &plan)?;

    match path {
        SetupPath::Research | SetupPath::Assisted => {
            configure_guided(cfg, env, pending_wallet, path, present, current_wallet)?
        }
        SetupPath::Advanced => {
            // "keep the current values" should mean the recommended ones
            apply_strategy_scope(cfg, 0);
            apply_safety_policy(cfg, 0);
            configure_advanced(cfg, env, pending_wallet, present, current_wallet)?;
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
    if on.is_empty() { "none enabled".into() } else { on.join(", ") }
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
    present: Present,
) -> Result<bool> {
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
                configure_wallet(cfg, pending_wallet, signer, current.as_ref())?;
            }
            1 => {
                let update = menu(
                    "Network & API keys",
                    &[
                        ("Keep current", &network, Some("current")),
                        (
                            "Update keys and endpoints",
                            "Hidden input; Enter keeps each stored value, '-' clears it.",
                            None,
                        ),
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
            5 => edit_mode(cfg, pending_wallet)?,
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

fn edit_mode(cfg: &mut Config, pending_wallet: &mut Option<PendingWallet>) -> Result<()> {
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
        configure_wallet(cfg, pending_wallet, true, None)?;
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
    present: Present,
    current_wallet: Option<&CurrentWallet>,
) -> Result<()> {
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
    configure_wallet(cfg, pending_wallet, assisted, current_wallet)?;

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
        return success(&format!("Keeping wallet {}", kept.pubkey));
    };

    *pending_wallet = None;
    match selection {
        0 => {
            let path = next_wallet_path();
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
    Ok(())
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

fn next_wallet_path() -> PathBuf {
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/mobius/wallets");
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
    present: Present,
    current_wallet: Option<&CurrentWallet>,
) -> Result<()> {
    let path = SetupPath::Advanced;
    path.step(1, &["LIVE and CONFIRM send real transactions and need a signing wallet and a typed phrase."]);
    configure_profile(cfg, pending_wallet, current_wallet)?;

    path.step(2, &["All optional. Values are hidden while typed and stored only in .env (0600)."]);
    *env = configure_credentials(present, true)?;

    path.step(3, &["Endpoints, request rates and timeouts. The defaults fit Jupiter's free plan."]);
    if customize("Adjust API endpoints and rate limits?")? {
        configure_network(cfg)?;
    }

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

fn configure_profile(
    cfg: &mut Config,
    pending_wallet: &mut Option<PendingWallet>,
    current_wallet: Option<&CurrentWallet>,
) -> Result<()> {
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
        configure_wallet(cfg, pending_wallet, false, current_wallet)?;
        cfg.paper.equity_lamports = prompt_sol("Virtual PAPER equity", cfg.paper.equity_lamports)?;
        return Ok(());
    }

    cfg.execution.live_enabled = false;
    cfg.paper.simulation_taker = None;
    configure_wallet(cfg, pending_wallet, true, current_wallet)?;
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
    if setup_ui::active() {
        setup_ui::step(current, total, title, details);
    } else {
        let heading = format!("{current}/{total}  {title}");
        println!("\n{heading}\n{}", "─".repeat(heading.chars().count()));
        for detail in details {
            println!("{detail}");
        }
    }
}

fn note(title: &str, rows: &[(&str, String)]) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::note(title, rows);
    }
    println!("\n{title}");
    let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (key, value) in rows {
        if key.is_empty() {
            println!("  {value}");
        } else {
            println!("  {key:<width$}  {value}");
        }
    }
    Ok(())
}

fn outline(title: &str, rows: &[(&str, String)]) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::outline(title, rows);
    }
    note(title, rows)
}

fn success(message: &str) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::success(message);
    }
    println!("✓ {message}");
    Ok(())
}

fn info(message: &str) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::info(message);
    }
    println!("  {message}");
    Ok(())
}

fn warn(message: &str) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::warn(message);
    }
    println!("! {message}");
    Ok(())
}

fn finish(title: &str, hint: &str) -> Result<()> {
    if setup_ui::active() {
        return setup_ui::finish(title, hint);
    }
    println!("\n{title} · {hint}");
    Ok(())
}

fn menu(prompt: &str, items: &[(&str, &str, Option<&str>)], default: usize) -> Result<usize> {
    let items: Vec<MenuItem<'_>> =
        items.iter().map(|(title, description, badge)| MenuItem { title, description, badge: *badge }).collect();
    menu_items(prompt, &items, default)
}

fn menu_items(prompt: &str, items: &[MenuItem<'_>], default: usize) -> Result<usize> {
    if let Some(value) = setup_ui::prompt_menu(prompt, items, default)? {
        return Ok(value);
    }
    println!("\n{prompt}");
    for (index, item) in items.iter().enumerate() {
        let badge = item.badge.map_or(String::new(), |value| format!(" ({value})"));
        println!("  {}. {}{badge}", index + 1, item.title);
        if !item.description.is_empty() {
            println!("     {}", item.description);
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
    if let Some(value) = setup_ui::prompt_bool(prompt, default)? {
        return Ok(value);
    }
    loop {
        let hint = if default { "Y/n" } else { "y/N" };
        let value = read_plain(&format!("{prompt} [{hint}]: "), false)?;
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
    if let Some(value) = setup_ui::prompt_text(spec)? {
        return Ok(value);
    }
    let hint = if spec.placeholder.is_empty() { String::new() } else { format!(" [{}]", spec.placeholder) };
    loop {
        let value = read_plain(&format!("{}{hint}: ", spec.label), spec.hidden)?;
        match spec.validate.map_or(Ok(()), |validate| validate(value.trim())) {
            Ok(()) => return Ok(value),
            Err(reason) => println!("{reason}"),
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
}
