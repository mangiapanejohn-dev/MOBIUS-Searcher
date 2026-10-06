//! The Wallet page: what the wallet holds, where it receives, what came and
//! went, and sending to another wallet. As with the bots, the TUI neither
//! reads the chain nor signs anything: it shows the [`WalletView`] the
//! application fills and hands the operator's wishes back through a
//! [`WalletPort`].
//!
//! A transfer cannot be taken back, so it is never one key: the application
//! checks it first (the address, the amount, what the chain says of who
//! receives it), the page shows that back in full, and it is sent only once
//! the operator has typed the end of the address it goes to.

use crate::app::{App, Hit};
use crate::bots::BotState;
use crate::chart::{put, text, text_fit, width};
use crate::hub::ViewModel;
use crate::panels::{fill, overlay, overlay_hint, section, wrap_lines, wrap_words};
use crate::theme::Depth;
use parking_lot::{RwLock, RwLockReadGuard};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use searcher_core::Ts;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Asset {
    Sol,
    Usdc,
}

impl Asset {
    pub fn name(self) -> &'static str {
        match self {
            Asset::Sol => "SOL",
            Asset::Usdc => "USDC",
        }
    }

    /// Decimals the chain counts it in.
    pub fn decimals(self) -> usize {
        match self {
            Asset::Sol => 9,
            Asset::Usdc => 6,
        }
    }
}

/// One transaction, as it changed the wallet.
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    /// When (ms).
    pub at: i64,
    /// Change of its SOL, the fee inside.
    pub sol: f64,
    pub usdc: f64,
    /// SOL the wallet paid as its fee (none when another paid it).
    pub fee: f64,
    pub failed: bool,
    pub signature: String,
}

impl Moved {
    /// What it was, told by what it changed.
    pub fn kind(&self, zh: bool) -> &'static str {
        let sol = self.sol + self.fee;
        let (some_sol, some_usdc) = (sol.abs() > 1e-9, self.usdc.abs() > 1e-9);
        let word = |en, cn| if zh { cn } else { en };
        if self.failed {
            word("failed", "失败")
        } else if some_sol && some_usdc && (sol > 0.0) != (self.usdc > 0.0) {
            word("swap", "兑换")
        } else if (some_usdc && self.usdc > 0.0) || (!some_usdc && some_sol && sol > 0.0) {
            word("received", "收到")
        } else if some_usdc || some_sol {
            word("sent", "转出")
        } else {
            word("fee only", "仅手续费")
        }
    }
}

/// What the operator asks to send, as typed.
#[derive(Clone, Debug, PartialEq)]
pub struct SendRequest {
    pub asset: Asset,
    pub to: String,
    pub amount: String,
}

/// A request the application checked: what would happen, to be read before it does.
#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub request: SendRequest,
    /// The address as the chain reads it.
    pub to: String,
    pub amount: f64,
    /// What the chain says of the address, in the operator's language.
    pub recipient: String,
    /// SOL: the network's fee.
    pub fee: f64,
    /// SOL that opening the recipient's USDC account takes, when it has none.
    pub opens_account: Option<f64>,
    /// What the wallet holds after it.
    pub left_sol: f64,
    pub left_usdc: f64,
    /// What to know before sending, in the operator's language.
    pub notes: Vec<String>,
}

/// Where a transfer stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Sending {
    Checking(SendRequest),
    /// Checked and not possible: why.
    Refused(SendRequest, String),
    Reviewed(Review),
    Sending(Review),
    Done {
        review: Review,
        signature: String,
    },
    /// Not sent, or sent and not known to have arrived: what happened, in words.
    Failed {
        review: Review,
        why: String,
        signature: Option<String>,
    },
}

/// An address this wallet sent to before.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipient {
    pub address: String,
    /// When it was last sent to (ms).
    pub last: i64,
    pub times: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WalletView {
    pub address: Option<String>,
    pub sol: Option<f64>,
    pub usdc: Option<f64>,
    /// When the balances were read (ms).
    pub read_at: Option<i64>,
    /// The address as a QR code: rows of modules, `true` = dark.
    pub qr: Vec<Vec<bool>>,
    /// Newest first.
    pub moved: Vec<Moved>,
    /// SOL kept back for fees (what the rest of the program needs to go on).
    pub reserve: f64,
    /// Why nothing can be sent from here, when it cannot.
    pub cannot_send: Option<String>,
    pub sending: Option<Sending>,
    /// Why the wallet could not be read, when it could not.
    pub error: Option<String>,
    /// Addresses it sent to before, newest first: offered when a transfer is written.
    pub recipients: Vec<Recipient>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WalletAction {
    /// Check a request; the answer appears as the view's `sending`.
    Check(SendRequest),
    /// Send what was reviewed (anything else is not sent).
    Send(SendRequest),
    /// Forget the transfer on the page.
    Clear,
    /// Read the wallet again now.
    Refresh,
}

type ViewFn = dyn Fn() -> WalletView + Send + Sync;
type ActFn = dyn Fn(WalletAction) + Send + Sync;

/// How the TUI reaches the wallet: the application's two functions. Neither waits for the chain.
#[derive(Clone)]
pub struct WalletPort {
    pub view: Arc<ViewFn>,
    pub act: Arc<ActFn>,
}

impl std::fmt::Debug for WalletPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalletPort")
    }
}

/// The view, kept fresh on a thread of its own (a frame never waits for it).
pub struct Wallet {
    state: Arc<RwLock<WalletView>>,
    port: Option<WalletPort>,
    stop: Arc<AtomicBool>,
}

impl Wallet {
    /// Read now, then again three times a second until dropped.
    pub fn start(port: WalletPort) -> Wallet {
        let state = Arc::new(RwLock::new((port.view)()));
        let stop = Arc::new(AtomicBool::new(false));
        let (view, shared, stopped) = (port.view.clone(), state.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("wallet".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(300));
                let fresh = view();
                *shared.write() = fresh;
            }
        });
        Wallet { state, port: Some(port), stop }
    }

    /// A view that does not change (snapshots, tests).
    pub fn fixed(view: WalletView) -> Wallet {
        Wallet { state: Arc::new(RwLock::new(view)), port: None, stop: Arc::new(AtomicBool::new(false)) }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, WalletView> {
        self.state.read()
    }

    /// Hand it over, then read the view again so the page shows what changed.
    pub fn act(&self, action: WalletAction) {
        if let Some(port) = &self.port {
            (port.act)(action);
            *self.state.write() = (port.view)();
        }
    }
}

impl Drop for Wallet {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Characters a transfer is confirmed by: the end of the address it goes to.
pub const CONFIRM: usize = 4;

/// The transfer being written on the page.
#[derive(Clone, Debug, PartialEq)]
pub struct SendForm {
    pub asset: Asset,
    pub to: String,
    pub amount: String,
    /// 0: what, 1: to whom, 2: how much.
    pub field: usize,
    /// What the operator typed to confirm: the end of the address.
    pub typed: String,
    /// Addresses sent to before, newest first: `↑` `↓` on the address put one in.
    pub known: Vec<String>,
    /// Which of them is in, when one is.
    pub pick: Option<usize>,
}

/// What a key did to the form.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Stay,
    Act(WalletAction),
    /// The form goes; the transfer on the page is forgotten.
    Close,
}

impl SendForm {
    pub fn new(asset: Asset) -> SendForm {
        SendForm {
            asset,
            to: String::new(),
            amount: String::new(),
            field: 1,
            typed: String::new(),
            known: Vec::new(),
            pick: None,
        }
    }

    fn request(&self) -> SendRequest {
        SendRequest { asset: self.asset, to: self.to.clone(), amount: self.amount.clone() }
    }

    /// One key. `max`: the most that can be sent of the asset, for `m`.
    pub fn on_key(&mut self, k: KeyEvent, sending: Option<&Sending>, max: impl Fn(Asset) -> Option<f64>) -> Outcome {
        match sending {
            // a check can be walked away from; a transfer that is being sent cannot be taken back
            Some(Sending::Checking(_)) if k.code == KeyCode::Esc => Outcome::Act(WalletAction::Clear),
            Some(Sending::Checking(_) | Sending::Sending(_)) => Outcome::Stay,
            Some(Sending::Done { .. } | Sending::Failed { .. }) => Outcome::Close,
            Some(Sending::Reviewed(review)) => match k.code {
                KeyCode::Esc => {
                    self.typed.clear();
                    Outcome::Act(WalletAction::Clear)
                }
                KeyCode::Backspace => {
                    self.typed.pop();
                    Outcome::Stay
                }
                KeyCode::Char(c) if self.typed.chars().count() < CONFIRM && !c.is_whitespace() => {
                    self.typed.push(c);
                    Outcome::Stay
                }
                KeyCode::Enter if self.typed == tail(&review.to) => {
                    self.typed.clear();
                    Outcome::Act(WalletAction::Send(review.request.clone()))
                }
                _ => Outcome::Stay,
            },
            None | Some(Sending::Refused(..)) => self.edit(k, max),
        }
    }

    fn edit(&mut self, k: KeyEvent, max: impl Fn(Asset) -> Option<f64>) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match (k.code, self.field) {
            (KeyCode::Esc, _) => return Outcome::Close,
            // an address sent to before is not typed again: it is still checked and confirmed as any other
            (KeyCode::Down | KeyCode::Up, 1) if !self.known.is_empty() => {
                let n = self.known.len();
                let next = match (self.pick, k.code) {
                    (None, KeyCode::Down) => 0,
                    (None, _) => n - 1,
                    (Some(i), KeyCode::Down) => (i + 1) % n,
                    (Some(i), _) => (i + n - 1) % n,
                };
                (self.pick, self.to) = (Some(next), self.known[next].clone());
            }
            (KeyCode::Tab | KeyCode::Down, _) => self.field = (self.field + 1) % 3,
            (KeyCode::BackTab | KeyCode::Up, _) => self.field = (self.field + 2) % 3,
            (KeyCode::Enter, 0 | 1) => self.field += 1,
            (KeyCode::Enter, _) if !self.to.is_empty() && !self.amount.is_empty() => {
                return Outcome::Act(WalletAction::Check(self.request()));
            }
            (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), 0) => {
                self.asset = if self.asset == Asset::Sol { Asset::Usdc } else { Asset::Sol };
                self.amount.clear();
            }
            (KeyCode::Char('u'), 1 | 2) if ctrl => {
                if self.field == 1 {
                    self.to.clear()
                } else {
                    self.amount.clear()
                }
            }
            // a chord is not text
            (KeyCode::Char(_), _) if ctrl => {}
            // an address is base58: nothing else is taken, so a stray key cannot change it unseen
            (KeyCode::Char(c), 1) if self.to.len() < 44 && c.is_ascii_alphanumeric() && !"0OIl".contains(c) => {
                self.pick = None;
                self.to.push(c)
            }
            (KeyCode::Backspace, 1) => {
                self.pick = None;
                self.to.pop();
            }
            (KeyCode::Char('m'), 2) => {
                if let Some(most) = max(self.asset).filter(|m| *m > 0.0) {
                    self.amount = amount_text(most, self.asset);
                }
            }
            (KeyCode::Char(c), 2)
                if self.amount.len() < 20 && (c.is_ascii_digit() || (c == '.' && !self.amount.contains('.'))) =>
            {
                self.amount.push(c)
            }
            (KeyCode::Backspace, 2) => {
                self.amount.pop();
            }
            _ => {}
        }
        Outcome::Stay
    }
}

/// The end of an address: what a transfer to it is confirmed by.
pub fn tail(address: &str) -> &str {
    &address[address.len().saturating_sub(CONFIRM)..]
}

/// An amount as it is typed: every decimal the chain counts, no trailing zeros.
fn amount_text(v: f64, asset: Asset) -> String {
    // down to the chain's unit, never up: more than there is cannot be sent
    let unit = 10f64.powi(asset.decimals() as i32);
    // (a hair added first: 2.005052 is a hair under itself as a float)
    let s = format!("{:.*}", asset.decimals(), (v * unit + 1e-6).floor() / unit);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `en` or `cn`, by the operator's language.
fn t<'a>(zh: bool, en: &'a str, cn: &'a str) -> &'a str {
    if zh { cn } else { en }
}

/// An address in fours, as it is read aloud and compared.
fn in_fours(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

fn short(address: &str) -> String {
    if address.len() <= 10 {
        return address.to_string();
    }
    format!("{}…{}", &address[..4], tail(address))
}

/// USD a SOL: the exchange's price when the page has one, else the session's.
pub(crate) fn sol_usd(app: &App, vm: &ViewModel) -> Option<f64> {
    let live =
        app.cex.as_ref().and_then(|c| c.state.read().ticker.as_ref().filter(|t| t.base() == "SOL").map(|t| t.last));
    live.or_else(|| vm.sol_price.and_then(|p| p.value(1_000_000_000, 9)).map(|u| u.0 as f64 / 1e6)).filter(|p| *p > 0.0)
}

/// What the real bots hold of the wallet: their names, SOL and USDC.
fn bots_hold(app: &App) -> Option<(String, f64, f64)> {
    let view = app.bots.as_ref()?.read();
    let held: Vec<_> = view
        .bots
        .iter()
        .filter(|b| b.real && b.funded && !matches!(b.state, BotState::Ended(_)) && (b.sol > 0.0 || b.cash > 0.0))
        .collect();
    let names: Vec<&str> = held.iter().map(|b| b.name.as_str()).collect();
    (!held.is_empty()).then(|| (names.join(" + "), held.iter().map(|b| b.sol).sum(), held.iter().map(|b| b.cash).sum()))
}

/// The real bots that run now: their names, and whether one has a swap under way.
fn bots_run(app: &App) -> Option<(String, bool)> {
    let view = app.bots.as_ref()?.read();
    let running: Vec<_> = view.bots.iter().filter(|b| b.real && b.state == BotState::Running).collect();
    let names: Vec<&str> = running.iter().map(|b| b.name.as_str()).collect();
    (!running.is_empty()).then(|| (names.join(" + "), running.iter().any(|b| b.pending)))
}

/// The most of `asset` that can go without touching what the bots hold or the SOL kept for fees.
pub fn spendable(app: &App, view: &WalletView, asset: Asset) -> Option<f64> {
    let (_, bot_sol, bot_usdc) = bots_hold(app).unwrap_or_default();
    match asset {
        Asset::Sol => view.sol.map(|s| (s - bot_sol - view.reserve).max(0.0)),
        Asset::Usdc => view.usdc.map(|u| (u - bot_usdc).max(0.0)),
    }
}

fn signed(v: f64, decimals: usize, unit: &str) -> String {
    format!("{v:+.decimals$} {unit}")
}

pub fn page_wallet(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let title = t(zh, "WALLET", "钱包");
    let view = app.wallet.as_ref().map(|w| w.read());
    let Some((view, address)) = view.as_ref().and_then(|v| Some((v, v.address.as_ref()?))) else {
        let b = section(buf, body, title, false, "", th, g);
        let lines: [&str; 3] = if app.wallet.is_none() {
            [t(zh, "The wallet is not part of this view.", "这个视图里没有钱包。"), "", ""]
        } else if zh {
            [
                "还没有配置钱包。",
                "",
                "运行 mobius-searcher --setup，或者在配置文件的 [wallet] 里写上 pubkey（看余额）和 keypair_path（转出时用）。",
            ]
        } else {
            [
                "No wallet is configured yet.",
                "",
                "Run mobius-searcher --setup, or give [wallet] pubkey (to see it) and keypair_path (to send from it) in your config.",
            ]
        };
        for (i, l) in lines.iter().enumerate().take(b.height as usize) {
            text_fit(buf, b.x, b.y + i as u16, l, b.width, if i == 0 { th.text() } else { th.faint() });
        }
        return;
    };
    let price = sol_usd(app, vm);
    // the code beside what it holds when there is room for it; else the address alone, under
    let beside = body.width >= 110 && body.height >= 26 && !view.qr.is_empty();
    // (kept close to it on a wide screen: what belongs together is read together)
    let rw = (view.qr.first().map_or(0, Vec::len) as u16 + 4).max(42);
    let left = if beside { Rect { width: (body.width - rw - 3).min(118), ..body } } else { body };
    // on a tall screen, where the money is stands as a table of its own under what there is
    let tabled = body.height >= 50 && view.sol.is_some();
    let mut y = holdings(buf, left, app, view, address, price, tabled) + 1;
    if tabled {
        y = whereabouts(buf, Rect { y, height: left.bottom().saturating_sub(y), ..left }, app, view, price);
    }
    // what the money made: each bot and all of them, where there is a real bot and room to say it
    if left.bottom().saturating_sub(y) >= 26 {
        y = earnings(buf, Rect { y, height: left.bottom() - y, ..left }, app, vm);
    }
    if beside {
        receive(buf, Rect { x: left.right() + 3, width: rw, ..body }, app, view, address);
    } else {
        let r = section(
            buf,
            Rect { y, height: 4, ..left },
            t(zh, "RECEIVE", "收款"),
            false,
            t(zh, "c copy", "c 复制"),
            th,
            g,
        );
        text_fit(buf, r.x, r.y, address, r.width, th.text().add_modifier(Modifier::BOLD));
        text_fit(buf, r.x, r.y + 1, receive_note(zh), r.width, th.faint());
        y += 4;
    }
    // sending: what it is, or why it cannot be done here
    let keys = if view.cannot_send.is_some() {
        ""
    } else {
        t(zh, "s send SOL · u send USDC", "s 转出 SOL · u 转出 USDC")
    };
    let words = match &view.cannot_send {
        Some(why) => (why.clone(), th.warn()),
        None => (
            t(
                zh,
                "To another Solana address. Before anything is sent it is checked and shown back to you, and you confirm it by typing the end of the address.",
                "转到另一个 Solana 地址。发出前会先核对并把结果给你看，你再输入地址末尾几位确认。",
            )
            .to_string(),
            th.muted(),
        ),
    };
    let lines = wrap_words(&words.0, left.width.saturating_sub(1));
    let h = (lines.len() as u16 + 2).min(left.bottom().saturating_sub(y));
    if h >= 2 {
        let s = section(buf, Rect { y, height: h, ..left }, t(zh, "SEND", "转出"), false, keys, th, g);
        for (i, line) in lines.iter().enumerate().take(s.height as usize) {
            text(buf, s.x, s.y + i as u16, line, s.width, words.1);
        }
        y += h;
    }
    activity(buf, Rect { y, height: left.bottom().saturating_sub(y), ..left }, app, view);
}

/// What it holds and what that is worth. Returns the row after it.
fn holdings(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    view: &WalletView,
    address: &str,
    price: Option<f64>,
    tabled: bool,
) -> u16 {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let age = view.read_at.map(|at| ((Ts::now().0 / 1000 - at) / 1000).max(0));
    let read = match (age, zh) {
        (Some(s), false) if s < 90 => format!("read {s} s ago"),
        (Some(s), true) if s < 90 => format!("{s} 秒前读取"),
        (Some(s), false) => format!("read {} min ago", s / 60),
        (Some(s), true) => format!("{} 分钟前读取", s / 60),
        (None, false) => "not read yet".to_string(),
        (None, true) => "还没读到".to_string(),
    };
    let right = format!("{} · {read} · {}", short(address), t(zh, "r read again", "r 刷新"));
    let inner = section(buf, area, t(zh, "WALLET", "钱包"), true, &right, th, g);
    let mut y = inner.y;
    let row = |y: u16| y < inner.bottom();
    if let Some(e) = view.error.as_ref().filter(|_| row(y)) {
        text_fit(buf, inner.x, y, e, inner.width, th.warn());
        y += 1;
    }
    let (sol_usd, usdc_usd) = (view.sol.zip(price).map(|(s, p)| s * p), view.usdc);
    let total = match (sol_usd, usdc_usd) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        _ => None,
    };
    if row(y) {
        let mut x = inner.x + text(buf, inner.x, y, t(zh, "Worth", "总值"), 12, th.muted()).max(12);
        let worth = total.map_or("—".to_string(), |v| format!("≈ {v:.2} USD"));
        x += text(buf, x, y, &worth, 24, th.text().add_modifier(Modifier::BOLD)) + 3;
        if let Some(p) = price {
            let at = if zh { format!("SOL 现价 {p:.2}") } else { format!("a SOL is {p:.2} USD") };
            text_fit(buf, x, y, &at, inner.right().saturating_sub(x), th.faint());
        }
        y += 2;
    }
    // each of the two: how much, what that is worth, and its share of the whole as a bar
    let bar_w = inner.width.saturating_sub(62).min(48);
    for (name, held, usd, decimals) in [("SOL", view.sol, sol_usd, 6), ("USDC", view.usdc, usdc_usd, 6)] {
        if !row(y) {
            break;
        }
        text(buf, inner.x, y, name, 12, th.muted());
        let amount = held.map_or("—".to_string(), |v| format!("{v:.decimals$} {name}"));
        text(buf, inner.x + 12, y, &amount, 22, th.text());
        let worth = usd.map_or(String::new(), |v| format!("≈ {v:.2} USD"));
        text(buf, inner.x + 35, y, &worth, 18, th.muted());
        if let (Some(v), Some(all)) = (usd, total.filter(|a| *a > 0.0))
            && bar_w >= 8
        {
            let share = (v / all).clamp(0.0, 1.0);
            let filled = (share * bar_w as f64).round() as u16;
            for i in 0..bar_w {
                let st = if i < filled { th.accent() } else { th.rule() };
                put(buf, inner.x + 54 + i, y, if g.unicode { "━" } else { "=" }, st);
            }
            text(buf, inner.x + 55 + bar_w, y, &format!("{:.0}%", share * 100.0), 5, th.faint());
        }
        y += 1;
    }
    y += 1;
    // what the bots hold is in those numbers: said, so that it is not sent away by mistake
    if let Some((names, sol, usdc)) = bots_hold(app).filter(|_| row(y) && !tabled) {
        let held = match (sol > 0.0, usdc > 0.0) {
            (true, true) => format!("{sol:.6} SOL + {usdc:.4} USDC"),
            (true, false) => format!("{sol:.6} SOL"),
            _ => format!("{usdc:.4} USDC"),
        };
        let line = if zh {
            format!("其中机器人 {names} 持有 {held}（算在上面的余额里，转走它就卖不出去了）")
        } else {
            format!("Of that, {held} is what the bot {names} holds (sent away, the bot cannot sell it)")
        };
        text_fit(buf, inner.x, y, &line, inner.width, th.warn());
        y += 1;
    }
    if row(y) {
        let free = |a| spendable(app, view, a).map_or("—".to_string(), |v| amount_text(v, a));
        let line = if zh {
            format!(
                "可以转出    SOL {}（已留 {} 付手续费）  ·  USDC {}",
                free(Asset::Sol),
                amount_text(view.reserve, Asset::Sol),
                free(Asset::Usdc)
            )
        } else {
            format!(
                "Free to send    SOL {} ({} kept for fees)  ·  USDC {}",
                free(Asset::Sol),
                amount_text(view.reserve, Asset::Sol),
                free(Asset::Usdc)
            )
        };
        text_fit(buf, inner.x, y, &line, inner.width, th.text());
        y += 1;
    }
    y
}

/// Where the wallet's money is: what each real bot holds, what is kept for
/// fees, and what is free to send, each in SOL, in USDC and as its share of
/// the whole. Returns the row after it.
fn whereabouts(buf: &mut Buffer, area: Rect, app: &App, view: &WalletView, price: Option<f64>) -> u16 {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let (sol, usdc) = (view.sol.unwrap_or(0.0), view.usdc.unwrap_or(0.0));
    let mut rows: Vec<(String, f64, f64, Style)> = Vec::new();
    if let Some(bots) = &app.bots {
        let v = bots.read();
        for b in v.bots.iter().filter(|b| b.real && b.funded && !matches!(b.state, BotState::Ended(_))) {
            if b.sol > 0.0 || b.cash > 0.0 {
                let state = match (&b.state, zh) {
                    (BotState::Running, true) => "运行中",
                    (BotState::Running, false) => "running",
                    (_, true) => "已停止",
                    (_, false) => "stopped",
                };
                // which of the two its money is in says what it waits for: USDC to buy with, or SOL to sell
                let doing = match (b.sol > 0.0, zh) {
                    (true, true) => "已买入，拿着 SOL 等卖出",
                    (false, true) => "还没买，拿着 USDC 等买入",
                    (true, false) => "bought: holds SOL to sell",
                    (false, false) => "not bought: holds USDC to buy with",
                };
                let name = if zh {
                    format!("机器人 {}（{state} · {doing}）", b.name)
                } else {
                    format!("bot {} ({state}; {doing})", b.name)
                };
                rows.push((name, b.sol, b.cash, th.text()));
            }
        }
    }
    let (bot_sol, bot_usdc) = rows.iter().fold((0.0, 0.0), |a, r| (a.0 + r.1, a.1 + r.2));
    let kept = view.reserve.min((sol - bot_sol).max(0.0));
    rows.push((t(zh, "kept for fees", "留作手续费").to_string(), kept, 0.0, th.muted()));
    let free = ((sol - bot_sol - kept).max(0.0), (usdc - bot_usdc).max(0.0));
    rows.push((t(zh, "free to send", "空闲（可以转出）").to_string(), free.0, free.1, Style::new().fg(th.profit)));
    let usd = |s: f64, u: f64| price.map(|p| s * p + u);
    let all = usd(sol, usdc).filter(|a| *a > 0.0);
    let h = (rows.len() as u16 + 4).min(area.height);
    let inner = section(buf, Rect { height: h, ..area }, t(zh, "WHERE THE MONEY IS", "钱都在哪"), false, "", th, g);
    let cols: [(u16, &str); 5] = [
        (1, t(zh, "WHERE", "去向")),
        (58, "SOL"),
        (71, "USDC"),
        (83, t(zh, "ABOUT, USD", "约合 USD")),
        (96, t(zh, "SHARE", "占比")),
    ];
    for (x, name) in cols {
        text(buf, inner.x + x, inner.y, name, inner.width.saturating_sub(x), th.faint());
    }
    // (none of it is a nought: a dash was read as "it cannot")
    let amount = |v: f64, d: usize| if v > 0.0 { format!("{v:.d$}") } else { "0".to_string() };
    let mut y = inner.y + 1;
    let total = (t(zh, "in all", "合计").to_string(), sol, usdc, th.text().add_modifier(Modifier::BOLD));
    for (i, (name, s, u, st)) in rows.iter().chain(std::iter::once(&total)).enumerate() {
        if y >= inner.bottom() {
            break;
        }
        if i == rows.len() {
            for x in inner.x + 1..inner.x + inner.width.min(104) {
                put(buf, x, y, g.h, th.rule());
            }
            y += 1;
        }
        let worth = usd(*s, *u);
        let cells = [
            name.clone(),
            amount(*s, 6),
            amount(*u, 4),
            worth.map_or("—".to_string(), |w| format!("{w:.2}")),
            worth.zip(all).map_or(String::new(), |(w, a)| format!("{:.0} %", w / a * 100.0)),
        ];
        for ((x, _), v) in cols.iter().zip(cells) {
            text_fit(buf, inner.x + x, y, &v, inner.width.saturating_sub(*x), *st);
        }
        y += 1;
    }
    area.y + h + 1
}

/// What the money made, in all: a row a real bot (what it was given, what
/// that is worth, what its closed trades made, what it is up or down on what
/// it holds), the runs that ended long ago in one row, their sum, and the
/// arbitrage of this session. Returns the row after it (`area.y` when there
/// is no real bot to speak of).
fn earnings(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel) -> u16 {
    use crate::bots::{money_of, price_now};
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let Some(bots) = &app.bots else { return area.y };
    let view = bots.read();
    let real: Vec<_> = view.bots.iter().filter(|b| b.real).collect();
    if real.is_empty() && view.older.is_none() {
        return area.y;
    }
    let cols: [(u16, &str); 8] = [
        (1, t(zh, "BOT", "机器人")),
        (19, t(zh, "STATE", "状态")),
        (30, t(zh, "BUDGET", "预算")),
        (40, t(zh, "WORTH", "现值")),
        (52, t(zh, "MADE, CLOSED", "已实现")),
        (66, t(zh, "OPEN, NOW", "浮动")),
        (80, t(zh, "IN ALL", "合计")),
        (94, t(zh, "CLOSED", "平仓")),
    ];
    let usd = |v: f64| format!("{v:+.4}");
    // a row: its cells and the colour of its last three numbers
    let mut rows: Vec<([String; 8], [f64; 3], bool)> = Vec::new();
    let (mut budget, mut worth, mut realized, mut floating, mut closed) = (0.0, 0.0, 0.0, 0.0, 0usize);
    for b in &real {
        let now = price_now(app, &b.inst).or(b.closes.last().map(|c| c.1));
        let m = money_of(b, now);
        let over = matches!(b.state, BotState::Ended(_));
        let state = match (&b.state, zh) {
            (BotState::Running, true) => "运行中",
            (BotState::Stopped, true) => "已停止",
            (BotState::Ended(_), true) => "已结束",
            (BotState::Running, false) => "running",
            (BotState::Stopped, false) => "stopped",
            (BotState::Ended(_), false) => "ended",
            (BotState::Paper, _) => "paper",
        };
        let held = now.filter(|_| b.funded && !over).map(|p| b.cash + b.sol * p);
        rows.push((
            [
                b.name.clone(),
                state.to_string(),
                format!("{:.2}", b.budget),
                held.map_or("—".to_string(), |w| format!("{w:.4}")),
                usd(m.realized),
                if over { "—".to_string() } else { usd(m.floating) },
                usd(m.total()),
                m.closed.to_string(),
            ],
            [m.realized, m.floating, m.total()],
            over,
        ));
        if !over && b.funded {
            budget += b.budget;
            worth += held.unwrap_or(b.cash + b.paid);
        }
        realized += m.realized;
        floating += m.floating;
        closed += m.closed;
    }
    if let Some((runs, made, trades)) = view.older {
        let name = if zh { format!("更早结束的 {runs} 轮") } else { format!("{runs} older runs, ended") };
        rows.push((
            [name, String::new(), String::new(), "—".into(), usd(made), "—".into(), usd(made), trades.to_string()],
            [made, 0.0, made],
            true,
        ));
        realized += made;
        closed += trades;
    }
    // (sums that began at nothing keep no sign)
    let (realized, floating) = (realized + 0.0, floating + 0.0);
    let total = realized + floating;
    let h = (rows.len() as u16 + 6).min(area.height);
    let hint = t(zh, "USD · page 9 has each bot's trades", "单位 USD · 每个机器人的交易在 9");
    let inner =
        section(buf, Rect { height: h, ..area }, t(zh, "WHAT THE MONEY MADE", "盈亏（累计）"), false, hint, th, g);
    for (x, name) in cols {
        text(buf, inner.x + x, inner.y, name, inner.width.saturating_sub(x), th.faint());
    }
    let mut y = inner.y + 1;
    let draw = |buf: &mut Buffer, y: u16, cells: &[String; 8], pnl: &[f64; 3], dim: bool, bold: bool| {
        for (i, ((x, _), v)) in cols.iter().zip(cells).enumerate() {
            let st = match i {
                4..=6 => th.pnl(pnl[i - 4]),
                0 if !dim => th.text(),
                _ if dim => th.muted(),
                _ => th.text(),
            };
            let st = if bold { st.add_modifier(Modifier::BOLD) } else { st };
            text_fit(buf, inner.x + x, y, v, inner.width.saturating_sub(*x), st);
        }
    };
    for (cells, pnl, dim) in &rows {
        if y >= inner.bottom() {
            return area.y + h;
        }
        draw(buf, y, cells, pnl, *dim, false);
        y += 1;
    }
    if y + 1 < inner.bottom() {
        for x in inner.x + 1..inner.x + inner.width.min(102) {
            put(buf, x, y, g.h, th.rule());
        }
        let sum = [
            t(zh, "All the bots", "机器人合计").to_string(),
            String::new(),
            format!("{budget:.2}"),
            format!("{worth:.4}"),
            usd(realized),
            usd(floating),
            usd(total),
            closed.to_string(),
        ];
        draw(buf, y + 1, &sum, &[realized, floating, total], false, true);
        y += 2;
    }
    // the arbitrage's own, of this session: it has no account of its own to sum over the days
    if y < inner.bottom() {
        let made = vm.realized.0 as f64 / 1e6;
        let line = if zh {
            format!("套利（本次运行）   已实现 {made:+.4} USD · 成交 {} 笔", vm.executed)
        } else {
            format!("Arbitrage (this session)   made {made:+.4} USD · {} executed", vm.executed)
        };
        text_fit(buf, inner.x + 1, y, &line, inner.width.saturating_sub(1), th.muted());
    }
    area.y + h + 1
}

fn receive_note(zh: bool) -> &'static str {
    t(
        zh,
        "Solana network only: SOL and USDC. From an exchange, choose the Solana network; sent over another network it does not arrive.",
        "只收 Solana 网络上的 SOL 和 USDC。从交易所提币时网络要选 Solana，选错网络的钱收不到。",
    )
}

/// Where it receives: the address as a code to scan and as text to copy.
fn receive(buf: &mut Buffer, area: Rect, app: &App, view: &WalletView, address: &str) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    let inner = section(buf, area, t(zh, "RECEIVE", "收款"), false, t(zh, "c copy address", "c 复制地址"), th, g);
    let mut y = inner.y + 1;
    // two modules a cell (`▀` is the upper one), black on white whatever the theme, so that it scans
    let (dark, light) = match th.depth {
        Depth::TrueColor => (Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)),
        _ => (Color::Black, Color::White),
    };
    let blank = vec![false; view.qr.first().map_or(0, Vec::len)];
    let qx = inner.x + inner.width.saturating_sub(blank.len() as u16) / 2;
    for pair in view.qr.chunks(2) {
        if y >= inner.bottom() {
            return;
        }
        let (top, bottom) = (&pair[0], pair.get(1).unwrap_or(&blank));
        for (i, (&up, &down)) in top.iter().zip(bottom).enumerate() {
            let x = qx + i as u16;
            if x >= inner.right() {
                break;
            }
            if th.depth == Depth::None {
                // no colour: a dark background is assumed, as everywhere else
                let glyph = match (up, down) {
                    (false, false) => "█",
                    (true, false) => "▄",
                    (false, true) => "▀",
                    (true, true) => " ",
                };
                put(buf, x, y, glyph, Style::new());
            } else {
                let shade = |is_dark: bool| if is_dark { dark } else { light };
                put(buf, x, y, "▀", Style::new().fg(shade(up)).bg(shade(down)));
            }
        }
        y += 1;
    }
    y += 1;
    // the address in two halves under the code, then what it takes
    let half = address.len().div_ceil(2);
    let bold = th.text().add_modifier(Modifier::BOLD);
    let lines = [
        (t(zh, "Your address (Solana)", "你的地址（Solana 网络）").to_string(), th.muted(), true),
        (address[..half].to_string(), bold, true),
        (address[half..].to_string(), bold, true),
        (String::new(), th.faint(), false),
    ];
    let note = wrap_words(receive_note(zh), inner.width);
    for (line, st, centred) in lines.iter().cloned().chain(note.into_iter().map(|l| (l, th.faint(), false))) {
        if y >= inner.bottom() {
            return;
        }
        let x = if centred { inner.x + inner.width.saturating_sub(width(&line)) / 2 } else { inner.x };
        text(buf, x, y, &line, inner.width, st);
        y += 1;
    }
}

/// What came and went, newest first.
fn activity(buf: &mut Buffer, area: Rect, app: &App, view: &WalletView) {
    let (th, g, zh) = (&app.theme, &app.glyphs, app.zh);
    if area.height < 2 {
        return;
    }
    // what the transactions listed come to: received and sent (a swap is neither), and the fees this wallet paid
    let (mut got, mut gave, mut fees) = ((0.0, 0.0), (0.0, 0.0), 0.0);
    for m in view.moved.iter().filter(|m| !m.failed) {
        fees += m.fee;
        let moved = (m.sol + m.fee, m.usdc);
        match m.kind(false) {
            "received" => got = (got.0 + moved.0.max(0.0), got.1 + moved.1.max(0.0)),
            "sent" => gave = (gave.0 - moved.0.min(0.0), gave.1 - moved.1.min(0.0)),
            _ => {}
        }
    }
    let n = view.moved.len();
    let sums = if zh {
        format!(
            "这 {n} 笔合计：收到 {:.4} SOL + {:.2} USDC · 转出 {:.4} SOL + {:.2} USDC · 网络费 {fees:.6} SOL",
            got.0, got.1, gave.0, gave.1
        )
    } else {
        format!(
            "these {n} in all: in {:.4} SOL + {:.2} USDC · out {:.4} SOL + {:.2} USDC · fees {fees:.6} SOL",
            got.0, got.1, gave.0, gave.1
        )
    };
    let keys =
        if view.moved.is_empty() { "" } else { t(zh, "j/k select · ⏎ all of it", "j/k 选择 · ⏎ 详情") };
    let inner = section(buf, area, t(zh, "IN AND OUT", "最近进出"), false, keys, th, g);
    if view.moved.is_empty() {
        let none = if view.read_at.is_none() {
            t(zh, "reading…", "读取中…")
        } else {
            t(zh, "nothing in the wallet's recent history", "钱包最近没有进出记录")
        };
        text(buf, inner.x, inner.y, none, inner.width, th.faint());
        return;
    }
    let sel = app.wallet_selected.min(view.moved.len() - 1);
    // the selected one stays on the page
    let rows = inner.height as usize;
    let first = sel.saturating_sub(rows.saturating_sub(1));
    for (i, m) in view.moved.iter().enumerate().skip(first).take(rows) {
        let y = inner.y + (i - first) as u16;
        let line = Rect { x: inner.x, y, width: inner.width, height: 1 };
        app.hit(line, Hit::WalletRow(i));
        if i == sel {
            fill(buf, line, Style::new().bg(th.select_bg));
        }
        let bg = |st: Style| if i == sel { st.bg(th.select_bg) } else { st };
        let mut x = inner.x + 1;
        x += text(buf, x, y, &Ts(m.at * 1000).format("%m-%d %H:%M"), 11, bg(th.faint())) + 2;
        let kind_st = if m.failed { th.warn() } else { th.text() };
        text(buf, x, y, m.kind(zh), 10, bg(kind_st));
        x += 11;
        // what moved, the fee apart: a fee alone is said as one
        let sol = m.sol + m.fee;
        let (some_sol, some_usdc) = (sol.abs() > 1e-9, m.usdc.abs() > 1e-9);
        if some_usdc {
            text(buf, x, y, &signed(m.usdc, 4, "USDC"), 18, bg(th.pnl(m.usdc)));
        }
        x += 19;
        if some_sol {
            text(buf, x, y, &signed(sol, 6, "SOL"), 18, bg(th.pnl(sol)));
        } else if m.fee > 0.0 && !some_usdc {
            let fee = format!("{} {:.6} SOL", t(zh, "fee", "手续费"), m.fee);
            text(buf, x, y, &fee, 24, bg(th.muted()));
        }
        x += 19;
        text_fit(buf, x, y, &short(&m.signature), inner.right().saturating_sub(x), bg(th.faint()));
    }
    // under the last of them, where there is a row left: what they come to
    let under = inner.y + (view.moved.len() - first).min(rows) as u16 + 1;
    if under < inner.bottom() {
        text_fit(buf, inner.x + 1, under, &sums, inner.width.saturating_sub(1), th.muted());
    }
}

/// The whole box of an overlay from the inner area [`overlay`] returned: a click in it stays in it.
fn outer(inner: Rect) -> Rect {
    Rect { x: inner.x - 2, y: inner.y - 1, width: inner.width + 4, height: inner.height + 2 }
}

/// Everything of one transaction, for the overlay.
pub fn moved_detail(m: &Moved, zh: bool) -> (String, String) {
    let title = format!("{} · {}", m.kind(zh), Ts(m.at * 1000).format("%Y-%m-%d %H:%M:%S"));
    let w = |en: &'static str, cn: &'static str| if zh { cn } else { en };
    let body = format!(
        "| {} | {} |\n|---|---|\n| {} | {:+.9} SOL |\n| USDC | {:+.6} USDC |\n| {} | {:.9} SOL |\n| {} | {} |\n| {} | https://solscan.io/tx/{} |\n",
        w("Item", "项目"),
        w("In this transaction", "这笔交易"),
        w("SOL, its fee inside", "SOL 变化（含手续费）"),
        m.sol,
        m.usdc,
        w("Fee this wallet paid", "这个钱包付的手续费"),
        m.fee,
        w("Signature", "签名（链上的凭证）"),
        m.signature,
        w("Block explorer", "区块浏览器"),
        m.signature
    );
    (title, body)
}

/// The transfer being written, checked, confirmed or sent, over the page.
pub fn send_overlay(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, form: &SendForm) {
    let (th, zh) = (&app.theme, app.zh);
    let Some(view) = app.wallet.as_ref().map(|w| w.read()) else { return };
    let on = |st: Style| st.bg(th.select_bg);
    let w = 84.min(area.width.saturating_sub(2));
    let key_w = 12;
    let price = sol_usd(app, vm);
    match &view.sending {
        None | Some(Sending::Refused(..)) | Some(Sending::Checking(_)) => {
            let refused = match &view.sending {
                Some(Sending::Refused(_, why)) => wrap_words(why, w.saturating_sub(4)),
                _ => Vec::new(),
            };
            let checking = matches!(view.sending, Some(Sending::Checking(_)));
            let inner = overlay(buf, area, w, 9 + refused.len() as u16, t(zh, "Send", "转出"), th, &app.glyphs);
            app.hit(outer(inner), Hit::Overlay);
            let field = |buf: &mut Buffer, row: u16, i: usize, name: &str| {
                let st = if form.field == i && !checking { th.accent_bold() } else { th.muted() };
                text(buf, inner.x, inner.y + row, name, key_w, on(st));
            };
            // what
            field(buf, 1, 0, t(zh, "What", "币种"));
            let mut x = inner.x + key_w;
            for a in [Asset::Sol, Asset::Usdc] {
                let st = if a == form.asset {
                    th.text().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    on(th.faint())
                };
                x += text(buf, x, inner.y + 1, &format!(" {} ", a.name()), 8, st) + 2;
            }
            if form.field == 0 {
                text(buf, x + 1, inner.y + 1, t(zh, "←→ to change", "←→ 切换"), 20, on(th.faint()));
            }
            // to whom
            field(buf, 3, 1, t(zh, "To", "收款地址"));
            let cursor = |on_field: bool| if on_field && !checking { "▏" } else { "" };
            let to = if form.to.is_empty() && form.field != 1 {
                (t(zh, "a Solana address", "对方的 Solana 钱包地址").to_string(), on(th.faint()))
            } else {
                (format!("{}{}", form.to, cursor(form.field == 1)), on(th.text()))
            };
            text_fit(buf, inner.x + key_w, inner.y + 3, &to.0, inner.width - key_w, to.1);
            if form.field == 1 && !checking && !view.recipients.is_empty() {
                let known: Vec<String> = view
                    .recipients
                    .iter()
                    .take(3)
                    .map(|r| {
                        let day = Ts(r.last * 1000).format("%m-%d");
                        if zh {
                            format!("{}（{} 次 · {day}）", short(&r.address), r.times)
                        } else {
                            format!("{} ({}×, {day})", short(&r.address), r.times)
                        }
                    })
                    .collect();
                let hint = format!("{} {}", t(zh, "↑↓ sent to before:", "↑↓ 选转过的地址："), known.join("  "));
                text_fit(buf, inner.x + key_w, inner.y + 4, &hint, inner.width - key_w, on(th.faint()));
            }
            // how much
            field(buf, 5, 2, t(zh, "Amount", "数量"));
            let amount = format!("{}{} {}", form.amount, cursor(form.field == 2), form.asset.name());
            let aw = text(buf, inner.x + key_w, inner.y + 5, &amount, 30, on(th.text()));
            let most = spendable(app, &view, form.asset).map_or("—".to_string(), |v| amount_text(v, form.asset));
            let free = if zh {
                format!("可转出 {most}（m 填入全部）")
            } else {
                format!("{most} free to send (m fills it in)")
            };
            text_fit(
                buf,
                inner.x + key_w + aw + 3,
                inner.y + 5,
                &free,
                inner.width.saturating_sub(key_w + aw + 3),
                on(th.faint()),
            );
            for (i, line) in refused.iter().enumerate() {
                text(buf, inner.x, inner.y + 7 + i as u16, line, inner.width, on(th.warn()));
            }
            let hint = match (checking, zh) {
                (true, false) => "checking it on the chain… · Esc cancels",
                (true, true) => "正在链上核对… · Esc 取消",
                (false, false) => "Tab next · ⏎ check it · Esc cancel",
                (false, true) => "Tab 下一项 · ⏎ 核对 · Esc 取消",
            };
            overlay_hint(buf, inner, hint, th);
        }
        Some(Sending::Reviewed(r)) => {
            let unit = r.request.asset.name();
            let usd = match r.request.asset {
                Asset::Sol => price.map(|p| r.amount * p),
                Asset::Usdc => Some(r.amount),
            };
            let mut rows: Vec<(String, String, Style)> = vec![
                (
                    t(zh, "Send", "转出").into(),
                    format!(
                        "{} {unit}{}",
                        amount_text(r.amount, r.request.asset),
                        usd.map_or(String::new(), |u| format!("   ≈ {u:.2} USD"))
                    ),
                    th.text().add_modifier(Modifier::BOLD),
                ),
                (t(zh, "To", "收款地址").into(), in_fours(&r.to), th.text().add_modifier(Modifier::BOLD)),
                (t(zh, "Which is", "对方").into(), r.recipient.clone(), th.text()),
                (t(zh, "Network fee", "网络费").into(), format!("{:.6} SOL", r.fee), th.text()),
            ];
            if let Some(rent) = r.opens_account {
                let what = if zh {
                    format!("{rent:.6} SOL（对方还没有 USDC 账户，这是替它开户的链上押金）")
                } else {
                    format!("{rent:.6} SOL (they have no USDC account yet: the chain's deposit to open one)")
                };
                rows.push((t(zh, "Opens account", "开户押金").into(), what, th.warn()));
            }
            rows.push((
                t(zh, "Left after", "转出后剩").into(),
                format!("{:.6} SOL · {:.4} USDC", r.left_sol, r.left_usdc),
                th.text(),
            ));
            // what the bots hold is not the operator's to send without knowing it
            let mut notes = r.notes.clone();
            if let Some((names, sol, usdc)) = bots_hold(app) {
                let (left, held) = match r.request.asset {
                    Asset::Sol => (r.left_sol, sol),
                    Asset::Usdc => (r.left_usdc, usdc),
                };
                if held > 0.0 && left + 1e-9 < held {
                    notes.push(if zh {
                        format!("这会动用机器人 {names} 持有的 {unit}：它之后的买卖会失败。先在 Bots 页把它平仓更稳妥。")
                    } else {
                        format!("This takes {unit} the bot {names} holds: its next trade will fail. Close it on the Bots page first.")
                    });
                }
            }
            // a bot keeps its account by what the wallet's balances do: a transfer in the moment of its swap is read as part of it
            if let Some((names, pending)) = bots_run(app) {
                notes.push(match (pending, zh) {
                    (true, true) => format!(
                        "机器人 {names} 此刻正有一笔兑换在路上：等它核对完再转，否则这笔转账会被它算进自己的买卖。"
                    ),
                    (false, true) => format!(
                        "机器人 {names} 正在运行，它靠钱包余额的变化给自己的买卖记账：别在它下单的那一分钟（每根 K 线收盘时）转账。"
                    ),
                    (true, false) => format!(
                        "The bot {names} has a swap under way right now: wait until it is accounted for, or this transfer is counted as part of it."
                    ),
                    (false, false) => format!(
                        "The bot {names} is running and keeps its account by what the wallet's balances do: do not send in the minute it trades (when a bar closes)."
                    ),
                });
            }
            let notes: Vec<String> =
                notes.iter().flat_map(|n| wrap_words(&format!("! {n}"), w.saturating_sub(4))).collect();
            let wrapped: Vec<(String, Vec<String>, Style)> =
                rows.into_iter().map(|(k, v, st)| (k, wrap_words(&v, w.saturating_sub(4 + key_w)), st)).collect();
            let body: u16 = wrapped.iter().map(|r| r.1.len() as u16).sum();
            let h = body + notes.len() as u16 + 8;
            let inner = overlay(buf, area, w, h, t(zh, "Check this transfer", "核对这笔转出"), th, &app.glyphs);
            app.hit(outer(inner), Hit::Overlay);
            let mut y = inner.y + 1;
            for (k, lines, st) in &wrapped {
                text(buf, inner.x, y, k, key_w, on(th.muted()));
                for line in lines {
                    text(buf, inner.x + key_w, y, line, inner.width - key_w, on(*st));
                    y += 1;
                }
            }
            y += 1;
            for line in &notes {
                text(buf, inner.x, y, line, inner.width, on(th.warn()));
                y += 1;
            }
            let last = t(
                zh,
                "Once sent it cannot be taken back. Compare the address with where it should go, character by character.",
                "链上转账发出后无法撤回。请把收款地址和你要转去的地址逐位对一遍。",
            );
            for line in wrap_words(last, inner.width) {
                text(buf, inner.x, y, &line, inner.width, on(th.muted()));
                y += 1;
            }
            y += 1;
            let ask = if zh {
                format!("输入收款地址的最后 {CONFIRM} 位来确认：")
            } else {
                format!("Type the last {CONFIRM} characters of the address to confirm: ")
            };
            let ax = inner.x + text(buf, inner.x, y, &ask, inner.width, on(th.text()));
            let full = form.typed.chars().count() == CONFIRM;
            let right = form.typed == tail(&r.to);
            let st = match (full, right) {
                (true, true) => Style::new().fg(th.profit),
                (true, false) => Style::new().fg(th.loss),
                _ => th.accent(),
            };
            let tx = ax
                + text(
                    buf,
                    ax,
                    y,
                    &format!("{}▏", form.typed),
                    CONFIRM as u16 + 1,
                    on(st.add_modifier(Modifier::BOLD)),
                );
            if full && !right {
                text(buf, tx + 2, y, t(zh, "not the end of this address", "和这个地址的末尾不一致"), 30, on(th.warn()));
            }
            let hint = match (right, zh) {
                (true, false) => "⏎ send it · Esc back",
                (true, true) => "⏎ 发出 · Esc 返回修改",
                (false, false) => "Esc back",
                (false, true) => "Esc 返回修改",
            };
            overlay_hint(buf, inner, hint, th);
        }
        Some(Sending::Sending(r)) => {
            let inner = overlay(buf, area, w, 7, t(zh, "Sending", "正在发出"), th, &app.glyphs);
            app.hit(outer(inner), Hit::Overlay);
            let what =
                format!("{} {} → {}", amount_text(r.amount, r.request.asset), r.request.asset.name(), short(&r.to));
            text(buf, inner.x, inner.y + 1, &what, inner.width, on(th.text().add_modifier(Modifier::BOLD)));
            let wait = t(
                zh,
                "Signed and handed to the network; waiting for the chain to confirm it. It takes a few seconds, two minutes at the most. Leave this window open.",
                "已签名并交给网络，正在等链上确认。一般几秒，最多两分钟。请不要关闭窗口。",
            );
            for (i, line) in wrap_words(wait, inner.width).iter().enumerate().take(3) {
                text(buf, inner.x, inner.y + 3 + i as u16, line, inner.width, on(th.muted()));
            }
        }
        Some(Sending::Done { review: r, signature }) => {
            // (a signature has no spaces to break at: it is cut where the line ends)
            let lines = wrap_lines(signature, w.saturating_sub(4));
            let inner = overlay(buf, area, w, 8 + lines.len() as u16, t(zh, "Arrived", "已到账"), th, &app.glyphs);
            app.hit(outer(inner), Hit::Overlay);
            let what = if zh {
                format!(
                    "{} {} 已转到 {}，链上已确认。",
                    amount_text(r.amount, r.request.asset),
                    r.request.asset.name(),
                    short(&r.to)
                )
            } else {
                format!(
                    "{} {} went to {}: confirmed on the chain.",
                    amount_text(r.amount, r.request.asset),
                    r.request.asset.name(),
                    short(&r.to)
                )
            };
            text(
                buf,
                inner.x,
                inner.y + 1,
                &what,
                inner.width,
                on(Style::new().fg(th.profit).add_modifier(Modifier::BOLD)),
            );
            text(
                buf,
                inner.x,
                inner.y + 3,
                t(zh, "Signature (its receipt on the chain)", "签名（这笔转账在链上的凭证）"),
                inner.width,
                on(th.muted()),
            );
            for (i, line) in lines.iter().enumerate() {
                text(buf, inner.x, inner.y + 4 + i as u16, line, inner.width, on(th.text()));
            }
            overlay_hint(buf, inner, t(zh, "any key closes", "任意键关闭"), th);
        }
        Some(Sending::Failed { why, signature, .. }) => {
            let mut lines = wrap_words(why, w.saturating_sub(4));
            if let Some(s) = signature {
                lines.push(String::new());
                lines.push(t(zh, "Signature", "签名").to_string());
                lines.extend(wrap_lines(s, w.saturating_sub(4)));
            }
            let inner = overlay(buf, area, w, 4 + lines.len() as u16, t(zh, "Not sent", "没有转出"), th, &app.glyphs);
            app.hit(outer(inner), Hit::Overlay);
            for (i, line) in lines.iter().enumerate() {
                text(buf, inner.x, inner.y + 1 + i as u16, line, inner.width, on(th.warn()));
            }
            overlay_hint(buf, inner, t(zh, "any key closes", "任意键关闭"), th);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn review() -> Review {
        Review {
            request: SendRequest {
                asset: Asset::Sol,
                to: "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU".into(),
                amount: "0.05".into(),
            },
            to: "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU".into(),
            amount: 0.05,
            recipient: "a wallet that holds 1.2 SOL".into(),
            fee: 0.000007,
            opens_account: None,
            left_sol: 0.1,
            left_usdc: 2.0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn what_changed_says_what_it_was() {
        let m = |sol, usdc, fee, failed| Moved { at: 0, sol, usdc, fee, failed, signature: "s".into() };
        assert_eq!(m(-0.016703, 2.005052, 0.000007, false).kind(false), "swap");
        assert_eq!(m(0.2, 0.0, 0.0, false).kind(false), "received");
        assert_eq!(m(0.0, 5.0, 0.0, false).kind(true), "收到");
        assert_eq!(m(-0.050007, 0.0, 0.000007, false).kind(false), "sent");
        // USDC that left, with the SOL its recipient's account took: a sending, not a swap
        assert_eq!(m(-0.002046, -1.0, 0.000007, false).kind(true), "转出");
        assert_eq!(m(-0.000007, 0.0, 0.000007, false).kind(false), "fee only");
        assert_eq!(m(-0.000007, 0.0, 0.000007, true).kind(true), "失败");
    }

    #[test]
    fn an_address_takes_only_its_own_characters_and_an_amount_only_a_number() {
        let mut f = SendForm::new(Asset::Sol);
        let none = |_| None;
        for c in "7xKX 0OIl-tg2\n".chars() {
            f.on_key(key(if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) }), None, none);
        }
        assert_eq!((f.to.as_str(), f.field), ("7xKXtg2", 2), "base58 only; Enter goes on to the amount");
        for c in "1.2.5x".chars() {
            f.on_key(key(KeyCode::Char(c)), None, none);
        }
        assert_eq!(f.amount, "1.25");
        // `m` fills in the most there is to send, cut to the chain's unit
        f.on_key(key(KeyCode::Char('m')), None, |_| Some(0.1282209999));
        assert_eq!(f.amount, "0.128220999");
        let asked = f.on_key(key(KeyCode::Enter), None, none);
        assert_eq!(
            asked,
            Outcome::Act(WalletAction::Check(SendRequest {
                asset: Asset::Sol,
                to: "7xKXtg2".into(),
                amount: "0.128220999".into()
            }))
        );
        assert_eq!(f.on_key(key(KeyCode::Esc), None, none), Outcome::Close);
    }

    #[test]
    fn a_transfer_is_sent_only_after_the_end_of_its_address_is_typed() {
        let mut f = SendForm::new(Asset::Sol);
        let reviewed = Sending::Reviewed(review());
        let none = |_| None;
        assert_eq!(f.on_key(key(KeyCode::Enter), Some(&reviewed), none), Outcome::Stay, "nothing typed: nothing sent");
        for c in "gAsX".chars() {
            f.on_key(key(KeyCode::Char(c)), Some(&reviewed), none);
        }
        assert_eq!(f.on_key(key(KeyCode::Enter), Some(&reviewed), none), Outcome::Stay, "not its end: nothing sent");
        f.on_key(key(KeyCode::Backspace), Some(&reviewed), none);
        f.on_key(key(KeyCode::Char('U')), Some(&reviewed), none);
        assert_eq!(
            f.on_key(key(KeyCode::Enter), Some(&reviewed), none),
            Outcome::Act(WalletAction::Send(review().request))
        );
        // while it is being sent no key does anything; once it is over any key closes
        let sending = Sending::Sending(review());
        assert_eq!(f.on_key(key(KeyCode::Esc), Some(&sending), none), Outcome::Stay);
        let done = Sending::Done { review: review(), signature: "sig".into() };
        assert_eq!(f.on_key(key(KeyCode::Char('x')), Some(&done), none), Outcome::Close);
        // a review can be walked back from
        assert_eq!(f.on_key(key(KeyCode::Esc), Some(&reviewed), none), Outcome::Act(WalletAction::Clear));
    }

    #[test]
    fn an_address_sent_to_before_is_picked_instead_of_typed() {
        let mut f = SendForm::new(Asset::Usdc);
        let none = |_| None;
        // none known: the arrows move between the fields, as they did
        f.on_key(key(KeyCode::Down), None, none);
        assert_eq!((f.field, f.to.as_str()), (2, ""));
        f.field = 1;
        f.known = vec![
            "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU".into(),
            "9HjpU1e5QSjvyHyLz95YfnJxzJoxhjDwH9Pw7LXnFZv5".into(),
        ];
        f.on_key(key(KeyCode::Down), None, none);
        assert_eq!((f.field, f.to.as_str(), f.pick), (1, "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU", Some(0)));
        f.on_key(key(KeyCode::Down), None, none);
        assert_eq!(f.to, "9HjpU1e5QSjvyHyLz95YfnJxzJoxhjDwH9Pw7LXnFZv5");
        f.on_key(key(KeyCode::Up), None, none);
        assert_eq!(f.pick, Some(0));
        // changed by hand it is no longer the one that was picked; Tab still goes on
        f.on_key(key(KeyCode::Backspace), None, none);
        assert_eq!((f.pick, f.to.len()), (None, 43));
        f.on_key(key(KeyCode::Tab), None, none);
        assert_eq!(f.field, 2);
    }

    #[test]
    fn amounts_and_addresses_are_written_to_be_compared() {
        assert_eq!(amount_text(0.05, Asset::Sol), "0.05");
        assert_eq!(amount_text(2.005052, Asset::Usdc), "2.005052");
        assert_eq!(amount_text(3.0, Asset::Usdc), "3");
        assert_eq!(tail("7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU"), "gAsU");
        assert_eq!(in_fours("7xKXtg2CW8"), "7xKX tg2C W8");
        assert_eq!(short("7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU"), "7xKX…gAsU");
    }
}
