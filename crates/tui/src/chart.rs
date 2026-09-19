//! Time-series chart, drawn like the Markets page chart: values on a
//! right-hand axis with the latest value as a filled tag, a dotted line at
//! that value and dashed lines from the visible high and low with their
//! values; time axis, cursor, A/B guides, a marker lane on the baseline;
//! box-drawing / braille / ASCII lines and real-sample candles (half-cell).
//! Overlays never draw over the data. Everything is recomputed from the area
//! each frame (resize-safe); below 5 rows the chart degrades to a one-line
//! sparkline.

use crate::hub::{Marker, MarkerKind};
use crate::theme::{Glyphs, Theme};
use crate::workspace::{ChartStyle, LineStyle};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::Ts;
use searcher_core::metrics::Unit;
use searcher_core::series::TimeSeries;
use unicode_width::UnicodeWidthStr;

pub struct ChartInput<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    pub series: Option<&'a TimeSeries>,
    pub unit: Unit,
    pub t0: Ts,
    pub t1: Ts,
    pub style: ChartStyle,
    pub line: LineStyle,
    pub cursor: Option<Ts>,
    pub a: Option<Ts>,
    pub b: Option<Ts>,
    pub markers: &'a [Marker],
    pub active: bool,
    /// Shared value-label width so plots align across stacked graphs.
    pub y_label_w: u16,
    pub candle_min_samples: u32,
    /// Shown in an empty plot.
    pub empty_note: &'a str,
}

/// What was drawn (for tests and the inspector).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChartInfo {
    pub compact: bool,
    pub plot: Rect,
    pub y_range: Option<(f64, f64)>,
    pub candles_drawn: usize,
    pub candle_fallback: bool,
    /// Cells that contain the curve (x, y).
    pub curve_cells: Vec<(u16, u16)>,
}

const MIN_FULL_HEIGHT: u16 = 5;

pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    if let Some(c) = buf.cell_mut((x, y)) {
        c.set_symbol(s);
        c.set_style(style);
    }
}

fn is_empty(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y)).is_some_and(|c| c.symbol() == " ")
}

/// Write `s` clipped to `max_w` columns; returns columns written.
pub fn text(buf: &mut Buffer, x: u16, y: u16, s: &str, max_w: u16, style: Style) -> u16 {
    if max_w == 0 {
        return 0;
    }
    let s = crate::theme::fold(s);
    let (end_x, _) = buf.set_stringn(x, y, s.as_ref(), max_w as usize, style);
    end_x.saturating_sub(x)
}

pub fn width(s: &str) -> u16 {
    UnicodeWidthStr::width(crate::theme::fold(s).as_ref()) as u16
}

fn time_label(t: Ts, span_us: i64) -> String {
    if span_us > 2 * 3_600_000_000 { t.format("%H:%M") } else { t.hms() }
}

/// Median spacing between samples (µs) inside the window.
fn median_gap(pts: &[(Ts, f64)]) -> i64 {
    if pts.len() < 2 {
        return i64::MAX / 4;
    }
    let mut g: Vec<i64> = pts.windows(2).map(|w| w[1].0.0 - w[0].0.0).collect();
    g.sort_unstable();
    g[g.len() / 2].max(1)
}

/// One value per bucket: last sample in the bucket, else carry the previous
/// value forward if the gap is small (≤ 4× median spacing or 2 buckets).
/// Never extends past the last sample.
fn bucketize(series: &TimeSeries, t0: Ts, t1: Ts, n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; n];
    if n == 0 || t1 <= t0 {
        return out;
    }
    let dt = (t1.0 - t0.0) as f64 / n as f64;
    let pts: Vec<(Ts, f64)> = series.range(Ts(t0.0 - (dt as i64) * 8), t1).copied().collect();
    if pts.is_empty() {
        return out;
    }
    let max_gap = (median_gap(&pts) * 4).max((dt * 2.0) as i64);
    let last_ts = pts.last().unwrap().0;
    let mut j = 0;
    let mut prev: Option<(Ts, f64)> = None;
    for (i, slot) in out.iter_mut().enumerate() {
        let start = t0.0 as f64 + i as f64 * dt;
        let end = start + dt;
        let mut in_bucket = None;
        while j < pts.len() && (pts[j].0.0 as f64) < end {
            if (pts[j].0.0 as f64) >= start {
                in_bucket = Some(pts[j]);
            }
            prev = Some(pts[j]);
            j += 1;
        }
        *slot = match (in_bucket, prev) {
            (Some(p), _) => Some(p.1),
            (None, Some(p)) if (start as i64) <= last_ts.0 && (start as i64 - p.0.0) <= max_gap => Some(p.1),
            _ => None,
        };
    }
    out
}

fn nice_bucket(target_us: i64) -> i64 {
    const LADDER: [i64; 13] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1_800, 3_600];
    let s = (target_us / 1_000_000).max(1);
    LADDER.iter().copied().find(|l| *l >= s).unwrap_or(3_600) * 1_000_000
}

pub fn render_chart(area: Rect, buf: &mut Buffer, inp: &ChartInput<'_>, th: &Theme, g: &Glyphs) -> ChartInfo {
    let mut info = ChartInfo::default();
    crate::theme::set_ascii(!g.unicode);
    if area.width < 12 || area.height == 0 {
        return info;
    }
    let title_style = if inp.active { th.accent_bold() } else { th.text().add_modifier(Modifier::BOLD) };
    let span = (inp.t1.0 - inp.t0.0).max(1);

    // Values visible in the window (for header + range).
    let visible: Vec<(Ts, f64)> = inp.series.map(|s| s.range(inp.t0, inp.t1).copied().collect()).unwrap_or_default();
    let last = inp.series.and_then(|s| s.last());

    if area.height < MIN_FULL_HEIGHT {
        info.compact = true;
        let mut x = area.x;
        x += text(buf, x, area.y, inp.title, area.width.min(18), title_style) + 1;
        let tail = last.map(|(_, v)| inp.unit.format(v)).unwrap_or_else(|| "--".into());
        let tail_w = width(&tail) + 1;
        let spark_w = area.right().saturating_sub(x + tail_w);
        if let Some(s) = inp.series
            && spark_w > 0
        {
            let vals = bucketize(s, inp.t0, inp.t1, spark_w as usize);
            let known: Vec<f64> = vals.iter().flatten().copied().collect();
            let (lo, hi) = known.iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            for (i, v) in vals.iter().enumerate() {
                let sym = match v {
                    Some(v) if hi > lo => g.spark((v - lo) / (hi - lo)),
                    Some(_) => g.spark[3],
                    None => " ",
                };
                put(buf, x + i as u16, area.y, sym, if inp.active { th.accent() } else { th.muted() });
            }
        }
        text(buf, area.right().saturating_sub(tail_w - 1), area.y, &tail, tail_w, th.text());
        return info;
    }

    // Geometry: plot, then the value axis on the right (labels and tag).
    let aw = (inp.y_label_w.max(4) + 3).min(area.width / 3);
    let plot = Rect { x: area.x, y: area.y + 1, width: area.width.saturating_sub(aw), height: area.height - 3 };
    let axis_x = plot.right();
    info.plot = plot;
    let base_y = plot.bottom();
    let xlab_y = base_y + 1;

    // Header: title · subtitle ............ ▸ cursor · min · max. The latest
    // value is the tag on the axis; the subtitle shows only if it fits whole.
    text(buf, area.x, area.y, inp.title, area.width, title_style);
    let (lo_v, hi_v) = visible.iter().fold((f64::MAX, f64::MIN), |(a, b), (_, v)| (a.min(*v), b.max(*v)));
    let mut parts: Vec<String> = Vec::new();
    if let (Some(c), Some(s)) = (inp.cursor, inp.series)
        && let Some((t, v)) = s.value_at(c)
    {
        let approx = if t == c { "" } else { "≈" };
        parts.push(format!("▸ {approx}{} @ {}", inp.unit.format(v), c.hms()));
    }
    if !visible.is_empty() {
        parts.push(format!("min {}  max {}", inp.unit.format(lo_v), inp.unit.format(hi_v)));
    }
    let title_end = area.x + width(inp.title) + 2;
    let mut readout_x = area.right();
    while !parts.is_empty() {
        let right = parts.join("   ");
        let rw = width(&right);
        if title_end + rw < area.right() {
            readout_x = area.right() - rw;
            text(buf, readout_x, area.y, &right, rw, th.muted());
            break;
        }
        parts.pop();
    }
    if !inp.subtitle.is_empty() && title_end + width(inp.subtitle) + 2 <= readout_x {
        text(buf, title_end, area.y, inp.subtitle, width(inp.subtitle), th.faint());
    }

    // Axes: the value axis on the right, the baseline (marker lane) below.
    for y in plot.y..base_y {
        put(buf, axis_x, y, g.axis_v, th.rule());
    }
    put(buf, axis_x, base_y, if g.unicode { "┘" } else { "+" }, th.rule());
    for x in plot.x..plot.right() {
        put(buf, x, base_y, g.h, th.rule());
    }

    let Some(series) = inp.series.filter(|_| !visible.is_empty()) else {
        let msg = if inp.empty_note.is_empty() { "no samples in window" } else { inp.empty_note };
        let mx = plot.x + plot.width.saturating_sub(width(msg)) / 2;
        text(buf, mx, plot.y + plot.height / 2, msg, plot.width, th.faint());
        draw_time_axis(buf, plot, xlab_y, inp.t0, inp.t1, span, th, area);
        return info;
    };

    // Y range with 5% padding; flat series get a small band.
    let (mut lo, mut hi) = (lo_v, hi_v);
    if (hi - lo).abs() < f64::EPSILON {
        let pad = (lo.abs() * 0.0005).max(1e-6);
        lo -= pad;
        hi += pad;
    } else {
        let pad = (hi - lo) * 0.05;
        lo -= pad;
        hi += pad;
    }
    info.y_range = Some((lo, hi));
    let ph = plot.height.max(1);
    let row_of = |v: f64| -> u16 {
        let f = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        base_y - 1 - ((f * (ph - 1) as f64).round() as u16).min(ph - 1)
    };

    // Value labels on the right axis.
    crate::kline::value_axis(buf, plot, aw, (lo, hi), &|v| inp.unit.format(v), th, g);
    draw_time_axis(buf, plot, xlab_y, inp.t0, inp.t1, span, th, area);

    let line_style = if inp.active { th.accent() } else { th.text() };
    let col_of = |t: Ts| -> Option<u16> {
        if t < inp.t0 || t > inp.t1 {
            return None;
        }
        let f = (t.0 - inp.t0.0) as f64 / span as f64;
        Some(plot.x + ((f * plot.width as f64) as u16).min(plot.width.saturating_sub(1)))
    };

    // Series.
    let mut drew_candles = false;
    if inp.style == ChartStyle::Candle {
        let n = (plot.width / 2).max(1) as i64;
        let bucket = nice_bucket(span / n);
        let candles = series.ohlc(inp.t0, inp.t1, bucket);
        let mut samples: Vec<u32> = candles.iter().map(|c| c.samples).collect();
        samples.sort_unstable();
        let median = samples.get(samples.len() / 2).copied().unwrap_or(0);
        if !candles.is_empty() && median >= inp.candle_min_samples {
            drew_candles = true;
            let halves = ph as i64 * 2;
            let half = |v: f64| -> i64 {
                (((hi - v) / (hi - lo)) * (halves - 1) as f64).round().clamp(0.0, (halves - 1) as f64) as i64
            };
            for c in &candles {
                let Some(x) = col_of(Ts(c.start.0 + bucket / 2).min(inp.t1)) else { continue };
                let st = th.pnl(if c.close >= c.open { 1.0 } else { -1.0 });
                crate::kline::draw_candle(buf, x, plot, &half, (c.open, c.high, c.low, c.close), st, g.unicode);
                let (rh, rl) = (row_of(c.high), row_of(c.low));
                info.curve_cells.extend((rh..=rl).map(|y| (x, y)));
                info.candles_drawn += 1;
            }
        } else {
            info.candle_fallback = true;
        }
    }
    if info.candle_fallback {
        let note = format!("line view: <{} real samples per candle", inp.candle_min_samples);
        text(buf, plot.x + 1, plot.y, &note, plot.width.saturating_sub(1), th.faint());
    }
    if !drew_candles {
        match (inp.line, g.unicode) {
            (LineStyle::Braille, true) => draw_braille(buf, plot, series, inp, lo, hi, line_style, &mut info),
            _ => draw_box(buf, plot, series, inp, &row_of, g, line_style, &mut info),
        }
    }

    // Overlays (empty cells only): a dotted line at the latest value when it
    // is in view, dashed lines from the visible high and low.
    let latest = last.filter(|(t, _)| *t >= inp.t0 && *t <= inp.t1);
    let up = last.is_some_and(|(_, v)| series.iter().rev().map(|p| p.1).find(|p| *p != v).is_none_or(|p| v >= p));
    let dir = Style::new().fg(if up { th.profit } else { th.loss });
    if let Some((_, v)) = latest {
        crate::kline::live_line(buf, plot, row_of(v), dir, g);
    }
    let hi_p =
        visible.iter().fold(None, |m: Option<(Ts, f64)>, p| if m.is_none_or(|m| p.1 > m.1) { Some(*p) } else { m });
    let lo_p =
        visible.iter().fold(None, |m: Option<(Ts, f64)>, p| if m.is_none_or(|m| p.1 < m.1) { Some(*p) } else { m });
    if let (Some(h), Some(l)) = (hi_p, lo_p)
        && row_of(h.1) != row_of(l.1)
        && plot.height >= 4
    {
        for ((t, v), st) in [(h, th.muted()), (l, Style::new().fg(th.accent))] {
            if let Some(x) = col_of(t) {
                crate::kline::extreme_line(buf, plot, (x, row_of(v)), &inp.unit.format(v), st, g);
            }
        }
    }

    // Guides: only into empty cells, so they never cover the curve.
    let mut guide = |t: Option<Ts>, style: Style| {
        if let Some(x) = t.and_then(col_of) {
            for y in plot.y..base_y {
                if is_empty(buf, x, y) {
                    put(buf, x, y, g.guide, style);
                }
            }
        }
    };
    guide(inp.a, th.accent_dim_style());
    guide(inp.b, th.accent_dim_style());
    if let Some(x) = inp.cursor.and_then(col_of) {
        for y in plot.y..base_y {
            if is_empty(buf, x, y) {
                put(buf, x, y, g.cursor, th.faint());
            }
        }
    }
    // Selected sample: highlight the curve cell at the cursor column.
    if let Some(x) = inp.cursor.and_then(col_of) {
        for (cx, cy) in info.curve_cells.iter().filter(|(cx, _)| *cx == x) {
            if let Some(c) = buf.cell_mut((*cx, *cy)) {
                c.set_style(th.accent_bold().add_modifier(Modifier::REVERSED));
            }
        }
    }

    // Marker lane on the baseline: A/B > execution > entry/exit > opportunity.
    let mut lane: Vec<(u16, u8, &str, Style)> = Vec::new();
    for m in inp.markers.iter().filter(|m| m.ts >= inp.t0 && m.ts <= inp.t1) {
        if let Some(x) = col_of(m.ts) {
            let (p, s, st) = match m.kind {
                MarkerKind::Execution => (3, g.marker_exec, Style::new().fg(th.profit)),
                MarkerKind::Entry => (2, g.marker_entry, th.muted()),
                MarkerKind::Exit => (2, g.marker_exit, th.muted()),
                MarkerKind::Opportunity => (1, g.marker_opp, th.faint()),
            };
            lane.push((x, p, s, st));
        }
    }
    for (t, s) in [(inp.a, "A"), (inp.b, "B")] {
        if let Some(x) = t.and_then(col_of) {
            lane.push((x, 9, s, th.accent_bold()));
        }
    }
    lane.sort_by_key(|(x, p, _, _)| (*x, *p));
    for (x, _, s, st) in lane {
        put(buf, x, base_y, s, st);
    }

    // The latest value as a tag on the axis (at the plot edge when it is
    // outside the visible range).
    if let Some((_, v)) = last {
        crate::kline::value_tag(buf, axis_x, aw, row_of(v), &inp.unit.format(v), dir, th, g);
    }
    info
}

impl Theme {
    pub fn accent_dim_style(&self) -> Style {
        Style::new().fg(self.accent_dim)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_time_axis(buf: &mut Buffer, plot: Rect, y: u16, t0: Ts, t1: Ts, span: i64, th: &Theme, area: Rect) {
    if y >= area.bottom() {
        return;
    }
    let l0 = time_label(t0, span);
    let l1 = time_label(t1, span);
    let lm = time_label(Ts(t0.0 + span / 2), span);
    let w0 = width(&l0);
    text(buf, plot.x, y, &l0, plot.width, th.faint());
    let x1 = plot.right().saturating_sub(width(&l1));
    if x1 > plot.x + w0 + 1 {
        text(buf, x1, y, &l1, width(&l1), th.faint());
    }
    let xm = plot.x + plot.width / 2 - width(&lm) / 2;
    if xm > plot.x + w0 + 2 && xm + width(&lm) + 2 < x1 {
        text(buf, xm, y, &lm, width(&lm), th.faint());
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_box(
    buf: &mut Buffer,
    plot: Rect,
    series: &TimeSeries,
    inp: &ChartInput<'_>,
    row_of: &dyn Fn(f64) -> u16,
    g: &Glyphs,
    st: Style,
    info: &mut ChartInfo,
) {
    let vals = bucketize(series, inp.t0, inp.t1, plot.width as usize);
    let mut prev: Option<u16> = None;
    for (i, v) in vals.iter().enumerate() {
        let x = plot.x + i as u16;
        let Some(v) = v else {
            prev = None;
            continue;
        };
        let r = row_of(*v);
        match prev {
            None => {
                let isolated = vals.get(i + 1).is_none_or(|n| n.is_none());
                put(buf, x, r, if isolated { g.dot } else { g.h }, st);
                info.curve_cells.push((x, r));
            }
            Some(p) if p == r => {
                put(buf, x, r, g.h, st);
                info.curve_cells.push((x, r));
            }
            Some(p) => {
                let (at_old, at_new) = if r < p { g.up_turn } else { g.down_turn };
                put(buf, x, p, at_old, st);
                put(buf, x, r, at_new, st);
                let (a, b) = (p.min(r), p.max(r));
                for y in a + 1..b {
                    put(buf, x, y, g.v, st);
                }
                info.curve_cells.extend((a..=b).map(|y| (x, y)));
            }
        }
        prev = Some(r);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_braille(
    buf: &mut Buffer,
    plot: Rect,
    series: &TimeSeries,
    inp: &ChartInput<'_>,
    lo: f64,
    hi: f64,
    st: Style,
    info: &mut ChartInfo,
) {
    let (w, h) = (plot.width as usize * 2, plot.height as usize * 4);
    let vals = bucketize(series, inp.t0, inp.t1, w);
    let mut dots = vec![0u8; plot.width as usize * plot.height as usize];
    let dot_y =
        |v: f64| -> i64 { (h as f64 - 1.0 - ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * (h as f64 - 1.0)).round() as i64 };
    let mut set = |x: i64, y: i64| {
        if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
            return;
        }
        const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
        let (cx, cy) = (x as usize / 2, y as usize / 4);
        dots[cy * plot.width as usize + cx] |= BITS[x as usize % 2][y as usize % 4];
    };
    let mut prev: Option<(i64, i64)> = None;
    for (i, v) in vals.iter().enumerate() {
        match v {
            Some(v) => {
                let p = (i as i64, dot_y(*v));
                if let Some(q) = prev {
                    let steps = (p.1 - q.1).abs().max(p.0 - q.0).max(1);
                    for s in 0..=steps {
                        set(q.0 + (p.0 - q.0) * s / steps, q.1 + (p.1 - q.1) * s / steps);
                    }
                } else {
                    set(p.0, p.1);
                }
                prev = Some(p);
            }
            None => prev = None,
        }
    }
    for (i, d) in dots.iter().enumerate() {
        if *d != 0 {
            let x = plot.x + (i % plot.width as usize) as u16;
            let y = plot.y + (i / plot.width as usize) as u16;
            let ch = char::from_u32(0x2800 + *d as u32).unwrap_or(' ');
            put(buf, x, y, &ch.to_string(), st);
            info.curve_cells.push((x, y));
        }
    }
}

/// Inline sparkline string for tables (System page).
pub fn sparkline(values: &[u32], width: usize, g: &Glyphs) -> String {
    if values.is_empty() || width == 0 {
        return " ".repeat(width);
    }
    let tail: Vec<u32> = values.iter().rev().take(width).rev().copied().collect();
    let hi = *tail.iter().max().unwrap_or(&1) as f64;
    let lo = *tail.iter().min().unwrap_or(&0) as f64;
    let mut s: String =
        tail.iter().map(|v| if hi > lo { g.spark((*v as f64 - lo) / (hi - lo)) } else { g.spark[1] }).collect();
    let w = UnicodeWidthStr::width(s.as_str());
    if w < width {
        s = format!("{}{s}", " ".repeat(width - w));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Depth;

    fn series(n: i64, f: impl Fn(i64) -> f64) -> TimeSeries {
        let mut s = TimeSeries::default();
        for i in 0..n {
            s.push(Ts(i * 1_000_000), f(i));
        }
        s
    }

    fn input<'a>(s: &'a TimeSeries, markers: &'a [Marker]) -> ChartInput<'a> {
        ChartInput {
            title: "Price",
            subtitle: "SOL/USD · jupiter /build",
            series: Some(s),
            unit: Unit::Usd,
            t0: Ts(0),
            t1: Ts(119_000_000),
            style: ChartStyle::Line,
            line: LineStyle::Box,
            cursor: Some(Ts(60_000_000)),
            a: Some(Ts(20_000_000)),
            b: Some(Ts(90_000_000)),
            markers,
            active: true,
            y_label_w: 9,
            candle_min_samples: 3,
            empty_note: "",
        }
    }

    fn dump(buf: &Buffer) -> String {
        let a = buf.area;
        (a.y..a.bottom())
            .map(|y| (a.x..a.right()).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn full_chart_has_axes_labels_and_markers_do_not_cover_curve() {
        let s = series(120, |i| 105.0 + (i as f64 / 10.0).sin());
        let markers = vec![
            Marker { ts: Ts(30_000_000), kind: MarkerKind::Opportunity, opportunity: Default::default() },
            Marker { ts: Ts(31_000_000), kind: MarkerKind::Execution, opportunity: Default::default() },
        ];
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        let th = Theme::with_depth(Depth::TrueColor);
        let info = render_chart(area, &mut buf, &input(&s, &markers), &th, &Glyphs::unicode());
        let out = dump(&buf);
        assert!(!info.compact);
        assert!(out.contains("Price"));
        assert!(out.contains("┘"), "axis corner\n{out}");
        assert!(out.contains("├"), "value ticks on the right-hand axis");
        // the latest value (105 + sin(11.9)) as a filled tag right of the plot
        let tag = format!(" {} ", Unit::Usd.format(105.0 + (119f64 / 10.0).sin()));
        let (tx, ty) = (0..area.height)
            .find_map(|y| {
                out.lines()
                    .nth(y as usize)
                    .unwrap()
                    .find(&tag)
                    .map(|b| (out.lines().nth(y as usize).unwrap()[..b].chars().count() as u16, y))
            })
            .expect("tag");
        assert!(tx > info.plot.right() - 1 && buf[(tx + 1, ty)].modifier.contains(Modifier::REVERSED));
        assert!(out.contains('┈'), "dotted line at the latest value\n{out}");
        assert!(out.contains('←') || out.contains('→'), "high / low labels\n{out}");
        assert!(out.contains("╭") || out.contains("╮"), "box-drawing curve\n{out}");
        assert!(out.contains('A') && out.contains('B'), "A/B on baseline\n{out}");
        assert!(out.contains("▲"), "execution marker");
        // every curve cell still holds a curve glyph (guides never overwrite it)
        let curve = ["─", "│", "╭", "╮", "╯", "╰", "·"];
        for (x, y) in &info.curve_cells {
            assert!(curve.contains(&buf[(*x, *y)].symbol()), "curve cell ({x},{y}) overwritten");
        }
        // time labels on the last row
        let last_row = out.lines().last().unwrap();
        assert!(last_row.contains(':'), "{last_row}");
    }

    #[test]
    fn resize_recomputes_and_small_heights_degrade_to_sparkline() {
        let s = series(120, |i| i as f64);
        let th = Theme::with_depth(Depth::Ansi256);
        for (w, h) in [(30u16, 5u16), (60, 8), (100, 20), (160, 30)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            let info = render_chart(area, &mut buf, &input(&s, &[]), &th, &Glyphs::unicode());
            assert!(!info.compact);
            assert_eq!(info.plot.width, w - 12.min(w / 3), "plot width tracks area width");
            assert_eq!(info.plot.height, h - 3);
        }
        let area = Rect::new(0, 0, 50, 2);
        let mut buf = Buffer::empty(area);
        let info = render_chart(area, &mut buf, &input(&s, &[]), &th, &Glyphs::unicode());
        assert!(info.compact);
        assert!(dump(&buf).contains('█') || dump(&buf).contains('▇'));
    }

    #[test]
    fn candles_only_from_enough_real_samples() {
        let th = Theme::with_depth(Depth::TrueColor);
        let dense = series(120, |i| 100.0 + (i % 7) as f64);
        let area = Rect::new(0, 0, 40, 12);
        let mut inp = input(&dense, &[]);
        inp.style = ChartStyle::Candle;
        let mut buf = Buffer::empty(area);
        let info = render_chart(area, &mut buf, &inp, &th, &Glyphs::unicode());
        assert!(info.candles_drawn > 0 && !info.candle_fallback);
        // sparse: one sample per 30 s → not enough for candles → line fallback
        let mut sparse = TimeSeries::default();
        for i in 0..4 {
            sparse.push(Ts(i * 30_000_000), 100.0 + i as f64);
        }
        let mut inp = input(&sparse, &[]);
        inp.style = ChartStyle::Candle;
        let area = Rect::new(0, 0, 120, 12);
        let mut buf = Buffer::empty(area);
        let info = render_chart(area, &mut buf, &inp, &th, &Glyphs::unicode());
        assert!(info.candle_fallback);
        assert_eq!(info.candles_drawn, 0);
    }

    #[test]
    fn ascii_and_braille_modes() {
        let s = series(120, |i| (i as f64 / 7.0).cos());
        let th = Theme::with_depth(Depth::None);
        let area = Rect::new(0, 0, 70, 10);
        let mut buf = Buffer::empty(area);
        render_chart(area, &mut buf, &input(&s, &[]), &th, &Glyphs::ascii());
        let out = dump(&buf);
        assert!(out.is_ascii(), "ascii mode must not emit unicode:\n{out}");
        let mut inp = input(&s, &[]);
        inp.line = LineStyle::Braille;
        let mut buf = Buffer::empty(area);
        let info = render_chart(area, &mut buf, &inp, &th, &Glyphs::unicode());
        assert!(!info.curve_cells.is_empty());
        assert!(dump(&buf).chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)));
    }

    #[test]
    fn empty_window_says_so() {
        let s = TimeSeries::default();
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        render_chart(area, &mut buf, &input(&s, &[]), &Theme::with_depth(Depth::None), &Glyphs::unicode());
        assert!(dump(&buf).contains("no samples in window"));
    }
}
