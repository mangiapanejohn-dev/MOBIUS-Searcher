//! UI state, keyboard and mouse handling. Produces engine `Command`s only
//! for the kill switch, confirmations and quit; everything else is local view
//! state. The mouse drives the same state as the keys.

use crate::cex::{BARS, Cex, OkxSource};
use crate::hub::ViewModel;
use crate::markets::{BottomTab, SideTab};
use crate::theme::{Glyphs, Theme};
use crate::timeline::Timeline;
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
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::Overview,
        Page::Markets,
        Page::Opportunities,
        Page::Graphs,
        Page::Trades,
        Page::Risk,
        Page::System,
        Page::Logs,
    ];

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
        }
    }

    /// Focus regions available on this page, in Tab order.
    pub fn focuses(self) -> &'static [Focus] {
        match self {
            Page::Overview => &[Focus::Opportunities, Focus::Graphs, Focus::Inspector, Focus::Stream],
            Page::Opportunities => &[Focus::Opportunities, Focus::Inspector],
            Page::Graphs | Page::Markets => &[Focus::Graphs],
            Page::Trades => &[Focus::Opportunities, Focus::Graphs],
            Page::Logs => &[Focus::Stream],
            Page::Risk | Page::System => &[Focus::Stream],
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
            if matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.detail = None;
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
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('T') if vm.replay => self.flash("thresholds can be changed in a live session only"),
            KeyCode::Char('T') => self.thresholds = Some(crate::thresholds::Panel::default()),
            KeyCode::Char(c @ '1'..='8') => self.goto(Page::ALL[(c as u8 - b'1') as usize]),
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
            KeyCode::Char('f') if matches!(self.focus, Focus::Opportunities) => {
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

    fn goto(&mut self, p: Page) {
        self.page = p;
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
        self.kill_release_prompt || self.picker.is_some() || self.detail.is_some() || self.help
    }

    fn click(&mut self, hit: Option<Hit>, x: u16, y: u16, vm: &ViewModel) -> Vec<Command> {
        // Overlays: their own items, otherwise a click closes them. Releasing
        // the kill switch stays on the keyboard (y).
        if self.kill_release_prompt {
            self.kill_release_prompt = false;
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
            Some(Hit::Help) => self.help = true,
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
        if let Some(sel) = self.picker {
            let n = MetricId::ALL.len();
            self.picker = Some((sel as i64 + dir).rem_euclid(n as i64) as usize);
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
                    "\n\nroute       {}\nstatus      {:?}\nnet         {} lamports ({})",
                    o.route.dex_path(),
                    o.status,
                    o.eval.expected_net,
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
        }
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
