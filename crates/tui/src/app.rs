//! UI state, keyboard and mouse handling. Produces engine `Command`s only
//! for the kill switch, confirmations and quit; everything else is local view
//! state. The mouse drives the same state as the keys.

use crate::bots::{BotAction, BotPort, Bots, BudgetForm, NewBotForm};
use crate::cex::{BARS, Cex, OkxSource};
use crate::hub::ViewModel;
use crate::markets::{BottomTab, SideTab};
use crate::theme::{Glyphs, Theme};
use crate::timeline::Timeline;
use crate::wallet::{Asset, Outcome, SendForm, Wallet, WalletAction, WalletPort};
use crate::workspace::{AddError, ChartStyle, LineStyle, Workspace};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use searcher_core::Ts;
use searcher_core::config::{ColorMode, GlyphMode};
use searcher_core::event::Command;
use searcher_core::metrics::MetricId;
use searcher_core::model::{OppStatus, Opportunity, OpportunityId};
use std::cell::{Cell, RefCell};
use std::time::Instant;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Overview,
    Markets,
    Opportunities,
    Graphs,
    Trades,
    Risk,
    System,
    Logs,
    /// Rules that hold a position (buy low, sell high), paper and real.
    Bots,
    /// What the wallet holds, receiving and sending.
    Wallet,
}

impl Page {
    pub const ALL: [Page; 10] = [
        Page::Overview,
        Page::Markets,
        Page::Opportunities,
        Page::Graphs,
        Page::Trades,
        Page::Risk,
        Page::System,
        Page::Logs,
        Page::Bots,
        Page::Wallet,
    ];

    /// The key that goes to the page: `1`–`9`, then `0`.
    pub fn key(self) -> char {
        let i = Page::ALL.iter().position(|p| *p == self).unwrap_or(0);
        char::from_digit((i as u32 + 1) % 10, 10).unwrap_or('1')
    }

    pub fn label(self) -> &'static str {
        match self {
            Page::Overview => "Overview",
            Page::Markets => "Markets",
            Page::Opportunities => "Opportunities",
            Page::Graphs => "Graphs",
            Page::Trades => "Trades",
            Page::Risk => "Risk",
            Page::System => "System",
            Page::Logs => "Logs",
            Page::Bots => "Bots",
            Page::Wallet => "Wallet",
        }
    }

    /// The page's name for an operator who reads Chinese.
    pub fn label_zh(self) -> &'static str {
        match self {
            Page::Overview => "总览",
            Page::Markets => "行情",
            Page::Opportunities => "机会",
            Page::Graphs => "图表",
            Page::Trades => "成交",
            Page::Risk => "风控",
            Page::System => "系统",
            Page::Logs => "日志",
            Page::Bots => "机器人",
            Page::Wallet => "钱包",
        }
    }

    /// The opportunity list is on this page (its keys act on something visible).
    pub fn lists_opps(self) -> bool {
        matches!(self, Page::Overview | Page::Opportunities)
    }

    /// The event stream (Overview) or the log (Logs) is on this page.
    pub fn has_stream(self) -> bool {
        matches!(self, Page::Overview | Page::Logs)
    }

    /// Focus regions available on this page, in Tab order.
    pub fn focuses(self) -> &'static [Focus] {
        match self {
            Page::Overview => &[Focus::Opportunities, Focus::Graphs, Focus::Inspector, Focus::Stream],
            Page::Opportunities => &[Focus::Opportunities, Focus::Inspector],
            Page::Graphs | Page::Markets => &[Focus::Graphs],
            Page::Trades => &[Focus::Opportunities, Focus::Graphs],
            Page::Logs => &[Focus::Stream],
            Page::Risk | Page::System | Page::Bots | Page::Wallet => &[Focus::Stream],
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Opportunities,
    Graphs,
    Inspector,
    Stream,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OppFilter {
    All,
    GrossPositive,
    Executable,
    Skipped,
}

impl OppFilter {
    pub fn label(self) -> &'static str {
        match self {
            OppFilter::All => "all",
            OppFilter::GrossPositive => "gross>0",
            OppFilter::Executable => "executable",
            OppFilter::Skipped => "skipped",
        }
    }

    pub fn next(self) -> OppFilter {
        match self {
            OppFilter::All => OppFilter::GrossPositive,
            OppFilter::GrossPositive => OppFilter::Executable,
            OppFilter::Executable => OppFilter::Skipped,
            OppFilter::Skipped => OppFilter::All,
        }
    }

    pub fn keep(self, o: &Opportunity) -> bool {
        match self {
            OppFilter::All => true,
            OppFilter::GrossPositive => o.eval.gross_pnl > 0,
            OppFilter::Executable => {
                matches!(
                    o.status,
                    OppStatus::Executable | OppStatus::PaperFilled | OppStatus::Landed | OppStatus::Submitted
                )
            }
            OppFilter::Skipped => o.status.skip().is_some(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub glyphs: GlyphMode,
    pub color: ColorMode,
    pub fps: u16,
    pub max_graphs: usize,
    /// Graphs registered at start (see [`default_graphs`]).
    pub graphs: Vec<MetricId>,
    /// Capture the mouse (clicks, wheel). Text selection then needs the
    /// terminal's modifier (usually Shift or Option while dragging).
    pub mouse: bool,
    /// OKX market data for the Markets page (None: on-chain data only).
    pub okx: Option<OkxSource>,
    /// The application's bots, for the Bots page (None: the page says so).
    pub bots: Option<BotPort>,
    /// The application's wallet, for the Wallet page (None: the page says so).
    pub wallet: Option<WalletPort>,
    /// The operator reads Chinese: the pages about their own money are in it.
    pub zh: bool,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            glyphs: GlyphMode::Auto,
            color: ColorMode::Auto,
            fps: 15,
            max_graphs: 6,
            graphs: LEGACY_GRAPHS.to_vec(),
            mouse: true,
            okx: Some(OkxSource::default()),
            bots: None,
            wallet: None,
            zh: false,
        }
    }
}

/// Graphs of sessions recorded before the on-chain feeds existed.
const LEGACY_GRAPHS: [MetricId; 4] = [MetricId::Price, MetricId::NetEdge, MetricId::JupiterLatency, MetricId::Spread];

/// Start-up graphs. Live with on-chain feeds: the live pool mid and spread
/// first. Replay (`vm` given): the preferred metrics the session actually
/// has, topped up with the legacy set.
pub fn default_graphs(vm: Option<&ViewModel>, live_feeds: bool) -> Vec<MetricId> {
    const PREFERRED: [MetricId; 4] = [MetricId::PoolMid, MetricId::PoolSpread, MetricId::NetEdge, MetricId::Price];
    match vm {
        None if live_feeds => PREFERRED.to_vec(),
        None => LEGACY_GRAPHS.to_vec(),
        Some(vm) => {
            let mut v: Vec<MetricId> =
                PREFERRED.into_iter().filter(|m| vm.series(*m).is_some_and(|s| !s.is_empty())).collect();
            for m in LEGACY_GRAPHS {
                if v.len() < 4 && !v.contains(&m) {
                    v.push(m);
                }
            }
            v
        }
    }
}

/// A clickable region, registered while rendering (the last one registered at
/// a cell is the one on top).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Page(Page),
    Panel(Focus),
    Opp(OpportunityId),
    /// Event-stream line, as its offset from the newest.
    Stream(usize),
    /// Logs-page line, as its offset from the newest.
    Log(usize),
    /// A chart: `plot` maps x to time over `t0..t1`; `index` is its slot in the
    /// graph workspace (None for fixed charts).
    Graph {
        index: Option<usize>,
        metric: Option<MetricId>,
        plot: Rect,
        t0: Ts,
        t1: Ts,
    },
    /// A row of the samples panel.
    Sample(Ts),
    PickerItem(usize),
    /// Inside an overlay (clicks there do not fall through).
    Overlay,
    Help,
    Kill,
    /// Markets page: a candle (its start), a bar, the pair, a tab.
    Candle(Ts),
    MkBar(usize),
    MkPair,
    MkSide(SideTab),
    MkTab(BottomTab),
    /// Bots page: a bot of the list, a bar of its chart.
    Bot(usize),
    BotBar(usize),
    /// Wallet page: a transaction of the list.
    WalletRow(usize),
}

/// Overlay showing the full detail of a stream/log line.
#[derive(Clone, Debug)]
pub struct Detail {
    pub title: String,
    pub body: String,
}

pub struct App {
    pub page: Page,
    pub focus: Focus,
    pub theme: Theme,
    pub glyphs: Glyphs,
    pub workspace: Workspace,
    pub timeline: Timeline,
    pub line_style: LineStyle,
    pub market_style: ChartStyle,
    pub opp_filter: OppFilter,
    /// `None` = follow newest.
    pub opp_selected: Option<OpportunityId>,
    /// Lines from the end in the event stream (0 = newest, follow).
    pub stream_offset: usize,
    pub log_offset: usize,
    pub inspector_scroll: u16,
    pub detail: Option<Detail>,
    /// First line shown of a detail longer than its overlay.
    pub detail_scroll: u16,
    pub picker: Option<usize>,
    pub help: bool,
    pub kill_release_prompt: bool,
    /// Threshold panel (`T`), when open.
    pub thresholds: Option<crate::thresholds::Panel>,
    pub status: Option<(String, Instant)>,
    pub quit: bool,
    /// The logo as a real image when the terminal has a graphics protocol
    /// (set by the terminal loop; `None` = block-art logo).
    pub logo_image: Option<ratatui_image::protocol::Protocol>,
    /// Clickable regions of the last frame (filled by the renderer).
    pub hits: RefCell<Vec<(Rect, Hit)>>,
    last_click: Option<(Instant, u16, u16)>,
    /// Graph being scrubbed with the left button held.
    dragging: Option<Hit>,
    /// Markets page: OKX pair and candle bar (indices into the source's markets /
    /// `cex::BARS`), right panel and bottom tab.
    pub mk_pair: usize,
    pub mk_bar: usize,
    pub mk_side: SideTab,
    pub mk_tab: BottomTab,
    /// OKX poller (live sessions only; set by the terminal loop).
    pub cex: Option<Cex>,
    /// Oldest / newest candle start of the last Markets frame.
    pub kline_span: Cell<Option<(Ts, Ts)>>,
    /// The application's bots (as they were at start; the terminal loop keeps them fresh).
    pub bots: Option<Bots>,
    pub bot_selected: usize,
    /// The action waiting for its `y`, and the bot it is for.
    pub bot_prompt: Option<(String, BotAction)>,
    /// The budget being written for a bot (`b`), before it is asked about.
    pub bot_budget: Option<BudgetForm>,
    /// A new bot being written (`n`).
    pub bot_new: Option<NewBotForm>,
    /// The bot's chart as a line of closes instead of candles (`v`).
    pub bot_line: bool,
    /// Paper experiments are listed beside the real bots (`p`); they are not, until asked for.
    pub bot_paper: bool,
    /// The bar its chart is looked at in (`[` `]`), an index into `cex::BARS`; `None`: the rule's own.
    pub bot_bar: Option<usize>,
    /// The application's wallet (as it was at start; the terminal loop keeps it fresh).
    pub wallet: Option<Wallet>,
    pub wallet_selected: usize,
    /// The transfer being written, when one is.
    pub wallet_form: Option<SendForm>,
    /// The pages about the operator's own money are in Chinese.
    pub zh: bool,
}

impl App {
    pub fn new(opts: &TuiOptions) -> App {
        App {
            page: Page::Overview,
            focus: Focus::Opportunities,
            theme: Theme::detect(opts.color),
            glyphs: Glyphs::detect(opts.glyphs),
            workspace: Workspace::new(opts.max_graphs, &opts.graphs),
            timeline: Timeline::default(),
            line_style: LineStyle::Box,
            market_style: ChartStyle::Line,
            opp_filter: OppFilter::All,
            opp_selected: None,
            stream_offset: 0,
            log_offset: 0,
            inspector_scroll: 0,
            detail: None,
            detail_scroll: 0,
            picker: None,
            help: false,
            kill_release_prompt: false,
            thresholds: None,
            status: None,
            quit: false,
            logo_image: None,
            hits: RefCell::new(Vec::new()),
            last_click: None,
            dragging: None,
            mk_pair: 0,
            mk_bar: 1,
            mk_side: SideTab::default(),
            mk_tab: BottomTab::default(),
            cex: None,
            kline_span: Cell::new(None),
            bots: opts.bots.as_ref().map(|p| Bots::fixed((p.view)())),
            bot_selected: 0,
            bot_prompt: None,
            bot_budget: None,
            bot_new: None,
            bot_line: false,
            bot_paper: false,
            bot_bar: None,
            wallet: opts.wallet.as_ref().map(|p| Wallet::fixed((p.view)())),
            wallet_selected: 0,
            wallet_form: None,
            zh: opts.zh,
        }
    }

    /// Register a clickable region for the frame being drawn.
    pub fn hit(&self, r: Rect, h: Hit) {
        if r.width > 0 && r.height > 0 {
            self.hits.borrow_mut().push((r, h));
        }
    }

    fn hit_at(&self, x: u16, y: u16) -> Option<Hit> {
        let p = ratatui::layout::Position { x, y };
        self.hits.borrow().iter().rev().find(|(r, _)| r.contains(p)).map(|(_, h)| *h)
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().filter(|(_, t)| t.elapsed().as_secs() < 4).map(|(s, _)| s.as_str())
    }

    pub fn filtered_opps<'a>(&self, vm: &'a ViewModel) -> Vec<&'a Opportunity> {
        vm.opps_newest().filter(|o| self.opp_filter.keep(o)).collect()
    }

    /// The opportunity shown in the inspector.
    pub fn selected_opp<'a>(&self, vm: &'a ViewModel) -> Option<&'a Opportunity> {
        match self.opp_selected {
            Some(id) => vm.opps.get(&id),
            None => vm.opps_newest().find(|o| self.opp_filter.keep(o)),
        }
    }

    fn session_bounds(vm: &ViewModel) -> (searcher_core::Ts, searcher_core::Ts) {
        let last = vm.last_ts.unwrap_or_else(searcher_core::Ts::now);
        (vm.first_ts.unwrap_or(last), last)
    }

    pub fn active_series<'a>(&self, vm: &'a ViewModel) -> Option<&'a searcher_core::series::TimeSeries> {
        self.workspace.active_metric().and_then(|m| vm.series(m))
    }

    fn move_opp(&mut self, vm: &ViewModel, delta: i64) {
        let list = self.filtered_opps(vm);
        if list.is_empty() {
            return;
        }
        let cur = self.opp_selected.and_then(|id| list.iter().position(|o| o.id == id)).unwrap_or(0) as i64;
        let ni = (cur + delta).clamp(0, list.len() as i64 - 1) as usize;
        let o = list[ni];
        self.select_opp(vm, o, !(ni == 0 && delta < 0));
    }

    /// Select `o` (`pin = false` follows the newest again) and move the shared
    /// cursor to its detection time so every graph shows the market around it.
    fn select_opp(&mut self, vm: &ViewModel, o: &Opportunity, pin: bool) {
        self.opp_selected = pin.then_some(o.id);
        self.inspector_scroll = 0;
        let (first, latest) = Self::session_bounds(vm);
        if let Some(s) = vm.series(MetricId::Price).or_else(|| self.active_series(vm))
            && let Some((t, _)) = s.nearest(o.detected_at)
        {
            self.timeline.cursor = Some(t);
            if let Some(span) = self.timeline.tf.span_us() {
                self.timeline.follow = false;
                self.timeline.right = searcher_core::Ts((t.0 + span / 3).min(latest.0.max(t.0)));
            } else {
                self.timeline.follow = false;
                self.timeline.right = latest.max(first);
            }
        }
    }

    /// Handle one key. Returns engine commands (kill switch, confirm, quit).
    pub fn on_key(&mut self, k: KeyEvent, vm: &ViewModel) -> Vec<Command> {
        let mut out = Vec::new();
        let (first, latest) = Self::session_bounds(vm);

        // Modal overlays first.
        if let Some(mut form) = self.wallet_form.take() {
            let Some(wallet) = &self.wallet else { return out };
            let done = {
                let view = wallet.read();
                form.on_key(k, view.sending.as_ref(), |a| crate::wallet::spendable(self, &view, a))
            };
            match done {
                Outcome::Stay => self.wallet_form = Some(form),
                Outcome::Act(action) => {
                    wallet.act(action);
                    self.wallet_form = Some(form);
                }
                Outcome::Close => wallet.act(WalletAction::Clear),
            }
            return out;
        }
        if let Some(mut form) = self.bot_new.take() {
            if k.code == KeyCode::Esc {
                return out;
            }
            if form.on_key(k) {
                // ⏎ on the last field: made if it can be, else said why and left to be corrected
                let made = form.spec(self.zh).and_then(|spec| {
                    let said =
                        self.bots.as_ref().map_or(Err("not available in this view".to_string()), |b| b.create(&spec));
                    said.map(|said| (said, spec.name))
                });
                match made {
                    Ok((said, name)) => {
                        // the one just made is the one selected: s starts it
                        let at = self
                            .bots
                            .as_ref()
                            .and_then(|b| b.read().bots.iter().position(|b| b.real && b.name == name));
                        self.bot_selected = at.unwrap_or(self.bot_selected);
                        self.sync_stream();
                        self.flash(said);
                    }
                    Err(why) => {
                        form.error = Some(why);
                        self.bot_new = Some(form);
                    }
                }
            } else {
                self.bot_new = Some(form);
            }
            return out;
        }
        if let Some(mut form) = self.bot_budget.take() {
            match k.code {
                KeyCode::Esc => {}
                // a number that is a budget, and another than it has (or one that takes a wish back), goes on
                // to the question; anything else stays to be corrected
                KeyCode::Enter => {
                    let bot = self.bots.as_ref().and_then(|b| b.read().bots.iter().find(|b| b.id == form.id).cloned());
                    match (form.cents(), bot) {
                        (Some(cents), Some(b))
                            if b.wish.is_some() || (f64::from(cents) / 100.0 - b.budget).abs() >= 0.005 =>
                        {
                            self.ask_bot_of(&form.id, BotAction::Budget { cents })
                        }
                        _ => self.bot_budget = Some(form),
                    }
                }
                KeyCode::Backspace => {
                    form.text.pop();
                    self.bot_budget = Some(form);
                }
                KeyCode::Char(c) => {
                    form.type_in(c);
                    self.bot_budget = Some(form);
                }
                _ => self.bot_budget = Some(form),
            }
            return out;
        }
        if let Some((id, action)) = self.bot_prompt.take() {
            if matches!(k.code, KeyCode::Char('y')) {
                match self.bots.as_ref().map(|b| b.act(&id, action)) {
                    Some(Ok(done)) => self.flash(done),
                    Some(Err(why)) if self.zh => self.flash(format!("没有执行：{why}")),
                    Some(Err(why)) => self.flash(format!("not done: {why}")),
                    None => {}
                }
            }
            return out;
        }
        if self.kill_release_prompt {
            if matches!(k.code, KeyCode::Char('y')) {
                out.push(Command::KillSwitch { engage: false, reason: "released by operator".into() });
                self.flash("kill switch released");
            }
            self.kill_release_prompt = false;
            return out;
        }
        if let Some(p) = &mut self.thresholds {
            use crate::thresholds::Outcome;
            match p.on_key(k, vm) {
                Outcome::Stay => {}
                Outcome::Close(msg) => {
                    self.thresholds = None;
                    if let Some(m) = msg {
                        self.flash(m);
                    }
                }
                Outcome::Send(cmd, msg) => {
                    self.thresholds = None;
                    out.push(cmd);
                    self.flash(msg);
                }
            }
            return out;
        }
        if let Some(sel) = self.picker {
            let n = MetricId::ALL.len();
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('+') => self.picker = None,
                KeyCode::Down | KeyCode::Char('j') => self.picker = Some((sel + 1) % n),
                KeyCode::Up | KeyCode::Char('k') => self.picker = Some((sel + n - 1) % n),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let m = MetricId::ALL[sel];
                    if self.workspace.contains(m) {
                        let idx = self.workspace.entries.iter().position(|e| e.metric == m).unwrap();
                        self.workspace.active = idx;
                        self.workspace.remove_active();
                        self.flash(format!("removed {}", m.label()));
                    } else {
                        match self.workspace.add(m) {
                            Ok(_) => self.flash(format!("added {}", m.label())),
                            Err(AddError::Limit(n)) => self.flash(format!("graph limit reached ({n})")),
                            Err(AddError::Duplicate) => {}
                        }
                    }
                }
                _ => {}
            }
            return out;
        }
        if self.detail.is_some() {
            // the renderer clamps the scroll to the text
            match k.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.detail = None,
                KeyCode::Down | KeyCode::Char('j') => self.detail_scroll = self.detail_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                KeyCode::Home => self.detail_scroll = 0,
                _ => {}
            }
            return out;
        }
        if self.help {
            self.help = false;
            return out;
        }

        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            // ── global ──
            KeyCode::Char('q') => {
                self.quit = true;
                out.push(Command::Shutdown);
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
                out.push(Command::Shutdown);
            }
            KeyCode::Char('K') => {
                if vm.kill_engaged() {
                    self.kill_release_prompt = true;
                } else {
                    out.push(Command::KillSwitch { engage: true, reason: "operator (K)".into() });
                    self.flash("KILL SWITCH ENGAGED — no new trades");
                }
            }
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('T') if vm.replay => self.flash("thresholds can be changed in a live session only"),
            KeyCode::Char('T') => self.thresholds = Some(crate::thresholds::Panel::default()),
            KeyCode::Char(c @ '0'..='9') => {
                if let Some(p) = Page::ALL.iter().find(|p| p.key() == c) {
                    self.goto(*p)
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let f = self.page.focuses();
                let i = f.iter().position(|x| *x == self.focus).unwrap_or(0);
                let n = f.len();
                let back = k.code == KeyCode::BackTab || shift;
                self.focus = f[if back { (i + n - 1) % n } else { (i + 1) % n }];
            }
            KeyCode::Char('y') | KeyCode::Char('n') if !vm.pending_confirm.is_empty() => {
                let id = vm.pending_confirm[0];
                let approve = k.code == KeyCode::Char('y');
                out.push(Command::Confirm { opportunity: id, approve });
                self.flash(format!("{} {id}", if approve { "approved" } else { "declined" }));
            }
            // ── Bots page ──
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Up | KeyCode::Char('k') if self.page == Page::Bots => {
                let n = self.bots.as_ref().map_or(0, |b| crate::bots::shown(self, &b.read()));
                let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
                if n > 0 {
                    self.bot_selected =
                        if down { (self.bot_selected + 1).min(n - 1) } else { self.bot_selected.saturating_sub(1) };
                }
                self.sync_stream();
            }
            KeyCode::Char('v') if self.page == Page::Bots => self.bot_line = !self.bot_line,
            KeyCode::Char('n') if self.page == Page::Bots && vm.pending_confirm.is_empty() => {
                self.bot_new = Some(NewBotForm::default())
            }
            KeyCode::Char('p') if self.page == Page::Bots => {
                self.bot_paper = !self.bot_paper;
                let n = self.bots.as_ref().map_or(0, |b| crate::bots::shown(self, &b.read()));
                self.bot_selected = self.bot_selected.min(n.saturating_sub(1));
                self.sync_stream();
                self.flash(match (self.bot_paper, self.zh) {
                    (true, true) => "纸面实验已显示：它们是模拟账户，不是真钱",
                    (false, true) => "纸面实验已隐藏",
                    (true, false) => "paper experiments shown: simulated accounts, no money",
                    (false, false) => "paper experiments hidden",
                });
            }
            KeyCode::Char('[') | KeyCode::Char(']') if self.page == Page::Bots => {
                let shown = self.bots.as_ref().and_then(|b| {
                    b.read().bots.get(self.bot_selected).and_then(|b| crate::bots::shown_stream(self, b))
                });
                if let Some((_, bar)) = shown {
                    let d = if k.code == KeyCode::Char('[') { -1 } else { 1 };
                    self.bot_bar = Some((bar as i64 + d).clamp(0, BARS.len() as i64 - 1) as usize);
                    self.sync_stream();
                }
            }
            KeyCode::Char('b') if self.page == Page::Bots => {
                let Some(b) = self.bots.as_ref().and_then(|b| b.read().bots.get(self.bot_selected).cloned()) else {
                    return out;
                };
                match b.can(BotAction::Budget { cents: 0 }, self.zh) {
                    Ok(()) => self.bot_budget = Some(BudgetForm { id: b.id, text: String::new() }),
                    Err(why) => self.flash(why),
                }
            }
            KeyCode::Char('s') | KeyCode::Char('x') | KeyCode::Char('c') | KeyCode::Char('t')
                if self.page == Page::Bots =>
            {
                let action = match k.code {
                    KeyCode::Char('s') => BotAction::Start,
                    KeyCode::Char('x') => BotAction::Stop,
                    KeyCode::Char('t') => BotAction::Mode,
                    _ => BotAction::Close,
                };
                self.ask_bot(action);
            }
            KeyCode::Enter if self.page == Page::Bots => {
                let selected = self.bots.as_ref().and_then(|b| b.read().bots.get(self.bot_selected).cloned());
                if let Some(b) = selected.filter(|b| !b.journal.is_empty()) {
                    let title =
                        if self.zh { format!("{} 做过什么", b.name) } else { format!("What {} did", b.name) };
                    self.detail = Some(Detail { title, body: crate::bots::journal_doc(&b, self.zh) });
                    self.detail_scroll = u16::MAX; // the renderer clamps it: the newest lines
                }
            }
            // ── Wallet page ──
            KeyCode::Char('s') | KeyCode::Char('u') if self.page == Page::Wallet => {
                let asset = if k.code == KeyCode::Char('s') { Asset::Sol } else { Asset::Usdc };
                match self.wallet.as_ref().map(|w| w.read().cannot_send.clone()) {
                    Some(None) => {
                        let known = self
                            .wallet
                            .as_ref()
                            .map(|w| w.read().recipients.iter().map(|r| r.address.clone()).collect());
                        self.wallet_form = Some(SendForm { known: known.unwrap_or_default(), ..SendForm::new(asset) })
                    }
                    Some(Some(why)) => self.flash(why),
                    None => {}
                }
            }
            KeyCode::Char('r') if self.page == Page::Wallet => {
                if let Some(w) = &self.wallet {
                    w.act(WalletAction::Refresh);
                    self.flash(if self.zh { "正在重新读取钱包" } else { "reading the wallet again" });
                }
            }
            KeyCode::Char('c') if self.page == Page::Wallet => {
                if let Some(address) = self.wallet.as_ref().and_then(|w| w.read().address.clone()) {
                    copy(&address);
                    self.flash(if self.zh {
                        "地址已复制（终端不支持的话请手动选中复制）"
                    } else {
                        "address copied (select it by hand if the terminal did not take it)"
                    });
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Up | KeyCode::Char('k') if self.page == Page::Wallet => {
                let n = self.wallet.as_ref().map_or(0, |w| w.read().moved.len());
                let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j'));
                if n > 0 {
                    self.wallet_selected = if down {
                        (self.wallet_selected + 1).min(n - 1)
                    } else {
                        self.wallet_selected.saturating_sub(1)
                    };
                }
            }
            KeyCode::Enter if self.page == Page::Wallet => {
                if let Some(m) = self.wallet.as_ref().and_then(|w| w.read().moved.get(self.wallet_selected).cloned()) {
                    let (title, body) = crate::wallet::moved_detail(&m, self.zh);
                    self.detail = Some(Detail { title, body });
                    self.detail_scroll = 0;
                }
            }
            // ── Markets page ──
            KeyCode::Char('[') | KeyCode::Char(']') if self.page == Page::Markets => {
                let d = if k.code == KeyCode::Char('[') { -1 } else { 1 };
                self.set_market((self.mk_pair, (self.mk_bar as i64 + d).clamp(0, BARS.len() as i64 - 1) as usize));
            }
            KeyCode::Char('p') if self.page == Page::Markets => {
                let n = self.cex.as_ref().map_or(0, |c| c.source.markets.len());
                if n > 0 {
                    self.set_market(((self.mk_pair + 1) % n, self.mk_bar));
                    let name = self.market().unwrap_or_default().replace('-', "/");
                    self.flash(format!("pair {name}"));
                }
            }
            KeyCode::Char('t') if self.page == Page::Markets => {
                self.mk_side = match self.mk_side {
                    SideTab::QuoteBook => SideTab::LastTrades,
                    SideTab::LastTrades => SideTab::QuoteBook,
                }
            }
            KeyCode::Char('o') if self.page == Page::Markets => self.mk_tab = self.mk_tab.next(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') if self.page == Page::Markets => {
                let back = matches!(k.code, KeyCode::Left | KeyCode::Char('h'));
                let step = if shift { 10 } else { 1 };
                self.step_candle(if back { -step } else { step });
            }
            // ── shared timeline (works from any page) ──
            KeyCode::Char('a') => {
                let t = self.timeline.cursor_ts(self.active_series(vm));
                self.timeline.mark_a(t);
                self.flash(t.map(|t| format!("A @ {}", t.hms_millis())).unwrap_or_else(|| "no sample to mark".into()));
            }
            KeyCode::Char('b') => {
                let t = self.timeline.cursor_ts(self.active_series(vm));
                self.timeline.mark_b(t);
                self.flash(t.map(|t| format!("B @ {}", t.hms_millis())).unwrap_or_else(|| "no sample to mark".into()));
            }
            KeyCode::Char('x') => {
                self.timeline.clear_marks();
                self.flash("A/B cleared");
            }
            KeyCode::Char('[') => self.timeline.tf = self.timeline.tf.narrower(),
            KeyCode::Char(']') => self.timeline.tf = self.timeline.tf.wider(),
            KeyCode::End | KeyCode::Char('G') => {
                self.timeline.end();
                self.opp_selected = None;
                self.stream_offset = 0;
                self.log_offset = 0;
            }
            KeyCode::Home => {
                if let Some(s) = self.active_series(vm) {
                    self.timeline.home(s, first, latest)
                }
            }
            KeyCode::Char('s') => {
                self.line_style = match self.line_style {
                    LineStyle::Box => LineStyle::Braille,
                    LineStyle::Braille => LineStyle::Box,
                };
            }
            KeyCode::Char('c') => {
                if self.page == Page::Markets {
                    self.market_style = match self.market_style {
                        ChartStyle::Line => ChartStyle::Candle,
                        ChartStyle::Candle => ChartStyle::Line,
                    };
                } else {
                    self.workspace.toggle_style();
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.picker = Some(0),
            KeyCode::Char('f') if self.focus == Focus::Opportunities && self.page.lists_opps() => {
                self.opp_filter = self.opp_filter.next();
                self.opp_selected = None;
            }
            KeyCode::Esc => {
                self.opp_selected = None;
                self.timeline.end();
            }
            _ => self.on_focus_key(k, vm, first, latest),
        }
        out
    }

    fn on_focus_key(&mut self, k: KeyEvent, vm: &ViewModel, first: searcher_core::Ts, latest: searcher_core::Ts) {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let step = if shift { 10 } else { 1 };
        match (self.focus, k.code) {
            (_, KeyCode::Left | KeyCode::Char('h')) if alt => self.timeline.pan(-1, first, latest),
            (_, KeyCode::Right | KeyCode::Char('l')) if alt => self.timeline.pan(1, first, latest),
            (_, KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H')) => {
                if let Some(s) = self.active_series(vm) {
                    self.timeline.step(s, -step, first, latest)
                }
            }
            (_, KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L')) => {
                if let Some(s) = self.active_series(vm) {
                    self.timeline.step(s, step, first, latest)
                }
            }
            // a list that is not on the page is not moved from it
            (Focus::Opportunities, _) if !self.page.lists_opps() => {}
            (Focus::Stream, _) if !self.page.has_stream() => {}
            (Focus::Opportunities, KeyCode::Down | KeyCode::Char('j')) => self.move_opp(vm, 1),
            (Focus::Opportunities, KeyCode::Up | KeyCode::Char('k')) => self.move_opp(vm, -1),
            (Focus::Opportunities, KeyCode::PageDown) => self.move_opp(vm, 10),
            (Focus::Opportunities, KeyCode::PageUp) => self.move_opp(vm, -10),
            (Focus::Opportunities, KeyCode::Enter) => {
                if self.page == Page::Overview || self.page == Page::Opportunities {
                    self.focus = Focus::Inspector;
                }
            }
            (Focus::Inspector, KeyCode::Down | KeyCode::Char('j')) => {
                self.inspector_scroll = self.inspector_scroll.saturating_add(1)
            }
            (Focus::Inspector, KeyCode::Up | KeyCode::Char('k')) => {
                self.inspector_scroll = self.inspector_scroll.saturating_sub(1)
            }
            (Focus::Inspector, KeyCode::Enter) => self.focus = Focus::Opportunities,
            (Focus::Graphs, KeyCode::Down | KeyCode::Char('j')) if shift => self.workspace.move_active(1),
            (Focus::Graphs, KeyCode::Up | KeyCode::Char('k')) if shift => self.workspace.move_active(-1),
            (Focus::Graphs, KeyCode::Down | KeyCode::Char('j')) => self.workspace.select(1),
            (Focus::Graphs, KeyCode::Up | KeyCode::Char('k')) => self.workspace.select(-1),
            (Focus::Graphs, KeyCode::Char('-') | KeyCode::Delete | KeyCode::Char('d')) => {
                if let Some(e) = self.workspace.remove_active() {
                    self.flash(format!("removed {}", e.metric.label()));
                }
            }
            (Focus::Stream, KeyCode::Up | KeyCode::Char('k')) => self.scroll_stream(1),
            (Focus::Stream, KeyCode::Down | KeyCode::Char('j')) => self.scroll_stream(-1),
            (Focus::Stream, KeyCode::PageUp) => self.scroll_stream(10),
            (Focus::Stream, KeyCode::PageDown) => self.scroll_stream(-10),
            (Focus::Stream, KeyCode::Enter) => self.open_detail(vm),
            _ => {}
        }
    }

    /// The selected OKX market (`SOL-USDT`), when OKX data is on.
    pub fn market(&self) -> Option<&str> {
        self.cex.as_ref()?.source.markets.get(self.mk_pair).map(String::as_str)
    }

    /// Select the Markets pair and bar; the cursor goes back to live.
    fn set_market(&mut self, (pair, bar): (usize, usize)) {
        if (pair, bar) != (self.mk_pair, self.mk_bar) {
            self.timeline.end();
        }
        (self.mk_pair, self.mk_bar) = (pair, bar);
        if let Some(c) = &self.cex {
            c.select(pair, bar);
        }
    }

    /// Move the Markets cursor by whole candles; past the newest = live.
    fn step_candle(&mut self, n: i64) {
        let Some((oldest, newest)) = self.kline_span.get() else { return };
        let t = Ts(self.timeline.cursor.unwrap_or(newest).0 + n * BARS[self.mk_bar].2);
        if t > newest {
            self.timeline.end();
        } else {
            self.timeline.cursor = Some(t.max(oldest));
            self.timeline.follow = false;
        }
    }

    /// Ask before an action on the selected bot, or say why it cannot be done.
    fn ask_bot(&mut self, action: BotAction) {
        let Some(b) = self.bots.as_ref().and_then(|b| b.read().bots.get(self.bot_selected).cloned()) else {
            return self.flash(if self.zh { "没有可操作的机器人" } else { "no bot to act on" });
        };
        match b.can(action, self.zh) {
            Ok(()) => self.bot_prompt = Some((b.id, action)),
            Err(why) if self.zh => self.flash(format!("不能{} {}：{why}", action.verb(true), b.name)),
            Err(why) => self.flash(format!("cannot {} {}: {why}", action.verb(false), b.name)),
        }
    }

    /// `?`: the keys; for who reads Chinese, the page one is on explained (a
    /// document with its tables, in the overlay every detail is read in).
    fn open_help(&mut self) {
        if self.zh {
            let title = format!("说明 · {}", self.page.label_zh());
            self.detail = Some(Detail { title, body: crate::guide::doc(self.page) });
            self.detail_scroll = 0;
        } else {
            self.help = true;
        }
    }

    /// Ask about `action` on the bot of this id (the one a form was opened for).
    fn ask_bot_of(&mut self, id: &str, action: BotAction) {
        let found = self.bots.as_ref().and_then(|b| b.read().bots.iter().find(|b| b.id == id).cloned());
        match found.map(|b| b.can(action, self.zh)) {
            Some(Ok(())) => self.bot_prompt = Some((id.to_string(), action)),
            Some(Err(why)) => self.flash(why),
            None => {}
        }
    }

    /// The exchange stream follows the page: the selected bot's market on
    /// the Bots page (its candles are live there), the Markets page's own otherwise.
    pub fn sync_stream(&self) {
        let Some(cex) = &self.cex else { return };
        let bot = (self.page == Page::Bots)
            .then(|| {
                self.bots.as_ref()?.read().bots.get(self.bot_selected).and_then(|b| crate::bots::shown_stream(self, b))
            })
            .flatten();
        let (pair, bar) = bot.unwrap_or((self.mk_pair, self.mk_bar));
        cex.select(pair, bar);
    }

    fn goto(&mut self, p: Page) {
        self.page = p;
        self.sync_stream();
        let f = self.page.focuses();
        if !f.contains(&self.focus) {
            self.focus = f[0];
        }
    }

    fn focus_if_available(&mut self, f: Focus) {
        if self.page.focuses().contains(&f) {
            self.focus = f;
        }
    }

    /// Put the cursor on the sample nearest to column `x` of a chart and
    /// freeze the window there (End returns to live).
    fn cursor_at(&mut self, vm: &ViewModel, g: Hit, x: u16) {
        let Hit::Graph { metric, plot, t0, t1, .. } = g else { return };
        let span = (plot.width.max(2) - 1) as i64;
        let dx = (x.clamp(plot.x, plot.right().saturating_sub(1)) - plot.x) as i64;
        let t = Ts(t0.0 + (t1.0 - t0.0) * dx / span);
        let series = metric.and_then(|m| vm.series(m)).or_else(|| self.active_series(vm));
        if let Some((ts, _)) = series.and_then(|s| s.nearest(t)) {
            self.timeline.cursor = Some(ts);
            self.timeline.follow = false;
            self.timeline.right = t1;
        }
    }

    /// Second click on the same cell within 400 ms.
    fn double_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let dbl = self.last_click.is_some_and(|(t, px, py)| px == x && py == y && t.elapsed().as_millis() < 400);
        self.last_click = if dbl { None } else { Some((now, x, y)) };
        dbl
    }

    /// Handle one mouse event. Returns engine commands (only the kill switch).
    pub fn on_mouse(&mut self, m: MouseEvent, vm: &ViewModel) -> Vec<Command> {
        let hit = self.hit_at(m.column, m.row);
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => return self.click(hit, x, y, vm),
            MouseEventKind::Down(MouseButton::Right) => {
                if let (Some(g @ Hit::Graph { .. }), false) = (hit, self.modal()) {
                    self.cursor_at(vm, g, x);
                    let t = self.timeline.cursor;
                    let set_b = self.timeline.a.is_some() && self.timeline.b.is_none();
                    if set_b {
                        self.timeline.mark_b(t);
                    } else {
                        self.timeline.clear_marks();
                        self.timeline.mark_a(t);
                    }
                    let what = if set_b { "B" } else { "A" };
                    self.flash(
                        t.map(|t| format!("{what} @ {}", t.hms_millis())).unwrap_or_else(|| "no sample here".into()),
                    );
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match (self.dragging, hit) {
                (Some(Hit::Candle(_)), Some(Hit::Candle(t))) => {
                    self.timeline.cursor = Some(t);
                    self.timeline.follow = false;
                }
                (Some(g @ Hit::Graph { .. }), _) => self.cursor_at(vm, g, x),
                _ => {}
            },
            MouseEventKind::Up(_) => self.dragging = None,
            MouseEventKind::ScrollUp => self.wheel(hit, -1, vm),
            MouseEventKind::ScrollDown => self.wheel(hit, 1, vm),
            _ => {}
        }
        Vec::new()
    }

    fn modal(&self) -> bool {
        self.kill_release_prompt
            || self.bot_prompt.is_some()
            || self.bot_budget.is_some()
            || self.bot_new.is_some()
            || self.wallet_form.is_some()
            || self.picker.is_some()
            || self.detail.is_some()
            || self.help
    }

    fn click(&mut self, hit: Option<Hit>, x: u16, y: u16, vm: &ViewModel) -> Vec<Command> {
        // Overlays: their own items, otherwise a click closes them. Releasing
        // the kill switch stays on the keyboard (y), and so does a transfer:
        // a click neither sends it nor throws away what was written.
        if self.wallet_form.is_some() || self.bot_budget.is_some() || self.bot_new.is_some() {
            return Vec::new();
        }
        if self.kill_release_prompt || self.bot_prompt.is_some() {
            self.kill_release_prompt = false;
            self.bot_prompt = None;
            return Vec::new();
        }
        if self.picker.is_some() {
            match hit {
                Some(Hit::PickerItem(i)) => {
                    self.picker = Some(i);
                    return self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), vm);
                }
                Some(Hit::Overlay) => {}
                _ => self.picker = None,
            }
            return Vec::new();
        }
        if self.detail.is_some() || self.help {
            if hit != Some(Hit::Overlay) || self.help {
                self.detail = None;
                self.help = false;
            }
            return Vec::new();
        }
        let dbl = self.double_click(x, y);
        match hit {
            Some(Hit::Page(p)) => self.goto(p),
            Some(Hit::Help) => self.open_help(),
            Some(Hit::Kill) => return self.on_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE), vm),
            Some(Hit::Panel(f)) => self.focus_if_available(f),
            Some(Hit::Opp(id)) => {
                self.focus_if_available(Focus::Opportunities);
                if dbl || self.opp_selected == Some(id) {
                    self.focus_if_available(Focus::Inspector);
                } else if let Some(o) = vm.opps.get(&id) {
                    self.select_opp(vm, o, true);
                }
            }
            Some(Hit::Stream(off)) => {
                self.focus_if_available(Focus::Stream);
                self.open_stream_detail(vm, off);
            }
            Some(Hit::Log(off)) => self.open_log_detail(vm, off),
            Some(g @ Hit::Graph { index, .. }) => {
                self.focus_if_available(Focus::Graphs);
                if let Some(i) = index {
                    self.workspace.active = i;
                }
                self.cursor_at(vm, g, x);
                self.dragging = Some(g);
            }
            Some(Hit::Sample(t)) => {
                self.timeline.cursor = Some(t);
                self.timeline.follow = false;
            }
            Some(h @ Hit::Candle(t)) => {
                self.timeline.cursor = Some(t);
                self.timeline.follow = false;
                self.dragging = Some(h);
            }
            Some(Hit::Bot(i)) => {
                self.bot_selected = i;
                self.sync_stream();
            }
            Some(Hit::BotBar(i)) => {
                self.bot_bar = Some(i);
                self.sync_stream();
            }
            Some(Hit::WalletRow(i)) => self.wallet_selected = i,
            Some(Hit::MkBar(i)) => self.set_market((self.mk_pair, i)),
            Some(Hit::MkPair) => return self.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), vm),
            Some(Hit::MkSide(s)) => self.mk_side = s,
            Some(Hit::MkTab(t)) => self.mk_tab = t,
            Some(Hit::PickerItem(_)) | Some(Hit::Overlay) | None => {}
        }
        Vec::new()
    }

    /// Wheel: scroll lists, zoom graphs (`dir` > 0 = down / wider).
    fn wheel(&mut self, hit: Option<Hit>, dir: i64, vm: &ViewModel) {
        if self.wallet_form.is_some() || self.bot_budget.is_some() || self.bot_new.is_some() {
            return;
        }
        if let Some(sel) = self.picker {
            let n = MetricId::ALL.len();
            self.picker = Some((sel as i64 + dir).rem_euclid(n as i64) as usize);
            return;
        }
        if self.detail.is_some() {
            self.detail_scroll = (self.detail_scroll as i64 + dir * 3).clamp(0, u16::MAX as i64) as u16;
            return;
        }
        if self.modal() {
            return;
        }
        match hit {
            Some(Hit::Opp(_)) | Some(Hit::Panel(Focus::Opportunities)) => self.move_opp(vm, dir),
            Some(Hit::Stream(_)) | Some(Hit::Panel(Focus::Stream)) => {
                self.stream_offset = (self.stream_offset as i64 - dir * 3).max(0) as usize
            }
            Some(Hit::Log(_)) => self.log_offset = (self.log_offset as i64 - dir * 3).max(0) as usize,
            Some(Hit::Panel(Focus::Inspector)) => {
                self.inspector_scroll = (self.inspector_scroll as i64 + dir * 2).clamp(0, u16::MAX as i64) as u16
            }
            Some(Hit::Candle(_)) | Some(Hit::MkBar(_)) => {
                self.set_market((self.mk_pair, (self.mk_bar as i64 + dir).clamp(0, BARS.len() as i64 - 1) as usize))
            }
            Some(Hit::Graph { .. }) | Some(Hit::Panel(Focus::Graphs)) | Some(Hit::Sample(_)) => {
                self.timeline.tf = if dir < 0 { self.timeline.tf.narrower() } else { self.timeline.tf.wider() }
            }
            _ => {}
        }
    }

    fn scroll_stream(&mut self, d: i64) {
        if self.page == Page::Logs {
            self.log_offset = (self.log_offset as i64 + d).max(0) as usize;
        } else {
            self.stream_offset = (self.stream_offset as i64 + d).max(0) as usize;
        }
    }

    fn open_detail(&mut self, vm: &ViewModel) {
        if self.page == Page::Logs {
            self.open_log_detail(vm, self.log_offset);
        } else {
            self.open_stream_detail(vm, self.stream_offset);
        }
    }

    fn open_log_detail(&mut self, vm: &ViewModel, offset: usize) {
        if let Some(l) = crate::ui::merged_log(vm).into_iter().rev().nth(offset) {
            self.detail = Some(Detail { title: format!("{} · {}", l.ts.hms_millis(), l.kind), body: l.detail });
            self.detail_scroll = 0;
        }
    }

    fn open_stream_detail(&mut self, vm: &ViewModel, offset: usize) {
        if let Some(s) = vm.stream.iter().rev().nth(offset) {
            let mut body = format!(
                "opportunity {}\nstage       {}\nresult      {}\nsubject     {}\nvalue       {}\n\n{}",
                s.opportunity,
                s.stage.label(),
                if s.ok { "ok" } else { "not ok" },
                s.subject,
                s.value,
                s.detail
            );
            if let Some(o) = vm.opps.get(&s.opportunity) {
                body.push_str(&format!(
                    "\n\nroute       {}\nstatus      {}\nnet         {} lamports ({})",
                    o.route.dex_path(),
                    crate::panels::status_label(&o.status),
                    crate::panels::signed_thousands(o.eval.expected_net),
                    o.eval.net_edge
                ));
                if let Some(sim) = &o.simulation {
                    body.push_str("\n\nsimulation logs (tail):\n");
                    for l in
                        sim.txs.iter().flat_map(|t| t.logs.iter()).rev().take(16).collect::<Vec<_>>().into_iter().rev()
                    {
                        body.push_str(&format!("  {l}\n"));
                    }
                }
            }
            self.detail = Some(Detail { title: format!("{} · {}", s.ts.hms_millis(), s.stage.label()), body });
            self.detail_scroll = 0;
        }
    }
}

/// Put `text` on the system clipboard through the terminal (OSC 52), where it has one.
fn copy(text: &str) {
    use base64::Engine;
    use std::io::{IsTerminal, Write};
    let mut out = std::io::stdout();
    if out.is_terminal() {
        let _ = write!(out, "\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(text));
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::Event;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn kill_switch_from_any_page_and_release_needs_confirmation() {
        let mut vm = ViewModel::new(false);
        let mut app = App::new(&TuiOptions::default());
        for p in ['1', '2', '3', '4', '5', '6', '7', '8'] {
            app.on_key(key(KeyCode::Char(p)), &vm);
            let cmds = app.on_key(key(KeyCode::Char('K')), &vm);
            assert!(matches!(cmds.as_slice(), [Command::KillSwitch { engage: true, .. }]), "page {p}");
        }
        vm.apply(&Event::KillSwitch { ts: searcher_core::Ts(1), engaged: true, reason: "x".into() });
        assert!(app.on_key(key(KeyCode::Char('K')), &vm).is_empty());
        assert!(app.kill_release_prompt);
        assert!(app.on_key(key(KeyCode::Char('n')), &vm).is_empty(), "anything but y cancels");
        app.on_key(key(KeyCode::Char('K')), &vm);
        let cmds = app.on_key(key(KeyCode::Char('y')), &vm);
        assert!(matches!(cmds.as_slice(), [Command::KillSwitch { engage: false, .. }]));
    }

    #[test]
    fn start_graphs_prefer_live_feeds_and_what_a_replay_has() {
        assert_eq!(default_graphs(None, true)[..2], [MetricId::PoolMid, MetricId::PoolSpread]);
        assert_eq!(default_graphs(None, false), LEGACY_GRAPHS.to_vec());
        let mut vm = ViewModel::new(true);
        vm.apply(&Event::Metric { ts: searcher_core::Ts(1), metric: MetricId::NetEdge, value: -5.0 });
        vm.apply(&Event::Metric { ts: searcher_core::Ts(1), metric: MetricId::Price, value: 105.0 });
        // an old session: no pool series → NetEdge, Price, then legacy fill
        assert_eq!(
            default_graphs(Some(&vm), true),
            vec![MetricId::NetEdge, MetricId::Price, MetricId::JupiterLatency, MetricId::Spread]
        );
    }

    #[test]
    fn ab_marks_follow_cursor_and_picker_registers_graphs() {
        let mut vm = ViewModel::new(false);
        for i in 0..50 {
            vm.apply(&Event::Metric { ts: searcher_core::Ts(i * 1_000_000), metric: MetricId::Price, value: i as f64 });
        }
        let mut app = App::new(&TuiOptions::default());
        app.focus = Focus::Graphs;
        app.on_key(key(KeyCode::Char('a')), &vm);
        assert_eq!(app.timeline.a, Some(searcher_core::Ts(49_000_000)));
        app.on_key(key(KeyCode::Left), &vm);
        app.on_key(key(KeyCode::Left), &vm);
        app.on_key(key(KeyCode::Char('b')), &vm);
        assert_eq!(app.timeline.b, Some(searcher_core::Ts(47_000_000)));
        app.on_key(key(KeyCode::Char('x')), &vm);
        assert!(app.timeline.ab().is_none());
        let n = app.workspace.entries.len();
        app.on_key(key(KeyCode::Char('+')), &vm);
        for _ in 0..4 {
            app.on_key(key(KeyCode::Down), &vm); // → Pnl
        }
        app.on_key(key(KeyCode::Enter), &vm);
        assert_eq!(app.workspace.entries.len(), n + 1);
        assert!(app.workspace.contains(MetricId::Pnl));
    }
}
