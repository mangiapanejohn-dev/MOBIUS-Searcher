//! The Bots page: rules that hold a position (buy low, sell high), as the
//! application reports them. The TUI neither reads their records nor starts
//! anything itself: it shows the [`BotsView`] the application fills and hands
//! the operator's three actions (start, stop, close) back through a
//! [`BotPort`], each after a `y`.
//!
//! What the page is for: to say at a glance how far the price is from the
//! bot's next action. A ruler carries the buy and the sell price as fixed
//! ticks and the live price as the mark that moves; the candles under it are
//! live (the exchange's, each second) with the same prices drawn across; and
//! the numbers behind those prices are written out.

use crate::app::{App, Hit};
use crate::cex::{BARS, Candle};
use crate::chart::{put, text, text_fit, width};
use crate::kline::{Kline, Level, price, render_kline};
use crate::panels::{fill, section, wrap_words};
use parking_lot::{RwLock, RwLockReadGuard};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::Ts;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum BotState {
    /// Its program is running: it acts at each bar's close.
    Running,
    /// Not running. What it holds stays as it is.
    Stopped,
    /// Over for good, and why.
    Ended(String),
    /// A paper account: nothing is ever sent.
    Paper,
}

/// Prices at which the rule acts next, when it has such prices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Levels {
    /// Buys when a bar closes under this.
    pub buy: Option<f64>,
    /// Sells when a bar closes above this.
    pub sell: Option<f64>,
    /// Sells at a loss when a bar closes under this.
    pub stop: Option<f64>,
}

/// The numbers a rule of the "under its average" kind works its prices out from.
#[derive(Clone, Debug, PartialEq)]
pub struct Calc {
    /// Bars the average is taken over.
    pub window: usize,
    pub mean: f64,
    /// Deviation of those closes around their average.
    pub sd: f64,
    /// Deviations under the average at which it buys.
    pub k: f64,
    /// Deviations from the average at which it sells again.
    pub exit_z: f64,
    /// Share under what a buy cost at which it is sold at a loss.
    pub stop: Option<f64>,
}

/// One bot as the page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BotView {
    /// What an action names it by.
    pub id: String,
    pub name: String,
    pub real: bool,
    /// Instrument and bar, e.g. `SOL-USDT` and `15m`.
    pub inst: String,
    pub bar: String,
    pub bar_ms: i64,
    /// What the rule does, in a sentence (in the operator's language).
    pub rule: String,
    pub state: BotState,
    /// Its budget is set aside (a paper account always is).
    pub funded: bool,
    /// A swap was sent and is not accounted for yet.
    pub pending: bool,
    pub budget: f64,
    /// Worth this or less, everything is sold and the run ends.
    pub stop_at: Option<f64>,
    /// USD it holds as USDC.
    pub cash: f64,
    pub sol: f64,
    /// USD paid for the SOL it holds.
    pub paid: f64,
    /// What it holds, at the last close.
    pub worth: Option<f64>,
    /// Closed trades: when (ms), USD paid in, USD net of everything.
    pub trades: Vec<(i64, f64, f64)>,
    pub levels: Levels,
    pub calc: Option<Calc>,
    /// Recent bars: close time (ms) and close.
    pub closes: Vec<(i64, f64)>,
    /// Its buys (`true`) and sells: when (ms).
    pub fills: Vec<(i64, bool)>,
    /// What it said, oldest first: when (ms) and the line.
    pub journal: Vec<(i64, String)>,
    /// Its rules file, when the application knows it (it starts from it).
    pub file: Option<String>,
    /// A budget it was asked to change to and has not yet (USD).
    pub wish: Option<f64>,
    /// When the SOL it holds was bought (ms), while it holds some.
    pub opened: Option<i64>,
    /// What it was worth at each bar's close, oldest first: when (ms) and USD.
    pub equity: Vec<(i64, f64)>,
}

/// One buy or sale of a bot, as its record tells it.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    /// When (ms).
    pub at: i64,
    pub buy: bool,
    pub sol: f64,
    pub usdc: f64,
    /// USDC a SOL, every cost inside.
    pub price: f64,
    /// What the trade made, on its sale.
    pub net: Option<f64>,
}

/// The buys and sales its record tells of, oldest first.
pub fn fills_of(b: &BotView) -> Vec<Fill> {
    let num = |s: &str| s.parse::<f64>().ok();
    b.journal
        .iter()
        .filter_map(|(at, line)| {
            if let Some(v) = fit(line, "bought {} SOL for {} USDC ({} a SOL, every cost inside)") {
                let (sol, usdc) = (num(v[0])?, num(v[1])?);
                return Some(Fill { at: *at, buy: true, sol, usdc, price: num(v[2])?, net: None });
            }
            let v = fit(line, "sold {} SOL for {} USDC; this trade {} USD")?;
            let (sol, usdc) = (num(v[0])?, num(v[1])?);
            Some(Fill {
                at: *at,
                buy: false,
                sol,
                usdc,
                price: if sol > 0.0 { usdc / sol } else { 0.0 },
                net: num(v[2]),
            })
        })
        .collect()
}

/// Every buy and sale of the real bots, newest first, each with its bot's name:
/// what the Trades page lists beside the arbitrage's own.
pub fn bot_fills(app: &App) -> Vec<(String, Fill)> {
    let Some(bots) = &app.bots else { return Vec::new() };
    let view = bots.read();
    let mut all: Vec<(String, Fill)> = view
        .bots
        .iter()
        .filter(|b| b.real)
        .flat_map(|b| fills_of(b).into_iter().map(|f| (b.name.clone(), f)))
        .collect();
    all.sort_by_key(|f| std::cmp::Reverse(f.1.at));
    all
}

/// A length of time in the words it is said in: `6 h 13 min`, `6 小时 13 分`.
fn span(ms: i64, zh: bool) -> String {
    let m = (ms / 60_000).max(0);
    match (m / 1440, m / 60 % 24, m % 60, zh) {
        (0, 0, min, true) => format!("{min} 分钟"),
        (0, 0, min, false) => format!("{min} min"),
        (0, h, min, true) => format!("{h} 小时 {min} 分"),
        (0, h, min, false) => format!("{h} h {min} min"),
        (d, h, _, true) => format!("{d} 天 {h} 小时"),
        (d, h, _, false) => format!("{d} d {h} h"),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BotsView {
    pub bots: Vec<BotView>,
    /// Why the view could not be read, when it could not.
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BotAction {
    /// Start its program (or start it again where it stopped).
    Start,
    /// Stop its program; what it holds stays.
    Stop,
    /// Sell what it holds and end it for good.
    Close,
    /// Change the USD it may use to this many cents.
    Budget { cents: u32 },
}

impl BotAction {
    pub fn verb(self, zh: bool) -> &'static str {
        match (self, zh) {
            (BotAction::Start, false) => "start",
            (BotAction::Stop, false) => "stop",
            (BotAction::Close, false) => "close",
            (BotAction::Budget { .. }, false) => "change it",
            (BotAction::Start, true) => "启动",
            (BotAction::Stop, true) => "停止",
            (BotAction::Close, true) => "平仓",
            (BotAction::Budget { .. }, true) => "调整",
        }
    }
}

/// A bot to be made from the page (`n`): a rule that buys SOL under its
/// average and sells it back at it, with a budget of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct NewBot {
    pub name: String,
    /// Bars of 15 minutes the average is taken over.
    pub window: usize,
    /// Deviations under the average at which it buys.
    pub k: f64,
    /// Share under what a buy cost at which it is sold at a loss.
    pub stop: f64,
    pub budget: f64,
    /// Share of the budget: down this much, everything is sold and the run ends.
    pub total_stop: f64,
    /// The words that say it may lose money, as the operator typed them.
    pub acknowledge: String,
}

type ViewFn = dyn Fn() -> BotsView + Send + Sync;
type ActFn = dyn Fn(&str, BotAction) -> Result<String, String> + Send + Sync;
type CreateFn = dyn Fn(&NewBot) -> Result<String, String> + Send + Sync;

/// How the TUI reaches the bots: the application's two functions.
#[derive(Clone)]
pub struct BotPort {
    pub view: Arc<ViewFn>,
    /// Do `action` to the bot of this id. `Ok`: what was done, in a few words.
    pub act: Arc<ActFn>,
    /// Write the rules file of a new bot (it is not started). `Ok`: what was made.
    pub create: Arc<CreateFn>,
}

impl std::fmt::Debug for BotPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BotPort")
    }
}

/// The view, kept fresh on a thread of its own (a frame never waits for it).
pub struct Bots {
    state: Arc<RwLock<BotsView>>,
    port: Option<BotPort>,
    stop: Arc<AtomicBool>,
}

impl Bots {
    /// Read now, then again every second until dropped.
    pub fn start(port: BotPort) -> Bots {
        let state = Arc::new(RwLock::new((port.view)()));
        let stop = Arc::new(AtomicBool::new(false));
        let (view, shared, stopped) = (port.view.clone(), state.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("bots".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                // short naps: a dropped page is noticed at once
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(100));
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                }
                let fresh = view();
                *shared.write() = fresh;
            }
        });
        Bots { state, port: Some(port), stop }
    }

    /// A view that does not change (snapshots, tests).
    pub fn fixed(view: BotsView) -> Bots {
        Bots { state: Arc::new(RwLock::new(view)), port: None, stop: Arc::new(AtomicBool::new(false)) }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, BotsView> {
        self.state.read()
    }

    /// Do it, then read the view again so the page shows what changed.
    pub fn act(&self, id: &str, action: BotAction) -> Result<String, String> {
        let Some(port) = &self.port else { return Err("not available in this view".into()) };
        let done = (port.act)(id, action);
        *self.state.write() = (port.view)();
        done
    }
}

impl Bots {
    /// Make a new bot's rules file, then read the view again so the page lists it.
    pub fn create(&self, spec: &NewBot) -> Result<String, String> {
        let Some(port) = &self.port else { return Err("not available in this view".into()) };
        let made = (port.create)(spec);
        *self.state.write() = (port.view)();
        made
    }
}

impl Drop for Bots {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `en` or `cn`, by the operator's language.
fn t<'a>(zh: bool, en: &'a str, cn: &'a str) -> &'a str {
    if zh { cn } else { en }
}

/// A line the bot's program printed, in the operator's language. The program
/// writes English; the lines it writes are few, and each is told by its shape.
pub fn said(zh: bool, line: &str) -> String {
    if !zh {
        return line.to_string();
    }
    const SHAPES: [(&str, &str); 18] = [
        (
            "budget raised from {} to {} USD with {} USDC the wallet held",
            "预算从 {} 调高到 {} 美元：用的是钱包里空闲的 {} USDC",
        ),
        (
            "raising the budget from {} to {} USD: selling {} SOL for about {} USDC",
            "调高预算（{} → {} 美元）：卖出 {} SOL，换约 {} USDC",
        ),
        (
            "budget raised from {} to {} USD: {} SOL became {} USDC; {} more USD is the rule's",
            "预算从 {} 调高到 {} 美元：{} SOL 换成 {} USDC；规则多了 {} 美元可用",
        ),
        (
            "budget lowered from {} to {} USD: {} USDC is the wallet's again",
            "预算从 {} 调低到 {} 美元：{} USDC 还给钱包",
        ),
        ("budget changed from {} to {} USD before it was set aside", "预算在划拨之前从 {} 改成 {} 美元"),
        (
            "to lower the budget to {} USD the rule gives {} USD back, and it holds {} in USDC (the rest is SOL): it waits until it has sold",
            "要把预算调低到 {} 美元需要退回 {} 美元，而它手里只有 {} USDC（其余是 SOL）：等它卖出后再调",
        ),
        ("budget not changed: {}", "预算没有改：{}"),
        ("the wallet holds {} USDC: {} of it is the rule's budget", "钱包里有 {} USDC：其中 {} 作为这条规则的预算"),
        ("setting the budget aside: selling {} SOL for about {} USDC", "划拨预算：卖出 {} SOL，换约 {} USDC"),
        (
            "budget set aside: {} SOL became {} USDC; the rule has {} USD",
            "预算已划拨：{} SOL 换成 {} USDC；规则可用 {} 美元",
        ),
        ("bought {} SOL for {} USDC ({} a SOL, every cost inside)", "买入 {} SOL，花费 {} USDC（均价 {}，含全部费用）"),
        ("sold {} SOL for {} USDC; this trade {} USD", "卖出 {} SOL，得到 {} USDC；这一笔 {} 美元"),
        ("closing by hand: selling what the run holds", "手动平仓：卖出持仓"),
        (
            "the swap that was under way did not change the wallet: it did not land",
            "上一笔兑换没有改变钱包余额：没有成交",
        ),
        ("the wallet has not changed yet: looked at again at the next bar", "钱包余额还没变化：下一根线再核对"),
        ("the run has ended: {}", "这一轮已结束：{}"),
        ("STOP: {}", "止损：{}"),
        ("not sent: {}", "没有发出：{}"),
    ];
    // what was sent and what became of it: one after the other, each told by its own shape
    const SENT: [(&str, &str); 8] = [
        (
            "spend {} USDC for at least {} SOL (quoted {}) via {}, {} CU, priority fee {} + tip {} lamports",
            "用 {} USDC 买入，最少到手 {} SOL（报价 {}），经 {}，{} CU，优先费 {} + 小费 {} lamports",
        ),
        (
            "sell {} SOL for at least {} USDC (quoted {}) via {}, {} CU, priority fee {} + tip {} lamports",
            "卖出 {} SOL，最少到手 {} USDC（报价 {}），经 {}，{} CU，优先费 {} + 小费 {} lamports",
        ),
        ("confirmed", "链上已确认"),
        ("signature {}", "签名 {}"),
        ("it expired without landing", "过期了，没有上链"),
        ("nothing was heard of it in two minutes", "两分钟内没有回音"),
        (
            "it landed and failed ({}): its fee is paid, nothing was swapped",
            "上链了但执行失败（{}）：手续费已付，没有兑换",
        ),
        ("refused by {}", "{} 没有接收"),
    ];
    if let Some(rest) = line.strip_prefix("sent: ") {
        let parts: Vec<String> = rest.split("; ").map(|p| shaped(p, &SENT).unwrap_or_else(|| p.to_string())).collect();
        return format!("已发出：{}", parts.join("；"));
    }
    shaped(line, &SHAPES).unwrap_or_else(|| line.to_string())
}

/// `line` in the words of the shape it has among `shapes`, its values where they were.
fn shaped(line: &str, shapes: &[(&str, &str)]) -> Option<String> {
    let (values, cn) = shapes.iter().find_map(|(shape, cn)| Some((fit(line, shape)?, cn)))?;
    let mut out = String::new();
    for (i, part) in cn.split("{}").enumerate() {
        if i > 0 {
            out.push_str(values.get(i - 1).copied().unwrap_or_default());
        }
        out.push_str(part);
    }
    Some(out)
}

/// The whole record of a bot as a document (see `panels::doc_rows`): a
/// heading and a table a day, a row for each thing it did, and what a thing
/// is made of as named parts under one another in its row, so that a swap is
/// read as its parts and not as one long line.
pub fn journal_doc(b: &BotView, zh: bool) -> String {
    // (a bar inside a cell would end it)
    let cell = |s: &str| s.replace('|', "/");
    let (mut out, mut day) = (String::new(), String::new());
    for (ts, line) in &b.journal {
        let at = Ts(ts * 1000);
        let d = at.format("%Y-%m-%d");
        if d != day {
            if !out.is_empty() {
                out.push('\n');
            }
            out += &format!("# {d}\n\n");
            out += t(
                zh,
                "| Time | What | Item | Detail |\n|---|---|---|---|\n",
                "| 时间 | 事件 | 项目 | 内容 |\n|---|---|---|---|\n",
            );
            day = d;
        }
        let (title, fields) = entry(line, zh);
        if fields.is_empty() {
            // something it said that has no parts: the words themselves are the detail
            out += &format!("| {} | {} | | {} |\n", at.hms(), t(zh, "note", "说明"), cell(&title));
        }
        for (i, (name, value)) in fields.iter().enumerate() {
            let (when, what) = if i == 0 { (at.hms(), title.clone()) } else { (String::new(), String::new()) };
            out += &format!("| {when} | {what} | {} | {} |\n", cell(name), cell(value));
        }
    }
    out
}

/// One line of a record as an entry: what it was, and its parts by name.
fn entry(line: &str, zh: bool) -> (String, Vec<(String, String)>) {
    let w = |en: &str, cn: &str| t(zh, en, cn).to_string();
    if let Some(v) = fit(line, "bought {} SOL for {} USDC ({} a SOL, every cost inside)") {
        return (
            w("Bought", "买入"),
            vec![
                (w("Amount", "数量"), format!("{} SOL", v[0])),
                (w("Paid", "花费"), format!("{} USDC", v[1])),
                (
                    w("Price", "均价"),
                    format!("{} {}", v[2], w("USDC a SOL, every cost inside", "USDC 一个 SOL（含全部费用）")),
                ),
            ],
        );
    }
    if let Some(v) = fit(line, "sold {} SOL for {} USDC; this trade {} USD") {
        return (
            w("Sold", "卖出"),
            vec![
                (w("Amount", "数量"), format!("{} SOL", v[0])),
                (w("Got", "得到"), format!("{} USDC", v[1])),
                (w("This trade", "这一笔盈亏"), format!("{} USD", v[2])),
            ],
        );
    }
    if let Some(v) = fit(line, "the wallet holds {} USDC: {} of it is the rule's budget") {
        return (
            w("Budget set aside", "划拨预算"),
            vec![
                (w("The wallet held", "钱包里有"), format!("{} USDC", v[0])),
                (w("Its budget", "作为预算"), format!("{} USDC{}", v[1], w(" (nothing was swapped)", "（不用兑换）"))),
            ],
        );
    }
    if let Some(v) = fit(line, "budget set aside: {} SOL became {} USDC; the rule has {} USD") {
        return (
            w("Budget set aside", "预算已划拨"),
            vec![
                (w("Sold", "卖出"), format!("{} SOL", v[0])),
                (w("Got", "得到"), format!("{} USDC", v[1])),
                (w("Its budget", "规则可用"), format!("{} USD", v[2])),
            ],
        );
    }
    if let Some(v) = fit(line, "not sent: {}") {
        return (w("Not sent", "没有发出"), vec![(w("Why", "原因"), v[0].to_string())]);
    }
    if let Some(rest) = line.strip_prefix("sent: ") {
        let mut fields = Vec::new();
        for part in rest.split("; ") {
            let swap = [
                (
                    false,
                    "spend {} USDC for at least {} SOL (quoted {}) via {}, {} CU, priority fee {} + tip {} lamports",
                ),
                (true, "sell {} SOL for at least {} USDC (quoted {}) via {}, {} CU, priority fee {} + tip {} lamports"),
            ];
            if let Some((sells, v)) = swap.iter().find_map(|(sells, shape)| Some((*sells, fit(part, shape)?))) {
                let (gives, gets) = if sells { ("SOL", "USDC") } else { ("USDC", "SOL") };
                fields.push((
                    w("Swap", "兑换"),
                    if zh {
                        format!("用 {} {gives} 换 {gets}", v[0])
                    } else {
                        format!("{} {gives} for {gets}", v[0])
                    },
                ));
                fields.push((
                    w("At least", "最少到手"),
                    if zh {
                        format!("{} {gets}（报价 {}）", v[1], v[2])
                    } else {
                        format!("{} {gets} (quoted {})", v[1], v[2])
                    },
                ));
                fields.push((w("Through", "经过"), v[3].to_string()));
                fields.push((
                    w("Fees", "网络费"),
                    if zh {
                        format!("优先费 {} + 小费 {} lamports（计算量 {} CU）", v[5], v[6], v[4])
                    } else {
                        format!("priority fee {} + tip {} lamports ({} CU)", v[5], v[6], v[4])
                    },
                ));
            } else if part == "confirmed" {
                fields.push((w("Outcome", "结果"), w("confirmed on the chain", "链上已确认")));
            } else if let Some(v) = fit(part, "signature {}") {
                fields.push((w("Signature", "签名"), v[0].to_string()));
            } else if part.starts_with("refused by ") && part.contains("already processed") {
                // one of the two ways it is sent saw it land through the other first: no failure
                fields.push((
                    w("Note", "备注"),
                    w(
                        "Jito answered that it had landed already (it went through the RPC first): not a failure",
                        "Jito 回答“交易已经上链”（它先从另一条通道到了）：不是失败",
                    ),
                ));
            } else if let Some(v) = fit(part, "refused by {}") {
                fields.push((
                    w("Note", "备注"),
                    if zh { format!("{} 没有接收", v[0]) } else { format!("refused by {}", v[0]) },
                ));
            } else {
                fields.push((w("Outcome", "结果"), shaped_sent(part, zh)));
            }
        }
        return (w("Swap sent", "已发出兑换"), fields);
    }
    (said(zh, line), Vec::new())
}

/// What became of a sent swap, in the operator's language (the parts `said` knows).
fn shaped_sent(part: &str, zh: bool) -> String {
    let whole = said(zh, &format!("sent: {part}"));
    whole.strip_prefix("已发出：").unwrap_or(part).to_string()
}

/// The values of `line` where `shape` has `{}`, when the line has that shape.
fn fit<'a>(line: &'a str, shape: &str) -> Option<Vec<&'a str>> {
    let mut parts = shape.split("{}");
    let mut rest = line.strip_prefix(parts.next()?)?;
    let mut values = Vec::new();
    let parts: Vec<&str> = parts.collect();
    for (i, part) in parts.iter().enumerate() {
        // the last value runs to the end of the line when nothing follows it
        let at = if part.is_empty() && i == parts.len() - 1 { rest.len() } else { rest.find(part)? };
        values.push(&rest[..at]);
        rest = &rest[at + part.len()..];
    }
    rest.is_empty().then_some(values)
}

/// Why a run ended, in the operator's language.
fn ended(zh: bool, why: &str) -> String {
    if !zh {
        return why.to_string();
    }
    if let Some(v) = fit(why, "closed by hand at {} USD of {}") {
        return format!("手动平仓，结束时 {} 美元（预算 {}）", v[0], v[1]);
    }
    if let Some(v) = fit(why, "stopped at {} USD of {}") {
        return format!("触发总止损，结束时 {} 美元（预算 {}）", v[0], v[1]);
    }
    why.to_string()
}

impl BotView {
    /// USD its closed trades made, net of everything.
    pub fn result(&self) -> f64 {
        self.trades.iter().map(|t| t.2).sum()
    }

    /// Instrument and bar, as one name.
    pub fn market(&self) -> String {
        format!("{} {}", self.inst, self.bar)
    }

    /// What a SOL it holds cost, every cost inside.
    fn cost(&self) -> Option<f64> {
        (self.sol > 0.0 && self.paid > 0.0).then(|| self.paid / self.sol)
    }

    fn last_close(&self) -> Option<f64> {
        self.closes.last().map(|c| c.1).filter(|p| *p > 0.0)
    }

    /// The bar, as the operator says it: `15m bar`, `15 分钟线`.
    fn bar_name(&self, zh: bool) -> String {
        if !zh {
            return format!("{} bar", self.bar);
        }
        match self.bar_ms / 60_000 {
            m if m >= 60 && m % 60 == 0 => format!("{} 小时线", m / 60),
            m => format!("{m} 分钟线"),
        }
    }

    /// Whether `action` makes sense now; if not, why not.
    pub fn can(&self, action: BotAction, zh: bool) -> Result<(), &'static str> {
        match (action, &self.state) {
            (_, BotState::Paper) => Err(t(
                zh,
                "a paper run is started and stopped by its own command (--lab)",
                "纸面实验由它自己的命令（--lab）启动和停止",
            )),
            (_, BotState::Ended(_)) => Err(t(
                zh,
                "this run has ended; a changed rules file starts a new one",
                "这一轮已经结束；改动规则文件会开始新的一轮",
            )),
            (BotAction::Start, BotState::Running) => Err(t(zh, "it is running already", "它已经在运行")),
            (BotAction::Stop, BotState::Stopped) => Err(t(zh, "it is not running", "它没有在运行")),
            (BotAction::Close, BotState::Running) => {
                Err(t(zh, "stop it first (x), then close it", "先停止（x），再平仓"))
            }
            (BotAction::Start | BotAction::Close, _) if self.file.is_none() => Err(t(
                zh,
                "its rules file is not known here: start it once with --trade FILE",
                "找不到它的规则文件：先用 --trade FILE 启动一次",
            )),
            _ => Ok(()),
        }
    }

    /// A few words: what it is doing.
    pub fn brief(&self, zh: bool) -> String {
        match &self.state {
            BotState::Ended(_) => t(zh, "ended", "已结束").into(),
            BotState::Stopped => t(zh, "stopped", "已停止").into(),
            _ if !self.funded => t(zh, "setting its budget aside", "正在划拨预算").into(),
            _ if self.sol > 0.0 => match self.worth.filter(|_| self.budget > 0.0) {
                Some(w) if zh => format!("持仓中 {:+.1}%", (w / self.budget - 1.0) * 100.0),
                Some(w) => format!("in SOL {:+.1} %", (w / self.budget - 1.0) * 100.0),
                None => t(zh, "in SOL", "持仓中").into(),
            },
            _ => match self.levels.buy {
                Some(b) if zh => format!("等待买入，低于 {} 就买", price(b, 2)),
                Some(b) => format!("waiting, buys under {}", price(b, 2)),
                None => t(zh, "waiting to buy", "等待买入").into(),
            },
        }
    }

    /// What it is doing or waiting for, in sentences. `live`: the price now,
    /// when there is one; without it the last close stands in.
    pub fn doing(&self, zh: bool, live: Option<f64>) -> String {
        if let BotState::Ended(why) = &self.state {
            let why = ended(zh, why);
            return if zh {
                format!("已结束：{why}。结束的一轮不会再启动；改动规则文件就是新的一轮。")
            } else {
                format!("Ended: {why}. A run that ended does not start again; a changed rules file is a new one.")
            };
        }
        let mut s = String::new();
        if self.state == BotState::Stopped {
            s.push_str(t(
                zh,
                "Not running: nothing is bought or sold while it is stopped, and its stops do not act. ",
                "没在运行：停着的时候不买不卖，止损也不生效。",
            ));
        }
        if !self.funded {
            return s + t(
                zh,
                "Its budget is not set aside yet: that is the first thing it does.",
                "预算还没划拨，这是它启动后做的第一件事。",
            );
        }
        if self.pending {
            s.push_str(t(zh, "A swap was sent and is being accounted for. ", "有一笔兑换已发出，正在核对到账。"));
        }
        let (now, bar) = (live.or(self.last_close()), self.bar_name(zh));
        let what = match (live.is_some(), zh) {
            (true, false) => "the price now",
            (false, false) => "the last close",
            (true, true) => "现价",
            (false, true) => "上一根收盘价",
        };
        if self.sol > 0.0 {
            s += &if zh {
                format!("持有 {:.6} SOL，成本 {:.4} 美元。", self.sol, self.paid)
            } else {
                format!("Holds {:.6} SOL bought for {:.4} USD.", self.sol, self.paid)
            };
            if let Some(v) = self.levels.sell {
                let far = now.map(|p| (v / p - 1.0) * 100.0);
                s += &match (far, zh) {
                    (Some(d), true) if d > 0.0 => {
                        format!(
                            "一根{bar}收盘高于 {} 就卖出，{what} {}，还要再涨 {d:.2}%。",
                            price(v, 2),
                            price(now.unwrap_or(0.0), 2)
                        )
                    }
                    (Some(_), true) => format!(
                        "{what} {} 已经高于卖出线 {}：这根线收盘时还在线上就卖出。",
                        price(now.unwrap_or(0.0), 2),
                        price(v, 2)
                    ),
                    (None, true) => format!("一根{bar}收盘高于 {} 就卖出。", price(v, 2)),
                    (Some(d), false) if d > 0.0 => format!(
                        " Sells when a {bar} closes above {}, {d:.2} % above {what} ({}).",
                        price(v, 2),
                        price(now.unwrap_or(0.0), 2)
                    ),
                    (Some(_), false) => format!(
                        " Sells when a {bar} closes above {}. {} ({}) is above it.",
                        price(v, 2),
                        capital(what),
                        price(now.unwrap_or(0.0), 2)
                    ),
                    (None, false) => format!(" Sells when a {bar} closes above {}.", price(v, 2)),
                };
            }
            if let Some(v) = self.levels.stop {
                s += &if zh {
                    format!("收盘低于 {} 止损卖出。", price(v, 2))
                } else {
                    format!(" Sells at a loss when one closes under {}.", price(v, 2))
                };
            }
        } else {
            let Some(v) = self.levels.buy else {
                return s + t(zh, "Waiting for its rule to say buy.", "等待规则给出买入信号。");
            };
            let far = now.map(|p| (1.0 - v / p) * 100.0);
            s += &match (far, zh) {
                (Some(d), true) if d > 0.0 => {
                    format!(
                        "等待买入：一根{bar}收盘低于 {} 就买，{what} {}，还要再跌 {d:.2}%。",
                        price(v, 2),
                        price(now.unwrap_or(0.0), 2)
                    )
                }
                (Some(_), true) => format!(
                    "等待买入：{what} {} 已经低于买入线 {}，这根线收盘时还在线下就买。",
                    price(now.unwrap_or(0.0), 2),
                    price(v, 2)
                ),
                (None, true) => format!("等待买入：一根{bar}收盘低于 {} 就买。", price(v, 2)),
                (Some(d), false) if d > 0.0 => format!(
                    "Waiting to buy: when a {bar} closes under {}, {d:.2} % below {what} ({}).",
                    price(v, 2),
                    price(now.unwrap_or(0.0), 2)
                ),
                (Some(_), false) => format!(
                    "Waiting to buy: when a {bar} closes under {}. {} ({}) is under it.",
                    price(v, 2),
                    capital(what),
                    price(now.unwrap_or(0.0), 2)
                ),
                (None, false) => format!("Waiting to buy: when a {bar} closes under {}.", price(v, 2)),
            };
        }
        s
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
}

/// When a journal line was said: the hour, and the day too when it is not `today`'s.
fn clock(ms: i64, today: &str) -> String {
    let t = Ts(ms * 1000);
    if t.format("%m-%d") == today { t.hms() } else { t.format("%m-%d %H:%M") }
}

fn state_mark(b: &BotView, app: &App) -> (&'static str, Style, &'static str) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    match &b.state {
        BotState::Running => (g.live, Style::new().fg(th.profit), t(zh, "running", "运行中")),
        BotState::Stopped => (g.off, th.warn(), t(zh, "stopped", "已停止")),
        BotState::Ended(_) => (g.off, th.faint(), t(zh, "ended", "已结束")),
        BotState::Paper => (g.off, th.muted(), t(zh, "paper", "纸面")),
    }
}

/// The header's word on the bots: the real one that runs, else the newest real one.
pub fn chip(view: &BotsView, zh: bool) -> Option<(String, bool)> {
    let real = || view.bots.iter().filter(|b| b.real && !matches!(b.state, BotState::Ended(_)));
    let b = real().find(|b| b.state == BotState::Running).or_else(|| real().next())?;
    Some((format!("{} · {}", b.name, b.brief(zh)), b.state == BotState::Running))
}

/// The exchange stream of a bot's market: its pair and bar among those the Markets page has.
pub fn stream_of(app: &App, b: &BotView) -> Option<(usize, usize)> {
    let cex = app.cex.as_ref()?;
    let pair = cex.source.markets.iter().position(|m| *m == b.inst)?;
    let bar = BARS.iter().position(|(label, _, _)| *label == b.bar)?;
    Some((pair, bar))
}

/// The stream its chart shows: its own bar, or the one chosen on the page (`[` `]`).
pub fn shown_stream(app: &App, b: &BotView) -> Option<(usize, usize)> {
    let (pair, own) = stream_of(app, b)?;
    Some((pair, app.bot_bar.unwrap_or(own).min(BARS.len() - 1)))
}

/// The bot's candles and the price now: the exchange's own when its stream is
/// this market's (the newest candle follows the ticker), else the closes the
/// bot recorded, as flat candles.
fn candles_of(app: &App, b: &BotView) -> (Vec<Candle>, Option<f64>, bool) {
    if let (Some(sel), Some(cex)) = (shown_stream(app, b), app.cex.as_ref()) {
        let s = cex.state.read();
        if s.candles_for == Some(sel) && !s.candles.is_empty() {
            let live = s.ticker.as_ref().filter(|t| t.inst == b.inst).map(|t| t.last);
            let mut c = s.candles.clone();
            if let (Some(last), Some(p)) = (c.last_mut(), live) {
                last.close = p;
                last.high = last.high.max(p);
                last.low = last.low.min(p);
            }
            return (c, live, true);
        }
    }
    let mut prev = None;
    let c = b
        .closes
        .iter()
        .map(|(at, close)| {
            let open = prev.replace(*close).unwrap_or(*close);
            Candle {
                start: Ts((at - b.bar_ms) * 1000),
                open,
                high: open.max(*close),
                low: open.min(*close),
                close: *close,
                vol: 0.0,
            }
        })
        .collect();
    (c, None, false)
}

/// How many of the view's bots the page lists: the real ones (they come
/// first), and the paper experiments only once they are asked for (`p`):
/// a paper account is not the operator's money, and is not shown as if it were.
pub fn shown(app: &App, view: &BotsView) -> usize {
    if app.bot_paper { view.bots.len() } else { view.bots.iter().take_while(|b| b.real).count() }
}

pub fn page_bots(buf: &mut Buffer, body: Rect, app: &App) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let title = t(zh, "BOTS", "机器人");
    let Some(bots) = &app.bots else {
        let b = section(buf, body, title, false, "", th, g);
        text(buf, b.x, b.y, t(zh, "Bots are not part of this view.", "这个视图里没有机器人。"), b.width, th.faint());
        return;
    };
    let view = bots.read();
    let n = shown(app, &view);
    if n == 0 {
        let b = section(buf, body, title, false, "", th, g);
        // paper experiments there are, kept out of sight until asked for
        if !view.bots.is_empty() && b.height > 7 {
            let hidden = if zh {
                format!("另有 {} 个纸面实验（模拟账户，不是真钱）没有显示：按 p 显示。", view.bots.len())
            } else {
                format!(
                    "{} paper experiments (simulated accounts, no money) are not shown: p shows them.",
                    view.bots.len()
                )
            };
            text_fit(buf, b.x, b.y + 6, &hidden, b.width, th.faint());
        }
        let lines: [&str; 5] = if zh {
            [
                "还没有机器人运行过。",
                "",
                "一个机器人就是一条规则：低价买入 SOL、涨回去卖出，只用你钱包里的一小笔预算。",
                "按 n 在这里新建一个：选类型、填预算和止损，创建后按 s 启动。",
                "（也可以自己写规则文件：config/trade.toml 是例子，docs/TRADE.md 有说明）",
            ]
        } else {
            [
                "No bot has run yet.",
                "",
                "A bot is one rule that buys SOL low and sells it higher, with a small budget from your wallet.",
                "n makes one here: its kind, its budget and its stops; once made, s starts it.",
                "(Or write its rules file yourself: config/trade.toml is an example, docs/TRADE.md explains it.)",
            ]
        };
        for (i, l) in lines.iter().enumerate().take(b.height as usize) {
            text_fit(buf, b.x, b.y + i as u16, l, b.width, if i == 0 { th.text() } else { th.faint() });
        }
        if let Some(e) = &view.error {
            text_fit(buf, b.x, b.bottom().saturating_sub(1), e, b.width, th.warn());
        }
        return;
    }
    let sel = app.bot_selected.min(n - 1);
    let wide = body.width >= 110;
    let (list, detail) = if wide {
        let lw = if body.width >= 150 { 44 } else { 34 };
        (Rect { width: lw, ..body }, Rect { x: body.x + lw + 2, width: body.width - lw - 2, ..body })
    } else {
        let lh = (n as u16 * 2 + 1).min(body.height / 4).max(3);
        (Rect { height: lh, ..body }, Rect { y: body.y + lh, height: body.height - lh, ..body })
    };
    bot_list(buf, list, app, &view, sel);
    bot_detail(buf, detail, app, &view.bots[sel]);
    if let Some(e) = &view.error {
        text_fit(buf, list.x, list.bottom().saturating_sub(1), e, list.width, th.warn());
    }
}

fn bot_list(buf: &mut Buffer, area: Rect, app: &App, view: &BotsView, sel: usize) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let n = shown(app, view);
    let paper = view.bots.iter().filter(|b| !b.real).count();
    let right = match (paper, app.bot_paper, zh) {
        (0, _, true) if n > 1 => "j/k 选择".to_string(),
        (0, _, false) if n > 1 => "j/k select".to_string(),
        (0, ..) => String::new(),
        (p, false, true) => format!("p 显示纸面实验（{p}）"),
        (p, false, false) => format!("p paper runs ({p})"),
        (_, true, true) => "p 隐藏纸面实验".to_string(),
        (_, true, false) => "p hide paper runs".to_string(),
    };
    let inner = section(buf, area, t(zh, "BOTS", "机器人"), true, &right, th, g);
    let mut y = inner.y;
    for (i, b) in view.bots.iter().enumerate().take(n) {
        if y + 1 >= inner.bottom() {
            break;
        }
        let rows = Rect { x: inner.x, y, width: inner.width, height: 2 };
        app.hit(rows, Hit::Bot(i));
        if i == sel {
            fill(buf, rows, Style::new().bg(th.select_bg));
        }
        let bg = |st: Style| if i == sel { st.bg(th.select_bg) } else { st };
        let (dot, dot_st, word) = state_mark(b, app);
        let mut x = inner.x + 1;
        x += text(buf, x, y, dot, 1, bg(dot_st)) + 1;
        let name_w = if inner.width >= 40 { 18 } else { 12 };
        x += text_fit(buf, x, y, &b.name, name_w, bg(th.text().add_modifier(Modifier::BOLD))) + 2;
        // what it is worth against what it began with, at the right; the state keeps clear of it
        let pct = b
            .worth
            .filter(|_| b.budget > 0.0 && b.funded)
            .map(|w| (format!("{:+.2} %", (w / b.budget - 1.0) * 100.0), w));
        let pw = pct.as_ref().map_or(0, |(p, _)| width(p) + 2);
        let kx = x.max(inner.x + name_w + 4);
        text_fit(buf, kx, y, word, inner.right().saturating_sub(kx + pw + 1), bg(th.muted()));
        if let Some((pct, w)) = &pct {
            text(buf, inner.right().saturating_sub(width(pct) + 1), y, pct, width(pct), bg(th.pnl(w - b.budget)));
        }
        let holds = if matches!(b.state, BotState::Ended(_)) {
            // over: what it ended with went back to the wallet, it holds nothing
            format!("{} {:.2} USD", t(zh, "ended with", "结束时"), b.cash)
        } else if !b.funded {
            b.brief(zh)
        } else if b.sol > 0.0 {
            format!("{:.4} SOL · {}", b.sol, b.brief(zh))
        } else {
            format!("{:.4} USDC · {}", b.cash, b.brief(zh))
        };
        text_fit(buf, inner.x + 3, y + 1, &holds, inner.width.saturating_sub(4), bg(th.faint()));
        y += 3;
    }
}

fn bot_detail(buf: &mut Buffer, area: Rect, app: &App, b: &BotView) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let kind = match (b.real, zh) {
        (true, false) => "REAL MONEY",
        (false, false) => "PAPER",
        (true, true) => "真钱",
        (false, true) => "纸面",
    };
    let title = format!("{} · {kind} · {}", b.name.to_uppercase(), b.market());
    let keys = match (&b.state, zh) {
        (BotState::Running, false) => "x stop · b budget",
        (BotState::Stopped, false) => "s start · b budget · c close & sell",
        (BotState::Running, true) => "x 停止 · b 调预算",
        (BotState::Stopped, true) => "s 启动 · b 调预算 · c 卖出并结束",
        _ => "",
    };
    let inner = section(buf, area, &title, false, keys, th, g);
    if inner.height == 0 {
        return;
    }
    let (candles, live, from_exchange) = candles_of(app, b);
    let now = live.or(b.last_close());
    let mut y = inner.y;

    // 1. what it is doing, in words
    let doing_st = match &b.state {
        BotState::Running | BotState::Paper => th.text().add_modifier(Modifier::BOLD),
        BotState::Stopped => th.warn().add_modifier(Modifier::BOLD),
        BotState::Ended(_) => th.muted(),
    };
    for line in wrap_words(&b.doing(zh, live), inner.width).iter().take(3) {
        if y >= inner.bottom() {
            return;
        }
        text(buf, inner.x, y, line, inner.width, doing_st);
        y += 1;
    }
    // a budget it was asked to change to and has not yet: said until it has
    if let Some(to) = b.wish.filter(|_| y < inner.bottom()) {
        let when = match (&b.state, to < b.budget, zh) {
            (BotState::Stopped, _, true) => "它下次启动时生效",
            (_, true, true) => "它手里的 USDC 够退时生效（持有 SOL 的话要等卖出）",
            (_, false, true) => "几秒内生效；钱包里的 SOL 不够换时不会改，原因会写在记录里",
            (BotState::Stopped, _, false) => "when it is started",
            (_, true, false) => "once it holds that much in USDC (after it has sold, if it is in SOL)",
            (_, false, false) => {
                "within seconds; if the wallet's SOL is not enough it is not changed, and its record says why"
            }
        };
        let line = if zh {
            format!("已登记：预算从 {:.2} 调到 {to:.2} 美元，{when}。", b.budget)
        } else {
            format!("Noted: its budget goes from {:.2} to {to:.2} USD {when}.", b.budget)
        };
        text_fit(buf, inner.x, y, &line, inner.width, th.warn());
        y += 1;
    }
    y += 1;

    // 2. the ruler: where the price is between the prices it acts at
    if inner.bottom().saturating_sub(y) >= 16
        && let Some(p) = now
        && ruler(buf, Rect { y, height: 3, ..inner }, app, b, p)
    {
        y += 4;
    }

    // 3. the account: over the chart, or on a wide screen beside it, with more of it said
    let railed = inner.width >= 150 && inner.bottom().saturating_sub(y) >= 34;
    if !railed {
        y = account(buf, Rect { y, height: inner.bottom().saturating_sub(y), ..inner }, app, b) + 1;
    }

    // 4. the candles with its prices across them, 5. how they are worked out, 6. what it did
    let left = inner.bottom().saturating_sub(y);
    if left < 4 {
        return;
    }
    let calc_lines = worked_out(b, zh, now, inner.width.saturating_sub(2));
    let calc_h = if calc_lines.is_empty() || left < 18 { 0 } else { calc_lines.len() as u16 + 2 };
    let fills = fills_of(b);
    let records = if railed { b.journal.len().max(fills.len() + 1) } else { b.journal.len() };
    let journal_h = (records as u16 + 1).clamp(2, ((left - calc_h) / 4).max(2));
    let chart_h = left - calc_h - journal_h;
    let rail_w = 50;
    if chart_h >= 7 {
        let source = match (from_exchange, zh) {
            (true, false) => format!("live · OKX {} · v line/candles", b.inst),
            (false, false) => "its own closed bars (no live stream here)".to_string(),
            (true, true) => format!("实时 · OKX {} · v 切换折线", b.inst),
            (false, true) => "它自己记录的收盘价（这里没有实时行情）".to_string(),
        };
        let line = !from_exchange || app.bot_line;
        let name = t(zh, "PRICE", if line { "价格" } else { "K 线" });
        let chart_w = if railed { inner.width - rail_w - 3 } else { inner.width };
        if railed {
            let beside = Rect { x: inner.right() - rail_w, y, width: rail_w, height: chart_h.saturating_sub(1) };
            rail(buf, beside, app, b, now);
        }
        let c = section(buf, Rect { y, width: chart_w, height: chart_h, ..inner }, name, false, &source, th, g);
        // the bars to look at it in, as on any chart: the rule's own is marked, and goes on deciding whatever is shown
        let shown = shown_stream(app, b).filter(|_| from_exchange);
        let own = stream_of(app, b).map(|s| s.1);
        let c = match shown {
            Some((_, bar)) if c.height >= 9 => {
                let items: Vec<(String, usize, bool)> = BARS
                    .iter()
                    .enumerate()
                    .map(|(i, (l, _, _))| {
                        (if Some(i) == own { format!("{l}{}", g.bullet) } else { l.to_string() }, i, i == bar)
                    })
                    .collect();
                let end = crate::markets::tabs(buf, c.x, c.y, c.right(), &items, app, Hit::BotBar);
                let note = match (Some(bar) == own, zh) {
                    (true, true) => format!("{} 是规则自己用的周期 · [ ] 换周期", g.bullet),
                    (true, false) => format!("{} the rule's own bar · [ ] other bars", g.bullet),
                    (false, true) => format!("只是换个周期看价格：规则仍按{}判断", b.bar_name(true)),
                    (false, false) => {
                        format!(
                            "another look at the price only: the rule goes on deciding by the {}",
                            b.bar_name(false)
                        )
                    }
                };
                text_fit(buf, end + 1, c.y, &note, c.right().saturating_sub(end + 1), th.faint());
                Rect { y: c.y + 1, height: c.height - 1, ..c }
            }
            _ => c,
        };
        let bar_us = shown.map_or(b.bar_ms * 1000, |(_, bar)| BARS[bar].2);
        // while it holds SOL, what that cost stands where its buy price stood: it buys nothing more
        let cost = b.cost();
        let mut levels = Vec::new();
        for (v, en, cn, st) in [
            (b.levels.sell, "sells above", "卖出线", th.warn()),
            (cost, "what it paid", "买入成本", th.text()),
            (b.levels.buy.filter(|_| cost.is_none()), "buys under", "买入线", Style::new().fg(th.profit)),
            (b.levels.stop, "sells at a loss under", "止损线", Style::new().fg(th.loss)),
        ] {
            if let Some(value) = v {
                levels.push(Level { value, label: t(zh, en, cn).to_string(), style: st });
            }
        }
        let marks: Vec<(Ts, bool)> = b.fills.iter().map(|(ms, buy)| (Ts(ms * 1000), *buy)).collect();
        let k = Kline {
            candles: &candles,
            bar_us,
            cursor: None,
            live,
            line,
            decimals: 2,
            levels: &levels,
            marks: &marks,
            average: false,
            volume: true,
            zh,
        };
        render_kline(c, buf, &k, &|_, _| {}, th, g);
        y += chart_h;
    }
    if calc_h > 0 {
        let c = section(
            buf,
            Rect { y, height: calc_h, ..inner },
            t(zh, "HOW ITS PRICES ARE WORKED OUT", "这些线是怎么算出来的"),
            false,
            "",
            th,
            g,
        );
        for (i, (line, now)) in calc_lines.iter().enumerate().take(c.height as usize) {
            text_fit(buf, c.x, c.y + i as u16, line, c.width, if *now { th.text() } else { th.muted() });
        }
        y += calc_h;
    }
    // 6. what it did: on a wide screen its trades as a table, and its record beside them
    let rest = Rect { y, height: inner.bottom() - y, ..inner };
    if railed && !fills.is_empty() {
        let tw = (inner.width * 2 / 5).clamp(70, 96);
        trades_table(buf, Rect { width: tw, ..rest }, app, &fills);
        record(buf, Rect { x: rest.x + tw + 3, width: rest.width - tw - 3, ..rest }, app, b);
    } else {
        record(buf, rest, app, b);
    }
}

/// What it said, the newest lines.
fn record(buf: &mut Buffer, area: Rect, app: &App, b: &BotView) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let hint = if b.journal.is_empty() { "" } else { t(zh, "⏎ all of it", "⏎ 全部") };
    let j = section(buf, area, t(zh, "WHAT IT DID", "它做过什么"), false, hint, th, g);
    if b.journal.is_empty() {
        text(buf, j.x, j.y, t(zh, "nothing yet", "还没有"), j.width, th.faint());
    }
    let shown = b.journal.len().min(j.height as usize);
    let today = b.journal.last().map(|l| Ts(l.0 * 1000).format("%m-%d")).unwrap_or_default();
    for (i, (ts, line)) in b.journal[b.journal.len() - shown..].iter().enumerate() {
        let ly = j.y + i as u16;
        text(buf, j.x, ly, &clock(*ts, &today), 11, th.faint());
        let st = if line.starts_with("bought") || line.starts_with("sold") || line.starts_with("budget set") {
            th.text()
        } else if line.starts_with("STOP") || line.starts_with("not sent") {
            th.warn()
        } else {
            th.muted()
        };
        text_fit(buf, j.x + 13, ly, &said(zh, line), j.width.saturating_sub(13), st);
    }
}

/// Its buys and sales, newest first: when, which, how much for how much, at what price, and what a sale made.
fn trades_table(buf: &mut Buffer, area: Rect, app: &App, fills: &[Fill]) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let count = if zh { format!("{} 笔", fills.len()) } else { format!("{}", fills.len()) };
    let inner = section(buf, area, t(zh, "ITS TRADES", "成交记录"), false, &count, th, g);
    if inner.height == 0 {
        return;
    }
    let cols: [(u16, &str); 6] = [
        (0, t(zh, "WHEN", "时间")),
        (13, t(zh, "SIDE", "买/卖")),
        (20, "SOL"),
        (32, "USDC"),
        (43, t(zh, "PRICE", "成交价")),
        (54, t(zh, "MADE", "这笔盈亏")),
    ];
    for (x, name) in cols {
        text(buf, inner.x + x, inner.y, name, inner.width.saturating_sub(x), th.faint());
    }
    for (f, y) in fills.iter().rev().zip(inner.y + 1..inner.bottom()) {
        let side = match (f.buy, zh) {
            (true, true) => "买入",
            (false, true) => "卖出",
            (true, false) => "buy",
            (false, false) => "sell",
        };
        let side_st = Style::new().fg(if f.buy { th.profit } else { th.loss });
        let made = f.net.map_or(String::new(), |n| format!("{n:+.4} USD"));
        let cells = [
            (Ts(f.at * 1000).format("%m-%d %H:%M"), th.faint()),
            (side.to_string(), side_st),
            (format!("{:.6}", f.sol), th.text()),
            (format!("{:.4}", f.usdc), th.text()),
            (price(f.price, 2), th.text()),
            (made, th.pnl(f.net.unwrap_or(0.0))),
        ];
        for ((x, _), (v, st)) in cols.iter().zip(cells) {
            text(buf, inner.x + x, y, &v, inner.width.saturating_sub(*x), st);
        }
    }
}

/// The real bots' buys and sales on the Trades page, newest first, each with its bot's name.
pub fn fills_section(buf: &mut Buffer, area: Rect, app: &App, fills: &[(String, Fill)]) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let right = if zh {
        format!("{} 笔 · 9 机器人页看详情", fills.len())
    } else {
        format!("{} · page 9 has the bots", fills.len())
    };
    let title = t(zh, "BOT TRADES (buy low, sell high)", "机器人成交（低买高卖）");
    let inner = section(buf, area, title, false, &right, th, g);
    if inner.height == 0 {
        return;
    }
    if fills.is_empty() {
        let none = t(
            zh,
            "none yet: a bot's buys and sales are listed here as they happen",
            "还没有：机器人每买一次、卖一次，都会列在这里",
        );
        text_fit(buf, inner.x, inner.y, none, inner.width, th.faint());
        return;
    }
    let cols: [(u16, &str); 7] = [
        (0, t(zh, "WHEN", "时间")),
        (16, t(zh, "BOT", "机器人")),
        (34, t(zh, "SIDE", "买/卖")),
        (42, "SOL"),
        (55, "USDC"),
        (67, t(zh, "PRICE", "成交价")),
        (79, t(zh, "MADE", "这笔盈亏")),
    ];
    for (x, name) in cols {
        text(buf, inner.x + x, inner.y, name, inner.width.saturating_sub(x), th.faint());
    }
    for ((name, f), y) in fills.iter().zip(inner.y + 1..inner.bottom()) {
        let side = match (f.buy, zh) {
            (true, true) => "买入",
            (false, true) => "卖出",
            (true, false) => "buy",
            (false, false) => "sell",
        };
        let cells = [
            (Ts(f.at * 1000).format("%m-%d %H:%M:%S"), th.faint()),
            (name.clone(), th.muted()),
            (side.to_string(), Style::new().fg(if f.buy { th.profit } else { th.loss })),
            (format!("{:.6}", f.sol), th.text()),
            (format!("{:.4}", f.usdc), th.text()),
            (price(f.price, 2), th.text()),
            (f.net.map_or(String::new(), |n| format!("{n:+.4} USD")), th.pnl(f.net.unwrap_or(0.0))),
        ];
        for ((x, _), (v, st)) in cols.iter().zip(cells) {
            text_fit(buf, inner.x + x, y, &v, inner.width.saturating_sub(*x), st);
        }
    }
}

/// Beside the chart on a wide screen: the account, the position (or what it
/// waits for), what it has done so far, the market and what comes next, each
/// under a word of its own with room around it.
fn rail(buf: &mut Buffer, area: Rect, app: &App, b: &BotView, now: Option<f64>) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let key_w = 13;
    let mut y = area.y;
    // a group: its word, then its rows; left out whole when it does not fit
    let mut group = |buf: &mut Buffer, name: &str, rows: Vec<(&str, String, Style)>| {
        if rows.is_empty() || y + rows.len() as u16 + 1 > area.bottom() {
            return;
        }
        let inner = section(buf, Rect { y, height: rows.len() as u16 + 1, ..area }, name, false, "", th, g);
        for (i, (k, v, st)) in rows.into_iter().enumerate() {
            text(buf, inner.x, inner.y + i as u16, k, key_w, th.muted());
            text_fit(buf, inner.x + key_w, inner.y + i as u16, &v, inner.width.saturating_sub(key_w), st);
        }
        y += inner.height + 2;
    };
    let pct = |a: f64, of: f64| (a / of - 1.0) * 100.0;
    let bold = th.text().add_modifier(Modifier::BOLD);

    // the account
    let worth = b.worth.filter(|_| b.funded && b.budget > 0.0);
    let holds = match (b.funded, b.sol > 0.0, b.cash > 0.00005) {
        (false, ..) => t(zh, "nothing yet", "还没划拨").to_string(),
        (true, true, true) => format!("{:.6} SOL + {:.4} USDC", b.sol, b.cash),
        (true, true, false) => format!("{:.6} SOL", b.sol),
        (true, false, _) => format!("{:.4} USDC", b.cash),
    };
    let mut rows = vec![
        (
            t(zh, "Worth", "市值"),
            worth.map_or("—".to_string(), |w| format!("{w:.4} USD   {:+.2} %", pct(w, b.budget))),
            worth.map_or(th.muted(), |w| th.pnl(w - b.budget).add_modifier(Modifier::BOLD)),
        ),
        (t(zh, "Budget", "预算"), format!("{:.2} USD", b.budget), th.text()),
        (t(zh, "Holds", "持有"), holds, th.text()),
    ];
    if let Some(s) = b.stop_at {
        let v = if zh {
            format!("市值跌到 {s:.2} 美元就全部卖出")
        } else {
            format!("all sold at {s:.2} USD or less")
        };
        rows.push((t(zh, "Sold out at", "清仓线"), v, th.text()));
    }
    group(buf, t(zh, "ACCOUNT", "账户"), rows);

    // the position it holds, or what it waits for
    let mut rows = Vec::new();
    if let (Some(cost), Some(p)) = (b.cost(), now) {
        if let Some(at) = b.opened {
            rows.push((
                t(zh, "Bought", "买入"),
                format!("{}  ·  {}", Ts(at * 1000).format("%m-%d %H:%M"), price(cost, 2)),
                th.text(),
            ));
            rows.push((t(zh, "Held for", "已持有"), span(Ts::now().0 / 1000 - at, zh), th.text()));
        }
        let float = b.sol * p - b.paid;
        rows.push((t(zh, "Now", "浮动盈亏"), format!("{float:+.4} USD   {:+.2} %", pct(p, cost)), th.pnl(float)));
        if let Some(v) = b.levels.sell {
            rows.push((
                t(zh, "To its sale", "离卖出线"),
                format!("{:+.2} %   ({})", pct(v, p), price(v, 2)),
                th.text(),
            ));
        }
        if let Some(v) = b.levels.stop {
            rows.push((
                t(zh, "To its stop", "离止损线"),
                format!("{:+.2} %   ({})", pct(v, p), price(v, 2)),
                th.text(),
            ));
        }
        group(buf, t(zh, "THIS POSITION", "这笔持仓"), rows);
    } else if let (Some(v), Some(p), true) = (b.levels.buy, now, b.funded) {
        rows.push((t(zh, "Buys under", "买入线"), price(v, 2), th.text()));
        let far = pct(v, p);
        let gap = match (far < 0.0, zh) {
            (true, true) => format!("{}   还要再跌 {:.2} %", price(p, 2), -far),
            (true, false) => format!("{}   {:.2} % to fall", price(p, 2), -far),
            (false, true) => format!("{}   已在买入线下方", price(p, 2)),
            (false, false) => format!("{}   under it already", price(p, 2)),
        };
        rows.push((t(zh, "Price now", "现价"), gap, bold));
        if let Some(v) = b.levels.sell {
            rows.push((t(zh, "Then sells", "之后卖出线"), price(v, 2), th.text()));
        }
        group(buf, t(zh, "WAITING TO BUY", "等待买入"), rows);
    }

    // what it has done so far
    let (n, won, made) = (b.trades.len(), b.trades.iter().filter(|x| x.2 > 0.0).count(), b.result());
    let mut rows = vec![(
        t(zh, "Closed", "已平仓"),
        match (n, zh) {
            (0, true) => "还没有".to_string(),
            (0, false) => "none yet".to_string(),
            (_, true) => format!("{n} 笔，其中赚 {won} 笔"),
            (_, false) => format!("{n}, {won} of them won"),
        },
        th.text(),
    )];
    if let Some(last) = b.trades.last() {
        rows.push((t(zh, "Made", "已实现盈亏"), format!("{made:+.4} USD"), th.pnl(made)));
        rows.push((
            t(zh, "Last one", "最近一笔"),
            format!("{:+.4} USD  ·  {}", last.2, Ts(last.0 * 1000).format("%m-%d %H:%M")),
            th.pnl(last.2),
        ));
    }
    group(buf, t(zh, "SO FAR", "战绩"), rows);

    // the market of its instrument, when the exchange's ticker is this one
    let ticker = app.cex.as_ref().and_then(|c| c.state.read().ticker.clone()).filter(|x| x.inst == b.inst);
    if let Some(x) = ticker {
        let (_, day) = x.change();
        let rows = vec![
            (t(zh, "Price now", "现价"), price(x.last, 2), bold),
            (t(zh, "Today (UTC)", "今日涨跌"), format!("{day:+.2} %"), th.pnl(day)),
            (t(zh, "24 h high", "24 小时最高"), price(x.high24h, 2), th.text()),
            (t(zh, "24 h low", "24 小时最低"), price(x.low24h, 2), th.text()),
        ];
        group(buf, t(zh, "MARKET", "行情"), rows);
    }

    // what comes next: it decides once a bar
    let next = b.closes.last().filter(|_| b.bar_ms > 0).map(|l| {
        let now_ms = Ts::now().0 / 1000;
        let mut at = l.0 + b.bar_ms;
        while at <= now_ms {
            at += b.bar_ms;
        }
        (at, at - now_ms)
    });
    let rows = match (&b.state, next, zh) {
        (BotState::Running, Some((at, left)), true) => vec![(
            "下一次判断",
            format!("{}   {} 分 {:02} 秒后", Ts(at * 1000).format("%H:%M"), left / 60_000, left / 1000 % 60),
            th.text(),
        )],
        (BotState::Running, Some((at, left)), false) => vec![(
            "Decides at",
            format!("{}   in {} min {:02} s", Ts(at * 1000).format("%H:%M"), left / 60_000, left / 1000 % 60),
            th.text(),
        )],
        (BotState::Stopped, _, true) => vec![("下一次判断", "没在运行：按 s 启动".to_string(), th.warn())],
        (BotState::Stopped, _, false) => vec![("Decides at", "not running: s starts it".to_string(), th.warn())],
        _ => Vec::new(),
    };
    group(buf, t(zh, "NEXT", "接下来"), rows);

    // its worth bar by bar, as a line of blocks: over its budget in the colour of a gain, under it of a loss
    let w = area.width.saturating_sub(1) as usize;
    if b.equity.len() >= 2 && y + 3 <= area.bottom() {
        let shown = &b.equity[b.equity.len().saturating_sub(w)..];
        let (lo, hi) = shown.iter().fold((f64::MAX, f64::MIN), |(a, z), e| (a.min(e.1), z.max(e.1)));
        let range =
            if zh { format!("最低 {lo:.4} · 最高 {hi:.4}") } else { format!("low {lo:.4} · high {hi:.4}") };
        let inner =
            section(buf, Rect { y, height: 3, ..area }, t(zh, "WORTH, BAR BY BAR", "市值走势"), false, &range, th, g);
        for (i, (_, v)) in shown.iter().enumerate() {
            let level = if hi > lo { ((v - lo) / (hi - lo) * 7.0).round() as usize } else { 3 };
            let st = Style::new().fg(if *v >= b.budget { th.profit } else { th.loss });
            put(buf, inner.x + i as u16, inner.y, g.spark[level.min(7)], st);
        }
    }
}

/// The ruler: two fixed ticks and the price now as the mark that moves between
/// them. Waiting, the ticks are its buy and its sell price; holding, what its
/// SOL cost and its sell price. `false`: nothing to rule (the rule names no prices).
fn ruler(buf: &mut Buffer, area: Rect, app: &App, b: &BotView, now: f64) -> bool {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let (cost, Some(sell)) = (b.cost(), b.levels.sell) else { return false };
    let Some(from) = cost.or(b.levels.buy) else { return false };
    if area.width < 50 || area.height < 3 {
        return false;
    }
    // the two prices a third in from each end; the price now wherever it falls, held at the ends
    let (low, high) = (from.min(sell), from.max(sell));
    let span = (high - low).max(now.abs() * 0.001);
    let (lo, hi) = (low - span, high + span);
    let w = area.width - 2;
    let col = |v: f64| area.x + 1 + (((v - lo) / (hi - lo)).clamp(0.0, 1.0) * (w - 1) as f64).round() as u16;
    let (fx, sx, px) = (col(from), col(sell), col(now));
    let y = area.y + 1;
    // under the first tick: where it buys (waiting) or is at a loss (holding); past the second: where it sells
    let (from_st, sell_st) =
        (if cost.is_some() { Style::new().fg(th.loss) } else { Style::new().fg(th.profit) }, th.warn());
    for x in area.x + 1..area.x + 1 + w {
        let st = if x > sx {
            sell_st
        } else if x < fx {
            from_st
        } else {
            th.rule()
        };
        put(buf, x, y, if g.unicode { "━" } else { "=" }, st);
    }
    let tick = if g.unicode { "┃" } else { "|" };
    let from_tick = if cost.is_some() { th.text() } else { from_st };
    put(buf, fx, y, tick, from_tick.add_modifier(Modifier::BOLD));
    put(buf, sx, y, tick, sell_st.add_modifier(Modifier::BOLD));
    let mark = if now < lo {
        "<"
    } else if now > hi {
        ">"
    } else if g.unicode {
        "●"
    } else {
        "O"
    };
    put(buf, px, y, mark, th.text().add_modifier(Modifier::BOLD));
    // over the ruler: the two prices, each at its tick, kept apart when the ticks are close
    let from_label = match (cost.is_some(), zh) {
        (false, false) => format!("buys under {}", price(from, 2)),
        (false, true) => format!("买入线 {}", price(from, 2)),
        (true, false) => format!("paid {}", price(from, 2)),
        (true, true) => format!("买入成本 {}", price(from, 2)),
    };
    let sell_label = format!("{} {}", t(zh, "sells above", "卖出线"), price(sell, 2));
    let mid = (fx + sx) / 2;
    let place = |label: &str, x: u16, left: bool| {
        let lw = width(label);
        let at = x.saturating_sub(lw / 2);
        let at = if left { at.min(mid.saturating_sub(lw + 1)) } else { at.max(mid + 1) };
        at.clamp(area.x, area.right().saturating_sub(lw))
    };
    text(buf, place(&from_label, fx, fx <= sx), area.y, &from_label, width(&from_label), from_tick);
    text(buf, place(&sell_label, sx, sx < fx), area.y, &sell_label, width(&sell_label), sell_st);
    // under it: the price now and how far it has to go, at the mark
    let far = if cost.is_some() { (sell / now - 1.0) * 100.0 } else { (1.0 - from / now) * 100.0 };
    let gap = match (cost.is_some(), far > 0.0, zh) {
        (false, true, true) => format!("再跌 {far:.2}% 到买入线"),
        (false, false, true) => "已在买入线下方".to_string(),
        (true, true, true) => format!("再涨 {far:.2}% 到卖出线"),
        (true, false, true) => "已在卖出线上方".to_string(),
        (false, true, false) => format!("{far:.2} % down to its buy price"),
        (false, false, false) => "under its buy price".to_string(),
        (true, true, false) => format!("{far:.2} % up to its sell price"),
        (true, false, false) => "above its sell price".to_string(),
    };
    // holding: what selling now would make of what it paid
    let against = cost.map_or(String::new(), |c| {
        let d = (now / c - 1.0) * 100.0;
        match (d >= 0.0, zh) {
            (true, true) => format!(" · 比成本高 {d:.2}%"),
            (false, true) => format!(" · 比成本低 {:.2}%", -d),
            (true, false) => format!(" · {d:.2} % over what it paid"),
            (false, false) => format!(" · {:.2} % under what it paid", -d),
        }
    });
    let now_label = format!("{} {}  {gap}{against}", t(zh, "now", "现价"), price(now, 2));
    let lw = width(&now_label);
    let at = px.saturating_sub(lw / 2).clamp(area.x, area.right().saturating_sub(lw));
    text(buf, at, area.y + 2, &now_label, lw, th.text().add_modifier(Modifier::BOLD));
    true
}

/// What it is worth, holds, has closed and ends at: two rows of two when there is room.
/// Returns the row after it.
fn account(buf: &mut Buffer, area: Rect, app: &App, b: &BotView) -> u16 {
    let (th, zh) = (&app.theme, app.zh);
    let worth = match b.worth.filter(|_| b.funded) {
        Some(w) if b.budget > 0.0 => (
            format!("{w:.4} USD  {:+.2} %", (w / b.budget - 1.0) * 100.0),
            th.pnl(w - b.budget).add_modifier(Modifier::BOLD),
        ),
        _ => ("—".to_string(), th.muted()),
    };
    // what it began with, beside what it is worth now, not in its colour
    let budget = format!("  {} {:.2}", t(zh, "· budget", "· 预算"), b.budget);
    let holds = match (b.funded, b.sol > 0.0) {
        (false, _) => t(zh, "nothing yet", "还没有").to_string(),
        (true, true) if b.cash > 0.00005 => format!("{:.6} SOL · {:.4} USDC", b.sol, b.cash),
        (true, true) if zh => format!("{:.6} SOL（成本 {:.4} 美元）", b.sol, b.paid),
        (true, true) => format!("{:.6} SOL (paid {:.4} USD)", b.sol, b.paid),
        (true, false) if zh => format!("{:.4} USDC · 没有 SOL", b.cash),
        (true, false) => format!("{:.4} USDC · no SOL", b.cash),
    };
    let (won, result) = (b.trades.iter().filter(|t| t.2 > 0.0).count(), b.result());
    let trades = match (b.trades.len(), zh) {
        (0, false) => "none closed yet".to_string(),
        (0, true) => "还没有平仓的".to_string(),
        (n, false) => format!("{n} closed · {won} won · {result:+.4} USD"),
        (n, true) => format!("已平仓 {n} 笔 · 赚 {won} 笔 · {result:+.4} USD"),
    };
    let sold_out = match (b.stop_at, zh) {
        (Some(s), false) => format!("{s:.2} USD or less, then it ends"),
        (Some(s), true) => format!("市值跌到 {s:.2} 美元就全部卖出并结束"),
        (None, _) => "—".into(),
    };
    let rows: [(&str, String, Style, &str); 4] = [
        (t(zh, "Worth", "市值"), worth.0, worth.1, &budget),
        (t(zh, "Holds", "持有"), holds, th.text(), ""),
        (t(zh, "Trades", "成交"), trades, if b.trades.is_empty() { th.muted() } else { th.pnl(result) }, ""),
        (t(zh, "Sold out at", "清仓线"), sold_out, if b.stop_at.is_some() { th.text() } else { th.muted() }, ""),
    ];
    let cols: u16 = if area.width >= 104 { 2 } else { 1 };
    let (col_w, key_w) = (area.width / cols, 13);
    let mut last = area.y;
    for (i, (k, v, st, after)) in rows.iter().enumerate() {
        let (cx, cy) = (area.x + (i as u16 % cols) * col_w, area.y + i as u16 / cols);
        if cy >= area.bottom() {
            break;
        }
        text(buf, cx, cy, k, key_w, th.muted());
        let room = col_w.saturating_sub(key_w + 2);
        let used = text_fit(buf, cx + key_w, cy, v, room, *st);
        text_fit(buf, cx + key_w + used, cy, after, room.saturating_sub(used), th.muted());
        last = cy;
    }
    last + 1
}

/// The numbers behind the prices: what they are taken from, each price as a
/// sum, and (marked `true`) where the price stands now. Wrapped to `w` columns.
fn worked_out(b: &BotView, zh: bool, now: Option<f64>, w: u16) -> Vec<(String, bool)> {
    let plain = |lines: Vec<String>| lines.into_iter().map(|l| (l, false)).collect::<Vec<_>>();
    let Some(c) = &b.calc else {
        return plain(wrap_words(&b.rule, w));
    };
    let hours = c.window as f64 * b.bar_ms as f64 / 3_600_000.0;
    let span = match (hours >= 24.0, zh) {
        (true, false) => format!("{:.0} d", hours / 24.0),
        (false, false) => format!("{hours:.0} h"),
        (true, true) => format!("约 {:.0} 天", hours / 24.0),
        (false, true) => format!("约 {hours:.0} 小时"),
    };
    let (buy, sell) = (c.mean - c.k * c.sd, c.mean + c.exit_z * c.sd);
    let (from, sums) = if zh {
        let from = format!(
            "取最近 {} 根{}（{span}）的收盘价：平均价 {}，波动幅度（标准差）{}",
            c.window,
            b.bar_name(true),
            price(c.mean, 2),
            price(c.sd, 2)
        );
        let mut sums = vec![format!("买入线 = 平均价 − {} × 波动 = {}", c.k, price(buy, 2))];
        sums.push(if c.exit_z == 0.0 {
            format!("卖出线 = 平均价 = {}", price(sell, 2))
        } else {
            format!("卖出线 = 平均价 {:+} × 波动 = {}", c.exit_z, price(sell, 2))
        });
        sums.extend(c.stop.map(|s| format!("止损线 = 买入成本 × {:.2}（亏 {:.0}% 就卖）", 1.0 - s, s * 100.0)));
        (from, sums)
    } else {
        let from = format!(
            "From the closes of the last {} {}s ({span}): their average {}, their deviation {}",
            c.window,
            b.bar_name(false),
            price(c.mean, 2),
            price(c.sd, 2)
        );
        let mut sums = vec![format!("buy price = average − {} × deviation = {}", c.k, price(buy, 2))];
        sums.push(if c.exit_z == 0.0 {
            format!("sell price = the average = {}", price(sell, 2))
        } else {
            format!("sell price = average {:+} × deviation = {}", c.exit_z, price(sell, 2))
        });
        sums.extend(c.stop.map(|s| format!("loss price = what it paid × {:.2} ({:.0} % down)", 1.0 - s, s * 100.0)));
        (from, sums)
    };
    let mut out = plain(wrap_words(&from, w));
    // the sums side by side, as many to a line as fit
    let mut packed: Vec<String> = Vec::new();
    for sum in sums {
        match packed.last_mut() {
            Some(row) if width(row) + 4 + width(&sum) <= w => *row = format!("{row}    {sum}"),
            _ => packed.push(sum),
        }
    }
    out.extend(plain(packed));
    if let Some(p) = now.filter(|_| c.sd > 0.0) {
        let z = (p - c.mean) / c.sd;
        let next = b.closes.last().map(|l| Ts((l.0 + b.bar_ms) * 1000).format("%H:%M")).unwrap_or_default();
        let then = match (b.sol > 0.0, zh) {
            (false, true) => format!("低 {} 个就买", c.k),
            (true, true) => format!("收盘高于卖出线 {} 就卖", price(sell, 2)),
            (false, false) => format!("it buys {} under", c.k),
            (true, false) => format!("it sells above {}", price(sell, 2)),
        };
        let stands = if zh {
            format!(
                "现价 {} 比平均价{} {:.2} 个波动；{then}。只在每根线收盘时判断一次，下一次 {next}。",
                price(p, 2),
                if z < 0.0 { "低" } else { "高" },
                z.abs()
            )
        } else {
            format!(
                "The price now, {}, is {:.2} deviations {} the average; {then}. It decides only when a bar closes: next at {next}.",
                price(p, 2),
                z.abs(),
                if z < 0.0 { "under" } else { "over" }
            )
        };
        out.extend(wrap_words(&stands, w).into_iter().map(|l| (l, true)));
    }
    out
}

/// The two rules a new bot is made from: its name, the bars its average is taken over, its `k`.
pub const KINDS: [(&str, usize, &str); 2] = [("dip-1d", 96, "1"), ("dip-3d", 288, "2")];

/// The words a rules file must carry before anything is sent.
pub const ACK: &str = "ALLOW LOSS";

/// A new bot being written on the page (`n`).
#[derive(Clone, Debug, PartialEq)]
pub struct NewBotForm {
    /// Which of [`KINDS`].
    pub kind: usize,
    pub name: String,
    pub k: String,
    /// Per cent under what a buy cost at which it is sold at a loss.
    pub stop: String,
    pub budget: String,
    /// Per cent of the budget lost at which everything is sold and the run ends.
    pub total: String,
    pub ack: String,
    /// 0: kind, 1: name, 2: k, 3: stop, 4: budget, 5: total stop, 6: the words.
    pub field: usize,
    /// Why it was not made, when it was not.
    pub error: Option<String>,
}

impl Default for NewBotForm {
    fn default() -> Self {
        NewBotForm {
            kind: 0,
            name: KINDS[0].0.into(),
            k: KINDS[0].2.into(),
            stop: "5".into(),
            budget: "2".into(),
            total: "50".into(),
            ack: String::new(),
            field: 0,
            error: None,
        }
    }
}

impl NewBotForm {
    /// One key into the form. `true`: ⏎ on its last field, it is to be made.
    pub fn on_key(&mut self, k: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyCode;
        self.error = None;
        let number = |s: &mut String, c: char| {
            if s.len() < 5 && (c.is_ascii_digit() || (c == '.' && !s.contains('.') && !s.is_empty())) {
                s.push(c);
            }
        };
        match (k.code, self.field) {
            (KeyCode::Tab | KeyCode::Down, _) => self.field = (self.field + 1) % 7,
            (KeyCode::BackTab | KeyCode::Up, _) => self.field = (self.field + 6) % 7,
            (KeyCode::Enter, 6) => return true,
            (KeyCode::Enter, _) => self.field += 1,
            (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), 0) => {
                self.kind = (self.kind + 1) % KINDS.len();
                (self.name, self.k) = (KINDS[self.kind].0.into(), KINDS[self.kind].2.into());
            }
            (KeyCode::Char(c), 1)
                if self.name.len() < 16 && (c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') =>
            {
                self.name.push(c)
            }
            (KeyCode::Char(c), 2) => number(&mut self.k, c),
            (KeyCode::Char(c), 3) => number(&mut self.stop, c),
            (KeyCode::Char(c), 4) => number(&mut self.budget, c),
            (KeyCode::Char(c), 5) => number(&mut self.total, c),
            (KeyCode::Char(c), 6) if self.ack.len() < 12 && (c.is_ascii_alphabetic() || c == ' ') => self.ack.push(c),
            (KeyCode::Backspace, f) => {
                let text = match f {
                    1 => &mut self.name,
                    2 => &mut self.k,
                    3 => &mut self.stop,
                    4 => &mut self.budget,
                    5 => &mut self.total,
                    6 => &mut self.ack,
                    _ => return false,
                };
                text.pop();
            }
            _ => {}
        }
        false
    }

    /// What is written, as a bot to make; else what is wrong with it.
    pub fn spec(&self, zh: bool) -> Result<NewBot, String> {
        let within = |text: &str, lo: f64, hi: f64, en: &str, cn: &str| {
            text.parse::<f64>().ok().filter(|v| *v >= lo && *v <= hi).ok_or_else(|| {
                if zh { format!("{cn}要在 {lo} 到 {hi} 之间") } else { format!("{en} is between {lo} and {hi}") }
            })
        };
        if self.name.is_empty() {
            return Err(t(zh, "it needs a name", "要有一个名字").into());
        }
        let k = within(&self.k, 0.5, 5.0, "k", "买入倍数 k ")?;
        let stop = within(&self.stop, 1.0, 50.0, "the stop of a trade (%)", "每笔止损（%）")?;
        let budget = within(&self.budget, BUDGET.0, BUDGET.1, "the budget (USD)", "预算（美元）")?;
        let total = within(&self.total, 5.0, 90.0, "the total stop (%)", "总止损（%）")?;
        if self.ack != ACK {
            return Err(if zh {
                format!("最后一项要原样输入 {ACK}（大写），表示你知道它可能亏钱")
            } else {
                format!("the last field takes the words {ACK}, as they are: that it can lose money is yours to say")
            });
        }
        Ok(NewBot {
            name: self.name.clone(),
            window: KINDS[self.kind].1,
            k,
            stop: stop / 100.0,
            budget,
            total_stop: total / 100.0,
            acknowledge: self.ack.clone(),
        })
    }
}

/// The form a new bot is written in: what it will do, each number with what it means, and what it has cost before.
pub fn new_bot_overlay(buf: &mut Buffer, area: Rect, app: &App, form: &NewBotForm) {
    let (th, zh) = (&app.theme, app.zh);
    let on = |st: Style| st.bg(th.select_bg);
    let w = 104.min(area.width.saturating_sub(2));
    let inner_w = w.saturating_sub(4);
    let does = t(
        zh,
        "What it does: when a 15-minute bar of SOL closes under its average by k deviations, it buys with all of its budget; back over the average, it sells; a trade that is down by its stop is sold at the loss.",
        "它做什么：SOL 的 15 分钟线收盘价跌到“平均价 − k × 波动”以下，就用全部预算买入；涨回平均价以上卖出；买入后跌过“每笔止损”就认亏卖出。",
    );
    let past = t(
        zh,
        "What the past says, not what will be: tried in 2026-10 on a year of prices with 2 USD and the numbers as they first stand here, the 1-day rule lost about 72 % and the 3-day rule 37 to 45 % (SOL itself fell about 47 %). It is an experiment that can lose its money: use what you can lose.",
        "过去不代表将来：2026-10 用过去一年的行情、2 美元本金、这里的默认数字回测，“看 1 天”这组亏了约 72%，“看 3 天”这组亏了 37%–45%（同期 SOL 本身跌了约 47%）。这是可能把钱亏掉的实验，只用亏得起的小钱。",
    );
    let (does, past) = (wrap_words(does, inner_w), wrap_words(past, inner_w));
    let error = form.error.as_ref().map(|e| wrap_words(e, inner_w)).unwrap_or_default();
    let h = does.len() + past.len() + error.len() + 14;
    let inner = crate::panels::overlay(buf, area, w, h as u16, t(zh, "A new bot", "新建机器人"), th, &app.glyphs);
    app.hit(Rect { x: inner.x - 2, y: inner.y - 1, width: inner.width + 4, height: inner.height + 2 }, Hit::Overlay);
    let mut y = inner.y;
    for line in &does {
        text(buf, inner.x, y, line, inner.width, on(th.text()));
        y += 1;
    }
    y += 1;
    let (key_w, val_w) = (14, 12);
    // the kind: both written out, the chosen one marked
    let name_st = |i: usize| if form.field == i { on(th.accent_bold()) } else { on(th.muted()) };
    text(buf, inner.x, y, t(zh, "Kind", "类型"), key_w, name_st(0));
    let mut x = inner.x + key_w;
    let kinds = [
        t(zh, "busy: the average of 1 day", "勤快：看 1 天的平均价"),
        t(zh, "calmer: the average of 3 days", "稳一点：看 3 天的平均价"),
    ];
    for (i, label) in kinds.iter().enumerate() {
        let st =
            if i == form.kind { th.text().add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { on(th.faint()) };
        x += text(buf, x, y, &format!(" {label} "), 40, st) + 2;
    }
    if form.field == 0 {
        text(buf, x + 1, y, t(zh, "←→ the other", "←→ 换另一种"), 20, on(th.faint()));
    }
    y += 1;
    let rows: [(usize, &str, &str, &str, &str); 5] = [
        (1, t(zh, "Name", "名字"), &form.name, "", t(zh, "small letters, digits and -", "小写字母、数字和 -")),
        (
            2,
            t(zh, "Buys at k", "买入倍数 k"),
            &form.k,
            "",
            t(
                zh,
                "deviations under the average at which it buys: larger, it acts less often",
                "跌到比平均价低几个“波动”才买。越大，出手越少",
            ),
        ),
        (
            3,
            t(zh, "Stop, a trade", "每笔止损"),
            &form.stop,
            "%",
            t(zh, "down this much from what it paid, it sells at the loss", "买入后跌这么多就认亏卖出"),
        ),
        (
            4,
            t(zh, "Budget", "预算"),
            &form.budget,
            t(zh, "USD", "美元"),
            t(
                zh,
                "1 to 25, set aside from the wallet when it is started",
                "1 到 25。启动时从钱包划出（先用 USDC，不够卖 SOL 换）",
            ),
        ),
        (
            5,
            t(zh, "Stop, in all", "总止损"),
            &form.total,
            "%",
            t(
                zh,
                "the budget down this much: everything is sold and it ends for good",
                "预算亏到这个比例就全部卖出并永久结束",
            ),
        ),
    ];
    for (i, name, value, unit, what) in rows {
        text(buf, inner.x, y, name, key_w, name_st(i));
        let cursor = if form.field == i { "▏" } else { "" };
        text(
            buf,
            inner.x + key_w,
            y,
            &format!("{value}{cursor} {unit}"),
            val_w + 4,
            on(th.text().add_modifier(Modifier::BOLD)),
        );
        text_fit(
            buf,
            inner.x + key_w + val_w + 6,
            y,
            what,
            inner.width.saturating_sub(key_w + val_w + 6),
            on(th.faint()),
        );
        y += 1;
    }
    y += 1;
    for line in &past {
        text(buf, inner.x, y, line, inner.width, on(th.warn()));
        y += 1;
    }
    y += 1;
    text(buf, inner.x, y, t(zh, "Your word", "确认"), key_w, name_st(6));
    let ask = if zh {
        format!("输入 {ACK} 表示你知道它可能亏钱：")
    } else {
        format!("type {ACK} to say you know it can lose money: ")
    };
    let ax = inner.x + key_w + text(buf, inner.x + key_w, y, &ask, inner.width - key_w, on(th.text()));
    let st = if form.ack == ACK { Style::new().fg(th.profit) } else { th.accent() };
    let cursor = if form.field == 6 { "▏" } else { "" };
    text(buf, ax, y, &format!("{}{cursor}", form.ack), 14, on(st.add_modifier(Modifier::BOLD)));
    y += 1;
    for line in &error {
        text(buf, inner.x, y, line, inner.width, on(Style::new().fg(th.loss)));
        y += 1;
    }
    let hint = t(
        zh,
        "Tab next · ⏎ make it (it is not started) · Esc cancel",
        "Tab 下一项 · ⏎ 创建（不会自动启动） · Esc 取消",
    );
    crate::panels::overlay_hint(buf, inner, hint, th);
}

/// The budget being written for a bot (`b` on the page).
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetForm {
    pub id: String,
    pub text: String,
}

/// The least and the most a budget may be, USD (the application refuses anything else too).
pub const BUDGET: (f64, f64) = (1.0, 25.0);

impl BudgetForm {
    /// What is written, as cents, when it is a budget.
    pub fn cents(&self) -> Option<u32> {
        let v: f64 = self.text.parse().ok()?;
        (v >= BUDGET.0 && v <= BUDGET.1).then(|| (v * 100.0).round() as u32)
    }

    /// A key into the number: digits and one point, two decimals at most.
    pub fn type_in(&mut self, c: char) {
        let decimals = self.text.split_once('.').map(|(_, d)| d.len());
        let fits = match c {
            '0'..='9' => decimals.is_none_or(|d| d < 2) && self.text.len() < 5,
            '.' => decimals.is_none() && !self.text.is_empty(),
            _ => false,
        };
        if fits {
            self.text.push(c);
        }
    }
}

/// The form a budget is changed in: what it is, what there is to give it, and what the number written would do.
pub fn budget_overlay(buf: &mut Buffer, area: Rect, app: &App, vm: &crate::hub::ViewModel, form: &BudgetForm) {
    let (th, zh) = (&app.theme, app.zh);
    let Some(b) = app.bots.as_ref().and_then(|v| v.read().bots.iter().find(|b| b.id == form.id).cloned()) else {
        return;
    };
    let on = |st: Style| st.bg(th.select_bg);
    let w = 92.min(area.width.saturating_sub(2));
    let price = crate::wallet::sol_usd(app, vm);
    let free = app.wallet.as_ref().map(|wl| {
        let v = wl.read();
        (
            crate::wallet::spendable(app, &v, crate::wallet::Asset::Usdc),
            crate::wallet::spendable(app, &v, crate::wallet::Asset::Sol),
        )
    });
    let (free_usdc, free_sol) = free.unwrap_or((None, None));
    let to = form.cents().map(|c| f64::from(c) / 100.0);
    // what the number written would do, in words
    let mut says: Vec<(String, Style)> = Vec::new();
    match to {
        None => says.push((
            if zh {
                format!("请输入 {:.0} 到 {:.0} 之间的数字（美元）。", BUDGET.0, BUDGET.1)
            } else {
                format!("Write a number of USD between {:.0} and {:.0}.", BUDGET.0, BUDGET.1)
            },
            th.muted(),
        )),
        Some(v) if (v - b.budget).abs() < 0.005 => says.push(match (b.wish, zh) {
            (Some(w), true) => (format!("和现在一样：这会取消已登记的调整（调到 {w:.2} 美元）。"), th.text()),
            (Some(w), false) => {
                (format!("What it is now: this takes back the change that was noted (to {w:.2} USD)."), th.text())
            }
            (None, true) => ("和现在一样，不用调。".to_string(), th.muted()),
            (None, false) => ("That is what it is now: nothing to change.".to_string(), th.muted()),
        }),
        Some(v) if v > b.budget => {
            let more = v - b.budget;
            let from_usdc = free_usdc.map_or(0.0, |u| u.min(more));
            let missing = more - from_usdc;
            // (the last cents are not swapped for: the budget is then what there is)
            let swaps = missing > more * 0.05;
            let sold = price.filter(|_| swaps).map(|p| missing / p);
            says.push((
                match (swaps, sold, zh) {
                    (false, _, true) => format!("调高 {more:.2} 美元：用钱包里空闲的 USDC，不用兑换。"),
                    (false, _, false) => format!("{more:.2} USD more, from USDC the wallet holds free: nothing is swapped."),
                    (true, Some(s), true) => format!(
                        "调高 {more:.2} 美元：先用钱包里空闲的 USDC（{from_usdc:.2}），不够的 {missing:.2} 美元卖出约 {s:.4} SOL 来换。"
                    ),
                    (true, None, true) => format!(
                        "调高 {more:.2} 美元：先用钱包里空闲的 USDC（{from_usdc:.2}），不够的 {missing:.2} 美元卖出 SOL 来换。"
                    ),
                    (true, Some(s), false) => format!(
                        "{more:.2} USD more: the wallet's free USDC first ({from_usdc:.2}), and about {s:.4} SOL sold for the {missing:.2} USD that is missing."
                    ),
                    (true, None, false) => format!(
                        "{more:.2} USD more: the wallet's free USDC first ({from_usdc:.2}), and SOL sold for the {missing:.2} USD that is missing."
                    ),
                },
                th.text(),
            ));
            if let (Some(s), Some(have)) = (sold, free_sol)
                && s > have
            {
                says.push((
                    if zh {
                        format!("钱包里可动用的 SOL 只有 {have:.4}，不够换：这样调不会成功。")
                    } else {
                        format!("The wallet has only {have:.4} SOL free for it: this would not go through.")
                    },
                    th.warn(),
                ));
            }
            if swaps {
                says.push((
                    t(zh, "That is a real swap, with its fee.", "这是一笔真实兑换，有一点手续费。").to_string(),
                    th.muted(),
                ));
            }
        }
        Some(v) => {
            let back = b.budget - v;
            says.push((
                if zh {
                    format!("调低 {back:.2} 美元：这部分以 USDC 还给钱包，不再归它用。不用兑换。")
                } else {
                    format!("{back:.2} USD less: that goes back to the wallet as USDC, no longer the bot's. Nothing is swapped.")
                },
                th.text(),
            ));
            if b.funded && b.cash + 1e-9 < back {
                says.push((
                    if zh {
                        format!("它现在手里只有 {:.2} USDC，其余是 SOL：要等它卖出后才会调低，到时自动生效。", b.cash)
                    } else {
                        format!("It holds only {:.2} in USDC now, the rest is SOL: it is lowered once it has sold, by itself.", b.cash)
                    },
                    th.warn(),
                ));
            }
        }
    }
    if let Some(v) = to.filter(|v| (v - b.budget).abs() >= 0.005) {
        let mut tail = t(zh, "What it has made or lost so far stays as it is.", "它已经赚到或亏掉的不变。").to_string();
        if let Some(s) = b.stop_at.filter(|_| b.budget > 0.0) {
            tail += &if zh {
                format!("清仓线变成 {:.2} 美元（市值跌到这里就全部卖出并结束）。", v * s / b.budget)
            } else {
                format!(" It is then sold out at {:.2} USD.", v * s / b.budget)
            };
        }
        says.push((tail, th.muted()));
    }
    let says: Vec<(String, Style)> = says
        .into_iter()
        .flat_map(|(l, st)| wrap_words(&l, w.saturating_sub(4)).into_iter().map(move |x| (x, st)))
        .collect();
    let title = if zh { format!("调整 {} 的预算", b.name) } else { format!("The budget of {}", b.name) };
    let inner = crate::panels::overlay(buf, area, w, 9 + says.len() as u16, &title, th, &app.glyphs);
    app.hit(Rect { x: inner.x - 2, y: inner.y - 1, width: inner.width + 4, height: inner.height + 2 }, Hit::Overlay);
    let key_w = 14;
    let row = |buf: &mut Buffer, y: u16, k: &str, v: &str, st: Style| {
        text(buf, inner.x, y, k, key_w, on(th.muted()));
        text_fit(buf, inner.x + key_w, y, v, inner.width.saturating_sub(key_w), on(st));
    };
    let holds = match (b.funded, b.sol > 0.0, zh) {
        (false, _, true) => "还没划拨".to_string(),
        (false, _, false) => "not set aside yet".to_string(),
        (true, true, true) => format!("它手里 {:.6} SOL + {:.2} USDC", b.sol, b.cash),
        (true, true, false) => format!("it holds {:.6} SOL + {:.2} USDC", b.sol, b.cash),
        (true, false, true) => format!("它手里 {:.2} USDC", b.cash),
        (true, false, false) => format!("it holds {:.2} USDC", b.cash),
    };
    let worth = b
        .worth
        .filter(|_| b.funded)
        .map_or(String::new(), |v| if zh { format!("，现值 {v:.2} 美元") } else { format!(", worth {v:.2} USD now") });
    let now = if zh {
        format!("{:.2} 美元（{holds}{worth}）", b.budget)
    } else {
        format!("{:.2} USD ({holds}{worth})", b.budget)
    };
    row(buf, inner.y + 1, t(zh, "Budget now", "现在的预算"), &now, th.text());
    let wallet = match (free_usdc, free_sol) {
        (Some(u), Some(s)) => {
            let usd = price.map_or(String::new(), |p| format!(" ≈ {:.2} USD", s * p));
            if zh {
                format!("USDC {u:.2} · SOL {s:.4}{usd}（留作手续费和归机器人的已扣掉）")
            } else {
                format!("USDC {u:.2} · SOL {s:.4}{usd} (fees kept and the bots' own left out)")
            }
        }
        _ => t(zh, "not read yet", "还没读到").to_string(),
    };
    row(buf, inner.y + 2, t(zh, "Wallet, free", "钱包里空闲"), &wallet, th.text());
    text(buf, inner.x, inner.y + 4, t(zh, "New budget", "新的预算"), key_w, on(th.accent_bold()));
    let typed = format!("{}▏", form.text);
    let tw = text(buf, inner.x + key_w, inner.y + 4, &typed, 8, on(th.text().add_modifier(Modifier::BOLD)));
    let unit = if zh {
        format!("美元   （{:.0} 到 {:.0} 之间）", BUDGET.0, BUDGET.1)
    } else {
        format!("USD   ({:.0} to {:.0})", BUDGET.0, BUDGET.1)
    };
    text(buf, inner.x + key_w + tw + 1, inner.y + 4, &unit, 40, on(th.faint()));
    for (i, (line, st)) in says.iter().enumerate() {
        text(buf, inner.x, inner.y + 6 + i as u16, line, inner.width, on(*st));
    }
    let can = to.is_some_and(|v| b.wish.is_some() || (v - b.budget).abs() >= 0.005);
    let hint = match (can, zh) {
        (true, true) => "⏎ 下一步（还会再问一次） · Esc 取消",
        (false, true) => "Esc 取消",
        (true, false) => "⏎ next (it asks once more) · Esc cancel",
        (false, false) => "Esc cancel",
    };
    crate::panels::overlay_hint(buf, inner, hint, th);
}

/// The text of the question asked before an action.
pub fn question(b: &BotView, action: BotAction, zh: bool) -> (String, String) {
    match (action, zh) {
        (BotAction::Start, false) => (
            format!("Start {} with real money?", b.name),
            format!(
                "Budget {:.2} USD. It buys and sells SOL by itself at each bar's close and can lose money. \
                 It runs in the background and goes on when this window is closed; x stops it.",
                b.budget
            ),
        ),
        (BotAction::Stop, false) => (
            format!("Stop {}?", b.name),
            "What it holds stays as it is: nothing is sold. While it is stopped it does not buy or sell, \
             and its stops do not act."
                .to_string(),
        ),
        (BotAction::Close, false) => (
            format!("Sell what {} holds and end it?", b.name),
            "Its SOL is sold for USDC, which stays in the wallet. A run that ended does not start again.".to_string(),
        ),
        (BotAction::Budget { cents }, _) => {
            let to = f64::from(cents) / 100.0;
            let floor = b.stop_at.filter(|_| b.budget > 0.0).map(|s| to * s / b.budget);
            if (to - b.budget).abs() < 0.005 {
                return if zh {
                    (format!("取消 {} 已登记的预算调整吗？", b.name), format!("预算保持 {:.2} 美元不变。", b.budget))
                } else {
                    (
                        format!("Take back the change noted for {}?", b.name),
                        format!("Its budget stays {:.2} USD.", b.budget),
                    )
                };
            }
            let title = if zh {
                format!("把 {} 的预算从 {:.2} 调到 {to:.2} 美元吗？", b.name, b.budget)
            } else {
                format!("Change the budget of {} from {:.2} to {to:.2} USD?", b.name, b.budget)
            };
            let mut body = match (to > b.budget, zh) {
                (true, true) => format!(
                    "会从钱包再划 {:.2} 美元给它：先用钱包里空闲的 USDC，不够的部分卖出 SOL 来换（真实兑换，有一点手续费）。",
                    to - b.budget
                ),
                (false, true) => format!(
                    "会把 {:.2} 美元还给钱包：以 USDC 留在钱包里，不再归它用。它手里的 USDC 不够时，会等卖出 SOL 之后再调。",
                    b.budget - to
                ),
                (true, false) => format!(
                    "{:.2} USD more of the wallet becomes its own: USDC the wallet holds free first, and SOL sold for what is missing (a real swap, with its fee).",
                    to - b.budget
                ),
                (false, false) => format!(
                    "{:.2} USD goes back to the wallet: it stays there as USDC, no longer the bot's. If it holds less than that in USDC, it waits until it has sold its SOL.",
                    b.budget - to
                ),
            };
            body += if zh {
                "它已经赚到或亏掉的不变。"
            } else {
                " What it has made or lost so far stays as it is."
            };
            if let Some(f) = floor {
                body += &if zh {
                    format!("清仓线随之变成 {f:.2} 美元。")
                } else {
                    format!(" It is then sold out at {f:.2} USD.")
                };
            }
            (title, body)
        }
        (BotAction::Start, true) => (
            format!("用真钱启动 {} 吗？", b.name),
            format!(
                "预算 {:.2} 美元。它会在每根线收盘时自己买卖 SOL，可能亏钱。它在后台运行，关掉这个窗口也会继续；按 x 可以停止。",
                b.budget
            ),
        ),
        (BotAction::Stop, true) => (
            format!("停止 {} 吗？", b.name),
            "它持有的东西保持原样，不会卖出。停着的时候它不买不卖，止损也不生效。".to_string(),
        ),
        (BotAction::Close, true) => (
            format!("卖出 {} 的持仓并结束它吗？", b.name),
            "它的 SOL 会换成 USDC 留在钱包里。结束的一轮不会再启动。".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn bot() -> BotView {
        BotView {
            id: "trade-417971eff3174c0b".into(),
            name: "dip-1d".into(),
            real: true,
            inst: "SOL-USDT".into(),
            bar: "15m".into(),
            bar_ms: 900_000,
            rule: "In when the close is 1 deviation under its 96-bar average.".into(),
            state: BotState::Running,
            funded: true,
            pending: false,
            budget: 2.0,
            stop_at: Some(1.0),
            cash: 2.0,
            sol: 0.0,
            paid: 0.0,
            worth: Some(2.0),
            trades: Vec::new(),
            levels: Levels { buy: Some(120.93), sell: Some(121.33), stop: None },
            calc: Some(Calc { window: 96, mean: 121.33, sd: 0.40, k: 1.0, exit_z: 0.0, stop: Some(0.05) }),
            closes: vec![(1_000, 121.6), (901_000, 121.69)],
            fills: Vec::new(),
            journal: Vec::new(),
            file: Some("/home/me/trade.toml".into()),
            wish: None,
            opened: None,
            equity: Vec::new(),
        }
    }

    #[test]
    fn it_says_what_it_waits_for_and_how_far_that_is() {
        let mut b = bot();
        assert_eq!(b.brief(false), "waiting, buys under 120.93");
        assert_eq!(
            b.doing(false, None),
            "Waiting to buy: when a 15m bar closes under 120.93, 0.62 % below the last close (121.69)."
        );
        // with a live price it is the price now that is measured from
        assert_eq!(
            b.doing(false, Some(121.11)),
            "Waiting to buy: when a 15m bar closes under 120.93, 0.15 % below the price now (121.11)."
        );
        // a price that is under the level already is said as that, not as a distance
        assert_eq!(
            b.doing(false, Some(120.8)),
            "Waiting to buy: when a 15m bar closes under 120.93. The price now (120.80) is under it."
        );
        assert_eq!(
            b.doing(true, Some(121.11)),
            "等待买入：一根15 分钟线收盘低于 120.93 就买，现价 121.11，还要再跌 0.15%。"
        );
        (b.sol, b.paid, b.cash, b.worth) = (0.0165, 2.0, 0.0, Some(2.02));
        b.levels.stop = Some(114.9);
        assert_eq!(b.brief(false), "in SOL +1.0 %");
        assert_eq!(b.brief(true), "持仓中 +1.0%");
        let d = b.doing(false, Some(121.11));
        assert!(d.starts_with("Holds 0.016500 SOL bought for 2.0000 USD. Sells when a 15m bar closes above 121.33, 0.18 % above the price now"), "{d}");
        assert!(d.ends_with("Sells at a loss when one closes under 114.90."), "{d}");
        b.state = BotState::Stopped;
        assert!(b.doing(false, None).starts_with("Not running:"), "{}", b.doing(false, None));
        b.state = BotState::Ended("stopped at 0.9900 USD of 2.00".into());
        assert!(b.doing(false, None).starts_with("Ended: stopped at 0.9900 USD of 2.00."));
        assert!(
            b.doing(true, None).starts_with("已结束：触发总止损，结束时 0.9900 美元（预算 2.00）。"),
            "{}",
            b.doing(true, None)
        );
    }

    #[test]
    fn an_action_is_offered_only_where_it_makes_sense() {
        let mut b = bot();
        let can = |b: &BotView, a| b.can(a, false);
        assert!(can(&b, BotAction::Stop).is_ok());
        assert!(can(&b, BotAction::Start).is_err() && can(&b, BotAction::Close).is_err(), "running: stop first");
        b.state = BotState::Stopped;
        assert!(
            can(&b, BotAction::Start).is_ok() && can(&b, BotAction::Close).is_ok() && can(&b, BotAction::Stop).is_err()
        );
        b.file = None;
        assert!(can(&b, BotAction::Start).unwrap_err().contains("--trade FILE"));
        b.state = BotState::Ended("closed by hand".into());
        assert!(can(&b, BotAction::Start).is_err());
        b.state = BotState::Paper;
        assert!(can(&b, BotAction::Stop).unwrap_err().contains("--lab"));
        assert!(b.can(BotAction::Stop, true).unwrap_err().contains("纸面"));
    }

    #[test]
    fn the_header_names_the_real_bot_that_runs() {
        let (mut running, mut stopped, mut paper) = (bot(), bot(), bot());
        (stopped.name, stopped.state) = ("old".into(), BotState::Stopped);
        (paper.real, paper.state) = (false, BotState::Paper);
        let view = BotsView { bots: vec![paper.clone(), stopped.clone(), running.clone()], ..Default::default() };
        assert_eq!(chip(&view, false), Some(("dip-1d · waiting, buys under 120.93".into(), true)));
        running.state = BotState::Ended("x".into());
        let view = BotsView { bots: vec![paper.clone(), stopped, running], ..Default::default() };
        assert_eq!(chip(&view, false), Some(("old · stopped".into(), false)));
        assert_eq!(
            chip(&BotsView { bots: vec![paper], ..Default::default() }, false),
            None,
            "paper is not in the header"
        );
    }

    #[test]
    fn what_the_program_printed_is_read_in_the_operators_language() {
        let zh = |line: &str| said(true, line);
        assert_eq!(
            zh("bought 0.016464 SOL for 2.0000 USDC (121.48 a SOL, every cost inside)"),
            "买入 0.016464 SOL，花费 2.0000 USDC（均价 121.48，含全部费用）"
        );
        assert_eq!(
            zh("sold 0.016464 SOL for 1.9965 USDC; this trade -0.0035 USD"),
            "卖出 0.016464 SOL，得到 1.9965 USDC；这一笔 -0.0035 美元"
        );
        assert_eq!(
            zh("the wallet holds 2.0051 USDC: 2.0000 of it is the rule's budget"),
            "钱包里有 2.0051 USDC：其中 2.0000 作为这条规则的预算"
        );
        assert_eq!(zh("not sent: 3 quotes failed in simulation"), "没有发出：3 quotes failed in simulation");
        assert_eq!(
            zh("sent: spend 2.0000 USDC for at least 0.016505 SOL (quoted 0.016555) via Deriverse + PancakeSwap, \
                115529 CU, priority fee 1999 + tip 3637 lamports; confirmed; signature 2jjN"),
            "已发出：用 2.0000 USDC 买入，最少到手 0.016505 SOL（报价 0.016555），经 Deriverse + PancakeSwap，\
             115529 CU，优先费 1999 + 小费 3637 lamports；链上已确认；签名 2jjN"
        );
        assert_eq!(
            zh("something it never said before"),
            "something it never said before",
            "an unknown line is left as it is"
        );
        assert_eq!(
            said(false, "bought 1 SOL for 2 USDC (2 a SOL, every cost inside)"),
            "bought 1 SOL for 2 USDC (2 a SOL, every cost inside)"
        );
    }

    #[test]
    fn its_record_is_a_document_of_days_entries_and_their_parts() {
        let mut b = bot();
        let day = 1_791_180_000_000i64;
        b.journal = vec![
            (day, "the wallet holds 2.0051 USDC: 2.0000 of it is the rule's budget".into()),
            (
                day + 9_000_000,
                "sent: spend 2.0000 USDC for at least 0.016505 SOL (quoted 0.016555) via Deriverse + PancakeSwap, 115529 CU, priority fee 1999 + tip 3637 lamports; confirmed; signature 3Examp1e; refused by Jito (jito rpc error -32602: bundle contains an already processed transaction)".into(),
            ),
            (day + 9_000_000, "bought 0.016542 SOL for 2.0000 USDC (120.90 a SOL, every cost inside)".into()),
            (day + 90_000_000, "something it never said before".into()),
        ];
        let doc = journal_doc(&b, true);
        let lines: Vec<&str> = doc.lines().collect();
        assert!(lines[0].starts_with("# 2026-"), "a heading a day: {doc}");
        assert_eq!(doc.matches("\n# ").count(), 1, "the last line is of the next day: {doc}");
        assert_eq!(doc.matches("| 时间 | 事件 | 项目 | 内容 |\n|---|---|---|---|").count(), 2, "a table a day: {doc}");
        for needle in [
            // what it did, said once, on the row of its first part; its other parts under it
            " | 划拨预算 | 钱包里有 | 2.0051 USDC |\n|  |  | 作为预算 | 2.0000 USDC（不用兑换） |",
            " | 已发出兑换 | 兑换 | 用 2.0000 USDC 换 SOL |\n|  |  | 最少到手 | 0.016505 SOL（报价 0.016555） |\n|  |  | 经过 | Deriverse + PancakeSwap |\n|  |  | 网络费 | 优先费 1999 + 小费 3637 lamports（计算量 115529 CU） |\n|  |  | 结果 | 链上已确认 |\n|  |  | 签名 | 3Examp1e |\n|  |  | 备注 | Jito 回答“交易已经上链”",
            " | 买入 | 数量 | 0.016542 SOL |\n|  |  | 花费 | 2.0000 USDC |\n|  |  | 均价 | 120.90 USDC 一个 SOL（含全部费用） |",
            " | 说明 | | something it never said before |",
        ] {
            assert!(doc.contains(needle), "missing {needle:?} in:\n{doc}");
        }
        let en = journal_doc(&b, false);
        assert!(
            en.contains("| Time | What | Item | Detail |") && en.contains(" | Bought | Amount | 0.016542 SOL |"),
            "{en}"
        );
        assert!(en.contains("|  |  | Outcome | confirmed on the chain |"), "{en}");
    }

    #[test]
    fn its_buys_and_sales_are_read_from_its_record() {
        let mut b = bot();
        b.journal = vec![
            (1_000, "the wallet holds 2.0051 USDC: 2.0000 of it is the rule's budget".into()),
            (2_000, "sent: spend 2.0000 USDC for at least 0.016505 SOL (quoted 0.016555) via X, 1 CU, priority fee 1 + tip 1 lamports".into()),
            (2_000, "bought 0.016542 SOL for 2.0000 USDC (120.90 a SOL, every cost inside)".into()),
            (9_000, "sold 0.016542 SOL for 2.0100 USDC; this trade +0.0100 USD".into()),
        ];
        let f = fills_of(&b);
        assert_eq!(f.len(), 2, "{f:?}");
        assert_eq!(
            (f[0].at, f[0].buy, f[0].sol, f[0].usdc, f[0].price, f[0].net),
            (2_000, true, 0.016542, 2.0, 120.9, None)
        );
        assert_eq!((f[1].at, f[1].buy, f[1].net), (9_000, false, Some(0.01)));
        assert!((f[1].price - 2.01 / 0.016542).abs() < 1e-9, "what a SOL was sold for, every cost inside");
        assert_eq!(
            (span(13 * 60_000, true), span(373 * 60_000, true), span(3_000 * 60_000, false)),
            ("13 分钟".into(), "6 小时 13 分".into(), "2 d 2 h".into())
        );
    }

    #[test]
    fn the_numbers_behind_the_prices_are_written_out() {
        let b = bot();
        let lines = |zh, w| worked_out(&b, zh, Some(121.11), w).into_iter().map(|l| l.0).collect::<Vec<_>>();
        let en = lines(false, 200);
        assert_eq!(en[0], "From the closes of the last 96 15m bars (1 d): their average 121.33, their deviation 0.40");
        assert!(
            en[1].starts_with("buy price = average − 1 × deviation = 120.93    sell price = the average = 121.33"),
            "{}",
            en[1]
        );
        assert!(en[1].ends_with("loss price = what it paid × 0.95 (5 % down)"), "{}", en[1]);
        assert!(
            en[2].starts_with("The price now, 121.11, is 0.55 deviations under the average; it buys 1 under."),
            "{}",
            en[2]
        );
        let cn = lines(true, 200);
        assert_eq!(cn[0], "取最近 96 根15 分钟线（约 1 天）的收盘价：平均价 121.33，波动幅度（标准差）0.40");
        assert!(cn[1].starts_with("买入线 = 平均价 − 1 × 波动 = 120.93    卖出线 = 平均价 = 121.33"), "{}", cn[1]);
        assert!(cn[2].starts_with("现价 121.11 比平均价低 0.55 个波动；低 1 个就买。"), "{}", cn[2]);
        // narrow: nothing is cut, the sums stand one under the other
        let cn = lines(true, 60);
        assert!(cn.iter().all(|l| width(l) <= 60), "{cn:?}");
        assert!(cn.contains(&"卖出线 = 平均价 = 121.33".to_string()), "{cn:?}");
        // while it holds SOL, the price is measured against where it sells
        let mut held = bot();
        (held.sol, held.paid) = (0.0165, 2.0);
        let last = worked_out(&held, true, Some(121.11), 200).pop().unwrap();
        assert!(last.1 && last.0.contains("收盘高于卖出线 121.33 就卖"), "{last:?}");
    }
}
