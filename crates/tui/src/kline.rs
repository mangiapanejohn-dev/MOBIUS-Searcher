//! Exchange-style price chart: one column per candle, newest at the right
//! edge, as a close-price line (default) or candles (bodies and wicks in
//! half-cell glyphs). Overlays: VWMA(20) in orange, a dotted line at the live
//! price, dashed lines from the visible high and low with their prices, and
//! the live price as a filled tag on the right-hand axis with the time left
//! in the bar. An OHLC readout of the selected (or latest) candle on top.

use crate::app::Hit;
use crate::cex::Candle;
use crate::chart::{put, text, width};
use crate::theme::{Glyphs, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::Ts;

pub struct Kline<'a> {
    /// Oldest first.
    pub candles: &'a [Candle],
    pub bar_us: i64,
    pub cursor: Option<Ts>,
    /// Live price for the tag (defaults to the last close).
    pub live: Option<f64>,
    /// Close line instead of candles.
    pub line: bool,
    pub decimals: usize,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct KlineInfo {
    pub plot: Rect,
    /// Start of the first and last candle shown.
    pub shown: Option<(Ts, Ts)>,
    /// Index into `candles` of the candle in the readout.
    pub selected: Option<usize>,
    /// Cell row of the live tag.
    pub tag_row: Option<u16>,
    /// VWMA at the readout candle (its legend is drawn by the caller).
    pub vwma: Option<f64>,
}

#[derive(Copy, Clone, PartialEq)]
enum Half {
    Empty,
    Wick,
    Body,
}

/// Glyph for a cell from its (top, bottom) halves.
fn glyph(top: Half, bottom: Half, unicode: bool) -> &'static str {
    use Half::*;
    if !unicode {
        return match (top, bottom) {
            (Empty, Empty) => " ",
            (Body, _) | (_, Body) => "#",
            _ => "|",
        };
    }
    match (top, bottom) {
        (Empty, Empty) => " ",
        (Empty, Wick) => "╷",
        (Empty, Body) => "╻",
        (Wick, Empty) => "╵",
        (Body, Empty) => "╹",
        (Wick, Wick) => "│",
        (Body, Body) => "┃",
        (Wick, Body) => "╽",
        (Body, Wick) => "╿",
    }
}

/// VWMA period (candles).
pub const VWMA_LEN: usize = 20;

/// Volume-weighted moving average of the closes over `n` candles (a plain
/// average where the source has no volume); `None` until `n` candles exist.
pub fn vwma(c: &[Candle], n: usize) -> Vec<Option<f64>> {
    (0..c.len())
        .map(|i| {
            let w = c.get((i + 1).checked_sub(n)?..=i)?;
            let vol: f64 = w.iter().map(|c| c.vol).sum();
            Some(if vol > 0.0 {
                w.iter().map(|c| c.close * c.vol).sum::<f64>() / vol
            } else {
                w.iter().map(|c| c.close).sum::<f64>() / n as f64
            })
        })
        .collect()
}

fn empty(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y)).is_some_and(|c| c.symbol() == " ")
}

/// A line through one row per column, stepping with box-drawing corners (as
/// the other charts draw lines); `None` leaves a gap.
fn step_line(buf: &mut Buffer, x0: u16, rows: &[Option<u16>], st: Style, g: &Glyphs) {
    let mut prev: Option<u16> = None;
    for (i, r) in rows.iter().enumerate() {
        let x = x0 + i as u16;
        let Some(r) = *r else {
            prev = None;
            continue;
        };
        match prev {
            Some(p) if p != r => {
                let (at_old, at_new) = if r < p { g.up_turn } else { g.down_turn };
                put(buf, x, p, at_old, st);
                put(buf, x, r, at_new, st);
                for y in p.min(r) + 1..p.max(r) {
                    put(buf, x, y, g.v, st);
                }
            }
            _ => put(buf, x, r, g.h, st),
        }
        prev = Some(r);
    }
}

/// Right-hand value axis: the axis line with evenly spaced ticks and labels
/// (every chart uses it; a live tag drawn afterwards covers one of them).
pub(crate) fn value_axis(
    buf: &mut Buffer,
    plot: Rect,
    aw: u16,
    (lo, hi): (f64, f64),
    fmt: &dyn Fn(f64) -> String,
    th: &Theme,
    g: &Glyphs,
) {
    let axis_x = plot.right();
    for y in plot.y..plot.bottom() {
        put(buf, axis_x, y, g.axis_v, th.rule());
    }
    let ticks = (plot.height / 4).clamp(1, 8).min(plot.height.saturating_sub(1).max(1));
    for i in 0..=ticks {
        let y = plot.y + ((plot.height - 1) as u32 * i as u32 / ticks as u32) as u16;
        let v = hi - (hi - lo) * (y - plot.y) as f64 / (plot.height - 1).max(1) as f64;
        put(buf, axis_x, y, if g.unicode { "├" } else { "+" }, th.rule());
        text(buf, axis_x + 2, y, &fmt(v), aw.saturating_sub(2), th.faint());
    }
}

/// Dotted line across the plot at the live value (empty cells only).
pub(crate) fn live_line(buf: &mut Buffer, plot: Rect, y: u16, st: Style, g: &Glyphs) {
    for x in plot.x..plot.right() {
        if empty(buf, x, y) {
            put(buf, x, y, if g.unicode { "┈" } else { "." }, st);
        }
    }
}

/// Dashed line from the point (x, y) to the plot's right edge, labelled with
/// `value` next to the point: just past the data's own cells on the right
/// ("← v"), else just before them on the left ("v →"). Empty cells only.
pub(crate) fn extreme_line(buf: &mut Buffer, plot: Rect, (x, y): (u16, u16), value: &str, st: Style, g: &Glyphs) {
    let dashed = if g.unicode { "╌" } else { "-" };
    for dx in x + 1..plot.right() {
        if empty(buf, dx, y) {
            put(buf, dx, y, dashed, st);
        }
    }
    let free = |buf: &Buffer, x: u16| buf.cell((x, y)).is_some_and(|c| c.symbol() == " " || c.symbol() == dashed);
    let mut r0 = x + 1;
    while r0 < plot.right() && !free(buf, r0) {
        r0 += 1;
    }
    let mut l1 = x;
    while l1 > plot.x && !free(buf, l1 - 1) {
        l1 -= 1;
    }
    let (right, left) = (format!(" ← {value} "), format!(" {value} → "));
    let (rw, lw) = (width(&right), width(&left));
    if r0 + rw <= plot.right() && (r0..r0 + rw).all(|x| free(buf, x)) {
        text(buf, r0, y, &right, rw, st);
    } else if l1 >= plot.x + lw && (l1 - lw..l1).all(|x| free(buf, x)) {
        text(buf, l1 - lw, y, &left, lw, st);
    }
}

/// The live value as a filled tag on the axis at row `y`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn value_tag(
    buf: &mut Buffer,
    axis_x: u16,
    aw: u16,
    y: u16,
    value: &str,
    st: Style,
    th: &Theme,
    g: &Glyphs,
) {
    put(buf, axis_x, y, if g.unicode { "├" } else { "+" }, th.rule());
    let tag = format!(" {value} ");
    text(buf, axis_x + 1, y, &tag, aw.saturating_sub(1), st.add_modifier(Modifier::REVERSED | Modifier::BOLD));
}

/// One candle in a column: half-cell bodies and wicks, a flat dash when open
/// and close share a half cell. `half` maps a value to its half-row index.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_candle(
    buf: &mut Buffer,
    x: u16,
    plot: Rect,
    half: &dyn Fn(f64) -> i64,
    (open, high, low, close): (f64, f64, f64, f64),
    st: Style,
    unicode: bool,
) {
    let body = (half(open.max(close)), half(open.min(close)));
    let wick = (half(high), half(low));
    let kind = |h: i64| {
        if h >= body.0 && h <= body.1 {
            Half::Body
        } else if h >= wick.0 && h <= wick.1 {
            Half::Wick
        } else {
            Half::Empty
        }
    };
    // A candle whose open and close share one half-cell (a doji: no move,
    // common on 1s bars) is a flat dash, as exchanges draw it.
    let doji = body.0 == body.1;
    for r in wick.0.min(body.0) / 2..=(wick.1.max(body.1) / 2).min(plot.height as i64 - 1) {
        let s = if doji && r == body.0 / 2 {
            flat(wick.0 < r * 2, wick.1 > r * 2 + 1, unicode)
        } else {
            glyph(kind(r * 2), kind(r * 2 + 1), unicode)
        };
        if s != " " {
            put(buf, x, plot.y + r as u16, s, st);
        }
    }
}

/// Flat candle body, joined to the wick above and/or below it.
fn flat(above: bool, below: bool, unicode: bool) -> &'static str {
    match (unicode, above, below) {
        (false, _, _) => "=",
        (true, true, true) => "┿",
        (true, true, false) => "┷",
        (true, false, true) => "┯",
        (true, false, false) => "━",
    }
}

pub fn price(v: f64, decimals: usize) -> String {
    format!("{v:.decimals$}")
}

/// Candle time as the readout shows it.
fn stamp(t: Ts, bar_us: i64) -> String {
    if bar_us < 60_000_000 {
        t.format("%m/%d %H:%M:%S")
    } else if bar_us < 86_400_000_000 {
        t.format("%m/%d %H:%M")
    } else {
        t.format("%Y/%m/%d")
    }
}

/// Time-axis label.
fn axis_label(t: Ts, bar_us: i64) -> String {
    if bar_us < 60_000_000 {
        t.format("%H:%M:%S")
    } else if bar_us < 3_600_000_000 {
        t.format("%H:%M")
    } else if bar_us < 86_400_000_000 {
        t.format("%m/%d %H:%M")
    } else {
        t.format("%m/%d")
    }
}

pub fn render_kline(
    area: Rect,
    buf: &mut Buffer,
    k: &Kline<'_>,
    hit: &dyn Fn(Rect, Hit),
    th: &Theme,
    g: &Glyphs,
) -> KlineInfo {
    let mut info = KlineInfo::default();
    if area.width < 24 || area.height < 5 {
        return info;
    }
    let dec = k.decimals;
    let live = k.live.or_else(|| k.candles.last().map(|c| c.close));
    // Right axis wide enough for the labels and the tag.
    let span_hint = k.candles.iter().map(|c| c.high).fold(live.unwrap_or(0.0), f64::max);
    let aw = width(&price(span_hint, dec)) + 3;
    let plot = Rect { x: area.x, y: area.y + 1, width: area.width.saturating_sub(aw), height: area.height - 2 };
    info.plot = plot;
    let axis_x = plot.right();
    let base_y = plot.bottom();
    for y in plot.y..base_y {
        put(buf, axis_x, y, g.axis_v, th.rule());
    }

    if k.candles.is_empty() {
        let msg = "waiting for candles";
        text(
            buf,
            plot.x + plot.width.saturating_sub(width(msg)) / 2,
            plot.y + plot.height / 2,
            msg,
            plot.width,
            th.faint(),
        );
        return info;
    }

    // Visible window: the newest candles, shifted left when the cursor is older.
    let n = k.candles.len();
    let cols = plot.width as usize;
    let sel = k.cursor.map(|c| k.candles.partition_point(|x| x.start <= c).saturating_sub(1));
    let end = match sel {
        Some(s) if s + cols < n => (s + cols / 2).min(n),
        _ => n,
    };
    let start = end.saturating_sub(cols);
    let vis = &k.candles[start..end];
    info.shown = Some((vis[0].start, vis[vis.len() - 1].start));
    let sel = sel.unwrap_or(n - 1).clamp(start, end - 1);
    info.selected = Some(sel);

    // Y range over what is drawn (line: closes; candles: highs and lows),
    // the VWMA and the live price, 5% padding.
    let ma = vwma(k.candles, VWMA_LEN);
    info.vwma = ma[sel];
    let span = |c: &Candle| if k.line { (c.close, c.close) } else { (c.high, c.low) };
    let (mut lo, mut hi) = vis.iter().fold((f64::MAX, f64::MIN), |(a, b), c| (a.min(span(c).1), b.max(span(c).0)));
    for v in ma[start..end].iter().flatten().chain(live.as_ref()) {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    let pad = ((hi - lo) * 0.05).max(hi.abs() * 0.0002).max(1e-9);
    lo -= pad;
    hi += pad;
    let halves = plot.height as i64 * 2;
    let half = |v: f64| -> i64 {
        (((hi - v) / (hi - lo)) * (halves - 1) as f64).round().clamp(0.0, (halves - 1) as f64) as i64
    };
    let row = |v: f64| -> u16 { plot.y + (half(v) / 2) as u16 };

    // Price labels on the right axis, evenly spaced (the tag goes on top).
    value_axis(buf, plot, aw, (lo, hi), &|v| price(v, dec), th, g);

    // VWMA first so the price draws over it.
    let x0 = plot.right() - vis.len() as u16;
    let ma_rows: Vec<Option<u16>> = ma[start..end].iter().map(|v| v.map(row)).collect();
    step_line(buf, x0, &ma_rows, Style::new().fg(th.warn), g);

    // Price: a close line, or candles.
    if k.line {
        let rows: Vec<Option<u16>> = vis.iter().map(|c| Some(row(c.close))).collect();
        step_line(buf, x0, &rows, th.text(), g);
    } else {
        for (i, c) in vis.iter().enumerate() {
            let st = Style::new().fg(if c.close >= c.open { th.profit } else { th.loss });
            draw_candle(buf, x0 + i as u16, plot, &half, (c.open, c.high, c.low, c.close), st, g.unicode);
        }
    }
    for (i, c) in vis.iter().enumerate() {
        hit(Rect { x: x0 + i as u16, y: plot.y, width: 1, height: plot.height }, Hit::Candle(c.start));
    }
    // Selected candle: a dot on its close (line) or the candle reversed.
    if k.cursor.is_some() {
        let x = x0 + (sel - start) as u16;
        if k.line {
            put(buf, x, row(k.candles[sel].close), g.bullet, th.accent_bold());
        } else {
            for y in plot.y..base_y {
                if let Some(cell) = buf.cell_mut((x, y)).filter(|c| c.symbol() != " ") {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    // Overlay lines go into empty cells only: they never cover the price.
    let up =
        live.is_some_and(|p| if k.line { n < 2 || p >= k.candles[n - 2].close } else { p >= k.candles[n - 1].open });
    let dir = Style::new().fg(if up { th.profit } else { th.loss });
    if let Some(p) = live {
        live_line(buf, plot, row(p), dir, g);
    }
    // Visible high and low: a dashed line from the point to the right edge,
    // with the price next to the point (arrow pointing at it).
    let (hi_i, hi_v) =
        vis.iter().enumerate().map(|(i, c)| (i, span(c).0)).fold((0, f64::MIN), |m, x| if x.1 > m.1 { x } else { m });
    let (lo_i, lo_v) =
        vis.iter().enumerate().map(|(i, c)| (i, span(c).1)).fold((0, f64::MAX), |m, x| if x.1 < m.1 { x } else { m });
    if row(hi_v) != row(lo_v) {
        for (i, v, st) in [(hi_i, hi_v, th.muted()), (lo_i, lo_v, Style::new().fg(th.accent))] {
            extreme_line(buf, plot, (x0 + i as u16, row(v)), &price(v, dec), st, g);
        }
    }

    // Live price tag on the axis, with the time left in the bar below it.
    if let Some(p) = live {
        let y = row(p);
        info.tag_row = Some(y);
        value_tag(buf, axis_x, aw, y, &price(p, dec), dir, th, g);
        let left = (k.candles[n - 1].start.0 + k.bar_us - Ts::now().0) / 1_000_000;
        if k.bar_us >= 60_000_000 && left > 0 && left * 1_000_000 <= k.bar_us && y + 1 < base_y {
            let cd = if left >= 3600 {
                format!(" {:02}:{:02}:{:02} ", left / 3600, left / 60 % 60, left % 60)
            } else {
                format!(" {:02}:{:02} ", left / 60, left % 60)
            };
            text(buf, axis_x + 1, y + 1, &cd, aw - 1, dir.add_modifier(Modifier::REVERSED));
        }
    }

    // Time axis: a label under a candle every ~18 columns, placed right to
    // left so the newest one always shows.
    let mut limit = plot.right();
    for i in (0..vis.len()).rev().step_by(18) {
        let lab = axis_label(vis[i].start, k.bar_us);
        let w = width(&lab);
        let x = (x0 + i as u16).saturating_sub(w / 2).min(limit.saturating_sub(w));
        if x < plot.x {
            break;
        }
        text(buf, x, base_y, &lab, w, th.faint());
        limit = x.saturating_sub(2);
    }

    // Readout: the selected candle (latest when following).
    let c = &k.candles[sel];
    let close = if sel == n - 1 { live.unwrap_or(c.close) } else { c.close };
    let (d, pct) = (close - c.open, if c.open > 0.0 { (close - c.open) / c.open * 100.0 } else { 0.0 });
    let range = if c.low > 0.0 { (c.high.max(close) - c.low.min(close)) / c.low * 100.0 } else { 0.0 };
    let vs = th.pnl(d);
    let mut x = area.x;
    let y = area.y;
    // Fields in priority order: once one does not fit, the rest are dropped.
    let mut full = false;
    let mut put_kv = |buf: &mut Buffer, k: &str, v: &str, st: Style| {
        full = full || x + width(k) + width(v) + 1 > area.right();
        if full {
            return;
        }
        if !k.is_empty() {
            x += text(buf, x, y, k, width(k), th.faint()) + 1;
        }
        x += text(buf, x, y, v, width(v), st) + 2;
    };
    put_kv(buf, "", &stamp(c.start, k.bar_us), th.muted());
    put_kv(buf, "O", &price(c.open, dec), vs);
    put_kv(buf, "H", &price(c.high.max(close), dec), vs);
    put_kv(buf, "L", &price(c.low.min(close), dec), vs);
    put_kv(buf, "C", &price(close, dec), vs);
    put_kv(buf, "Change", &format!("{d:+.dec$} ({pct:+.2}%)"), vs);
    put_kv(buf, "Range", &format!("{range:.2}%"), vs);
    if c.vol > 0.0 {
        put_kv(buf, "Vol", &crate::markets::compact(c.vol), th.muted());
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Depth;

    fn candles() -> Vec<Candle> {
        // rising then one falling candle
        let mut v: Vec<Candle> = (0..40)
            .map(|i| {
                let o = 100.0 + i as f64 * 0.1;
                Candle { start: Ts(i * 60_000_000), open: o, high: o + 0.15, low: o - 0.05, close: o + 0.1, vol: 1.0 }
            })
            .collect();
        v.push(Candle { start: Ts(40 * 60_000_000), open: 104.0, high: 104.1, low: 103.5, close: 103.6, vol: 2.0 });
        v
    }

    fn dump(buf: &Buffer) -> Vec<String> {
        let a = buf.area;
        (a.y..a.bottom()).map(|y| (a.x..a.right()).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    fn draw(k: &Kline<'_>, area: Rect) -> (Buffer, KlineInfo, Vec<(Rect, Hit)>) {
        let mut buf = Buffer::empty(area);
        let hits = std::cell::RefCell::new(Vec::new());
        let info = render_kline(
            area,
            &mut buf,
            k,
            &|r, h| hits.borrow_mut().push((r, h)),
            &Theme::with_depth(Depth::None),
            &Glyphs::unicode(),
        );
        (buf, info, hits.into_inner())
    }

    #[test]
    fn candles_right_aligned_with_live_tag_and_readout() {
        let c = candles();
        let k = Kline { candles: &c, bar_us: 60_000_000, cursor: None, live: Some(103.61), line: false, decimals: 2 };
        let area = Rect::new(0, 0, 80, 20);
        let (buf, info, hits) = draw(&k, area);
        let lines = dump(&buf);
        // every candle visible (41 < plot width), newest in the last plot column
        assert_eq!(info.shown, Some((Ts(0), Ts(40 * 60_000_000))));
        assert_eq!(hits.len(), 41);
        assert_eq!(hits.last().unwrap().0.x, info.plot.right() - 1);
        // the tag sits right of the plot at the live price and reads it
        let y = info.tag_row.unwrap() as usize;
        assert!(lines[y][..].contains(" 103.61 "), "{}", lines[y]);
        assert!(buf[(info.plot.right() + 2, y as u16)].modifier.contains(Modifier::REVERSED));
        // readout = latest candle, closed at the live price
        assert!(lines[0].contains("O 104.00") && lines[0].contains("C 103.61"), "{}", lines[0]);
        assert!(lines[0].contains("Change -0.39 (-0.38%)"), "{}", lines[0]);
        // bodies use heavy glyphs, wicks light ones; left of the first candle
        // only the live-price / high / low overlay lines
        let body: String = lines.iter().map(|l| l.chars().nth(info.plot.right() as usize - 1).unwrap()).collect();
        assert!(body.contains('┃') || body.contains('╻') || body.contains('╹'), "{body}");
        let left = info.plot.right() as usize - 41;
        assert!(lines[1..19].iter().all(|l| l.chars().take(left).all(|c| !"┃╻╹╽╿│╷╵".contains(c))));
        assert!(lines[y].chars().take(left).all(|c| c == '┈'), "live price line: {}", lines[y]);
        // time axis on the last row
        assert!(lines[19].contains(&Ts(40 * 60_000_000).format("%H:%M")), "{}", lines[19]);
    }

    #[test]
    fn cursor_selects_a_candle_and_scrolls_old_ones_into_view() {
        let c = candles();
        let k = Kline {
            candles: &c,
            bar_us: 60_000_000,
            cursor: Some(Ts(5 * 60_000_000 + 1)),
            live: None,
            line: false,
            decimals: 2,
        };
        let (buf, info, _) = draw(&k, Rect::new(0, 0, 40, 12));
        assert_eq!(info.selected, Some(5));
        let (first, _) = info.shown.unwrap();
        assert!(first <= Ts(5 * 60_000_000));
        assert!(dump(&buf)[0].contains("O 100.50"));
    }

    #[test]
    fn empty_and_line_mode() {
        let (buf, info, hits) = draw(
            &Kline { candles: &[], bar_us: 1_000_000, cursor: None, live: None, line: false, decimals: 2 },
            Rect::new(0, 0, 60, 10),
        );
        assert!(dump(&buf).iter().any(|l| l.contains("waiting for candles")));
        assert!(info.shown.is_none() && hits.is_empty());
        let c = candles();
        let (buf, _, _) = draw(
            &Kline { candles: &c, bar_us: 60_000_000, cursor: None, live: None, line: true, decimals: 2 },
            Rect::new(0, 0, 80, 20),
        );
        let all = dump(&buf).concat();
        assert!(!all.contains('┃'), "line mode draws no bodies");
    }

    #[test]
    fn line_mode_with_vwma_high_low_and_live_price_lines() {
        let c = candles();
        let k = Kline { candles: &c, bar_us: 60_000_000, cursor: None, live: Some(103.61), line: true, decimals: 2 };
        let (buf, info, _) = draw(&k, Rect::new(0, 0, 110, 20));
        let lines = dump(&buf);
        let all = lines.concat();
        // the close line steps with box corners
        assert!(all.contains('╭') || all.contains('╰'), "{all}");
        // high (close 104.00, near the right edge: label on its left) and
        // low (close 100.10, first column: label on its right)
        assert!(all.contains(" 104.00 → "), "{}", lines.join("\n"));
        assert!(all.contains(" ← 100.10 ╌"), "{}", lines.join("\n"));
        // dotted live-price line on the tag row; the VWMA of the latest
        // candle (20 closes ending at 103.6) comes back for the legend
        let y = info.tag_row.unwrap() as usize;
        assert!(lines[y].contains('┈') && lines[y].contains(" 103.61 "), "{}", lines[y]);
        let expect =
            (c[21..].iter().map(|c| c.close * c.vol).sum::<f64>()) / c[21..].iter().map(|c| c.vol).sum::<f64>();
        assert!((info.vwma.unwrap() - expect).abs() < 1e-9);
        // selecting a candle marks its close with a dot
        let k = Kline { cursor: Some(Ts(10 * 60_000_000)), ..k };
        let (buf, _, _) = draw(&k, Rect::new(0, 0, 80, 20));
        assert!(dump(&buf).concat().contains('●'));
    }

    #[test]
    fn doji_candles_are_flat_dashes() {
        let d = |m: i64, o: f64, h: f64, l: f64| Candle {
            start: Ts(m * 1_000_000),
            open: o,
            high: h,
            low: l,
            close: o,
            vol: 0.0,
        };
        // 1s bars: mostly no move, one with a wick either side, one real candle
        let mut c: Vec<Candle> = (0..30).map(|m| d(m, 100.0, 100.0, 100.0)).collect();
        c.push(d(30, 100.0, 101.0, 99.0));
        c.push(Candle { start: Ts(31_000_000), open: 99.0, high: 101.0, low: 99.0, close: 101.0, vol: 0.0 });
        let k = Kline { candles: &c, bar_us: 1_000_000, cursor: None, live: None, line: false, decimals: 2 };
        let (buf, info, _) = draw(&k, Rect::new(0, 0, 80, 20));
        let col =
            |x: u16| (info.plot.y..info.plot.bottom()).map(|y| buf[(x, y)].symbol().to_string()).collect::<String>();
        let x0 = info.plot.right() - c.len() as u16;
        assert!(col(x0).contains('━') && !col(x0).contains(['╻', '╹']), "flat 1s candle: {}", col(x0));
        assert!(col(x0 + 30).contains('┿'), "doji with wicks: {}", col(x0 + 30));
        assert!(col(x0 + 31).contains('┃'), "a real candle keeps its body");
    }

    #[test]
    fn vwma_weights_by_volume() {
        let c = |close: f64, vol: f64| Candle { start: Ts(0), open: close, high: close, low: close, close, vol };
        let v = vwma(&[c(1.0, 1.0), c(2.0, 1.0), c(3.0, 2.0)], 2);
        assert_eq!(v[0], None);
        assert_eq!(v[1], Some(1.5));
        assert!((v[2].unwrap() - 8.0 / 3.0).abs() < 1e-12);
        assert_eq!(vwma(&[c(1.0, 0.0), c(3.0, 0.0)], 2)[1], Some(2.0), "no volume: plain average");
    }
}
