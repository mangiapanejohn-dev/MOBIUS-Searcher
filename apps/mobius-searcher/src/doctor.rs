//! `--doctor`: where every setting comes from, which secrets are present
//! (names only), and whether each configured endpoint and venue answers from
//! this machine. Costs one Jupiter request. Reads the keypair file only in
//! CONFIRM/LIVE (to prove it loads and matches `wallet.pubkey`); never prints it.

use searcher_core::Address;
use searcher_core::config::{Config, Layer, Layered, OracleSourceKind, VenueConfig, VenueKind, display_url};
use searcher_execution::Wallet;
use searcher_jito::JitoClient;
use searcher_jupiter::{ApiKey, JupiterClient};
use searcher_market::{RpcClient, feed};
use searcher_telemetry::proxy::{self, ProxySetting};
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A secret's env var name and where it was found (never its value).
pub struct Secret {
    pub name: String,
    pub what: &'static str,
    pub source: Option<String>,
}

pub fn secrets(cfg: &Config, env_files: &[(PathBuf, Vec<String>)]) -> Vec<Secret> {
    let solana = [
        (cfg.jupiter.api_key_env.as_str(), "Jupiter API key (keyless works: ~5 requests / 10 s)"),
        (cfg.rpc.url_env.as_str(), "Solana RPC URL override (keyed provider)"),
        (cfg.rpc.ws_url_env.as_str(), "Solana RPC WebSocket URL override"),
        (cfg.jito.uuid_env.as_str(), "Jito UUID (optional)"),
        (cfg.feeds.pyth_api_key_env.as_str(), "Pyth Hermes key (only for oracle_source = \"hermes\")"),
    ];
    let venues = cfg
        .venues
        .values()
        .filter(|v| v.enabled)
        .flat_map(|v| v.credential_envs().into_iter().map(|n| (n, "venue credential (trading only)")));
    solana
        .into_iter()
        .chain(venues)
        .map(|(name, what)| {
            let from_file =
                env_files.iter().find(|(_, keys)| keys.iter().any(|k| k == name)).map(|(p, _)| p.display().to_string());
            let set = std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
            Secret {
                name: name.to_string(),
                what,
                source: set.then(|| from_file.unwrap_or_else(|| "process environment".into())),
            }
        })
        .collect()
}

fn proxy_line() -> String {
    match proxy::setting() {
        ProxySetting::Direct => "none (direct connections)".into(),
        ProxySetting::Url(u) => format!("{u} (configured)"),
        ProxySetting::Auto => {
            let env = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .into_iter()
                .find(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
            match (env, proxy::system_proxy(true)) {
                (Some(k), _) => format!("auto → ${k}"),
                (None, Some((h, p))) => format!("auto → system proxy {h}:{p}"),
                (None, None) => "auto → direct (no proxy found)".into(),
            }
        }
    }
}

struct Check {
    ok: bool,
    /// Has a fallback or only feeds display: a failure does not stop PAPER.
    optional: bool,
    name: String,
    target: String,
    detail: String,
}

impl Check {
    fn new(ok: bool, name: impl Into<String>, target: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { ok, optional: false, name: name.into(), target: target.into(), detail: detail.into() }
    }

    fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

fn http_client() -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(8));
    if let Some(p) = proxy::fallback_https_proxy() {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?);
    }
    if proxy::direct() {
        b = b.no_proxy();
    }
    b.build().map_err(|e| e.to_string())
}

/// Venue reachability and instrument check (public endpoints, no key).
async fn check_venue(name: &str, v: &VenueConfig) -> Check {
    let label = format!("venue {name}");
    let target = display_url(&v.rest_url);
    let http = match http_client() {
        Ok(h) => h,
        Err(e) => return Check::new(false, label, target, e).optional(),
    };
    match v.kind {
        VenueKind::Okx => {
            let base = v.rest_url.trim_end_matches('/');
            let t = Instant::now();
            let mut unknown = Vec::new();
            for inst in &v.markets {
                let url = format!("{base}/api/v5/market/ticker?instId={inst}");
                let body = match http.get(&url).send().await {
                    Ok(r) => r.text().await.unwrap_or_default(),
                    Err(e) => return Check::new(false, label, target, e.to_string()).optional(),
                };
                // OKX answers HTTP 200 with a non-zero `code` for unknown instruments
                let code = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|j| j.get("code").and_then(|c| c.as_str().map(str::to_string)));
                match code.as_deref() {
                    Some("0") => {}
                    Some(_) => unknown.push(inst.as_str()),
                    None => return Check::new(false, label, target, "not an OKX v5 API answer").optional(),
                }
            }
            let detail = if unknown.is_empty() {
                format!("okx · {} markets answered in {}", v.markets.len(), ms(t.elapsed()))
            } else {
                format!("unknown instruments in markets: {}", unknown.join(", "))
            };
            Check::new(unknown.is_empty(), label, target, detail).optional()
        }
    }
}

pub async fn run(l: &Layered, env_files: &[(PathBuf, Vec<String>)]) -> bool {
    let cfg = &l.config;
    println!("MØBIUS doctor\n");
    println!("config layers (later wins)");
    println!("  default  built-in");
    for layer in [Layer::Repo, Layer::User] {
        let file = l.files.iter().find(|(x, _)| *x == layer).map(|(_, p)| p.display().to_string());
        let set = l.origins.values().filter(|x| **x == layer).count();
        match file {
            Some(f) => println!("  {:<8} {f}  ({set} keys)", layer.label()),
            None => println!("  {:<8} (none)", layer.label()),
        }
    }
    let venues: Vec<String> =
        cfg.venues.iter().filter(|(_, v)| v.enabled).map(|(n, v)| format!("{n} ({:?})", v.kind)).collect();
    println!(
        "  → mode {} · live_enabled {} · venues: solana, {}",
        cfg.general.mode.label(),
        cfg.execution.live_enabled,
        venues.join(", ")
    );

    let files: Vec<String> = env_files.iter().map(|(p, _)| p.display().to_string()).collect();
    println!("\nsecrets (names only; process env → {})", files.join(" → "));
    let secrets = secrets(cfg, env_files);
    for s in &secrets {
        println!(
            "  {:<4} {:<20} {}  {}",
            if s.source.is_some() { "set" } else { "—" },
            s.name,
            s.what,
            s.source.as_deref().unwrap_or("")
        );
    }
    println!("\nproxy  {}", proxy_line());

    println!("\nendpoints");
    let telemetry = Arc::new(Telemetry::new());
    let mut checks = Vec::new();
    let rpc_url = cfg.rpc.resolved_url();
    let ws_url = cfg.rpc.resolved_ws_url();
    let from_env = |env: &str| {
        if std::env::var(env).is_ok_and(|v| !v.trim().is_empty()) { format!(" (${env})") } else { String::new() }
    };

    // Solana RPC HTTP: slot, then the configured pool / oracle accounts
    let mut rpc_client = None;
    match RpcClient::new(
        &rpc_url,
        LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
        cfg.rpc.simulate_rps,
        Duration::from_millis(cfg.rpc.timeout_ms),
        telemetry.clone(),
    ) {
        Ok(rpc) => {
            let t = Instant::now();
            let slot = rpc.get_slot().await;
            let detail = match &slot {
                Ok(s) => format!("getSlot {s} in {}", ms(t.elapsed())),
                Err(e) => e.to_string(),
            };
            let target = format!("{}{}", display_url(&rpc_url), from_env(&cfg.rpc.url_env));
            checks.push(Check::new(slot.is_ok(), "Solana RPC", target, detail));
            let feeds: Vec<(&str, Option<Address>)> = cfg
                .feeds
                .pools
                .iter()
                .map(|p| (p.dex.as_str(), p.address.parse().ok()))
                .chain(cfg.feeds.oracles.iter().map(|o| (o.symbol.as_str(), o.address.parse().ok())))
                .collect();
            let addrs: Vec<Address> = feeds.iter().filter_map(|(_, a)| *a).collect();
            if cfg.feeds.enabled && !addrs.is_empty() {
                let target = format!("{} pools, {} oracles", cfg.feeds.pools.len(), cfg.feeds.oracles.len());
                checks.push(match rpc.get_account_datas(&addrs).await {
                    Ok((_, found)) => {
                        let missing: Vec<&str> =
                            feeds.iter().zip(&found).filter(|(_, f)| f.is_none()).map(|((n, _), _)| *n).collect();
                        let detail = if missing.is_empty() {
                            "all exist on mainnet".to_string()
                        } else {
                            format!("missing: {}", missing.join(", "))
                        };
                        Check::new(missing.is_empty(), "Solana accounts", target, detail)
                    }
                    Err(e) => Check::new(false, "Solana accounts", target, e.to_string()),
                });
            }
            rpc_client = Some(rpc);
        }
        Err(e) => checks.push(Check::new(false, "Solana RPC", display_url(&rpc_url), e.to_string())),
    }

    // Solana RPC WebSocket: first slot notification (the event scheduler depends on it)
    let sub = r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#;
    let ws = feed::probe_ws(&ws_url, Some(sub), Duration::from_secs(8)).await;
    let target = format!("{}{}", display_url(&ws_url), from_env(&cfg.rpc.ws_url_env));
    checks.push(match ws {
        Ok((c, first)) => {
            Check::new(true, "Solana WebSocket", target, format!("connected {} · first slot {}", ms(c), ms(first)))
        }
        Err(e) => Check::new(false, "Solana WebSocket", target, e),
    });

    // Jupiter: one request; its x-ratelimit headers tell the tier
    let key = std::env::var(&cfg.jupiter.api_key_env).ok().and_then(ApiKey::new);
    let tier = if key.is_some() { "API key" } else { "keyless" };
    let target = format!("{} ({tier})", display_url(&cfg.jupiter.base_url));
    match JupiterClient::new(
        &cfg.jupiter.base_url,
        key,
        LimiterConfig::new(1.0, 1),
        Duration::from_millis(cfg.jupiter.timeout_ms),
        telemetry.clone(),
    ) {
        Ok(jup) => {
            let t = Instant::now();
            let labels = jup.dex_labels().await;
            let took = t.elapsed();
            let window = jup
                .server_window()
                .map(|w| format!(" · gateway window {} requests", w.current + w.remaining))
                .unwrap_or_default();
            checks.push(match labels {
                Ok(labels) => {
                    let known: std::collections::BTreeSet<&str> = labels.values().map(String::as_str).collect();
                    let unknown: Vec<&str> = cfg
                        .strategies
                        .cross_dex
                        .iter()
                        .filter(|s| s.enabled)
                        .flat_map(|s| s.dexes.iter().map(String::as_str))
                        .filter(|d| !known.contains(d))
                        .collect();
                    let detail = if unknown.is_empty() {
                        format!("{} DEX labels in {}{window}", labels.len(), ms(took))
                    } else {
                        format!("unknown DEX labels in strategies.cross_dex: {}", unknown.join(", "))
                    };
                    Check::new(unknown.is_empty(), "Jupiter", target, detail)
                }
                Err(e) => Check::new(false, "Jupiter", target, e.to_string()),
            });
        }
        Err(e) => checks.push(Check::new(false, "Jupiter", target, e.to_string())),
    }

    // Jito: tip floor (REST) and the tip stream
    let target = display_url(&cfg.jito.tip_floor_url);
    match JitoClient::new(&cfg.jito.block_engine_url, &cfg.jito.tip_floor_url, None, cfg.jito.rps, telemetry.clone()) {
        Ok(jito) => {
            let t = Instant::now();
            checks.push(match jito.tip_floor().await {
                Ok(_) => Check::new(true, "Jito tip floor", target, format!("answered in {}", ms(t.elapsed()))),
                Err(e) => Check::new(false, "Jito tip floor", target, e.to_string()),
            });
        }
        Err(e) => checks.push(Check::new(false, "Jito tip floor", target, e.to_string())),
    }
    if cfg.feeds.enabled && !cfg.feeds.tip_stream_url.is_empty() {
        let target = display_url(&cfg.feeds.tip_stream_url);
        checks.push(
            match feed::probe_ws(&cfg.feeds.tip_stream_url, None, Duration::from_secs(10)).await {
                Ok((c, first)) => Check::new(
                    true,
                    "Jito tip stream",
                    target,
                    format!("connected {} · first tip {}", ms(c), ms(first)),
                ),
                Err(e) => Check::new(false, "Jito tip stream", target, format!("{e} (REST tip floor is used instead)")),
            }
            .optional(),
        );
    }
    if cfg.feeds.oracle_source == OracleSourceKind::Hermes {
        let has = secrets.iter().any(|s| s.name == cfg.feeds.pyth_api_key_env && s.source.is_some());
        let detail = if has {
            "key present".to_string()
        } else {
            format!("${} not set: falls back to on-chain Pyth", cfg.feeds.pyth_api_key_env)
        };
        checks.push(Check::new(has, "Pyth Hermes", display_url(&cfg.feeds.hermes_url), detail).optional());
    }

    for (name, v) in cfg.venues.iter().filter(|(_, v)| v.enabled) {
        checks.push(check_venue(name, v).await);
    }

    // CONFIRM / LIVE: the signer must load, match, and afford fees
    if cfg.general.mode.sends_transactions() {
        let expected = cfg.wallet.pubkey.as_deref().and_then(|p| p.parse::<Address>().ok());
        let path = cfg.wallet.keypair_path.clone().unwrap_or_default();
        match Wallet::load(std::path::Path::new(&path), expected.as_ref()) {
            Ok(w) => {
                let pk = w.pubkey();
                checks.push(Check::new(true, "wallet keypair", pk.short(), "loads; matches wallet.pubkey"));
                let min = cfg.risk.min_wallet_sol_for_fees_lamports;
                let sol = |l: u64| format!("{:.4} SOL", l as f64 / 1e9);
                checks.push(match &rpc_client {
                    Some(rpc) => match rpc.get_balance(&pk).await {
                        Ok(b) => Check::new(
                            b >= min,
                            "wallet balance",
                            pk.short(),
                            format!("{} (fee reserve {} = risk.min_wallet_sol_for_fees_lamports)", sol(b), sol(min)),
                        ),
                        Err(e) => Check::new(false, "wallet balance", pk.short(), e.to_string()),
                    },
                    None => Check::new(false, "wallet balance", pk.short(), "no RPC"),
                });
            }
            // the error names the problem, never the key material
            Err(e) => checks.push(Check::new(false, "wallet keypair", "wallet.keypair_path", e.to_string())),
        }
    }

    for c in &checks {
        let status = match (c.ok, c.optional) {
            (true, _) => "ok",
            (false, false) => "FAIL",
            (false, true) => "warn",
        };
        println!("  {status:<4} {:<17} {:<44} {}", c.name, c.target, c.detail);
    }
    // warn lines have a fallback or feed display only (venues are market data today)
    let required_ok = checks.iter().filter(|c| !c.optional).all(|c| c.ok);
    println!(
        "\n{}",
        if required_ok {
            format!("{} mode can run with this configuration.", cfg.general.mode.label())
        } else {
            "Fix the FAIL lines above (config/mobius.toml explains each setting).".to_string()
        }
    );
    required_ok
}
