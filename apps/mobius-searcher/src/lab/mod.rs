//! The lab: rules that hold a position, tried on paper. `--lab-backtest FILE`
//! runs a rules file over past candles; `--lab FILE` runs it on live prices,
//! bar by bar, with a paper account; `--lab-report` says what the live runs
//! did. Nothing is signed or sent, no Jupiter request is made, and the
//! trading database is not touched: candles and the exchange's best bid and
//! ask come from OKX's public API, results go to `research.sqlite`.
//!
//! A rules file is frozen by its content: its run is named after a hash of
//! the file, so a changed file is a new run and cannot rewrite an old one.

pub mod desk;
pub mod model;
pub mod rules;
pub mod stats;
pub mod trade;

use anyhow::{Context, Result, bail};
use model::Model;
use rules::{Account, Bar, Costs, Ctx, Rule, Stops, step};
use searcher_core::config::Config;
use searcher_storage::ResearchStore;
use searcher_storage::research::LabFill;
use searcher_telemetry::proxy;
use serde::Deserialize;
use stats::{Point, Stats, date, stats};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const OKX: &str = "https://www.okx.com/api/v5/market";
/// What a model rule was trained on: these candles, and this coin beside them.
const MODEL_ON: (&str, &str, &str) = ("SOL-USDT", "15m", "BTC-USDT");
/// A signal older than this share of a bar is not acted on (the machine slept).
const LATE: f64 = 1.0 / 3.0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default = "default_instrument")]
    instrument: String,
    #[serde(default = "default_bar")]
    bar: String,
    #[serde(default = "default_capital")]
    capital_usd: f64,
    #[serde(default)]
    costs: Costs,
    #[serde(default)]
    stops: Stops,
    /// Only `--trade` reads it: the same file on paper and with real money.
    live: Option<trade::Live>,
    #[serde(default)]
    experiment: Vec<toml::Table>,
}

/// OKX's SOL-USDC trades too rarely for one-minute closes to move; USDT is the liquid one.
fn default_instrument() -> String {
    "SOL-USDT".into()
}
fn default_bar() -> String {
    "15m".into()
}
fn default_capital() -> f64 {
    23.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct Experiment {
    pub name: String,
    pub rule: Rule,
    /// The model of a model rule, once its file is read.
    pub model: Option<Arc<Model>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Hash of the file: the name of its run.
    pub id: String,
    pub manifest: String,
    pub instrument: String,
    pub bar: String,
    pub bar_ms: i64,
    pub capital: f64,
    pub costs: Costs,
    pub stops: Stops,
    pub experiments: Vec<Experiment>,
    /// What `--trade` may do with real money, when the file says.
    pub live: Option<trade::Live>,
}

/// Bar length in ms and OKX's name for it.
fn bar_of(name: &str) -> Option<(i64, &'static str)> {
    Some(match name {
        "1m" => (60_000, "1m"),
        "5m" => (300_000, "5m"),
        "15m" => (900_000, "15m"),
        "30m" => (1_800_000, "30m"),
        "1h" | "1H" => (3_600_000, "1H"),
        "4h" | "4H" => (14_400_000, "4H"),
        _ => return None,
    })
}

pub fn parse(text: &str) -> Result<Plan> {
    let file: File = toml::from_str(text).context("the rules file")?;
    let (bar_ms, _) = bar_of(&file.bar).with_context(|| format!("bar `{}`: one of 1m 5m 15m 30m 1h 4h", file.bar))?;
    if file.capital_usd <= 0.0 {
        bail!("capital_usd must be greater than zero");
    }
    if let Some(live) = &file.live {
        live.check().map_err(anyhow::Error::msg)?;
    }
    // with a [live] section the budget is the rule's capital, on paper too
    let capital = file.live.as_ref().map_or(file.capital_usd, |l| l.budget_usd);
    let mut experiments = Vec::new();
    for mut t in file.experiment {
        let name = match t.remove("name") {
            Some(toml::Value::String(s)) if !s.is_empty() => s,
            _ => bail!("every [[experiment]] needs a name"),
        };
        if experiments.iter().any(|e: &Experiment| e.name == name) {
            bail!("two experiments are called `{name}`");
        }
        let rule: Rule = t.try_into().with_context(|| format!("experiment `{name}`"))?;
        rule.check().map_err(|e| anyhow::anyhow!("experiment `{name}`: {e}"))?;
        experiments.push(Experiment { name, rule, model: None });
    }
    if experiments.is_empty() {
        bail!("the rules file has no [[experiment]]");
    }
    // FNV-1a over the file: a changed file is a different run
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    Ok(Plan {
        id: format!("{hash:016x}"),
        manifest: text.to_string(),
        instrument: file.instrument,
        bar: file.bar,
        bar_ms,
        capital,
        costs: file.costs,
        stops: file.stops,
        experiments,
        live: file.live,
    })
}

fn load(path: &Path) -> Result<Plan> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut plan = parse(&text).with_context(|| path.display().to_string())?;
    for e in &mut plan.experiments {
        let Rule::Model { file } = &e.rule else { continue };
        if (plan.instrument.as_str(), plan.bar.as_str()) != (MODEL_ON.0, MODEL_ON.1) {
            bail!(
                "experiment `{}`: a model reads {} {} candles; this file is {} {}",
                e.name,
                MODEL_ON.0,
                MODEL_ON.1,
                plan.instrument,
                plan.bar
            );
        }
        // as written, or beside the rules file
        let beside = path.parent().map(|d| d.join(file)).filter(|_| !Path::new(file).exists());
        let model = Model::load(beside.as_deref().unwrap_or(Path::new(file)));
        e.model = Some(Arc::new(model.map_err(|m| anyhow::anyhow!("experiment `{}`: {m}", e.name))?));
    }
    Ok(plan)
}

/// The other coin a model rule reads, if the plan has one.
fn other_of(plan: &Plan) -> Option<&'static str> {
    plan.experiments.iter().any(|e| e.model.is_some()).then_some(MODEL_ON.2)
}

/// For each of `bars`, the close of the bar of `other` that started with it (NaN: none).
fn beside(bars: &[Bar], other: &[Bar]) -> Vec<f64> {
    let mut j = 0;
    bars.iter()
        .map(|b| {
            while j < other.len() && other[j].ts < b.ts {
                j += 1;
            }
            other.get(j).filter(|o| o.ts == b.ts).map_or(f64::NAN, |o| o.close)
        })
        .collect()
}

fn http() -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if let Some(p) = proxy::fallback_https_proxy() {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    if proxy::direct() {
        b = b.no_proxy();
    }
    Ok(b.build()?)
}

async fn okx(http: &reqwest::Client, path: &str) -> Result<Vec<serde_json::Value>> {
    let mut last = None;
    for attempt in 0..4u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
        }
        let got: Result<serde_json::Value> =
            async { Ok(http.get(format!("{OKX}/{path}")).send().await?.json().await?) }.await;
        match got {
            Ok(v) if v["code"] == "0" => return Ok(v["data"].as_array().cloned().unwrap_or_default()),
            Ok(v) => last = Some(anyhow::anyhow!("OKX: {}", v["msg"].as_str().unwrap_or("no message"))),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("OKX did not answer")))
}

fn num(v: &serde_json::Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(f64::NAN)
}

/// Completed candles of an OKX answer, as it gives them (newest first).
fn candles(rows: &[serde_json::Value]) -> Vec<Bar> {
    rows.iter()
        .filter(|c| c[8] == "1")
        .map(|c| Bar {
            ts: num(&c[0]) as i64,
            open: num(&c[1]),
            high: num(&c[2]),
            low: num(&c[3]),
            close: num(&c[4]),
            volume: num(&c[5]),
        })
        .filter(|b| b.open.is_finite() && b.close.is_finite() && b.ts > 0)
        .collect()
}

/// Completed candles older than `before` (ms) back to `until`, oldest first.
async fn history(http: &reqwest::Client, plan: &Plan, inst: &str, before: i64, until: i64) -> Result<Vec<Bar>> {
    let (_, okx_bar) = bar_of(&plan.bar).context("bar")?;
    let (mut out, mut after, mut pages) = (Vec::new(), before, 0u32);
    while after > until {
        let path = format!("history-candles?instId={inst}&bar={okx_bar}&limit=100&after={after}");
        let rows = okx(http, &path).await?;
        let Some(oldest) = rows.last().map(|c| num(&c[0]) as i64) else { break };
        out.extend(candles(&rows).into_iter().filter(|b| b.ts >= until));
        after = oldest;
        pages += 1;
        if pages % 40 == 0 {
            eprintln!("  … {inst} back to {}", date(oldest));
        }
        tokio::time::sleep(Duration::from_millis(250)).await; // 4 requests a second, far under OKX's limit
    }
    out.sort_by_key(|b| b.ts);
    out.dedup_by_key(|b| b.ts);
    Ok(out)
}

fn now_ms() -> i64 {
    searcher_core::Ts::now().millis()
}

/// The candles from `from` on: what the database has, plus what it lacks at either end.
async fn bars_since(
    store: &ResearchStore,
    http: &reqwest::Client,
    plan: &Plan,
    inst: &str,
    from: i64,
) -> Result<Vec<Bar>> {
    let cached = store.lab_bars(inst, &plan.bar, from)?;
    let mut fetched = Vec::new();
    match (cached.first(), cached.last()) {
        (Some(first), Some(last)) => {
            fetched.extend(history(http, plan, inst, now_ms(), last.0 + 1).await?);
            if first.0 > from + plan.bar_ms {
                fetched.extend(history(http, plan, inst, first.0, from).await?);
            }
        }
        _ => fetched = history(http, plan, inst, now_ms(), from).await?,
    }
    save_bars(store, plan, inst, &fetched)?;
    let all = store.lab_bars(inst, &plan.bar, from)?;
    Ok(all.into_iter().map(|b| Bar { ts: b.0, open: b.1, high: b.2, low: b.3, close: b.4, volume: b.5 }).collect())
}

fn save_bars(store: &ResearchStore, plan: &Plan, inst: &str, bars: &[Bar]) -> Result<()> {
    let rows: Vec<_> = bars.iter().map(|b| (b.ts, b.open, b.high, b.low, b.close, b.volume)).collect();
    Ok(store.insert_lab_bars(inst, &plan.bar, &rows)?)
}

/// One experiment over `bars`: each decision filled at the next bar's open.
/// `other`: the other coin's close for each bar (only a model rule reads it).
pub fn simulate(plan: &Plan, e: &Experiment, bars: &[Bar], other: &[f64]) -> Stats {
    let mut acct = Account::new(plan.capital);
    let mut points = Vec::with_capacity(bars.len());
    for i in 0..bars.len().saturating_sub(1) {
        let sol = acct.sol();
        let next = &bars[i + 1];
        let (_, equity) = step(
            &e.rule,
            &plan.stops,
            &plan.costs,
            plan.capital,
            &mut acct,
            (&bars[..=i], Ctx { model: e.model.as_deref(), other: other.get(..=i).unwrap_or(&[]) }),
            (next.open, next.open, next.ts),
        );
        points.push(Point { ts: bars[i].ts, close: bars[i].close, equity, sol_value: sol * bars[i].close });
    }
    stats(&e.name, plan.capital, &plan.costs, &points, &acct)
}

fn header(plan: &Plan, what: &str, bars: &[Bar]) -> String {
    let (first, last) = (bars.first().map_or(0, |b| b.ts), bars.last().map_or(0, |b| b.ts));
    let px = bars.last().map_or(0.0, |b| b.close);
    let side_bps = plan.costs.of(plan.capital, px) / plan.capital * 1e4;
    let hold = match (bars.first(), bars.last()) {
        (Some(a), Some(b)) => format!("{:+.1} %", (b.close / a.close - 1.0) * 100.0),
        _ => "—".into(),
    };
    format!(
        "LAB · {what} · {} {} · {} bars, {} to {} (UTC) · run {}\n\
         capital {:.2} USD, starting in USDC · costs {:.2} bp of each trade + {:.0} lamports a transaction \
         (a {:.2} USD trade pays {:.2} bp a side)\n\
         holding SOL over these bars: {hold} · holding USDC: +0.0 %",
        plan.instrument,
        plan.bar,
        bars.len(),
        date(first),
        date(last),
        // a real run is filed as `trade-` and the file's name: eight characters of that say nothing
        &plan.id[..plan.id.len().min(if plan.id.starts_with("trade-") { 14 } else { 8 })],
        plan.capital,
        plan.costs.route_bps,
        plan.costs.fixed_fee_lamports,
        plan.capital,
        side_bps,
    )
}

/// `--lab-backtest FILE`: the rules over the last `days` days of candles.
pub async fn backtest(cfg: &Config, file: &Path, days: u32, json: bool) -> Result<()> {
    let plan = load(file)?;
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let http = http()?;
    let from = now_ms() - i64::from(days) * 86_400_000;
    eprintln!("candles of {} {} since {} (cached in {})…", plan.instrument, plan.bar, date(from), db.display());
    let bars = bars_since(&store, &http, &plan, &plan.instrument, from).await?;
    if bars.len() < 10 {
        bail!("only {} candles came back for {} {}", bars.len(), plan.instrument, plan.bar);
    }
    let other = match other_of(&plan) {
        Some(inst) => beside(&bars, &bars_since(&store, &http, &plan, inst, from).await?),
        None => Vec::new(),
    };
    let all: Vec<Stats> = plan.experiments.iter().map(|e| simulate(&plan, e, &bars, &other)).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
    } else {
        print!("{}", stats::render(&header(&plan, "backtest", &bars), &all));
        for e in plan.experiments.iter().filter(|e| e.model.is_some()) {
            let m = e.model.as_ref().expect("filtered");
            println!(
                "`{}` is a model trained on candles up to {}: it stays out before that day (it has seen those answers),\n\
                 so only the bars after it count for it.",
                e.name, m.trained_to
            );
        }
    }
    Ok(())
}

/// The exchange's best bid and ask now.
async fn book(http: &reqwest::Client, plan: &Plan) -> Result<(f64, f64)> {
    let rows = okx(http, &format!("ticker?instId={}", plan.instrument)).await?;
    let t = rows.first().context("no ticker")?;
    let (bid, ask) = (num(&t["bidPx"]), num(&t["askPx"]));
    if !(bid > 0.0 && ask >= bid) {
        bail!("OKX gave no usable bid/ask");
    }
    Ok((bid, ask))
}

/// `--lab FILE`: the rules on live prices, on paper, until Ctrl-C or `duration` seconds.
pub async fn run(cfg: &Config, file: &Path, duration: Option<u64>) -> Result<()> {
    let plan = load(file)?;
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let http = http()?;
    store.begin_lab_run(&plan.id, now_ms(), env!("CARGO_PKG_VERSION"), &plan.manifest)?;
    let warmup = plan.experiments.iter().map(|e| e.rule.warmup()).max().unwrap_or(2) as i64 + 4;
    let since = now_ms() - warmup * plan.bar_ms;
    let mut bars = bars_since(&store, &http, &plan, &plan.instrument, since).await?;
    let mut other_bars = match other_of(&plan) {
        Some(inst) => bars_since(&store, &http, &plan, inst, since).await?,
        None => Vec::new(),
    };
    let mut accounts: Vec<(i64, Account)> = Vec::new();
    for e in &plan.experiments {
        accounts.push(match store.lab_state(&plan.id, &e.name)? {
            Some((ts, json)) => {
                (ts, serde_json::from_str(&json).with_context(|| format!("saved state of `{}`", e.name))?)
            }
            // a new experiment starts at the last closed bar: nothing before it is acted on
            None => (bars.last().map_or(0, |b| b.ts), Account::new(plan.capital)),
        });
    }
    println!(
        "LAB · paper, live prices · {} {} · run {} · {} experiment(s) · results in {}\n\
         nothing is signed or sent; stop with Ctrl-C, start again with the same file to go on",
        plan.instrument,
        plan.bar,
        &plan.id[..8],
        plan.experiments.len(),
        db.display()
    );
    let started = std::time::Instant::now();
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        if duration.is_some_and(|d| started.elapsed().as_secs() >= d) {
            break;
        }
        let newest = bars.last().map_or(0, |b| b.ts);
        let fresh = match history(&http, &plan, &plan.instrument, now_ms(), newest + 1).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{}  candles: {e:#}", searcher_core::Ts::now().hms());
                continue;
            }
        };
        if fresh.is_empty() {
            continue;
        }
        // the other coin's bar of the same quarter hour; a model without it gives no signal
        if let Some(inst) = other_of(&plan) {
            let have = other_bars.last().map_or(0, |b| b.ts);
            match history(&http, &plan, inst, now_ms(), have + 1).await {
                Ok(f) => {
                    save_bars(&store, &plan, inst, &f)?;
                    other_bars.extend(f);
                }
                Err(e) => eprintln!("{}  candles of {inst}: {e:#}", searcher_core::Ts::now().hms()),
            }
        }
        save_bars(&store, &plan, &plan.instrument, &fresh)?;
        bars.extend(fresh);
        let other = beside(&bars, &other_bars);
        let last = *bars.last().expect("just extended");
        // the bar closed `late` ms ago; a signal that old is recorded but not acted on
        let late = now_ms() - (last.ts + plan.bar_ms);
        let on_time = (late as f64) <= plan.bar_ms as f64 * LATE;
        let quote = if on_time { book(&http, &plan).await.ok() } else { None };
        for (e, (seen, acct)) in plan.experiments.iter().zip(accounts.iter_mut()) {
            for (i, bar) in bars.iter().enumerate().filter(|(_, b)| b.ts > *seen) {
                let is_last = bar.ts == last.ts;
                let sol_value = acct.sol() * bar.close;
                let (fills, equity) = match quote.filter(|_| is_last) {
                    Some((bid, ask)) => step(
                        &e.rule,
                        &plan.stops,
                        &plan.costs,
                        plan.capital,
                        acct,
                        (&bars[..=i], Ctx { model: e.model.as_deref(), other: other.get(..=i).unwrap_or(&[]) }),
                        (ask, bid, now_ms()),
                    ),
                    // a bar the machine slept through, or no book: the account is marked, nothing is done
                    None => (Vec::new(), acct.equity(bar.close)),
                };
                let (bid, ask) = quote.unwrap_or((f64::NAN, f64::NAN));
                let rows: Vec<LabFill> = fills
                    .iter()
                    .map(|f| LabFill {
                        ts: now_ms(),
                        bar_ts: bar.ts,
                        buy: f.buy,
                        price: if f.buy { ask } else { bid },
                        bid,
                        ask,
                        usd: f.usd,
                        sol: f.sol,
                        cost_usd: f.cost_usd,
                    })
                    .collect();
                let row = (bar.ts, bar.open, bar.high, bar.low, bar.close, bar.volume);
                store.record_lab_bar(
                    (&plan.id, &e.name),
                    &row,
                    &rows,
                    equity,
                    sol_value,
                    &serde_json::to_string(acct)?,
                )?;
                for f in &fills {
                    println!(
                        "{}  {:<16} {} {:.4} SOL for {:.2} USD at {:.2} (cost {:.4} USD)",
                        searcher_core::Ts::now().hms(),
                        e.name,
                        if f.buy { "bought" } else { "sold  " },
                        f.sol,
                        f.usd,
                        if f.buy { ask } else { bid },
                        f.cost_usd
                    );
                }
            }
            *seen = last.ts;
        }
        let line: Vec<String> = plan
            .experiments
            .iter()
            .zip(&accounts)
            .map(|(e, (_, a))| {
                format!(
                    "{} {:+.2}%{}",
                    e.name,
                    (a.equity(last.close) / plan.capital - 1.0) * 100.0,
                    if a.lots.is_empty() { "" } else { " (in SOL)" }
                )
            })
            .collect();
        println!(
            "{}  bar {} closed at {:.2}{} · {}",
            searcher_core::Ts::now().hms(),
            plan.bar,
            last.close,
            if on_time { String::new() } else { format!(" · seen {} s late: marked, not acted on", late / 1000) },
            line.join(" · ")
        );
        // rules never look further back than their warm-up
        let keep = warmup as usize + 8;
        if bars.len() > keep * 2 {
            bars.drain(..bars.len() - keep);
        }
        if other_bars.len() > keep * 2 {
            other_bars.drain(..other_bars.len() - keep);
        }
    }
    println!("stopped; `--lab-report` shows what each experiment did");
    Ok(())
}

/// `--trade FILE`: the file's one rule with real money (see [`trade`]).
/// `dry_run`: build and simulate the first swap, sign and send nothing.
/// `close`: sell what the run holds and end it.
/// The closed bars and, after them, the one that is forming as if it closed at `price`:
/// what a rule that acts on the price itself decides on between two closes.
fn forming_bar(bars: &[Bar], start: i64, price: f64) -> Vec<Bar> {
    let mut with = bars.to_vec();
    with.push(Bar { ts: start, open: price, high: price, low: price, close: price, volume: 0.0 });
    with
}

/// Lamports of SOL that the wallet's other real runs hold (bought, not sold yet, the run not ended).
fn held_by_others(store: &ResearchStore, run: &str) -> Result<u64> {
    let mut lamports = 0u64;
    for (id, _, manifest) in store.lab_runs()? {
        if !id.starts_with("trade-") || id == run {
            continue;
        }
        let Ok(plan) = parse(&manifest) else { continue };
        let [e] = plan.experiments.as_slice() else { continue };
        let Some((_, json)) = store.lab_state(&id, &e.name)? else { continue };
        let Ok(state) = serde_json::from_str::<trade::State>(&json) else { continue };
        if state.ended.is_none() {
            lamports += (state.account.sol() * 1e9).round() as u64;
        }
    }
    Ok(lamports)
}

pub async fn trade(cfg: &Config, file: &Path, dry_run: bool, close: bool, duration: Option<u64>) -> Result<()> {
    use trade::{Chain, Mainnet, State, Trader};
    let plan = load(file)?;
    let Some(live) = plan.live.clone() else {
        bail!("{}: --trade needs a [live] section (budget_usd, stop_total_loss, acknowledge)", file.display());
    };
    let [e] = plan.experiments.as_slice() else {
        bail!("{}: --trade runs one rule: the file has {} experiments", file.display(), plan.experiments.len());
    };
    if plan.stops.total_loss.is_some() {
        bail!(
            "{}: with real money the total stop is [live] stop_total_loss (it sells everything and ends the run): \
             take [stops] total_loss out",
            file.display()
        );
    }
    let _lock = if dry_run {
        None
    } else {
        live.consent().map_err(anyhow::Error::msg)?;
        // one at a time: two would each read the other's swaps in the wallet as their own
        std::fs::create_dir_all(cfg.data_dir())?;
        let path = cfg.data_dir().join("trade.lock");
        let lock = std::fs::File::create(&path).with_context(|| format!("opening {}", path.display()))?;
        // a running one holds it for good; the Bots page only touches it for an instant to see whether one does
        let mut tries = 0;
        while lock.try_lock().is_err() {
            tries += 1;
            if tries > 5 {
                bail!("another --trade is running (it holds {}): stop it first", path.display());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // which run this is and from which file, for the Bots page
        desk::hold(&cfg.data_dir(), &format!("trade-{}", plan.id), file)?;
        // started from the Bots page it has no terminal, and it outlives the one it was started under
        #[cfg(unix)]
        tokio::spawn(async {
            if let Ok(mut hangup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                while hangup.recv().await.is_some() {}
            }
        });
        Some(lock)
    };
    let mut chain = Mainnet::new(cfg, &live, dry_run)?;
    let http = http()?;
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let run = format!("trade-{}", plan.id);
    // the wallet is one, the bots may be several: what another holds in SOL is not this one's to sell for its budget
    let others = held_by_others(&store, &run)?;
    chain.keep_also(others);
    if others > 0 {
        println!("another bot of this wallet holds {:.6} SOL: it is left alone", others as f64 / 1e9);
    }
    let now = || searcher_core::Ts::now().hms();
    println!(
        "{} · {} · rule `{}` on {} {} · wallet {}\n\
         budget {:.2} USD, set aside as USDC once · at {} % down everything is sold and the run ends · slippage {} bp",
        if dry_run { "DRY RUN: nothing is signed or sent" } else { "REAL MONEY" },
        &run[..14],
        e.name,
        plan.instrument,
        plan.bar,
        chain.taker(),
        live.budget_usd,
        trade::percent(live.stop_total_loss),
        live.slippage_bps
    );
    let (lamports, usdc) = chain.balances().await.map_err(|e| anyhow::anyhow!("reading the wallet: {e}"))?;
    println!("the wallet holds {:.6} SOL and {:.4} USDC", lamports as f64 / 1e9, usdc as f64 / 1e6);
    let (bid, _) = book(&http, &plan).await?;
    if dry_run {
        // the first swap a real run would send, up to the signature
        let mut state = State::default();
        let mut t = Trader {
            chain: &chain,
            live: &live,
            state: &mut state,
            said: Vec::new(),
            settle_wait: Duration::ZERO,
            save: None,
        };
        t.fund(bid, now_ms()).await;
        for line in t.said {
            println!("{}  {line}", now());
        }
        return Ok(());
    }
    store.begin_lab_run(&run, now_ms(), env!("CARGO_PKG_VERSION"), &plan.manifest)?;
    // the bar the saved state belongs to
    let seen = std::cell::Cell::new(0);
    let mut state = match store.lab_state(&run, &e.name)? {
        Some((ts, json)) => {
            seen.set(ts);
            serde_json::from_str(&json).context("the saved state of this run")?
        }
        None => State::default(),
    };
    let journal = |lines: Vec<String>| -> Result<()> {
        for line in lines {
            println!("{}  {line}", now());
            store.insert_lab_journal(&run, now_ms(), &line)?;
        }
        Ok(())
    };
    let save = |state: &State, bar_ts: i64| -> Result<()> {
        Ok(store.set_lab_state(&run, &e.name, bar_ts, &serde_json::to_string(state)?)?)
    };
    // between bars: before a swap is sent and after it is accounted for
    let write = |state: &State| {
        if let Err(err) = save(state, seen.get()) {
            eprintln!("the state could not be written: {err:#}");
        }
    };
    if let Some(why) = &state.ended {
        println!("this run has ended ({why}); a changed file starts a new one");
        return Ok(());
    }
    // a swap sent just before the program was stopped may still land: wait until it cannot
    if let Some(p) = &state.pending {
        let wait = trade::SETTLED_AFTER_MS - (now_ms() - p.ts);
        if wait > 0 {
            println!(
                "a swap was under way {} s ago: waiting {} s to see what became of it",
                (now_ms() - p.ts) / 1000,
                wait / 1000 + 1
            );
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
    }
    if close {
        let mut t = Trader {
            chain: &chain,
            live: &live,
            state: &mut state,
            said: Vec::new(),
            settle_wait: Duration::from_secs(2),
            save: Some(&write),
        };
        t.reconcile(now_ms()).await;
        if t.state.pending.is_none() {
            t.say_closing();
            if t.sell_all(now_ms(), bid).await {
                let budget = trade::budget_of(t.state, &live);
                let why = format!("closed by hand at {:.4} USD of {budget:.2}", t.state.account.cash);
                t.said.push(format!("the run has ended: {why}; its USDC stays in the wallet"));
                t.state.ended = Some(why);
            }
        }
        let said = std::mem::take(&mut t.said);
        journal(said)?;
        save(&state, seen.get())?;
        return Ok(());
    }
    println!("it starts in 10 seconds; Ctrl-C now and nothing happens");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => return Ok(()),
        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
    }
    let _awake = crate::budget::KeepAwake::start();
    let warmup = e.rule.warmup() as i64 + 4;
    let since = now_ms() - warmup * plan.bar_ms;
    let mut bars = bars_since(&store, &http, &plan, &plan.instrument, since).await?;
    let mut other_bars = match other_of(&plan) {
        Some(inst) => bars_since(&store, &http, &plan, inst, since).await?,
        None => Vec::new(),
    };
    if seen.get() == 0 {
        seen.set(bars.last().map_or(0, |b| b.ts));
    }
    // a wish to change the budget, left by the Bots page: a number of USD.
    // Tried when it is new and again after each bar, until it is over.
    let wish_file = desk::wish_path(&cfg.data_dir(), &run);
    let mut tried: Option<(u64, i64)> = None;
    // a rule that acts on the price itself looks between the closes too; after a try that sent
    // nothing, or with a swap under way, the next look waits a little
    let one_price = other_of(&plan).is_none();
    if live.trigger == trade::Trigger::Price && !one_price {
        bail!("{}: trigger = \"price\" is for rules that read one instrument's price (not the model)", file.display());
    }
    let mut next_look = std::time::Instant::now();
    // a wish to change how it acts, left by the Bots page; and its looks, written down for that page
    let acts_file = desk::acts_path(&cfg.data_dir(), &run);
    let looks_file = desk::looks_path(&cfg.data_dir(), &run);
    let mut looks: std::collections::VecDeque<trade::Look> = desk::looks(&looks_file).into();
    let mut note = |look: trade::Look| {
        looks.push_back(look);
        while looks.len() > desk::LOOKS_KEPT {
            looks.pop_front();
        }
        if let Err(err) = desk::write_looks(&looks_file, &looks) {
            eprintln!("its looks could not be written: {err:#}");
        }
    };
    // what a try that sent nothing said is said once, not again every few seconds; nor that a swap is still under way
    let mut failed_at: Option<std::time::Instant> = None;
    let mut under_way = false;
    // the budget is set aside before the first bar (a wish left while it was stopped comes first: it may be the budget)
    {
        let mut t = Trader {
            chain: &chain,
            live: &live,
            state: &mut state,
            said: Vec::new(),
            settle_wait: Duration::from_secs(2),
            save: Some(&write),
        };
        t.reconcile(now_ms()).await;
        if let Some(to) = desk::acts_wish(&acts_file) {
            t.change_acts(to, one_price);
            let _ = std::fs::remove_file(&acts_file);
        }
        if let Some(to) = desk::wish(&wish_file) {
            tried = Some((to.to_bits(), seen.get()));
            if t.rebudget(to, bid, now_ms()).await == trade::Wish::Done {
                let _ = std::fs::remove_file(&wish_file);
            }
        }
        if t.state.pending.is_none() {
            t.fund(bid, now_ms()).await;
        }
        let said = std::mem::take(&mut t.said);
        journal(said)?;
        save(&state, seen.get())?;
    }
    let started = std::time::Instant::now();
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    // (when the last round began: a rule that acts on the price looks every so many seconds, its requests included)
    let mut round = std::time::Instant::now();
    while state.ended.is_none() {
        let by_price = trade::acts_of(&state, &live).trigger == trade::Trigger::Price;
        let nap = if by_price {
            Duration::from_secs(trade::LOOK_SECS).saturating_sub(round.elapsed())
        } else {
            Duration::from_secs(5)
        };
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(nap) => {}
        }
        round = std::time::Instant::now();
        if duration.is_some_and(|d| started.elapsed().as_secs() >= d) {
            break;
        }
        if let Some(to) = desk::acts_wish(&acts_file) {
            let mut t = Trader {
                chain: &chain,
                live: &live,
                state: &mut state,
                said: Vec::new(),
                settle_wait: Duration::from_secs(2),
                save: Some(&write),
            };
            t.change_acts(to, one_price);
            let said = std::mem::take(&mut t.said);
            journal(said)?;
            let _ = std::fs::remove_file(&acts_file);
            continue;
        }
        if let Some(to) = desk::wish(&wish_file).filter(|to| tried != Some((to.to_bits(), seen.get())))
            && let Ok((bid, _)) = book(&http, &plan).await
        {
            tried = Some((to.to_bits(), seen.get()));
            let mut t = Trader {
                chain: &chain,
                live: &live,
                state: &mut state,
                said: Vec::new(),
                settle_wait: Duration::from_secs(2),
                save: Some(&write),
            };
            if t.rebudget(to, bid, now_ms()).await == trade::Wish::Done {
                let _ = std::fs::remove_file(&wish_file);
            }
            let said = std::mem::take(&mut t.said);
            journal(said)?;
            save(&state, seen.get())?;
        }
        let newest = bars.last().map_or(0, |b| b.ts);
        // (between two looks at the price no bar can have closed before the forming one ends: not asked for)
        let fresh = if by_price && now_ms() < newest + 2 * plan.bar_ms {
            Vec::new()
        } else {
            match history(&http, &plan, &plan.instrument, now_ms(), newest + 1).await {
                Ok(f) => f,
                Err(err) => {
                    eprintln!("{}  candles: {err:#}", now());
                    continue;
                }
            }
        };
        if fresh.is_empty() {
            // no bar has closed: a look at the one that is forming, as if it closed at the price now
            let forming = newest + plan.bar_ms;
            let due = by_price && now_ms() < forming + plan.bar_ms && std::time::Instant::now() >= next_look;
            if due && let Ok((bid, ask)) = book(&http, &plan).await {
                let with = forming_bar(&bars, forming, (bid + ask) / 2.0);
                let mut t = Trader {
                    chain: &chain,
                    live: &live,
                    state: &mut state,
                    said: Vec::new(),
                    settle_wait: Duration::from_secs(2),
                    save: Some(&write),
                };
                let look = t.look(&plan, e, (&with, &[]), (bid, ask), false, now_ms()).await;
                let mut said = std::mem::take(&mut t.said);
                // said once, not again every few seconds: that a try sent nothing, and that a swap is still under way
                let wait = match look.did {
                    // (the quote that was not good enough is asked for again, not at once: the key allows one a second)
                    trade::Did::Failed => {
                        if failed_at.is_some_and(|at| at.elapsed() < Duration::from_secs(300)) {
                            said.retain(|l| {
                                !["not sent: ", "a gain to take: ", "the wallet could not be read"]
                                    .iter()
                                    .any(|w| l.starts_with(w))
                            });
                        } else {
                            failed_at = Some(std::time::Instant::now());
                        }
                        10
                    }
                    trade::Did::Sent => {
                        if under_way {
                            said.retain(|l| !l.starts_with("a swap sent less than two minutes ago"));
                        }
                        15
                    }
                    _ => 0,
                };
                under_way = look.did == trade::Did::Sent;
                if !matches!(look.did, trade::Did::Nothing | trade::Did::Rests { .. } | trade::Did::Failed) {
                    failed_at = None;
                }
                next_look = std::time::Instant::now() + Duration::from_secs(wait);
                if !said.is_empty() || !matches!(look.did, trade::Did::Nothing | trade::Did::Rests { .. }) {
                    journal(said)?;
                    save(&state, seen.get())?;
                }
                note(look);
            }
            continue;
        }
        if let Some(inst) = other_of(&plan) {
            let have = other_bars.last().map_or(0, |b| b.ts);
            if let Ok(f) = history(&http, &plan, inst, now_ms(), have + 1).await {
                save_bars(&store, &plan, inst, &f)?;
                other_bars.extend(f);
            }
        }
        save_bars(&store, &plan, &plan.instrument, &fresh)?;
        bars.extend(fresh);
        let other = beside(&bars, &other_bars);
        let last = *bars.last().expect("just extended");
        let late = now_ms() - (last.ts + plan.bar_ms);
        let on_time = (late as f64) <= plan.bar_ms as f64 * LATE;
        let quote = if on_time { book(&http, &plan).await.ok() } else { None };
        let Some((bid, ask)) = quote else {
            println!(
                "{}  bar closed at {:.2}, seen {} s late or without a book: nothing done",
                now(),
                last.close,
                late / 1000
            );
            continue;
        };
        let sol_before = state.account.sol();
        seen.set(last.ts);
        let mut t = Trader {
            chain: &chain,
            live: &live,
            state: &mut state,
            said: Vec::new(),
            settle_wait: Duration::from_secs(2),
            save: Some(&write),
        };
        let look = t.look(&plan, e, (&bars, &other), (bid, ask), true, now_ms()).await;
        let said = std::mem::take(&mut t.said);
        journal(said)?;
        note(look);
        if state.funded {
            let equity = state.account.cash + state.account.sol() * bid;
            let row = (last.ts, last.open, last.high, last.low, last.close, last.volume);
            store.record_lab_bar(
                (&run, &e.name),
                &row,
                &[],
                equity,
                sol_before * last.close,
                &serde_json::to_string(&state)?,
            )?;
            println!(
                "{}  bar closed at {:.2} · the budget is worth {:.4} USD ({:+.1} %){}",
                now(),
                last.close,
                equity,
                (equity / trade::budget_of(&state, &live) - 1.0) * 100.0,
                if state.account.lots.is_empty() { "" } else { " · in SOL" }
            );
        } else {
            // an empty account is not a lost one: its history starts with the budget
            save(&state, last.ts)?;
            println!("{}  bar closed at {:.2} · the budget is not set aside yet", now(), last.close);
        }
        let keep = warmup as usize + 8;
        if bars.len() > keep * 2 {
            bars.drain(..bars.len() - keep);
        }
        if other_bars.len() > keep * 2 {
            other_bars.drain(..other_bars.len() - keep);
        }
    }
    match &state.ended {
        Some(why) => println!("the run has ended: {why}"),
        None => println!(
            "stopped; what the run holds stays as it is ({:.6} SOL, {:.4} USD). The same command goes on; --close sells and ends it",
            state.account.sol(),
            state.account.cash
        ),
    }
    Ok(())
}

/// `--lab-report`: every live paper run, from what it recorded.
pub fn report(cfg: &Config, json: bool) -> Result<()> {
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let runs = store.lab_runs()?;
    if runs.is_empty() {
        println!("no lab runs yet: `--lab FILE` starts one");
        return Ok(());
    }
    let mut out = Vec::new();
    for (id, _, manifest) in runs {
        let plan = parse(&manifest).with_context(|| format!("the rules of run {id}"))?;
        let plan = Plan { id: id.clone(), ..plan };
        let real = id.starts_with("trade-");
        let mut all = Vec::new();
        let mut seen: Vec<Bar> = Vec::new();
        let mut unfunded = false;
        let mut holds = None;
        for e in &plan.experiments {
            let mut rows = store.lab_equity(&id, &e.name)?;
            let acct = match store.lab_state(&id, &e.name)? {
                // a run with real money keeps its account inside its state
                Some((_, json)) if real => {
                    let state: trade::State = serde_json::from_str(&json)?;
                    unfunded = !state.funded;
                    holds = plan.live.as_ref().map(|live| trade::holds(&state, live));
                    state.account
                }
                Some((_, json)) => serde_json::from_str(&json)?,
                None => Account::new(plan.capital),
            };
            if real {
                // bars from before the budget was set aside: the account was empty, not lost
                rows.retain(|r| r.2 > 0.0);
            }
            let points: Vec<Point> =
                rows.iter().map(|r| Point { ts: r.0, close: r.1, equity: r.2, sol_value: r.3 }).collect();
            if points.len() > seen.len() {
                seen = points
                    .iter()
                    .map(|p| Bar { ts: p.ts, open: p.close, high: p.close, low: p.close, close: p.close, volume: 0.0 })
                    .collect();
            }
            all.push(stats(&e.name, plan.capital, &plan.costs, &points, &acct));
        }
        if json {
            out.push(
                serde_json::json!({ "run": id, "real_money": real, "budget_set_aside": !unfunded, "experiments": all }),
            );
        } else {
            // a real run that has no bar to show yet: its name, and what there is to say of it
            let bare = |what: &str| {
                println!(
                    "LAB · REAL MONEY · {} {} · run {} · rule `{}`\n{what}\n",
                    plan.instrument,
                    plan.bar,
                    &id[..id.len().min(14)],
                    plan.experiments.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", "),
                )
            };
            let holds = holds.unwrap_or_default();
            if unfunded {
                bare(&format!(
                    "the budget of {:.2} USD has not been set aside yet: no swap has landed and nothing was spent",
                    plan.capital
                ));
            } else if real && seen.is_empty() {
                bare(&format!("{holds}\nno bar has closed since the budget was set aside"));
            } else {
                let what = if real { "REAL MONEY" } else { "paper, live prices" };
                let mut head = header(&plan, what, &seen);
                // the hash in the header is the file's; a real run is filed under its own name
                if real {
                    head = head.replace("costs ", "the costs below are the wallet's own · modelled costs ");
                }
                let mut text = stats::render(&head, &all);
                if real {
                    text = text.replace(
                        "Fills in a backtest are an assumption (the next bar's open); nothing here was sent anywhere.",
                        "These were real swaps, accounted for from the wallet's balances; each is in the journal below.",
                    );
                    text = format!("{text}\n{holds}\n");
                }
                println!("{text}");
            }
            for (ts, line) in store.lab_journal(&id)? {
                // the day and the hour of this machine, as the run printed them
                println!("  {}  {line}", searcher_core::Ts(ts * 1000).format("%Y-%m-%d %H:%M:%S"));
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
capital_usd = 100.0
[costs]
route_bps = 0.0
fixed_fee_lamports = 0.0

[[experiment]]
name = "reversal"
rule = "sign-reversal"

[[experiment]]
name = "grid"
rule = "grid"
step = 0.01
lots = 4
"#;

    #[test]
    fn a_rule_that_acts_on_the_price_decides_on_the_bar_that_is_forming() {
        use rules::Account;
        // a dip rule over four bars: flat at 100, then the price now far under them
        let text = "[live]\nbudget_usd = 2.0\nstop_total_loss = 0.5\nacknowledge = \"ALLOW LOSS\"\ntrigger = \"price\"\n\n\
                    [[experiment]]\nname = \"dip\"\nrule = \"dip\"\nwindow = 4\nk = 1.0\n";
        let plan = parse(text).unwrap();
        assert_eq!(plan.live.as_ref().unwrap().trigger, trade::Trigger::Price);
        // a file that does not say acts at the close, and its plan is the one it was before the word existed
        let closes = parse(&text.replace("trigger = \"price\"\n", "")).unwrap();
        assert_eq!(closes.live.as_ref().unwrap().trigger, trade::Trigger::Close);
        assert!(!closes.manifest.contains("trigger"), "{}", closes.manifest);
        assert!(parse(&text.replace("\"price\"", "\"often\"")).is_err());
        let bars: Vec<Bar> = (0..4)
            .map(|i| Bar { ts: i * 900_000, open: 100.0, high: 100.0, low: 100.0, close: 100.0, volume: 1.0 })
            .collect();
        let acct = Account::new(2.0);
        let rule = &plan.experiments[0].rule;
        // at the last close nothing; with the forming bar at 97 it buys, at 99.9 it does not
        assert!(rule.decide(&bars, &acct, 2.0, true).is_empty());
        let with = forming_bar(&bars, 4 * 900_000, 97.0);
        assert_eq!((with.len(), with[4].ts, with[4].close), (5, 3_600_000, 97.0));
        assert_eq!(rule.decide(&with, &acct, 2.0, true).len(), 1, "a buy, this second");
        assert!(rule.decide(&forming_bar(&bars, 4 * 900_000, 100.0), &acct, 2.0, true).is_empty());
    }

    #[test]
    fn sol_that_another_bot_of_the_wallet_holds_is_not_this_ones_to_sell() {
        let live = |name: &str| {
            format!(
                "[live]\nbudget_usd = 2.0\nstop_total_loss = 0.5\nacknowledge = \"ALLOW LOSS\"\n\n\
                 [[experiment]]\nname = \"{name}\"\nrule = \"dip\"\nwindow = 4\nk = 1.0\n"
            )
        };
        let dir = std::env::temp_dir().join(format!("mobius-others-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ResearchStore::open(&dir.join("research.sqlite")).unwrap();
        // three real runs: one holds SOL, one holds SOL but has ended, one is this one; and a paper run
        let mut ids = Vec::new();
        for (name, sol, ended) in [("held", 0.016542, false), ("over", 0.5, true), ("mine", 0.02, false)] {
            let plan = parse(&live(name)).unwrap();
            let id = format!("trade-{}", plan.id);
            store.begin_lab_run(&id, 1, "test", &plan.manifest).unwrap();
            let mut state = trade::State { funded: true, ..Default::default() };
            state.account.bought(2.0, sol, 0, 120.0);
            state.ended = ended.then(|| "closed by hand".to_string());
            store.set_lab_state(&id, name, 0, &serde_json::to_string(&state).unwrap()).unwrap();
            ids.push(id);
        }
        let paper = parse(FILE).unwrap();
        store.begin_lab_run(&paper.id, 1, "test", &paper.manifest).unwrap();
        // for the third: only what the first holds (the ended one holds nothing any more, its own is its own)
        assert_eq!(held_by_others(&store, &ids[2]).unwrap(), 16_542_000);
        // for the first: what the third holds
        assert_eq!(held_by_others(&store, &ids[0]).unwrap(), 20_000_000);
        assert_eq!(held_by_others(&store, "trade-new").unwrap(), 36_542_000);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rules_file_gives_a_plan_named_after_its_content() {
        let plan = parse(FILE).unwrap();
        assert_eq!((plan.instrument.as_str(), plan.bar.as_str(), plan.bar_ms), ("SOL-USDT", "15m", 900_000));
        assert_eq!(plan.experiments.len(), 2);
        assert_eq!(plan.experiments[1].rule, Rule::Grid { step: 0.01, lots: 4 });
        assert_eq!(plan.id, parse(FILE).unwrap().id);
        assert_ne!(plan.id, parse(&FILE.replace("lots = 4", "lots = 5")).unwrap().id, "a changed file is another run");
    }

    #[test]
    fn mistakes_in_a_rules_file_are_refused_with_the_experiment_named() {
        let err = |text: &str| format!("{:#}", parse(text).unwrap_err());
        assert!(err("capital_usd = 10.0").contains("no [[experiment]]"));
        assert!(err("[[experiment]]\nrule = \"sign-reversal\"").contains("needs a name"));
        assert!(err("[[experiment]]\nname = \"a\"\nrule = \"grid\"\nstep = 0.01").contains("experiment `a`"));
        assert!(err("[[experiment]]\nname = \"a\"\nrule = \"moon\"").contains("experiment `a`"));
        assert!(err("bar = \"7m\"\n[[experiment]]\nname = \"a\"\nrule = \"sign-reversal\"").contains("one of 1m"));
        assert!(
            err(&format!("{FILE}\n[[experiment]]\nname = \"grid\"\nrule = \"sign-reversal\""))
                .contains("two experiments")
        );
        assert!(err(&FILE.replace("capital_usd", "capital")).contains("rules file"));
    }

    #[test]
    fn a_backtest_fills_at_the_next_open_and_reports_every_experiment() {
        let plan = parse(FILE).unwrap();
        // closes fall then rise; each bar opens where the last closed
        let closes = [100.0, 98.9, 97.8, 96.7, 97.9, 99.0, 100.2, 100.2, 100.2];
        let bars: Vec<Bar> = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let open = if i == 0 { c } else { closes[i - 1] };
                Bar { ts: i as i64 * 900_000, open, high: open.max(c), low: open.min(c), close: c, volume: 1.0 }
            })
            .collect();
        let all: Vec<Stats> = plan.experiments.iter().map(|e| simulate(&plan, e, &bars, &[])).collect();
        // reversal: in at the open after the first fall (98.9), out at the open after the first rise (97.9)
        assert_eq!(all[0].trades, 1);
        assert!((all[0].ret - (97.9 / 98.9 - 1.0)).abs() < 1e-9, "{}", all[0].ret);
        // grid: three quarters bought on the way down, all sold a step above their own buys
        assert_eq!((all[1].trades, all[1].open_lots), (3, 0));
        assert!(all[1].ret > 0.0 && all[1].exposure > 0.0 && all[1].exposure < 0.75);
        let out = stats::render(&header(&plan, "backtest", &bars), &all);
        assert!(
            out.contains("reversal") && out.contains("grid") && out.contains("holding SOL over these bars: +0.2 %"),
            "{out}"
        );
    }

    #[test]
    fn completed_candles_only() {
        let rows: Vec<serde_json::Value> = serde_json::from_str(
            r#"[["1791100800000","121.5","121.9","121.4","121.7","10","1217","1217","0"],
                ["1791099900000","121.2","121.6","121.1","121.5","12","1458","1458","1"]]"#,
        )
        .unwrap();
        let c = candles(&rows);
        assert_eq!(c.len(), 1, "the candle still forming is left out");
        assert_eq!((c[0].ts, c[0].open, c[0].close), (1_791_099_900_000, 121.2, 121.5));
    }
}
