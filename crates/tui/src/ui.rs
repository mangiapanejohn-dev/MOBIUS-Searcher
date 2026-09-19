//! Frame layout: header, pages, footer, overlays.

use crate::app::{App, Focus, Hit, Page};
use crate::chart::{ChartInput, put, render_chart, sparkline, text, width};
use crate::hub::{Marker, ViewModel};
use crate::panels::*;
use crate::theme::set_ascii;
use crate::workspace::ChartStyle;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::event::LogLevel;
use searcher_core::metrics::{MetricId, Unit};
use searcher_core::model::*;
use searcher_core::{Ts, UsdMicros};

pub const MIN_W: u16 = 40;
pub const MIN_H: u16 = 12;

pub fn draw(f: &mut Frame<'_>, app: &mut App, vm: &ViewModel) {
    let area = f.area();
    render(f.buffer_mut(), area, app, vm);
}

/// Pure render into a buffer (used by the terminal loop and by tests/snapshots).
pub fn render(buf: &mut Buffer, area: Rect, app: &mut App, vm: &ViewModel) {
    set_ascii(!app.glyphs.unicode);
    app.hits.borrow_mut().clear();
    let th = app.theme.clone();
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!("terminal too small: {}×{} (need ≥ {MIN_W}×{MIN_H})", area.width, area.height);
        text(buf, area.x, area.y, &msg, area.width, th.warn());
        if vm.kill_engaged() {
            text(buf, area.x, area.y + 1, "KILL SWITCH ENGAGED", area.width, Style::new().fg(th.loss));
        }
        return;
    }
    let compact = area.height < 30;
    let head_h = if compact { 2 } else { 3 };
    let header = Rect { height: head_h, ..area };
    let footer = Rect { y: area.bottom() - 1, height: 1, ..area };
    let mut body = Rect { y: area.y + head_h, height: area.height - head_h - 1, ..area };
    draw_header(buf, header, app, vm, compact);
    if let Some(&id) = vm.pending_confirm.first() {
        body = confirm_banner(buf, body, app, vm, id);
    }
    match app.page {
        Page::Overview => page_overview(buf, body, app, vm),
        Page::Markets => crate::markets::page_markets(buf, body, app, vm),
        Page::Opportunities => page_opportunities(buf, body, app, vm),
        Page::Graphs => page_graphs(buf, body, app, vm),
        Page::Trades => page_trades(buf, body, app, vm),
        Page::Risk => page_risk(buf, body, app, vm),
        Page::System => page_system(buf, body, app, vm),
        Page::Logs => page_logs(buf, body, app, vm),
    }
    draw_footer(buf, footer, app);
    if app.help {
        help_overlay(buf, area, app);
    }
    if let Some(sel) = app.picker {
        picker_overlay(buf, area, app, sel);
    }
    if let Some(d) = &app.detail {
        let inner = overlay(buf, area, 100, area.height - 4, &d.title, &th, &app.glyphs);
        app.hit(outer(inner), Hit::Overlay);
        wrap_text(buf, inner, &d.body, th.text().bg(th.select_bg));
    }
    if app.kill_release_prompt {
        let inner = overlay(buf, area, 56, 5, "Release kill switch?", &th, &app.glyphs);
        text(
            buf,
            inner.x,
            inner.y + 1,
            "press y to resume trading, any other key to cancel",
            inner.width,
            th.text().bg(th.select_bg),
        );
    }
}

// ───────────────────────────── header / footer ─────────────────────────────

fn draw_header(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, compact: bool) {
    let th = &app.theme;
    let g = &app.glyphs;
    let y0 = area.y;
    let clock = now_ts(vm).hms();
    let taker = vm.session.as_ref().and_then(|s| s.taker.clone()).unwrap_or_default();
    let right = if area.width > 110 && !taker.is_empty() { format!("{taker}   {clock}") } else { clock };
    let rw = width(&right);
    text(buf, area.right().saturating_sub(rw + 1), y0, &right, rw, th.faint());
    // optional items are dropped rather than run into the right block
    let limit = area.right().saturating_sub(rw + 3);
    let mut x = area.x + 1;
    x += crate::brand::wordmark(buf, x, y0, width(crate::brand::NAME), th) + 4;
    let mode = vm.mode();
    let mode_style = match mode {
        Mode::Paper => th.accent_bold(),
        Mode::Confirm => th.warn().add_modifier(Modifier::BOLD),
        Mode::Live => Style::new().fg(th.loss).add_modifier(Modifier::BOLD),
    };
    x += text(buf, x, y0, mode.label(), 7, mode_style) + 1;
    let live = !vm.replay && vm.slot_ts.is_some_and(|t| t.age_ms(Ts::now()) < 5_000);
    x += text(buf, x, y0, if live { g.live } else { g.off }, 1, if live { mode_style } else { th.faint() }) + 3;
    if vm.replay {
        x += text(buf, x, y0, " REPLAY ", 8, th.text().add_modifier(Modifier::REVERSED)) + 3;
    }
    if let Some(s) = vm.slot.map(|s| format!("slot {s}")).filter(|s| x + width(s) <= limit) {
        x += text(buf, x, y0, &s, 16, th.muted()) + 3;
    }
    if let Some(p) = vm.sol_price.map(|p| format!("SOL ${:.2}", p.f64())).filter(|p| x + width(p) <= limit) {
        text(buf, x, y0, &p, 12, th.text());
    }

    // KPI line
    let y1 = y0 + 1;
    let mut x = area.x + 1;
    let kpi = |buf: &mut Buffer, x: &mut u16, k: &str, v: &str, vs: Style| {
        if *x + width(k) + width(v) + 2 >= area.right() {
            return;
        }
        *x += text(buf, *x, y1, k, 20, th.muted()) + 1;
        *x += text(buf, *x, y1, v, 24, vs) + 3;
    };
    let eq = vm.equity_usd();
    kpi(buf, &mut x, "Equity", &eq.map(|e| e.to_string()).unwrap_or_else(|| "—".into()), th.text());
    let pnl = vm.session_pnl();
    let start_eq = vm
        .session
        .as_ref()
        .and_then(|s| s.paper_equity_lamports)
        .and_then(|l| vm.sol_price.and_then(|p| p.value(l as i128, 9)));
    let pct = start_eq.filter(|e| e.0 > 0).map(|e| pnl.0 as f64 / e.0 as f64 * 100.0);
    let session = match pct {
        Some(p) => format!("{} {:+.2}%", signed_usd(pnl), p),
        None => signed_usd(pnl),
    };
    kpi(buf, &mut x, "Session", &session, th.pnl(pnl.0 as f64));
    kpi(buf, &mut x, "Realized", &signed_usd(vm.realized), th.pnl(vm.realized.0 as f64));
    kpi(buf, &mut x, "Simulated", &signed_usd(vm.simulated), th.pnl(vm.simulated.0 as f64));
    kpi(buf, &mut x, "Opportunities", &vm.total_opps.to_string(), th.text());
    kpi(buf, &mut x, "Executed", &vm.executed.to_string(), th.text());
    if vm.rate_limited > 0 {
        kpi(buf, &mut x, "Rate-limited", &vm.rate_limited.to_string(), th.warn());
    }
    if vm.kill_engaged() {
        let k = " KILL SWITCH ";
        text(
            buf,
            area.right().saturating_sub(width(k) + 1),
            y1,
            k,
            width(k),
            Style::new().fg(th.loss).add_modifier(Modifier::REVERSED | Modifier::BOLD),
        );
    }
    if !compact {
        for rx in area.x..area.right() {
            put(buf, rx, y0 + 2, g.h, th.rule());
        }
    }
}

/// Signed USD; sub-cent non-zero values keep 4 decimals so tiny paper PnL
/// is not displayed as $0.00.
fn signed_usd(u: UsdMicros) -> String {
    if u.0 != 0 && u.0.abs() < 10_000 {
        return format!("{}${:.4}", if u.0 > 0 { "+" } else { "-" }, u.0.abs() as f64 / 1e6);
    }
    if u.0 > 0 { format!("+{u}") } else { u.to_string() }
}

fn draw_footer(buf: &mut Buffer, area: Rect, app: &App) {
    let th = &app.theme;
    let mut x = area.x + 1;
    let long = area.width >= 120;
    for (i, p) in Page::ALL.iter().enumerate() {
        let label = if long {
            format!("{} {}", i + 1, p.label())
        } else if area.width >= 90 {
            format!("{} {}", i + 1, &p.label()[..3.min(p.label().len())])
        } else {
            format!("{}", i + 1)
        };
        let st = if *p == app.page { th.accent_bold() } else { th.faint() };
        let w = text(buf, x, area.y, &label, 20, st);
        app.hit(Rect { x, y: area.y, width: w, height: 1 }, Hit::Page(*p));
        x += w + 2;
    }
    let hint = match (app.page, app.focus) {
        (_, _) if app.status_text().is_some() => app.status_text().unwrap_or("").to_string(),
        (Page::Markets, _) => {
            let c = if app.market_style == crate::workspace::ChartStyle::Line { "c candles" } else { "c line" };
            format!("[ ] bar  p pair  {c}  ←→ candle  t/o tabs  K kill  ? help")
        }
        (Page::Graphs, _) | (_, Focus::Graphs) => {
            "←→ cursor  a/b mark  x clear  [ ] zoom  + graph  c candle  s style  K kill  ? help".into()
        }
        (_, Focus::Opportunities) => "j/k select  ⏎ inspect  f filter  tab focus  a/b mark  K kill  ? help".into(),
        (_, Focus::Stream) => "j/k scroll  ⏎ detail  End live  tab focus  K kill  ? help".into(),
        _ => "tab focus  j/k scroll  K kill  ? help  q quit".into(),
    };
    // Whole hint items only: drop trailing ones that do not fit (a status
    // message is shown as is).
    let room = area.right().saturating_sub(x + 2);
    let hint = if app.status_text().is_some() || width(&hint) <= room {
        hint
    } else {
        let mut kept = String::new();
        for item in hint.split("  ").filter(|i| !i.is_empty()) {
            let next = if kept.is_empty() { item.to_string() } else { format!("{kept}  {item}") };
            if width(&next) > room {
                break;
            }
            kept = next;
        }
        kept
    };
    let hw = width(&hint).min(room);
    let st = if app.status_text().is_some() { th.accent() } else { th.faint() };
    let hx = area.right().saturating_sub(hw + 1);
    text(buf, hx, area.y, &hint, hw, st);
    for (needle, h) in [("K kill", Hit::Kill), ("? help", Hit::Help)] {
        if let Some(i) = hint.find(needle) {
            let (at, w) = (width(&hint[..i]), width(needle));
            if at + w <= hw {
                app.hit(Rect { x: hx + at, y: area.y, width: w, height: 1 }, h);
            }
        }
    }
}

fn confirm_banner(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel, id: OpportunityId) -> Rect {
    let th = &app.theme;
    let o = vm.opps.get(&id);
    let msg = match o {
        Some(o) => format!(
            " CONFIRM {} {}  net {} ({})  tip {}  — y send · n decline ",
            id,
            o.label,
            thousands(o.eval.expected_net),
            edge(o.eval.net_edge),
            thousands(o.costs.jito_tip as i64)
        ),
        None => format!(" CONFIRM {id} — y send · n decline "),
    };
    fill(buf, Rect { height: 1, ..body }, th.warn().add_modifier(Modifier::REVERSED));
    text(buf, body.x, body.y, &msg, body.width, th.warn().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    Rect { y: body.y + 1, height: body.height - 1, ..body }
}

// ───────────────────────────── graph workspace ─────────────────────────────

fn window(app: &App, vm: &ViewModel) -> (Ts, Ts) {
    let last = vm.last_ts.unwrap_or_else(Ts::now);
    let latest = if vm.replay { last } else { Ts::now().max(last) };
    app.timeline.window(vm.first_ts.unwrap_or(latest), latest)
}

pub fn graph_workspace(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, focused: bool, max_graphs: usize) {
    let th = &app.theme;
    let g = &app.glyphs;
    let tl = &app.timeline;
    let follow = if tl.follow { "live" } else { "history · End live" };
    let total = app.workspace.entries.len();
    let est_shown = ((area.height.saturating_sub(1) / 7) as usize).clamp(1, max_graphs.min(total.max(1)));
    let right = if est_shown < total {
        format!("{} · {} · {}/{} graphs (j/k)", tl.tf.label(), follow, est_shown, total)
    } else {
        format!("{} · {} · {} graphs", tl.tf.label(), follow, total)
    };
    let body = section(buf, area, "GRAPH WORKSPACE", focused, &right, th, g);
    if body.height == 0 {
        return;
    }
    app.hit(area, Hit::Panel(Focus::Graphs));
    let entries = &app.workspace.entries;
    if entries.is_empty() {
        text(buf, body.x, body.y, "no graphs registered — press + to add a metric", body.width, th.faint());
        return;
    }
    // Before the first data arrives the workspace shows the brand, not empty axes.
    if !entries.iter().any(|e| vm.series(e.metric).is_some_and(|s| !s.is_empty())) {
        crate::brand::draw_logo(buf, body, th, g, app.logo_image.as_ref(), "waiting for the first quotes…");
        return;
    }
    let (t0, t1) = window(app, vm);
    let markers: Vec<Marker> = vm.markers.iter().filter(|m| m.ts >= t0 && m.ts <= t1).cloned().collect();
    // How many fit at full size (≥ 7 rows each), keep the active one visible.
    let per_full = 7u16;
    let fit = ((body.height / per_full) as usize).clamp(1, max_graphs.min(entries.len()));
    let compact = body.height < 5;
    let n = if compact { (body.height as usize).min(entries.len()) } else { fit };
    let start = if app.workspace.active >= n { app.workspace.active + 1 - n } else { 0 };
    let shown: Vec<_> = entries.iter().enumerate().skip(start).take(n).collect();
    let h_each = if compact { 1 } else { body.height / n as u16 };
    for (k, (i, e)) in shown.iter().enumerate() {
        let y = body.y + k as u16 * h_each;
        let h = if k + 1 == shown.len() { body.bottom() - y } else { h_each };
        let r = Rect { x: body.x, y, width: body.width, height: h };
        let series = vm.series(e.metric);
        let subtitle = match (e.metric, e.style) {
            (MetricId::Price | MetricId::PoolMid | MetricId::OraclePrice, ChartStyle::Candle) => {
                "SOL/USD · candles from own samples · no volume data"
            }
            _ => e.metric.source(),
        };
        let active = *i == app.workspace.active;
        let inp = ChartInput {
            title: e.metric.label(),
            subtitle,
            series,
            unit: e.metric.unit(),
            t0,
            t1,
            style: e.style,
            line: app.line_style,
            cursor: tl.cursor,
            a: tl.a,
            b: tl.b,
            markers: &markers,
            active: active && focused,
            y_label_w: 9,
            candle_min_samples: 3,
            empty_note: "",
        };
        let info = render_chart(r, buf, &inp, th, g);
        app.hit(r, Hit::Graph { index: Some(*i), metric: Some(e.metric), plot: info.plot, t0, t1 });
        // spacer row between graphs is the chart's own time axis row
    }
}

// ───────────────────────────── pages ─────────────────────────────

fn split_h(r: Rect, left_pct: u16) -> (Rect, Rect) {
    let lw = r.width * left_pct / 100;
    (Rect { width: lw, ..r }, Rect { x: r.x + lw + 2, width: r.width.saturating_sub(lw + 2), ..r })
}

fn split_v(r: Rect, top_pct: u16) -> (Rect, Rect) {
    let th = r.height * top_pct / 100;
    (Rect { height: th, ..r }, Rect { y: r.y + th + 1, height: r.height.saturating_sub(th + 1), ..r })
}

/// Full box of an overlay from the inner area `overlay()` returned.
fn outer(inner: Rect) -> Rect {
    Rect {
        x: inner.x.saturating_sub(2),
        y: inner.y.saturating_sub(1),
        width: inner.width + 4,
        height: inner.height + 2,
    }
}

fn inset(r: Rect) -> Rect {
    Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r }
}

fn page_overview(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let body = inset(body);
    let f = app.focus;
    if body.width >= 100 {
        let (left, right) = split_h(body, 44);
        let (opps, stream) = split_v(left, 58);
        let (graphs, insp) = split_v(right, 60);
        opportunity_table(buf, opps, app, vm, f == Focus::Opportunities);
        event_stream(buf, stream, app, vm, f == Focus::Stream);
        graph_workspace(buf, graphs, app, vm, f == Focus::Graphs, 2);
        inspector(buf, insp, app, vm, f == Focus::Inspector);
    } else {
        let (opps, rest) = split_v(body, 34);
        let (graphs, stream) = split_v(rest, 55);
        opportunity_table(buf, opps, app, vm, f == Focus::Opportunities);
        graph_workspace(buf, graphs, app, vm, f == Focus::Graphs, 1);
        if f == Focus::Inspector {
            inspector(buf, stream, app, vm, true);
        } else {
            event_stream(buf, stream, app, vm, f == Focus::Stream);
        }
    }
}

fn page_opportunities(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let body = inset(body);
    if body.width >= 100 {
        let (l, r) = split_h(body, 52);
        opportunity_table(buf, l, app, vm, app.focus == Focus::Opportunities);
        inspector(buf, r, app, vm, app.focus == Focus::Inspector);
    } else {
        let (t, b) = split_v(body, 45);
        opportunity_table(buf, t, app, vm, app.focus == Focus::Opportunities);
        inspector(buf, b, app, vm, app.focus == Focus::Inspector);
    }
}

fn page_graphs(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let body = inset(body);
    let side = body.width >= 120;
    let (main, panel) = if side { split_h(body, 64) } else { split_v(body, 68) };
    graph_workspace(buf, main, app, vm, true, app.workspace.max);
    if app.timeline.ab().is_some() {
        ab_inspector(buf, panel, app, vm, false);
    } else {
        samples_panel(buf, panel, app, vm);
    }
}

/// Samples of the active graph around the cursor (winproc "Samples").
fn samples_panel(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let g = &app.glyphs;
    let m = app.workspace.active_metric();
    let title = m.map(|m| format!("SAMPLES · {}", m.label())).unwrap_or_else(|| "SAMPLES".into());
    let body = section(buf, area, &title, false, "a/b to mark", th, g);
    let Some(s) = m.and_then(|m| vm.series(m)) else {
        text(buf, body.x, body.y, "no samples", body.width, th.faint());
        return;
    };
    let unit = m.map(|m| m.unit()).unwrap_or(Unit::Count);
    let cursor = app.timeline.cursor_ts(Some(s));
    let pts: Vec<(Ts, f64)> = s.iter().copied().collect();
    let ci = cursor.map(|c| pts.partition_point(|p| p.0 < c)).unwrap_or(pts.len().saturating_sub(1));
    let rows = body.height.saturating_sub(1) as usize;
    let start = ci.saturating_sub(rows / 2);
    text(buf, body.x, body.y, "   TIME          VALUE        Δ", body.width, th.faint());
    for (k, i) in (start..pts.len()).take(rows).enumerate() {
        let y = body.y + 1 + k as u16;
        let (t, v) = pts[i];
        let mark = match (app.timeline.a == Some(t), app.timeline.b == Some(t)) {
            (true, true) => "AB",
            (true, _) => "A ",
            (_, true) => "B ",
            _ => "  ",
        };
        let st = if i == ci { th.selected() } else { th.text() };
        if i == ci {
            fill(buf, Rect { x: body.x, y, width: body.width, height: 1 }, st);
        }
        app.hit(Rect { x: body.x, y, width: body.width, height: 1 }, Hit::Sample(t));
        text(buf, body.x, y, mark, 2, th.accent_bold());
        text(buf, body.x + 3, y, &t.hms_millis(), 12, st);
        let vs = unit.format(v);
        text(buf, body.x + 16, y, &vs, 12, st);
        if i > 0 {
            let d = unit.format_delta(v - pts[i - 1].1);
            text(buf, body.x + 29, y, &d, body.width.saturating_sub(29), th.faint());
        }
    }
}

fn page_trades(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let g = &app.glyphs;
    let body = inset(body);
    let (tbl, chart) = split_v(body, 55);
    let paper = vm.trades.iter().filter(|t| t.paper).count();
    let right = format!("{} trades · {} paper (simulated) · {} live", vm.trades.len(), paper, vm.trades.len() - paper);
    let tb = section(buf, tbl, "TRADES", app.focus == Focus::Opportunities, &right, th, g);
    text(
        buf,
        tb.x,
        tb.y,
        "TIME       MODE   STRATEGY    ROUTE                         INPUT SOL        NET lamports       NET USD",
        tb.width,
        th.faint(),
    );
    for (k, t) in vm.trades.iter().rev().take(tb.height.saturating_sub(1) as usize).enumerate() {
        let y = tb.y + 1 + k as u16;
        text(buf, tb.x, y, &t.exit_ts.hms(), 9, th.faint());
        text(
            buf,
            tb.x + 11,
            y,
            if t.paper { "paper" } else { t.mode.label() },
            6,
            if t.paper { th.muted() } else { th.accent() },
        );
        text(buf, tb.x + 18, y, t.strategy.label(), 11, th.muted());
        text(buf, tb.x + 30, y, &t.label, 28, th.text());
        text(buf, tb.x + 60, y, &format!("{:>12}", sol_amount(t.input as i128)), 13, th.text());
        text(buf, tb.x + 74, y, &format!("{:>16}", signed_thousands(t.net)), 17, th.pnl(t.net as f64));
        let usd = t.net_usd.map(signed_usd).unwrap_or_default();
        text(buf, tb.x + 92, y, &format!("{usd:>12}"), tb.width.saturating_sub(92), th.pnl(t.net as f64));
    }
    if vm.trades.is_empty() {
        text(buf, tb.x, tb.y + 1, "no trades — nothing has passed simulation + risk yet", tb.width, th.faint());
    }
    let (t0, t1) = window(app, vm);
    let markers: Vec<Marker> = vm.markers.iter().filter(|m| m.ts >= t0 && m.ts <= t1).cloned().collect();
    let pnl = vm.series(MetricId::Pnl);
    let inp = ChartInput {
        title: "Session PnL",
        subtitle: "USD · paper fills are simulated; realized = on-chain only",
        series: pnl,
        unit: Unit::Usd,
        t0,
        t1,
        style: ChartStyle::Line,
        line: app.line_style,
        cursor: app.timeline.cursor,
        a: app.timeline.a,
        b: app.timeline.b,
        markers: &markers,
        active: app.focus == Focus::Graphs,
        y_label_w: 9,
        candle_min_samples: 3,
        empty_note: "no trades yet — PnL stays $0.00 until one passes simulation + risk",
    };
    let info = render_chart(chart, buf, &inp, th, g);
    app.hit(chart, Hit::Graph { index: None, metric: Some(MetricId::Pnl), plot: info.plot, t0, t1 });
}

fn bar(n: u64, max: u64, w: u16, g: &crate::theme::Glyphs) -> String {
    let full = if max == 0 { 0 } else { (n as f64 / max as f64 * w as f64).round() as usize };
    g.candle_body.repeat(full.max(usize::from(n > 0)))
}

fn page_risk(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let g = &app.glyphs;
    let body = inset(body);
    let (l, r) = if body.width >= 100 { split_h(body, 48) } else { split_v(body, 50) };
    let lb = section(buf, l, "RISK", false, "K / click: kill switch", th, g);
    let mut y = lb.y;
    let (ks, kst) = match &vm.kill {
        Some((true, reason, ts)) => {
            (format!("ENGAGED {} · {reason}", ts.hms()), Style::new().fg(th.loss).add_modifier(Modifier::BOLD))
        }
        _ => ("armed · not engaged".to_string(), Style::new().fg(th.profit)),
    };
    kv(buf, lb.x, y, lb.width, "Kill switch", &ks, kst, th);
    app.hit(Rect { x: lb.x, y, width: lb.width, height: 1 }, Hit::Kill);
    y += 1;
    kv(buf, lb.x, y, lb.width, "Mode", vm.mode().label(), th.accent(), th);
    y += 1;
    kv(
        buf,
        lb.x,
        y,
        lb.width,
        "Risk checks",
        &format!("{} · {} approved", vm.risk_checked, vm.risk_approved),
        th.text(),
        th,
    );
    y += 2;
    text(buf, lb.x, y, "LIMITS", lb.width, th.faint());
    y += 1;
    if let Some(s) = &vm.session {
        for (k, v) in &s.limits {
            if y >= lb.bottom() {
                break;
            }
            kv(buf, lb.x, y, lb.width, k, v, th.text(), th);
            y += 1;
        }
    }
    let rb = section(buf, r, "WHY OPPORTUNITIES WERE NOT EXECUTED", false, "", th, g);
    let mut rows: Vec<(String, u64)> = vm.skip_counts.iter().map(|(k, v)| (k.code().to_string(), *v)).collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    let max = rows.iter().map(|r| r.1).max().unwrap_or(0);
    let mut y = rb.y;
    for (k, n) in &rows {
        if y >= rb.bottom() {
            break;
        }
        text(buf, rb.x, y, k, 18, th.text());
        text(buf, rb.x + 19, y, &format!("{n:>6}"), 6, th.muted());
        let bw = rb.width.saturating_sub(28);
        text(buf, rb.x + 27, y, &bar(*n, max, bw, g), bw, th.accent_dim_style());
        y += 1;
    }
    if !vm.risk_violations.is_empty() && y + 2 < rb.bottom() {
        y += 1;
        text(buf, rb.x, y, "RISK VIOLATIONS", rb.width, th.faint());
        y += 1;
        for (k, n) in &vm.risk_violations {
            if y >= rb.bottom() {
                break;
            }
            text(buf, rb.x, y, &format!("{k:<28} {n:>6}"), rb.width, th.text());
            y += 1;
        }
    }
    if rows.is_empty() {
        text(buf, rb.x, rb.y, "nothing skipped yet", rb.width, th.faint());
    }
}

fn page_system(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let g = &app.glyphs;
    let body = inset(body);
    let sb =
        section(buf, body, "SYSTEM", false, "state · latency · errors · requests · rate limit · last success", th, g);
    let w = sb.width;
    let spark_w = if w >= 140 {
        24
    } else if w >= 110 {
        16
    } else {
        8
    };
    text(buf, sb.x, sb.y, "SERVICE        STATE          LAST    P50", w, th.faint());
    text(buf, sb.x + 44, sb.y, "LATENCY", spark_w, th.faint());
    let xc = sb.x + 45 + spark_w;
    text(buf, xc, sb.y, " ERR%      REQS   429   QUOTA  LAST OK", w.saturating_sub(xc - sb.x), th.faint());
    let now = now_ts(vm);
    for (k, s) in ServiceId::ALL.iter().enumerate() {
        let y = sb.y + 1 + k as u16 * 2;
        if y >= sb.bottom() {
            break;
        }
        let snap = vm.health.get(s).cloned().unwrap_or_default();
        text(buf, sb.x, y, s.label(), 14, th.text().add_modifier(Modifier::BOLD));
        let st_style = match snap.state {
            ServiceState::Ok => Style::new().fg(th.profit),
            ServiceState::Degraded | ServiceState::RateLimited => th.warn().add_modifier(Modifier::BOLD),
            ServiceState::Down => Style::new().fg(th.loss).add_modifier(Modifier::BOLD),
            _ => th.faint(),
        };
        let mut state = snap.state.label().to_string();
        if let Some(u) = snap.backoff_until {
            state = format!("429 · {}s", (u.0 - now.0).max(0) / 1_000_000 + 1);
        }
        text(buf, sb.x + 15, y, &state, 14, st_style);
        let last = snap.last_latency_ms.map(|l| format!("{l}ms")).unwrap_or_else(|| "—".into());
        text(buf, sb.x + 30, y, &format!("{last:>6}"), 6, th.text());
        let p50 = snap.p50_latency_ms.map(|l| format!("{l}ms")).unwrap_or_else(|| "—".into());
        text(buf, sb.x + 37, y, &format!("{p50:>6}"), 6, th.muted());
        text(buf, sb.x + 44, y, &sparkline(&snap.recent_latency, spark_w as usize, g), spark_w, th.accent_dim_style());
        let err = format!("{:>5.1}", snap.error_rate.pct_f64());
        text(buf, xc, y, &err, 6, if snap.error_rate.0 > 0 { th.warn() } else { th.faint() });
        text(buf, xc + 6, y, &format!("{:>9}", snap.requests), 9, th.muted());
        text(
            buf,
            xc + 16,
            y,
            &format!("{:>5}", snap.rate_limited),
            5,
            if snap.rate_limited > 0 { th.warn() } else { th.faint() },
        );
        let q = snap.quota_remaining.map(|q| q.to_string()).unwrap_or_else(|| "—".into());
        text(buf, xc + 22, y, &format!("{q:>7}"), 7, th.faint());
        let ok = snap.last_success.map(|t| age(t.age_ms(now))).unwrap_or_else(|| "never".into());
        text(buf, xc + 31, y, &ok, 8, th.faint());
        if let Some(e) = &snap.last_error {
            text(buf, sb.x + 15, y + 1, e, w.saturating_sub(15), th.faint());
        }
    }
    let y = sb.y + 2 + ServiceId::ALL.len() as u16 * 2;
    if y + 3 < sb.bottom() {
        let mut yy = y;
        text(buf, sb.x, yy, "PIPELINE", w, th.faint());
        yy += 1;
        let tip = vm
            .tip_floor
            .as_ref()
            .map(|t| {
                format!(
                    "p25 {}  p50 {}  p75 {}  p95 {}  ema50 {} · {} ago",
                    t.p25,
                    t.p50,
                    t.p75,
                    t.p95,
                    t.ema_p50,
                    age(t.ts.age_ms(now))
                )
            })
            .unwrap_or_else(|| "—".into());
        kv(buf, sb.x, yy, w.min(100), "Jito landed tips (lamports)", &tip, th.text(), th);
        yy += 1;
        kv(buf, sb.x, yy, w.min(100), "Simulations", &format!("{} · {} failed", vm.sims, vm.sim_failed), th.text(), th);
        yy += 1;
        kv(
            buf,
            sb.x,
            yy,
            w.min(100),
            "Block height",
            &vm.block_height.map(|b| b.to_string()).unwrap_or_else(|| "—".into()),
            th.text(),
            th,
        );
        yy += 1;
        kv(buf, sb.x, yy, w.min(100), "UI events dropped (display only)", &vm.ui_dropped.to_string(), th.text(), th);
        yy += 1;
        if let Some(s) = &vm.session {
            kv(buf, sb.x, yy, w.min(100), "Session", &s.session_id, th.text(), th);
        }
        yy += 2;
        network_and_feeds(buf, Rect { y: yy, height: sb.bottom().saturating_sub(yy), ..sb }, app, vm, now);
    }
}

/// Network conditions and the on-chain / oracle feeds (none of it spends the
/// Jupiter rate limit).
fn network_and_feeds(buf: &mut Buffer, r: Rect, app: &App, vm: &ViewModel, now: Ts) {
    let th = &app.theme;
    let w = r.width.min(100);
    let mut y = r.y;
    let fits = |y: u16| y < r.bottom();
    if !fits(y) {
        return;
    }
    text(buf, r.x, y, "NETWORK", r.width, th.faint());
    y += 1;
    let n = vm.network.as_ref();
    let tps = n
        .and_then(|n| {
            Some(format!("{} tx/s · {} non-vote", thousands(n.tps? as i64), thousands(n.non_vote_tps? as i64)))
        })
        .unwrap_or_else(|| "—".into());
    let fees = n
        .and_then(|n| {
            Some(format!(
                "p50 {} · p75 {} · p90 {} µL/CU · {} slots",
                thousands(n.fee_p50? as i64),
                thousands(n.fee_p75? as i64),
                thousands(n.fee_p90? as i64),
                n.fee_slots
            ))
        })
        .unwrap_or_else(|| "—".into());
    for (k, v) in [("Network TPS", tps), ("Priority fee · per-slot min, watched pools", fees)] {
        if fits(y) {
            kv(buf, r.x, y, w, k, &v, th.text(), th);
            y += 1;
        }
    }
    y += 1;
    if !fits(y) {
        return;
    }
    text(buf, r.x, y, "FEEDS · on-chain (accountSubscribe) · no Jupiter budget", r.width, th.faint());
    y += 1;
    let mut feeds: Vec<_> =
        vm.quotes.iter().filter(|q| matches!(q.side, SampleSide::Mid | SampleSide::Oracle)).collect();
    feeds.sort_by(|a, b| (a.side as u8, &a.source, &a.pair).cmp(&(b.side as u8, &b.source, &b.pair)));
    if feeds.is_empty() && fits(y) {
        text(buf, r.x, y, "no on-chain updates yet", r.width, th.faint());
    }
    for q in feeds {
        if !fits(y) {
            break;
        }
        let a = q.ts.age_ms(now);
        let price =
            if q.price.f64() < 10.0 { format!("{:.6}", q.price.f64()) } else { format!("{:.4}", q.price.f64()) };
        text(buf, r.x, y, &q.source, 20, th.text());
        text(buf, r.x + 21, y, &q.pair, 10, th.muted());
        text(buf, r.x + 32, y, &format!("{price:>12}"), 12, th.text());
        // quiet for a minute = the feed is probably stuck (Pyth heartbeats are slower)
        let stale = a > if q.side == SampleSide::Oracle { 180_000 } else { 60_000 };
        text(buf, r.x + 46, y, &format!("{:>7}", age(a)), 7, if stale { th.warn() } else { th.faint() });
        text(buf, r.x + 55, y, &format!("{} updates", q.samples), 16, th.faint());
        y += 1;
    }
}

pub struct LogItem {
    pub ts: Ts,
    pub kind: String,
    pub text: String,
    pub detail: String,
    pub level: LogLevel,
}

/// Engine logs + pipeline stages, chronological.
pub fn merged_log(vm: &ViewModel) -> Vec<LogItem> {
    let mut v: Vec<LogItem> = vm
        .logs
        .iter()
        .map(|l| LogItem {
            ts: l.ts,
            kind: l.source.clone(),
            text: l.message.clone(),
            detail: l.message.clone(),
            level: l.level,
        })
        .collect();
    v.extend(vm.stream.iter().map(|s| LogItem {
        ts: s.ts,
        kind: s.stage.label().to_string(),
        text: format!("{} {}  {}", s.opportunity, s.subject, s.value),
        detail: format!("{} · {} · {}\n\n{}", s.opportunity, s.subject, s.value, s.detail),
        level: if s.ok { LogLevel::Info } else { LogLevel::Warn },
    }));
    v.sort_by_key(|i| i.ts);
    v
}

fn page_logs(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let g = &app.glyphs;
    let body = inset(body);
    let items = merged_log(vm);
    let right = format!("{} lines · ⏎ detail · End live", items.len());
    let lb = section(buf, body, "LOGS", true, &right, th, g);
    let rows = lb.height as usize;
    let end = items.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(rows);
    for (k, it) in items[start..end].iter().enumerate() {
        let y = lb.y + k as u16;
        app.hit(Rect { x: lb.x, y, width: lb.width, height: 1 }, Hit::Log(items.len() - 1 - (start + k)));
        let sel = start + k + 1 == end && app.log_offset > 0;
        if sel {
            fill(buf, Rect { x: lb.x, y, width: lb.width, height: 1 }, th.selected());
        }
        text(buf, lb.x, y, &it.ts.hms_millis(), 12, th.faint());
        let ls = match it.level {
            LogLevel::Error => Style::new().fg(th.loss),
            LogLevel::Warn => th.warn(),
            _ => th.muted(),
        };
        text(buf, lb.x + 13, y, &it.kind, 12, ls);
        text(buf, lb.x + 26, y, &it.text, lb.width.saturating_sub(26), th.text());
    }
}

// ───────────────────────────── overlays ─────────────────────────────

fn help_overlay(buf: &mut Buffer, area: Rect, app: &App) {
    let th = &app.theme;
    let (lw, lh) = crate::brand::logo_size();
    let rows = HELP.lines().count() as u16;
    // Logo beside the keys when there is room (and art + Unicode glyphs).
    let with_logo =
        crate::brand::can_draw_logo(th, &app.glyphs) && area.width >= lw + 86 && area.height >= lh.max(rows) + 6;
    let (w, h) = if with_logo { (lw + 84, lh.max(rows) + 4) } else { (78, rows + 4) };
    let title = format!("{} · keys · v{}", crate::brand::NAME, env!("CARGO_PKG_VERSION"));
    let mut inner = overlay(buf, area, w, h, &title, th, &app.glyphs);
    app.hit(outer(inner), Hit::Overlay);
    if with_logo {
        let logo_area = Rect { width: lw + 2, ..inner };
        crate::brand::draw_logo(buf, logo_area, th, &app.glyphs, app.logo_image.as_ref(), "");
        inner = Rect { x: inner.x + lw + 4, width: inner.width.saturating_sub(lw + 4), ..inner };
    }
    inner.y += inner.height.saturating_sub(rows) / 2;
    wrap_text(buf, inner, HELP, th.text().bg(th.select_bg));
}

/// Key help; every line fits the 74-column overlay (also after ASCII folding).
const HELP: &str = "\
1-8        Overview Markets Opportunities Graphs Trades Risk System Logs
tab        cycle focus: opportunities · graphs · inspector · stream
K          KILL SWITCH — stop new trades (any page); K again → release (y)
j/k ↑/↓    select / scroll     ⏎ inspect / detail     Esc back to live
←/→ h/l    move cursor over samples (Shift ×10)   Alt+←/→ pan   Home/End
a / b / x  mark A / mark B at cursor / clear   → Δ in the A/B inspector
[ / ]      timeframe 1m · 5m · 15m · 1h · session
Markets    [ ] bar 1s–1D · p pair · t book/trades · o tabs · ←→ candle
+          add/remove graph metrics   -/Del remove active graph
Shift+↑/↓  reorder graphs     c line/candle     s box/braille line
f          filter opportunities (all · gross>0 · executable · skipped)
y / n      approve / decline a pending CONFIRM transaction
click      tabs · rows (again: inspect) · graph = cursor, drag to scrub
wheel      scroll lists · zoom graphs      right-click graph  mark A, B
q          quit (graceful; recording is flushed)";

fn picker_overlay(buf: &mut Buffer, area: Rect, app: &App, sel: usize) {
    let th = &app.theme;
    let inner = overlay(
        buf,
        area,
        60,
        MetricId::ALL.len() as u16 + 4,
        "Graph metrics  (⏎ toggle · Esc close)",
        th,
        &app.glyphs,
    );
    app.hit(outer(inner), Hit::Overlay);
    for (i, m) in MetricId::ALL.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        app.hit(Rect { x: inner.x, y, width: inner.width, height: 1 }, Hit::PickerItem(i));
        let on = app.workspace.contains(*m);
        let st = if i == sel { th.accent_bold().bg(th.select_bg) } else { th.text().bg(th.select_bg) };
        let line = format!("{} {:<24} {}", if on { "[x]" } else { "[ ]" }, m.label(), m.source());
        text(buf, inner.x, y, &line, inner.width, st);
    }
}
