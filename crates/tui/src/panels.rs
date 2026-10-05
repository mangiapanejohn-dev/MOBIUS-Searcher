//! Reusable panels: section headers, opportunity table, inspector, A/B delta
//! inspector, event stream, overlays. Terminal-native: section titles and
//! thin rules instead of boxed cards.

use crate::app::{App, Focus, Hit};
use crate::chart::{put, text, text_fit, width};
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

/// Key and value in two left-aligned columns, so the rows of a block line up.
/// A value too long for its column starts right after the key when that fits.
pub fn kv_cols(buf: &mut Buffer, x: u16, y: u16, w: u16, k: &str, v: &str, th: &Theme) {
    let mut kw = 34.min(w / 2);
    if width(v) > w - kw && width(k) + 2 + width(v) <= w {
        kw = width(k) + 2;
    }
    text_fit(buf, x, y, k, kw.saturating_sub(1), th.muted());
    text_fit(buf, x + kw, y, v, w - kw, th.text());
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
    let show_status = w >= 66;
    let show_age = w >= 40;
    let (cg, cn, ca, cs) = (7u16, 7u16, if show_age { 5 } else { 0 }, if show_status { 15 } else { 0 });
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
        text_fit(buf, xr, y, &label, route_w, route_style);
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
            text_fit(buf, xs, y, &status_label(&o.status), cs, st);
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
            text_fit(self.buf, self.area.x + kw, y, v, self.area.width - kw, vs);
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
                    text_fit(self.buf, nx, y, note, self.area.right() - nx, th.faint());
                }
            }
        }
    }
    fn line(&mut self, s: &str, st: Style) {
        if let Some(y) = self.row() {
            text_fit(self.buf, self.area.x, y, s, self.area.width, st);
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
        // capital locked in accounts left created: shown, not subtracted from net
        l.money("Deposit", c.ata_rent as i64, "account rent · capital, not in net", th.muted(), th);
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
        text_fit(buf, x_subj, y, &s.subject, subj_w, subj_style);
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
    // a wide character of the page that ends inside the box would be drawn over its edge: it gives way
    if r.x > full.x {
        for y in r.y..r.bottom() {
            if width(buf[(r.x - 1, y)].symbol()) > 1 {
                buf[(r.x - 1, y)].set_symbol(" ");
            }
        }
    }
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

/// Keys of an overlay, on its bottom border (`inner` as [`overlay`] returned it).
pub fn overlay_hint(buf: &mut Buffer, inner: Rect, hint: &str, th: &Theme) {
    let w = width(hint) + 2;
    if w + 2 <= inner.width {
        text(buf, inner.right() - w, inner.bottom(), &format!(" {hint} "), w, th.muted().bg(th.select_bg));
    }
}

/// `s` broken between words (and between Chinese characters) into lines of at most `w` columns.
pub fn wrap_words(s: &str, w: u16) -> Vec<String> {
    // a word, or one wide character (Chinese breaks between any two), with what
    // may not open a line (its punctuation) kept on the one before
    let closes = |c: char| "，。；：、！？）》」％%".contains(c);
    let mut atoms: Vec<(bool, String)> = Vec::new();
    let mut spaced = false;
    for c in s.chars() {
        if c == ' ' {
            spaced = true;
            continue;
        }
        let wide = |c: char| width(c.encode_utf8(&mut [0; 4])) > 1;
        match atoms.last_mut() {
            Some((_, last)) if !spaced && (closes(c) || !(wide(c) || last.chars().last().is_some_and(wide))) => {
                last.push(c)
            }
            _ => atoms.push((spaced, c.to_string())),
        }
        spaced = false;
    }
    let mut out = vec![String::new()];
    for (spaced, atom) in atoms {
        let line = out.last_mut().expect("starts with one line");
        if line.is_empty() {
            line.push_str(&atom);
        } else if width(line) + u16::from(spaced) + width(&atom) <= w {
            if spaced {
                line.push(' ');
            }
            line.push_str(&atom);
        } else {
            out.push(atom);
        }
    }
    out
}

/// What a row of a detail is, for how it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doc {
    /// `# …`: a heading (a day, a part).
    Head,
    /// `## …`: one entry under it.
    Sub,
    /// `name<TAB>value`: a named value; the name stands on its first row only.
    Field,
    Text,
    Blank,
    /// A table (`| a | b |` lines, as in Markdown): a line of its frame, its heading row, a row of its cells.
    TableRule,
    TableHead,
    TableRow,
}

/// The cells of a `| a | b |` line.
fn cells_of(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|');
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// A Markdown table as rows to draw, framed, in `w` columns: its heading
/// (when its second line is `|---|`), then its rows. Every column is as wide
/// as its widest cell but the last, which takes what is left and wraps. A row
/// whose first cell is empty goes on with the row over it; one whose first
/// cell is not starts under a line of its own.
fn table_rows(lines: &[&str], w: u16, unicode: bool) -> Vec<(Doc, String, String)> {
    let mut rows: Vec<Vec<String>> = lines.iter().map(|l| cells_of(l)).collect();
    let ruled =
        |r: &Vec<String>| !r.is_empty() && r.iter().all(|c| !c.is_empty() && c.chars().all(|x| x == '-' || x == ':'));
    let head = if rows.len() >= 2 && ruled(&rows[1]) {
        rows.remove(1);
        Some(rows.remove(0))
    } else {
        None
    };
    let n = rows.iter().chain(&head).map(Vec::len).max().unwrap_or(0);
    if n == 0 {
        return Vec::new();
    }
    let cell = |r: &Vec<String>, i: usize| r.get(i).cloned().unwrap_or_default();
    let mut widths: Vec<u16> =
        (0..n).map(|i| rows.iter().chain(&head).map(|r| width(&cell(r, i))).max().unwrap_or(0).max(1)).collect();
    // the frame takes a bar and two spaces a column and one bar more; the last column has the rest
    let frame = 3 * n as u16 + 1;
    let others: u16 = widths[..n - 1].iter().sum();
    let last = w.saturating_sub(frame + others);
    if last >= 12 || n == 1 {
        widths[n - 1] = widths[n - 1].min(last.max(1));
    } else {
        // too narrow for that: every column an equal share
        let share = (w.saturating_sub(frame) / n as u16).max(1);
        widths = vec![share; n];
    }
    let (v, h) = if unicode { ("│", "─") } else { ("|", "-") };
    let rule = |l: &str, m: &str, r: &str| {
        let (l, m, r) = if unicode { (l, m, r) } else { ("+", "+", "+") };
        let bars: Vec<String> = widths.iter().map(|x| h.repeat(*x as usize + 2)).collect();
        (Doc::TableRule, String::new(), format!("{l}{}{r}", bars.join(m)))
    };
    let draw = |kind: Doc, r: &Vec<String>| -> Vec<(Doc, String, String)> {
        let wrapped: Vec<Vec<String>> = (0..n).map(|i| wrap_hard(&cell(r, i), widths[i])).collect();
        let tall = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        (0..tall)
            .map(|k| {
                let parts: Vec<String> = (0..n)
                    .map(|i| {
                        let text = wrapped[i].get(k).cloned().unwrap_or_default();
                        let pad = (widths[i] as usize).saturating_sub(width(&text) as usize);
                        format!(" {text}{} ", " ".repeat(pad))
                    })
                    .collect();
                (kind, String::new(), format!("{v}{}{v}", parts.join(v)))
            })
            .collect()
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    if let Some(h) = &head {
        out.extend(draw(Doc::TableHead, h));
        out.push(rule("├", "┼", "┤"));
    }
    for (i, r) in rows.iter().enumerate() {
        if i > 0 && !cell(r, 0).is_empty() {
            out.push(rule("├", "┼", "┤"));
        }
        out.extend(draw(Doc::TableRow, r));
    }
    out.push(rule("└", "┴", "┘"));
    out
}

/// `s` in rows of at most `w` columns: between words where it can, and
/// through a word that is longer than a row (a signature, an address).
pub fn wrap_hard(s: &str, w: u16) -> Vec<String> {
    let w = w.max(2);
    let mut out = Vec::new();
    for line in wrap_words(s, w) {
        if width(&line) <= w {
            out.push(line);
            continue;
        }
        let mut row = String::new();
        for c in line.chars() {
            if width(&row) + width(c.encode_utf8(&mut [0; 4])) > w {
                out.push(std::mem::take(&mut row));
            }
            row.push(c);
        }
        out.push(row);
    }
    out
}

/// A detail's text as rows to draw: what each is, its name (a field's first
/// row) and its text. The text is plain, with marks borrowed from Markdown:
/// `# ` a heading, `## ` an entry, `| a | b |` lines a table, and a tab
/// between a name and its value. Returns the rows and the width of the names' column.
pub fn doc_rows(body: &str, w: u16, unicode: bool) -> (Vec<(Doc, String, String)>, u16) {
    let names = body.lines().filter_map(|l| l.split_once('\t')).map(|(k, _)| width(k));
    let key_w = names.max().map_or(0, |m| (m + 3).min(26)).min(w / 2);
    let lines: Vec<&str> = body.lines().collect();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with('|') {
            let end = lines[i..].iter().position(|l| !l.trim_start().starts_with('|')).map_or(lines.len(), |n| i + n);
            rows.extend(table_rows(&lines[i..end], w, unicode));
            i = end;
            continue;
        }
        if line.trim().is_empty() {
            rows.push((Doc::Blank, String::new(), String::new()));
        } else if let Some(h) = line.strip_prefix("## ") {
            rows.extend(wrap_hard(h, w).into_iter().map(|l| (Doc::Sub, String::new(), l)));
        } else if let Some(h) = line.strip_prefix("# ") {
            rows.extend(wrap_hard(h, w).into_iter().map(|l| (Doc::Head, String::new(), l)));
        } else if let Some((k, v)) = line.split_once('\t') {
            for (n, l) in wrap_hard(v, w.saturating_sub(key_w + 2)).into_iter().enumerate() {
                rows.push((Doc::Field, if n == 0 { k.to_string() } else { String::new() }, l));
            }
        } else {
            rows.extend(wrap_hard(line, w).into_iter().map(|l| (Doc::Text, String::new(), l)));
        }
        i += 1;
    }
    (rows, key_w)
}

/// `body` as lines of at most `w` columns (long lines continue on the next).
pub fn wrap_lines(body: &str, w: u16) -> Vec<String> {
    let w = w.max(1) as usize;
    let mut out = Vec::new();
    for raw in body.lines() {
        let chars: Vec<char> = raw.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        for part in chars.chunks(w) {
            out.push(part.iter().collect());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detail_is_rows_with_headings_entries_and_named_values() {
        let body = "# 2026-10-05\n\n## 05:15:14  Bought\nAmount\t0.016542 SOL\nSignature\tABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef\nplain words here";
        let (rows, key_w) = doc_rows(body, 30, true);
        assert_eq!(key_w, 12, "the longest name and room after it");
        let kinds: Vec<Doc> = rows.iter().map(|r| r.0).collect();
        assert_eq!(kinds, [Doc::Head, Doc::Blank, Doc::Sub, Doc::Field, Doc::Field, Doc::Field, Doc::Field, Doc::Text]);
        assert_eq!((rows[3].1.as_str(), rows[3].2.as_str()), ("Amount", "0.016542 SOL"));
        // a value longer than its column goes on under itself, its name said once, nothing of it lost
        assert_eq!((rows[4].1.as_str(), rows[5].1.as_str()), ("Signature", ""));
        let whole: String = rows[4..7].iter().map(|r| r.2.as_str()).collect();
        assert_eq!(whole, "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef");
        assert!(rows.iter().all(|r| width(&r.2) <= 30));
        assert_eq!(wrap_hard("一二三四五六", 6), ["一二三", "四五六"]);
    }

    #[test]
    fn a_markdown_table_is_drawn_framed_with_its_last_column_wrapping() {
        let body = "| 时间 | 事件 | 内容 |\n|---|---|---|\n| 05:15 | 买入 | 0.016542 SOL |\n| | | 花费 2.0000 USDC，均价 120.90 |\n| 07:30 | 卖出 | 得到 2.0100 USDC |";
        let (rows, _) = doc_rows(body, 44, true);
        let text: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(
            text,
            [
                "┌───────┬──────┬───────────────────────────┐",
                "│ 时间  │ 事件 │ 内容                      │",
                "├───────┼──────┼───────────────────────────┤",
                "│ 05:15 │ 买入 │ 0.016542 SOL              │",
                "│       │      │ 花费 2.0000 USDC，均价    │",
                "│       │      │ 120.90                    │",
                "├───────┼──────┼───────────────────────────┤",
                "│ 07:30 │ 卖出 │ 得到 2.0100 USDC          │",
                "└───────┴──────┴───────────────────────────┘",
            ],
            "{text:#?}"
        );
        assert_eq!(rows[1].0, Doc::TableHead);
        assert!(rows.iter().all(|r| width(&r.2) == 44), "every line as wide as the frame");
        // without the box characters it is still a table
        let (ascii, _) = doc_rows("| a | b |\n|---|---|\n| 1 | 2 |", 20, false);
        assert_eq!(ascii[0].2, "+---+---+");
        assert_eq!(ascii[3].2, "| 1 | 2 |");
    }

    #[test]
    fn words_wrap_between_words_and_chinese_between_characters() {
        assert_eq!(wrap_words("one two three", 7), ["one two", "three"]);
        // no spaces to break at: it breaks between characters, and a comma never opens a line
        assert_eq!(wrap_words("现价 120.77，还要再涨", 13), ["现价 120.77，", "还要再涨"]);
        assert_eq!(wrap_words("等待买入等待", 8), ["等待买入", "等待"]);
    }
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
