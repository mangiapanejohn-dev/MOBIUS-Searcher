//! Reusable panels: section headers, opportunity table, inspector, A/B delta
//! inspector, event stream, overlays. Terminal-native: section titles and
//! thin rules instead of boxed cards.

use crate::app::{App, Focus, Hit};
use crate::chart::{put, text, width};
use crate::hub::{MarkerKind, ViewModel};
use crate::theme::{Glyphs, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::event::Stage;
use searcher_core::metrics::MetricId;
use searcher_core::model::*;
use searcher_core::time::fmt_duration_us;
use searcher_core::units::format_atoms;
use searcher_core::{Ppm, Ts};

// ───────────────────────────── formatting ─────────────────────────────

/// `+.42%` / `-1.20%` — compact signed percent from ppm.
pub fn edge(p: Ppm) -> String {
    let s = p.to_string(); // "+0.42%"
    s.replacen("+0.", "+.", 1).replacen("-0.", "-.", 1)
}

pub fn thousands(v: i64) -> String {
    let neg = v < 0;
    let mut d = v.unsigned_abs().to_string();
    let mut i = d.len() as isize - 3;
    while i > 0 {
        d.insert(i as usize, ',');
        i -= 3;
    }
    if neg { format!("-{d}") } else { d }
}

pub fn signed_thousands(v: i64) -> String {
    if v > 0 { format!("+{}", thousands(v)) } else { thousands(v) }
}

pub fn age(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}h", ms / 3_600_000)
    }
}

/// Whether the cycle was fully quoted (every leg built). Unpriced rows have no edge.
pub fn is_priced(o: &Opportunity) -> bool {
    !matches!(o.status.skip(), Some(SkipReason::NoRoute | SkipReason::BuildFailed | SkipReason::RateLimited))
        && o.route.legs.last().map(|l| l.output_mint) == Some(o.base_mint)
}

pub fn status_label(s: &OppStatus) -> String {
    match s {
        OppStatus::Quoted => "QUOTED".into(),
        OppStatus::Skipped(r) => r.code().into(),
        OppStatus::Executable => "EXECUTABLE".into(),
        OppStatus::AwaitingConfirm => "AWAIT CONFIRM".into(),
        OppStatus::Submitted => "SUBMITTED".into(),
        OppStatus::PaperFilled => "PAPER FILL".into(),
        OppStatus::Landed => "LANDED".into(),
        OppStatus::Failed => "FAILED".into(),
    }
}

pub fn status_style(s: &OppStatus, th: &Theme) -> Style {
    match s {
        OppStatus::Skipped(_) => th.muted(),
        OppStatus::PaperFilled | OppStatus::Landed => Style::new().fg(th.profit),
        OppStatus::Failed => Style::new().fg(th.loss),
        OppStatus::AwaitingConfirm | OppStatus::Submitted | OppStatus::Executable => th.accent_bold(),
        OppStatus::Quoted => th.text(),
    }
}

/// "now" for ages: wall clock live, last event time in replay.
pub fn now_ts(vm: &ViewModel) -> Ts {
    if vm.replay { vm.last_ts.unwrap_or_else(Ts::now) } else { Ts::now() }
}

pub fn sol_amount(lamports: i128) -> String {
    format_atoms(lamports, 9, 6)
}

// ───────────────────────────── primitives ─────────────────────────────

pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            put(buf, x, y, " ", style);
        }
    }
}

/// Section title + thin rule; returns the content area below.
pub fn section(buf: &mut Buffer, area: Rect, title: &str, focused: bool, right: &str, th: &Theme, g: &Glyphs) -> Rect {
    if area.height == 0 {
        return area;
    }
    let mut x = area.x;
    if focused {
        x += text(buf, x, area.y, g.marker_entry, 2, th.accent());
        x += 1;
    }
    x += text(buf, x, area.y, title, area.width, th.header(focused));
    let rw = width(right);
    let rule_end = if rw > 0 && x + rw + 3 < area.right() { area.right() - rw - 1 } else { area.right() };
    if x + 1 < rule_end {
        for rx in (x + 1)..rule_end {
            put(buf, rx, area.y, g.h, th.rule());
        }
    }
    if rw > 0 && rule_end < area.right() {
        text(buf, rule_end + 1, area.y, right, rw, th.faint());
    }
    Rect { y: area.y + 1, height: area.height - 1, ..area }
}

/// Left/right aligned key-value row.
#[allow(clippy::too_many_arguments)]
pub fn kv(buf: &mut Buffer, x: u16, y: u16, w: u16, k: &str, v: &str, vs: Style, th: &Theme) {
    let kw = text(buf, x, y, k, w, th.muted());
    let vw = width(v);
    if vw + kw < w {
        text(buf, x + w - vw, y, v, vw, vs);
    } else if kw + 1 < w {
        text(buf, x + kw + 1, y, v, w - kw - 1, vs);
    }
}

// ───────────────────────────── opportunity table ─────────────────────────────

pub fn opportunity_table(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, focused: bool) {
    let th = &app.theme;
    let g = &app.glyphs;
    let list = app.filtered_opps(vm);
    let right = format!("{} · {} shown · f filter", app.opp_filter.label(), list.len());
    let body = section(buf, area, "OPPORTUNITIES", focused, &right, th, g);
    if body.height < 2 {
        return;
    }
    let w = body.width;
    let show_status = w >= 58;
    let show_age = w >= 40;
    let (cg, cn, ca, cs) = (7u16, 7u16, if show_age { 6 } else { 0 }, if show_status { 15 } else { 0 });
    let route_w = w.saturating_sub(2 + cg + cn + ca + cs + 3);
    let cols = |x0: u16| (x0 + 2, x0 + 2 + route_w + 1, x0 + 2 + route_w + 1 + cg + 1, x0 + 2 + route_w + cg + cn + 3);
    let (xr, xg, xn, xa) = cols(body.x);
    let xs = xa + ca + 1;
    let hy = body.y;
    let hs = th.faint();
    text(buf, xr, hy, "ROUTE", route_w, hs);
    text(buf, xg + cg - 5, hy, "GROSS", 5, hs);
    text(buf, xn + cn - 3, hy, "NET", 3, hs);
    if show_age {
        text(buf, xa + ca - 3, hy, "AGE", 3, hs);
    }
    if show_status {
        text(buf, xs, hy, "STATUS", cs, hs);
    }
    let rows = (body.height - 1) as usize;
    let sel_idx = app.opp_selected.and_then(|id| list.iter().position(|o| o.id == id)).unwrap_or(0);
    let start = sel_idx.saturating_sub(rows.saturating_sub(1));
    let now = now_ts(vm);
    app.hit(area, Hit::Panel(Focus::Opportunities));
    for (i, o) in list.iter().enumerate().skip(start).take(rows) {
        let y = body.y + 1 + (i - start) as u16;
        app.hit(Rect { x: body.x, y, width: w, height: 1 }, Hit::Opp(o.id));
        let selected = i == sel_idx;
        let row_style = if selected && focused { th.selected() } else { Style::new() };
        if selected {
            fill(buf, Rect { x: body.x, y, width: w, height: 1 }, row_style);
            put(buf, body.x, y, g.bullet, th.accent().patch(row_style.remove_modifier(Modifier::all())));
        }
        let label =
            if o.strategy == StrategyKind::CrossDex { o.label.clone() } else { o.label.replace("→", g.arrow) };
        // unpriced rows (no route / build failed / rate limited) say nothing
        // about the market: dimmed so the priced ones stand out
        let priced = is_priced(o);
        let route_style = match (selected, priced) {
            (true, _) => th.accent_bold(),
            (false, true) => th.text(),
            (false, false) => th.faint(),
        };
        text(buf, xr, y, &label, route_w, route_style);
        if is_priced(o) {
            let gs = edge(o.eval.gross_edge);
            text(buf, xg + cg.saturating_sub(width(&gs)), y, &gs, cg, th.pnl(o.eval.gross_pnl as f64));
            let net = o.eval.simulated_net.map_or(o.eval.expected_net, |s| s.min(o.eval.expected_net));
            let ns = edge(Ppm::ratio(net as i128, o.input.max(1) as i128).unwrap_or_default());
            text(buf, xn + cn.saturating_sub(width(&ns)), y, &ns, cn, th.pnl(net as f64));
        } else {
            // Route never completed (no route / build failed / rate limited): no edge exists.
            text(buf, xg + cg - 1, y, "—", 1, th.faint());
            text(buf, xn + cn - 1, y, "—", 1, th.faint());
        }
        if show_age {
            let a = age(o.detected_at.age_ms(now));
            text(buf, xa + ca.saturating_sub(width(&a)), y, &a, ca, th.faint());
        }
        if show_status {
            let st = if priced { status_style(&o.status, th) } else { th.faint() };
            text(buf, xs, y, &status_label(&o.status), cs, st);
        }
    }
    if list.is_empty() {
        text(buf, xr, body.y + 1, "waiting for the first evaluated cycle…", route_w + 20, th.faint());
    }
}

// ───────────────────────────── inspector ─────────────────────────────

struct Lines<'a> {
    buf: &'a mut Buffer,
    area: Rect,
    y: i32,
    skip: i32,
}

impl Lines<'_> {
    fn row(&mut self) -> Option<u16> {
        let y = self.y - self.skip;
        self.y += 1;
        (y >= 0 && (y as u16) < self.area.height).then(|| self.area.y + y as u16)
    }
    fn kv(&mut self, k: &str, v: &str, vs: Style, th: &Theme) {
        if let Some(y) = self.row() {
            let kw = 18.min(self.area.width / 2);
            text(self.buf, self.area.x, y, k, kw, th.muted());
            text(self.buf, self.area.x + kw, y, v, self.area.width - kw, vs);
        }
    }
    fn money(&mut self, k: &str, lamports: i64, note: &str, vs: Style, th: &Theme) {
        if let Some(y) = self.row() {
            let kw = 18.min(self.area.width / 2);
            text(self.buf, self.area.x, y, k, kw, th.muted());
            let v = thousands(lamports);
            let vx = self.area.x + kw + 12u16.saturating_sub(width(&v));
            text(self.buf, vx, y, &v, 12, vs);
            if !note.is_empty() {
                let nx = self.area.x + kw + 14;
                if nx < self.area.right() {
                    text(self.buf, nx, y, note, self.area.right() - nx, th.faint());
                }
            }
        }
    }
    fn line(&mut self, s: &str, st: Style) {
        if let Some(y) = self.row() {
            text(self.buf, self.area.x, y, s, self.area.width, st);
        }
    }
    fn gap(&mut self) {
        self.y += 1;
    }
}

pub fn inspector(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, focused: bool) {
    let th = &app.theme;
    let g = &app.glyphs;
    app.hit(area, Hit::Panel(Focus::Inspector));
    if app.focus == Focus::Graphs && app.timeline.ab().is_some() {
        return ab_inspector(buf, area, app, vm, focused);
    }
    let Some(o) = app.selected_opp(vm) else {
        let body = section(buf, area, "INSPECTOR", focused, "", th, g);
        text(buf, body.x, body.y, "no opportunity selected", body.width, th.faint());
        return;
    };
    let right = format!("{} · {}", o.id, o.strategy.label());
    let body = section(buf, area, "INSPECTOR", focused, &right, th, g);
    let mut l = Lines { buf, area: body, y: 0, skip: app.inspector_scroll as i32 };
    let tokens = searcher_core::token::TokenRegistry::defaults();
    let sym = |a: &searcher_core::Address| tokens.symbol(a);
    let dec = |a: &searcher_core::Address| tokens.decimals(a).unwrap_or(9);

    if let Some(y) = l.row() {
        let st = status_label(&o.status);
        let x = text(
            l.buf,
            body.x,
            y,
            &o.label,
            body.width.saturating_sub(width(&st) + 2),
            th.text().add_modifier(Modifier::BOLD),
        );
        let _ = x;
        text(l.buf, body.right().saturating_sub(width(&st)), y, &st, width(&st), status_style(&o.status, th));
    }
    l.line("Route", th.muted());
    let n = o.route.legs.len();
    for (i, leg) in o.route.legs.iter().enumerate() {
        let tree = if i + 1 == n { g.tree_end } else { g.tree_mid };
        let dexes = leg.dex_labels().join("+");
        let s = format!(
            "{tree} {dexes:<14} {} {} {}  {} {} {}",
            sym(&leg.input_mint),
            g.arrow,
            sym(&leg.output_mint),
            format_atoms(leg.in_amount as i128, dec(&leg.input_mint), 6),
            g.arrow,
            format_atoms(leg.out_amount as i128, dec(&leg.output_mint), 6),
        );
        l.line(&s, th.text());
        let hops = leg.hops.len();
        let detail = format!(
            "   {} hop{} · slip {}bp {} · impact {} · {} · {}ms",
            hops,
            if hops == 1 { "" } else { "s" },
            leg.slippage_bps,
            match leg.slippage_spec {
                SlippageSpec::Rtse => "rtse",
                SlippageSpec::Fixed(_) => "fixed",
            },
            leg.price_impact,
            match leg.mode {
                RoutingMode::Fast => "fast",
                RoutingMode::Normal => "normal",
            },
            leg.latency_ms
        );
        l.line(&detail, th.faint());
    }
    l.gap();
    let base = &o.base_mint;
    l.kv("Input", &format!("{} {}", format_atoms(o.input as i128, dec(base), 9), sym(base)), th.text(), th);
    if !is_priced(o) {
        let quoted = o.route.legs.len();
        l.kv(
            "Route incomplete",
            &format!("{quoted} leg(s) quoted before {} — no output, no edge", status_label(&o.status)),
            th.warn(),
            th,
        );
        return;
    }
    l.kv(
        "Expected output",
        &format!("{} {}", format_atoms(o.gross_output as i128, dec(base), 9), sym(base)),
        th.text(),
        th,
    );
    l.kv(
        "Gross edge",
        &format!("{}   {} lamports", edge(o.eval.gross_edge), signed_thousands(o.eval.gross_pnl)),
        th.pnl(o.eval.gross_pnl as f64),
        th,
    );
    l.gap();
    let c = &o.costs;
    l.kv("LP / swap cost", "embedded in quoted output", th.faint(), th);
    l.money("Base fee", c.base_fee as i64, &format!("{} sig × 5000", c.signatures), th.text(), th);
    let cu_note = match c.compute_units_used {
        Some(u) => format!(
            "{} CU used → {} limit × {} µL",
            thousands(u as i64),
            thousands(c.compute_units_limit as i64),
            thousands(c.compute_unit_price_micro as i64)
        ),
        None => format!(
            "est {} CU × {} µL",
            thousands(c.compute_units_limit as i64),
            thousands(c.compute_unit_price_micro as i64)
        ),
    };
    l.money("Priority fee", c.priority_fee as i64, &cu_note, th.text(), th);
    l.money("Jito tip", c.jito_tip as i64, "policy · if sent now", th.text(), th);
    if c.ata_rent > 0 {
        l.money("ATA rent", c.ata_rent as i64, &format!("{} account(s)", c.atas_created), th.text(), th);
    }
    l.money("Slippage buffer", c.expected_slippage as i64, "share of min-out tolerance", th.text(), th);
    l.money("Safety buffer", c.safety_buffer as i64, "", th.text(), th);
    if let Some(y) = l.row() {
        for x in body.x..body.right().min(body.x + 40) {
            put(l.buf, x, y, g.h, th.rule());
        }
    }
    let usd = o.eval.expected_net_usd.map(|u| u.to_string()).unwrap_or_default();
    if let Some(y) = l.row() {
        let kw = 18.min(body.width / 2);
        text(l.buf, body.x, y, "NET", kw, th.text().add_modifier(Modifier::BOLD));
        let v = format!("{}   {}   {}", thousands(o.eval.expected_net), edge(o.eval.net_edge), usd);
        text(
            l.buf,
            body.x + kw,
            y,
            &v,
            body.width - kw,
            th.pnl(o.eval.expected_net as f64).add_modifier(Modifier::BOLD),
        );
    }
    if let Some(s) = o.eval.simulated_net {
        l.kv("Simulated net", &format!("{} lamports (balance delta − buffers)", thousands(s)), th.pnl(s as f64), th);
    }
    l.gap();
    let now = now_ts(vm);
    l.kv("Quote age", &age(o.quote_age_ms(now)), th.text(), th);
    match &o.simulation {
        Some(sim) => {
            let v = if sim.ok {
                format!(
                    "pass · {} CU · {:?}/{:?} · {}ms",
                    thousands(sim.units_consumed() as i64),
                    sim.plan,
                    sim.fidelity,
                    sim.latency_ms
                )
            } else {
                format!("fail · {}", sim.failure.as_ref().map(|f| f.class.label()).unwrap_or("?"))
            };
            l.kv("Simulation", &v, if sim.ok { th.text() } else { Style::new().fg(th.loss) }, th);
            if let Some(t) = sim.txs.first() {
                l.kv("Transaction", &format!("{} B · {} accounts", t.size_bytes, t.accounts), th.text(), th);
            }
            if let Some(f) = &sim.failure {
                l.line(&format!("  {}", f.message), th.faint());
            }
        }
        None => l.kv("Simulation", "not simulated", th.faint(), th),
    }
    l.kv("CU used", &c.compute_units_used.map(|u| thousands(u as i64)).unwrap_or_else(|| "—".into()), th.text(), th);
    l.kv("Slot", &o.slot.map(|s| s.to_string()).unwrap_or_else(|| "—".into()), th.text(), th);
    let expiry = match (o.route.legs.iter().map(|l| l.last_valid_block_height).min(), vm.block_height) {
        (Some(lv), Some(bh)) if lv > 0 => format!("in {} blocks", lv as i64 - bh as i64),
        _ => "—".into(),
    };
    l.kv("Blockhash expiry", &expiry, th.text(), th);
    if let Some(r) = &o.risk {
        let v = if r.approved {
            "pass".to_string()
        } else {
            r.violations.iter().map(|v| v.code()).collect::<Vec<_>>().join(", ")
        };
        l.kv("Risk", &v, if r.approved { Style::new().fg(th.profit) } else { th.warn() }, th);
    }
}

pub fn ab_inspector(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, focused: bool) {
    let th = &app.theme;
    let g = &app.glyphs;
    let Some((a, b)) = app.timeline.ab() else { return };
    let body = section(buf, area, "A / B", focused, "x clear", th, g);
    let mut l = Lines { buf, area: body, y: 0, skip: 0 };
    l.line(&format!("A {}   {}   B {}", a.hms_millis(), g.arrow, b.hms_millis()), th.accent());
    l.kv("Δ time", &fmt_duration_us(b.0 - a.0), th.text().add_modifier(Modifier::BOLD), th);
    let delta = |m: MetricId| -> Option<(f64, f64)> {
        let s = vm.series(m)?;
        Some((s.value_at(a)?.1, s.value_at(b)?.1))
    };
    if let Some((va, vb)) = delta(MetricId::Price) {
        let pct = if va != 0.0 { (vb - va) / va * 100.0 } else { 0.0 };
        l.kv("Δ SOL price", &format!("{pct:+.3}%    {va:.3} {} {vb:.3}", g.arrow), th.pnl(vb - va), th);
    }
    for (m, name) in [
        (MetricId::Spread, "Δ spread"),
        (MetricId::NetEdge, "Δ net edge"),
        (MetricId::GrossEdge, "Δ gross edge"),
        (MetricId::JupiterLatency, "Δ Jupiter latency"),
        (MetricId::SimLatency, "Δ sim latency"),
    ] {
        if let Some((va, vb)) = delta(m) {
            l.kv(
                name,
                &format!(
                    "{}    {} {} {}",
                    m.unit().format_delta(vb - va),
                    m.unit().format(va),
                    g.arrow,
                    m.unit().format(vb)
                ),
                th.text(),
                th,
            );
        }
    }
    if let Some((va, vb)) = delta(MetricId::Pnl) {
        l.kv("Δ session PnL", &format!("{:+.4} USD", vb - va), th.pnl(vb - va), th);
    }
    if let Some((va, vb)) = delta(MetricId::JitoTip) {
        l.kv("Δ Jito tip", &format!("{:+} lamports", (vb - va) as i64), th.text(), th);
    }
    // Opportunities between A and B (expected PnL of the nearest ones).
    let (lo, hi) = (a.min(b), a.max(b));
    let within: Vec<&Opportunity> = vm.opps.values().filter(|o| o.detected_at >= lo && o.detected_at <= hi).collect();
    let near = |t: Ts| vm.opps.values().min_by_key(|o| (o.detected_at.0 - t.0).abs());
    if let (Some(oa), Some(ob)) = (near(a), near(b))
        && let (Some(ua), Some(ub)) = (oa.eval.expected_net_usd, ob.eval.expected_net_usd)
    {
        let d = searcher_core::UsdMicros(ub.0 - ua.0);
        l.kv("Δ expected PnL", &format!("{d}   ({} {} {})", oa.id, g.arrow, ob.id), th.pnl(d.0 as f64), th);
    }
    l.gap();
    let gross_pos = within.iter().filter(|o| o.eval.gross_pnl > 0).count();
    let execd = vm.markers.iter().filter(|m| m.kind == MarkerKind::Execution && m.ts >= lo && m.ts <= hi).count();
    let simfail = within.iter().filter(|o| o.simulation.as_ref().is_some_and(|s| !s.ok)).count();
    l.kv(
        "Between A and B",
        &format!("{} opps · {} gross+ · {} executed · {} sim fail", within.len(), gross_pos, execd, simfail),
        th.text(),
        th,
    );
    l.gap();
    l.line("Registered graphs  (value at A → B · min/max/avg in [A,B] · n)", th.faint());
    for e in &app.workspace.entries {
        let Some(s) = vm.series(e.metric) else { continue };
        let u = e.metric.unit();
        let va = s.value_at(a).map(|p| u.format(p.1)).unwrap_or_else(|| "--".into());
        let vb = s.value_at(b).map(|p| u.format(p.1)).unwrap_or_else(|| "--".into());
        let pts: Vec<f64> = s.range(lo, hi).map(|p| p.1).collect();
        let stats = if pts.is_empty() {
            "no samples".to_string()
        } else {
            let (mn, mx) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            let avg = pts.iter().sum::<f64>() / pts.len() as f64;
            format!("{}/{}/{} · {}", u.format(mn), u.format(mx), u.format(avg), pts.len())
        };
        l.line(&format!("  {:<16} {va} {} {vb}   {stats}", e.metric.label(), g.arrow), th.text());
    }
}

// ───────────────────────────── event stream ─────────────────────────────

pub fn stage_style(stage: Stage, ok: bool, th: &Theme) -> Style {
    match (stage, ok) {
        (Stage::Skip, _) => th.muted(),
        (Stage::Paper | Stage::Landed, true) => Style::new().fg(th.profit),
        (Stage::Paper | Stage::Landed, false) => Style::new().fg(th.loss),
        (Stage::Confirm | Stage::Submit, _) => th.accent_bold(),
        (_, false) => th.warn(),
        (Stage::Opportunity, true) => th.text(),
        _ => th.muted(),
    }
}

pub fn event_stream(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, focused: bool) {
    let th = &app.theme;
    let g = &app.glyphs;
    let right = if app.stream_offset > 0 {
        format!("scrolled {} · End live · ⏎ detail", app.stream_offset)
    } else {
        "live · ⏎ detail".into()
    };
    let body = section(buf, area, "EVENT STREAM", focused, &right, th, g);
    if body.height == 0 {
        return;
    }
    // Render newest at the bottom, blank line between opportunities.
    let mut lines: Vec<Option<(usize, &searcher_core::event::StageEvent)>> = Vec::new();
    let total = vm.stream.len();
    let mut prev: Option<OpportunityId> = None;
    let end = total.saturating_sub(app.stream_offset);
    let start = end.saturating_sub(body.height as usize * 2);
    for (i, s) in vm.stream.iter().enumerate().take(end).skip(start) {
        if prev.is_some_and(|p| p != s.opportunity) && s.stage == Stage::Opportunity {
            lines.push(None);
        }
        prev = Some(s.opportunity);
        lines.push(Some((i, s)));
    }
    let visible = lines.len().saturating_sub(body.height as usize);
    let sel = end.saturating_sub(1);
    let w = body.width;
    let (x_stage, x_subj) = (body.x + 10, body.x + 10 + 13);
    let val_w = 9u16;
    let subj_w = w.saturating_sub(10 + 13 + val_w + 1);
    app.hit(area, Hit::Panel(Focus::Stream));
    for (row, item) in lines.iter().skip(visible).enumerate() {
        let y = body.y + row as u16;
        let Some((i, s)) = item else { continue };
        app.hit(Rect { x: body.x, y, width: w, height: 1 }, Hit::Stream(total - 1 - i));
        if focused && *i == sel && app.stream_offset > 0 {
            fill(buf, Rect { x: body.x, y, width: w, height: 1 }, th.selected());
        }
        text(buf, body.x, y, &s.ts.hms(), 8, th.faint());
        // skips caused by the plumbing (rate limit, build failure, no route)
        // are dimmed; market skips (edge, slippage, risk…) stay amber
        let plumbing = s.stage == Stage::Skip
            && ["RATE_LIMITED", "BUILD_FAILED", "NO_ROUTE"].iter().any(|c| s.subject.starts_with(c));
        let stage_st = if plumbing { th.faint() } else { stage_style(s.stage, s.ok, th) };
        text(buf, x_stage, y, s.stage.label(), 12, stage_st);
        let subj_style = match (s.stage == Stage::Skip, plumbing) {
            (_, true) => th.faint(),
            (true, false) => th.warn(),
            _ => th.text(),
        };
        text(buf, x_subj, y, &s.subject, subj_w, subj_style);
        let vw = width(&s.value).min(val_w);
        let vs = if s.stage == Stage::Paper || s.stage == Stage::Opportunity || s.stage == Stage::Skip {
            if s.value.starts_with('+') {
                Style::new().fg(th.profit)
            } else if s.value.starts_with('-') {
                Style::new().fg(th.loss)
            } else {
                th.muted()
            }
        } else {
            th.muted()
        };
        text(buf, x_subj + subj_w + 1 + val_w - vw, y, &s.value, vw, vs);
    }
    if total == 0 {
        text(buf, body.x, body.y, "no events yet", w, th.faint());
    }
}

// ───────────────────────────── overlays ─────────────────────────────

pub fn overlay(buf: &mut Buffer, full: Rect, w: u16, h: u16, title: &str, th: &Theme, g: &Glyphs) -> Rect {
    let w = w.min(full.width.saturating_sub(2)).max(10);
    let h = h.min(full.height.saturating_sub(2)).max(4);
    let r = Rect { x: full.x + (full.width - w) / 2, y: full.y + (full.height - h) / 2, width: w, height: h };
    let bg = Style::new().bg(th.select_bg).fg(th.fg);
    // reset first: styling alone would keep modifiers (bold, …) of the page below
    ratatui::widgets::Widget::render(ratatui::widgets::Clear, r, buf);
    fill(buf, r, bg);
    // Thin frame: overlays are the one place a border helps (separates from page).
    let edge = th.rule().bg(th.select_bg);
    let (tl, tr, bl, br, hz, vt) =
        if g.unicode { ("╭", "╮", "╰", "╯", "─", "│") } else { ("+", "+", "+", "+", "-", "|") };
    for x in r.x + 1..r.right() - 1 {
        put(buf, x, r.y, hz, edge);
        put(buf, x, r.bottom() - 1, hz, edge);
    }
    for y in r.y + 1..r.bottom() - 1 {
        put(buf, r.x, y, vt, edge);
        put(buf, r.right() - 1, y, vt, edge);
    }
    put(buf, r.x, r.y, tl, edge);
    put(buf, r.right() - 1, r.y, tr, edge);
    put(buf, r.x, r.bottom() - 1, bl, edge);
    put(buf, r.right() - 1, r.bottom() - 1, br, edge);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) };
    text(buf, r.x + 2, r.y, &format!(" {title} "), r.width.saturating_sub(4), th.accent_bold().bg(th.select_bg));
    inner
}

pub fn wrap_text(buf: &mut Buffer, area: Rect, body: &str, style: Style) {
    let mut y = area.y;
    for raw in body.lines() {
        let mut line = raw.to_string();
        loop {
            if y >= area.bottom() {
                return;
            }
            let fit: String = line.chars().take(area.width as usize).collect();
            text(buf, area.x, y, &fit, area.width, style);
            y += 1;
            if line.chars().count() <= area.width as usize {
                break;
            }
            line = line.chars().skip(area.width as usize).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Depth;

    #[test]
    fn overlay_does_not_inherit_modifiers_from_the_page_below() {
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        fill(&mut buf, area, Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        let th = Theme::with_depth(Depth::TrueColor);
        let inner = overlay(&mut buf, area, 30, 8, "t", &th, &Glyphs::unicode());
        text(&mut buf, inner.x, inner.y + 1, "keys", 10, th.text().bg(th.select_bg));
        for x in inner.x..inner.x + 4 {
            assert!(buf[(x, inner.y + 1)].modifier.is_empty(), "{:?}", buf[(x, inner.y + 1)]);
        }
    }
}
