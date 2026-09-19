//! Markets page laid out like an exchange trading view: pair header with the
//! 24h stats, candlestick chart with bar selector, a quote book of the DEX
//! quotes (or the pair's last trades), tabs for open orders / order history /
//! assets / bots, and a ticker strip. Candles, 24h stats, trades and the
//! strip come from OKX (live sessions); the quote book is the on-chain data
//! the bot actually trades on.

use crate::app::{App, Hit};
use crate::cex::{BARS, Candle, CexState, Ticker, Trade};
use crate::chart::{put, text, width};
use crate::hub::{Quote, ViewModel};
use crate::kline::{Kline, VWMA_LEN, price, render_kline};
use crate::panels::{age, edge, now_ts, signed_thousands, sol_amount};
use crate::workspace::ChartStyle;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use searcher_core::metrics::MetricId;
use searcher_core::model::{ExecState, SampleSide, StrategyKind};
use searcher_core::{Ts, UsdMicros};

/// Right-hand panel of the chart row.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SideTab {
    #[default]
    QuoteBook,
    LastTrades,
}

/// Tabs under the chart.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum BottomTab {
    OpenOrders,
    OrderHistory,
    Assets,
    #[default]
    Bots,
}

impl BottomTab {
    pub const ALL: [BottomTab; 4] =
        [BottomTab::OpenOrders, BottomTab::OrderHistory, BottomTab::Assets, BottomTab::Bots];

    pub fn next(self) -> BottomTab {
        let i = BottomTab::ALL.iter().position(|t| *t == self).unwrap_or(0);
        BottomTab::ALL[(i + 1) % BottomTab::ALL.len()]
    }
}

/// 1,234,567 → "1.23M".
pub fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.2}K", v / 1e3)
    } else {
        format!("{v:.2}")
    }
}

/// Price with thousands separators and a precision that suits its size.
fn quote_px(v: f64) -> String {
    let s = if v >= 1000.0 {
        format!("{v:.1}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    };
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if frac.is_empty() { out } else { format!("{out}.{frac}") }
}

fn venue(q: &Quote) -> String {
    let s = q.source.replace("jupiter /build ", "").replace(" pool", " mid");
    match s.as_str() {
        "best route" => "Jupiter best".into(),
        "jupiter price v3 (reference)" => "Jupiter price".into(),
        "Pyth" => format!("Pyth {}", q.pair),
        _ => s,
    }
}

fn is_sol(q: &Quote) -> bool {
    q.pair.starts_with("SOL/")
}

/// Mean of the fresh on-chain pool mids, else the Jupiter best-route mid.
fn onchain_mid(vm: &ViewModel) -> Option<f64> {
    let mids: Vec<f64> =
        vm.quotes.iter().filter(|q| q.side == SampleSide::Mid && is_sol(q)).map(|q| q.price.f64()).collect();
    if !mids.is_empty() {
        return Some(mids.iter().sum::<f64>() / mids.len() as f64);
    }
    let best = |side| vm.quotes.iter().find(|q| q.side == side && q.source.contains("best")).map(|q| q.price.f64());
    Some((best(SampleSide::Buy)? + best(SampleSide::Sell)?) / 2.0)
}

/// Last value of `m` and whether it last moved up.
fn last_move(vm: &ViewModel, m: MetricId) -> Option<(f64, bool)> {
    let s = vm.series(m)?;
    let (_, v) = s.last()?;
    let prev = s.iter().rev().map(|p| p.1).find(|p| *p != v);
    Some((v, prev.is_none_or(|p| v >= p)))
}

fn inset(r: Rect) -> Rect {
    Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r }
}

fn rule(buf: &mut Buffer, x0: u16, x1: u16, y: u16, app: &App) {
    for x in x0..x1 {
        put(buf, x, y, app.glyphs.h, app.theme.rule());
    }
}

/// A row of tabs; the selected one bold in the accent colour. Returns the end x.
fn tabs<T: Copy>(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    max_x: u16,
    items: &[(String, T, bool)],
    app: &App,
    hit: fn(T) -> Hit,
) -> u16 {
    let th = &app.theme;
    let mut x = x;
    for (label, v, on) in items {
        let w = width(label);
        if x + w > max_x {
            break;
        }
        let st = if *on { th.accent_bold().add_modifier(Modifier::UNDERLINED) } else { th.muted() };
        text(buf, x, y, label, w, st);
        app.hit(Rect { x, y, width: w, height: 1 }, hit(*v));
        x += w + 3;
    }
    x
}

pub fn page_markets(buf: &mut Buffer, body: Rect, app: &App, vm: &ViewModel) {
    let body = inset(body);
    if body.height < 8 || body.width < 40 {
        return;
    }
    let cex: Option<CexState> = app.cex.as_ref().map(|c| c.state.read().clone());
    let ticker = cex.as_ref().and_then(|s| s.ticker.clone()).filter(|t| Some(t.inst.as_str()) == app.market());

    let strip_y = body.bottom() - 1;
    let bottom_h = match body.height {
        h if h >= 30 => 9,
        h if h >= 22 => 6,
        _ => 0,
    };
    let main =
        Rect { y: body.y + 3, height: body.height.saturating_sub(3 + 1 + bottom_h + u16::from(bottom_h > 0)), ..body };
    pair_header(buf, Rect { height: 2, ..body }, app, vm, cex.as_ref(), ticker.as_ref());
    rule(buf, body.x, body.right(), body.y + 2, app);

    let book_w = if main.width >= 100 { 40 } else { 0 };
    let chart = Rect { width: main.width.saturating_sub(book_w + u16::from(book_w > 0) * 2), ..main };
    chart_panel(buf, chart, app, vm, cex.as_ref());
    if book_w > 0 {
        let side = Rect { x: main.right() - book_w, width: book_w, ..main };
        for y in main.y..main.bottom() {
            put(buf, side.x - 2, y, app.glyphs.v, app.theme.rule());
        }
        side_panel(buf, side, app, vm, cex.as_ref());
    }
    if bottom_h > 0 {
        let b = Rect { y: main.bottom() + 1, height: bottom_h, ..body };
        bottom_panel(buf, b, app, vm);
    }
    ticker_strip(buf, Rect { y: strip_y, height: 1, ..body }, app, vm, cex.as_ref());
}

// ───────────────────────────── header ─────────────────────────────

fn pair_header(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, cex: Option<&CexState>, t: Option<&Ticker>) {
    let th = &app.theme;
    let (y0, y1) = (area.y, area.y + 1);
    let pair = match t {
        Some(t) => format!("{}/{} ▾", t.base(), t.quote()),
        None if app.cex.is_some() => format!("{} ▾", app.market().unwrap_or_default().replace('-', "/")),
        None => "SOL/USD".into(),
    };
    let mut x = area.x;
    let pw = width(&pair);
    text(buf, x, y0, &pair, pw, th.text().add_modifier(Modifier::BOLD));
    if app.cex.is_some() {
        app.hit(Rect { x, y: y0, width: pw, height: 2 }, Hit::MkPair);
        text(buf, x, y1, "OKX spot · p", pw.max(12), th.faint());
    } else {
        text(buf, x, y1, "on-chain", pw.max(8), th.faint());
    }
    x += pw.max(12) + 3;

    // Big price + change: OKX last vs 00:00 UTC, else the on-chain mid vs the
    // first mid of the session.
    let (px, chg, up) = match t {
        Some(t) => {
            let (d, p) = t.change();
            (price(t.last, 2), format!("{d:+.2} ({p:+.2}%)"), d >= 0.0)
        }
        None => match (last_move(vm, MetricId::PoolMid).or_else(|| last_move(vm, MetricId::Price)), onchain_mid(vm)) {
            (Some((v, up)), _) => {
                let first = vm.series(MetricId::PoolMid).or_else(|| vm.series(MetricId::Price)).and_then(|s| s.first());
                let chg = first.map(|(_, f)| format!("{:+.3} ({:+.2}%) session", v - f, (v / f - 1.0) * 100.0));
                (price(v, 3), chg.unwrap_or_default(), up)
            }
            (None, Some(m)) => (price(m, 3), String::new(), true),
            (None, None) => ("—".into(), String::new(), true),
        },
    };
    let st = Style::new().fg(if up { th.profit } else { th.loss }).add_modifier(Modifier::BOLD);
    let w = width(&px).max(width(&chg));
    text(buf, x, y0, &px, w, st);
    text(buf, x, y1, &chg, w, Style::new().fg(if up { th.profit } else { th.loss }));
    x += w + 4;

    // Stat columns: label above value.
    let mid = onchain_mid(vm).map(|m| price(m, 3)).unwrap_or_else(|| "—".into());
    let pyth =
        vm.quotes.iter().find(|q| q.side == SampleSide::Oracle && q.pair == "SOL/USD").map(|q| price(q.price.f64(), 3));
    let mut stats: Vec<(String, String)> = vec![("On-chain mid".into(), mid)];
    if let Some(p) = pyth {
        stats.push(("Pyth oracle".into(), p));
    }
    match t {
        Some(t) => stats.extend([
            ("24h low".into(), price(t.low24h, 2)),
            ("24h high".into(), price(t.high24h, 2)),
            (format!("24h volume ({})", t.base()), compact(t.vol24h)),
            (format!("24h turnover ({})", t.quote()), compact(t.vol_ccy24h)),
        ]),
        None => stats.extend([("24h low".into(), "—".into()), ("24h high".into(), "—".into())]),
    }
    // Source status on the right.
    let status = match (app.cex.is_some(), cex) {
        (false, _) => (if vm.replay { "OKX: live sessions only" } else { "OKX off" }.to_string(), th.faint()),
        (true, Some(s)) if s.error.is_some() && s.last_ok.is_none_or(|t| t.age_ms(Ts::now()) > 5_000) => {
            (s.error.clone().unwrap_or_default(), th.warn())
        }
        (true, Some(s)) => match s.last_ok {
            Some(t) => (format!("OKX {} {}", app.glyphs.live, age(t.age_ms(Ts::now()))), th.faint()),
            None => ("OKX connecting…".into(), th.faint()),
        },
        (true, None) => ("OKX connecting…".into(), th.faint()),
    };
    let sw = width(&status.0).min(area.width / 3);
    let limit = area.right().saturating_sub(sw + 2);
    for (k, v) in stats {
        let w = width(&k).max(width(&v));
        if x + w > limit {
            break;
        }
        text(buf, x, y0, &k, w, th.faint());
        text(buf, x, y1, &v, w, th.text());
        x += w + 3;
    }
    text(buf, area.right() - sw, y0, &status.0, sw, status.1);
}

// ───────────────────────────── chart ─────────────────────────────

/// Candles for the chart: OKX for the selected pair and bar, else candles
/// built from the recorded on-chain price (pool mid, or Jupiter's price).
fn candles(app: &App, vm: &ViewModel, cex: Option<&CexState>) -> (Vec<Candle>, Option<f64>, String, usize) {
    let (_, _, bar_us) = BARS[app.mk_bar];
    if let Some(s) = cex.filter(|s| s.candles_for == Some((app.mk_pair, app.mk_bar))) {
        let live = s.ticker.as_ref().filter(|t| Some(t.inst.as_str()) == app.market()).map(|t| t.last);
        let mut c = s.candles.clone();
        // the latest candle follows the ticker between candle polls
        if let (Some(last), Some(p)) = (c.last_mut(), live) {
            last.close = p;
            last.high = last.high.max(p);
            last.low = last.low.min(p);
        }
        return (c, live, format!("OKX {}", app.market().unwrap_or_default()), 2);
    }
    let (m, label) = if vm.series(MetricId::PoolMid).is_some_and(|s| !s.is_empty()) {
        (MetricId::PoolMid, "on-chain pool mid")
    } else {
        (MetricId::Price, "Jupiter executable price")
    };
    let Some(s) = vm.series(m) else { return (Vec::new(), None, label.into(), 3) };
    let t1 = now_ts(vm);
    let t0 = Ts(t1.0 - bar_us * 300);
    let c = s
        .ohlc(t0, t1, bar_us)
        .into_iter()
        .map(|o| Candle { start: o.start, open: o.open, high: o.high, low: o.low, close: o.close, vol: 0.0 })
        .collect();
    let waiting = if app.cex.is_some() { " (OKX loading)" } else { "" };
    (c, s.last().map(|p| p.1), format!("{label}{waiting}"), 3)
}

fn chart_panel(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, cex: Option<&CexState>) {
    let th = &app.theme;
    if area.height < 6 {
        return;
    }
    // Toolbar: bars on the left; on the right the VWMA legend (orange, like
    // its line), the chart type and, when there is room, the source.
    let items: Vec<(String, usize, bool)> =
        BARS.iter().enumerate().map(|(i, (l, _, _))| (l.to_string(), i, i == app.mk_bar)).collect();
    let end = tabs(buf, area.x, area.y, area.right(), &items, app, Hit::MkBar);
    let (cs, live, source, dec) = candles(app, vm, cex);
    let k = Kline {
        candles: &cs,
        bar_us: BARS[app.mk_bar].2,
        cursor: app.timeline.cursor,
        live,
        line: app.market_style == ChartStyle::Line,
        decimals: dec,
    };
    let plot = Rect { y: area.y + 1, height: area.height - 1, ..area };
    let info = render_kline(plot, buf, &k, &|r, h| app.hit(r, h), th, &app.glyphs);
    app.kline_span.set(cs.first().zip(cs.last()).map(|(a, b)| (a.start, b.start)));

    let kind = if app.market_style == ChartStyle::Line { "line (c)" } else { "candles (c)" };
    let legend = info.vwma.map(|v| format!("VWMA({VWMA_LEN}) {}", price(v, dec)));
    let mut parts: Vec<(String, Style)> = Vec::new();
    if let Some(l) = legend {
        parts.push((l, Style::new().fg(th.warn)));
        parts.push(("  ".into(), th.faint()));
    }
    parts.push((kind.into(), th.faint()));
    let full: u16 = parts.iter().map(|(s, _)| width(s)).sum();
    if end + full + width(&source) + 5 < area.right() {
        parts.insert(0, (format!("{source} · "), th.faint()));
    }
    let total: u16 = parts.iter().map(|(s, _)| width(s)).sum();
    if end + total + 2 < area.right() {
        let mut x = area.right() - total;
        for (s, st) in parts {
            x += text(buf, x, area.y, &s, width(&s), st);
        }
    }
}

// ───────────────────────────── quote book / trades ─────────────────────────────

fn side_panel(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, cex: Option<&CexState>) {
    let items = [
        ("Quote book".to_string(), SideTab::QuoteBook, app.mk_side == SideTab::QuoteBook),
        ("Last trades".to_string(), SideTab::LastTrades, app.mk_side == SideTab::LastTrades),
    ];
    let end = tabs(buf, area.x, area.y, area.right(), &items, app, Hit::MkSide);
    if end + 3 < area.right() {
        text(buf, area.right() - 1, area.y, "t", 1, app.theme.faint());
    }
    let inner = Rect { y: area.y + 1, height: area.height - 1, ..area };
    match app.mk_side {
        SideTab::QuoteBook => quote_book(buf, inner, app, vm),
        SideTab::LastTrades => last_trades(buf, inner, app, cex),
    }
}

/// Columns: price · venue · bp from the mid · age.
fn book_row(buf: &mut Buffer, area: Rect, y: u16, cols: [&str; 4], styles: [Style; 4]) {
    let (pw, bw, aw) = (10u16, 7u16, 6u16);
    let vw = area.width.saturating_sub(pw + bw + aw + 3);
    text(buf, area.x, y, cols[0], pw, styles[0]);
    text(buf, area.x + pw + 1, y, cols[1], vw, styles[1]);
    let bx = area.x + pw + 1 + vw + 1;
    text(buf, bx + bw.saturating_sub(width(cols[2])), y, cols[2], bw, styles[2]);
    text(buf, area.right().saturating_sub(width(cols[3])), y, cols[3], aw, styles[3]);
}

fn quote_book(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    if area.height < 4 {
        return;
    }
    let now = now_ts(vm);
    let mid = onchain_mid(vm);
    let bp = |p: f64| mid.map(|m| format!("{:+.1}bp", (p / m - 1.0) * 1e4)).unwrap_or_default();
    let mut asks: Vec<&Quote> = vm.quotes.iter().filter(|q| q.side == SampleSide::Buy && is_sol(q)).collect();
    let mut bids: Vec<&Quote> = vm.quotes.iter().filter(|q| q.side == SampleSide::Sell && is_sol(q)).collect();
    asks.sort_by_key(|q| std::cmp::Reverse(q.price));
    bids.sort_by_key(|q| std::cmp::Reverse(q.price));
    let refs: Vec<&Quote> = vm
        .quotes
        .iter()
        .filter(|q| matches!(q.side, SampleSide::Mid | SampleSide::Oracle | SampleSide::Reference) && is_sol(q))
        .collect();

    let f = th.faint();
    book_row(buf, area, area.y, ["Price", "Venue", "vs mid", "Age"], [f, f, f, f]);
    if asks.is_empty() && bids.is_empty() {
        text(buf, area.x, area.y + 2, "waiting for DEX quotes", area.width, f);
        return;
    }
    // Rows: asks (red, highest first) · mid · bids (green); references below.
    let footer = 1u16;
    let free = area.height.saturating_sub(2 + 1 + footer) as usize; // header, mid row, footer
    let ref_rows =
        if free > asks.len() + bids.len() + 2 { refs.len().min(free - asks.len() - bids.len() - 1) } else { 0 };
    let half = free.saturating_sub(ref_rows + usize::from(ref_rows > 0)) / 2;
    let asks = &asks[asks.len().saturating_sub(half)..];
    let bids = &bids[..bids.len().min(half)];
    let mut y = area.y + 1;
    let red = Style::new().fg(th.loss);
    let green = Style::new().fg(th.profit);
    for q in asks {
        let a = age(q.ts.age_ms(now));
        book_row(buf, area, y, [&price(q.price.f64(), 4), &venue(q), &bp(q.price.f64()), &a], [red, th.text(), f, f]);
        y += 1;
    }
    // mid row: the on-chain price, big and coloured by its last move
    let mv = last_move(vm, MetricId::PoolMid).or_else(|| last_move(vm, MetricId::Price));
    if let Some(m) = mid {
        let up = mv.is_none_or(|(_, up)| up);
        let arrow = if up { "↑" } else { "↓" };
        let st = Style::new().fg(if up { th.profit } else { th.loss }).add_modifier(Modifier::BOLD);
        let s = format!("{} {arrow}", price(m, 4));
        let w = text(buf, area.x, y, &s, area.width, st);
        text(buf, area.x + w + 2, y, "on-chain mid", area.width.saturating_sub(w + 2), f);
    }
    y += 1;
    for q in bids {
        let a = age(q.ts.age_ms(now));
        book_row(buf, area, y, [&price(q.price.f64(), 4), &venue(q), &bp(q.price.f64()), &a], [green, th.text(), f, f]);
        y += 1;
    }
    if ref_rows > 0 {
        y += 1;
        for q in refs.iter().take(ref_rows) {
            let a = age(q.ts.age_ms(now));
            book_row(
                buf,
                area,
                y,
                [&price(q.price.f64(), 4), &venue(q), &bp(q.price.f64()), &a],
                [th.muted(), th.muted(), f, f],
            );
            y += 1;
        }
    }
    // footer: best executable bid / ask and the spread between them
    let best_ask = vm.quotes.iter().filter(|q| q.side == SampleSide::Buy && is_sol(q)).min_by_key(|q| q.price);
    let best_bid = vm.quotes.iter().filter(|q| q.side == SampleSide::Sell && is_sol(q)).max_by_key(|q| q.price);
    if let (Some(a), Some(b)) = (best_ask, best_bid) {
        let (a, b) = (a.price.f64(), b.price.f64());
        let fy = area.bottom() - 1;
        let mut x = area.x;
        x += text(buf, x, fy, "bid ", 4, f);
        x += text(buf, x, fy, &price(b, 4), 10, green) + 2;
        x += text(buf, x, fy, "ask ", 4, f);
        x += text(buf, x, fy, &price(a, 4), 10, red) + 2;
        let sp = format!("{:+.1}bp", (a / b - 1.0) * 1e4);
        text(buf, x, fy, &sp, area.right().saturating_sub(x), f);
    }
}

fn last_trades(buf: &mut Buffer, area: Rect, app: &App, cex: Option<&CexState>) {
    let th = &app.theme;
    let f = th.faint();
    let inst = app.market().unwrap_or_default();
    let (base, quote) = inst.split_once('-').unwrap_or((inst, ""));
    let trades: &[Trade] = match cex {
        Some(s) if s.trades_for == Some(app.mk_pair) => &s.trades,
        Some(_) => {
            text(buf, area.x, area.y + 1, "loading…", area.width, f);
            return;
        }
        None => {
            text(buf, area.x, area.y + 1, "OKX trades: live sessions only", area.width, f);
            return;
        }
    };
    let (pw, sw) = (12u16, 14u16);
    let row = |buf: &mut Buffer, y: u16, c: [&str; 3], st: [Style; 3]| {
        text(buf, area.x, y, c[0], pw, st[0]);
        text(buf, area.x + pw + sw - width(c[1]).min(sw), y, c[1], sw, st[1]);
        text(buf, area.right().saturating_sub(width(c[2])), y, c[2], 8, st[2]);
    };
    row(buf, area.y, [&format!("Price ({quote})"), &format!("Amount ({base})"), "Time"], [f, f, f]);
    let rows = area.height.saturating_sub(2) as usize;
    for (i, t) in trades.iter().take(rows).enumerate() {
        let st = Style::new().fg(if t.buy { th.profit } else { th.loss });
        row(
            buf,
            area.y + 1 + i as u16,
            [&price(t.px, 2), &if t.sz < 0.01 { format!("{:.6}", t.sz) } else { format!("{:.4}", t.sz) }, &t.ts.hms()],
            [st, th.text(), f],
        );
    }
    // buy / sell share of the listed volume
    let (b, s) = trades.iter().fold((0.0, 0.0), |(b, s), t| if t.buy { (b + t.sz, s) } else { (b, s + t.sz) });
    if b + s > 0.0 {
        let fy = area.bottom() - 1;
        let pb = b / (b + s);
        let (lb, ls) = (format!("B {:.1}%", pb * 100.0), format!("{:.1}% S", (1.0 - pb) * 100.0));
        let bar_w = area.width.saturating_sub(width(&lb) + width(&ls) + 2);
        let nb = (bar_w as f64 * pb).round() as u16;
        let x = area.x + text(buf, area.x, fy, &lb, 10, Style::new().fg(th.profit)) + 1;
        for i in 0..bar_w {
            let st = Style::new().fg(if i < nb { th.profit } else { th.loss });
            put(buf, x + i, fy, if app.glyphs.unicode { "▬" } else { "=" }, st);
        }
        text(buf, x + bar_w + 1, fy, &ls, 10, Style::new().fg(th.loss));
    }
}

// ───────────────────────────── bottom tabs ─────────────────────────────

fn bottom_panel(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel) {
    let th = &app.theme;
    let open: Vec<_> = vm
        .executions
        .values()
        .filter(|e| matches!(e.state, ExecState::AwaitingConfirm | ExecState::Submitted | ExecState::Pending))
        .collect();
    let items = [
        (format!("Open orders ({})", open.len()), BottomTab::OpenOrders, app.mk_tab == BottomTab::OpenOrders),
        (
            format!("Order history ({})", vm.trades.len()),
            BottomTab::OrderHistory,
            app.mk_tab == BottomTab::OrderHistory,
        ),
        ("Assets".to_string(), BottomTab::Assets, app.mk_tab == BottomTab::Assets),
        (format!("Bots ({})", StrategyKind::ALL.len()), BottomTab::Bots, app.mk_tab == BottomTab::Bots),
    ];
    let end = tabs(buf, area.x, area.y, area.right(), &items, app, Hit::MkTab);
    let hint = "o switch";
    if end + width(hint) + 2 < area.right() {
        rule(buf, end, area.right() - width(hint) - 1, area.y, app);
        text(buf, area.right() - width(hint), area.y, hint, width(hint), th.faint());
    }
    let inner = Rect { y: area.y + 1, height: area.height - 1, ..area };
    let f = th.faint();
    let now = now_ts(vm);
    let paper = vm.mode() == searcher_core::model::Mode::Paper;
    match app.mk_tab {
        BottomTab::OpenOrders => {
            let cols = [(0, "Time"), (10, "Mode"), (18, "Strategy"), (31, "Route"), (62, "Input SOL"), (76, "State")];
            header(buf, inner, &cols, f);
            for (i, e) in open.iter().take(inner.height as usize - 1).enumerate() {
                let o = vm.opps.get(&e.opportunity);
                let y = inner.y + 1 + i as u16;
                let vals = [
                    e.created_at.hms(),
                    e.mode.label().to_string(),
                    o.map(|o| o.strategy.label()).unwrap_or("").to_string(),
                    o.map(|o| o.label.clone()).unwrap_or_default(),
                    o.map(|o| sol_amount(o.input as i128)).unwrap_or_default(),
                    format!("{:?}", e.state),
                ];
                cells(buf, inner, y, &cols, &vals, th.text());
            }
            if open.is_empty() {
                let msg = if paper {
                    "No open orders — PAPER simulates fills; nothing is signed or sent"
                } else {
                    "No open orders"
                };
                text(buf, inner.x, inner.y + 1, msg, inner.width, f);
            }
        }
        BottomTab::OrderHistory => {
            let cols = [
                (0, "Time"),
                (10, "Mode"),
                (18, "Strategy"),
                (31, "Route"),
                (62, "Input SOL"),
                (76, "Net lamports"),
                (92, "Net USD"),
            ];
            header(buf, inner, &cols, f);
            for (i, t) in vm.trades.iter().rev().take(inner.height as usize - 1).enumerate() {
                let y = inner.y + 1 + i as u16;
                let vals = [
                    t.exit_ts.hms(),
                    if t.paper { "paper".into() } else { t.mode.label().to_string() },
                    t.strategy.label().into(),
                    t.label.clone(),
                    sol_amount(t.input as i128),
                    signed_thousands(t.net),
                    t.net_usd.map(|u| u.to_string()).unwrap_or_default(),
                ];
                cells(buf, inner, y, &cols, &vals, th.pnl(t.net as f64));
            }
            if vm.trades.is_empty() {
                text(buf, inner.x, inner.y + 1, "No trades yet — nothing has passed simulation + risk", inner.width, f);
            }
        }
        BottomTab::Assets => {
            let eq = vm.equity_lamports.as_ref();
            let rows: Vec<(String, String, Style)> = vec![
                (
                    "SOL".into(),
                    match eq {
                        Some((l, src)) => format!(
                            "{}  ≈ {}  ({src})",
                            sol_amount(*l as i128),
                            vm.equity_usd().map(|u| u.to_string()).unwrap_or_default()
                        ),
                        None => "—".into(),
                    },
                    th.text(),
                ),
                ("Mode".into(), vm.mode().label().into(), th.text()),
                (
                    "Session PnL".into(),
                    format!("simulated {}   realized {}", usd(vm.simulated), usd(vm.realized)),
                    th.pnl((vm.simulated.0 + vm.realized.0) as f64),
                ),
                (
                    "Wallet".into(),
                    vm.session.as_ref().and_then(|s| s.taker.clone()).unwrap_or_else(|| "—".into()),
                    th.muted(),
                ),
            ];
            for (i, (k, v, st)) in rows.iter().take(inner.height as usize).enumerate() {
                let y = inner.y + i as u16;
                text(buf, inner.x, y, k, 14, f);
                text(buf, inner.x + 14, y, v, inner.width.saturating_sub(14), *st);
            }
        }
        BottomTab::Bots => {
            let cols = [
                (0, "Bot"),
                (24, "Status"),
                (38, "Last route"),
                (66, "Scans"),
                (74, "Gross+"),
                (82, "Trades"),
                (90, "Best net"),
                (101, "Last"),
                (109, "PnL"),
            ];
            header(buf, inner, &cols, f);
            for (i, s) in StrategyKind::ALL.iter().enumerate().take(inner.height as usize - 1) {
                let y = inner.y + 1 + i as u16;
                let (scans, gross) = vm.strategy_counts.get(s).copied().unwrap_or((0, 0));
                let latest = vm.opps.values().rev().find(|o| o.strategy == *s);
                // only opportunities that were fully priced have a real edge
                let best = vm
                    .opps
                    .values()
                    .filter(|o| o.strategy == *s && crate::panels::is_priced(o))
                    .map(|o| o.eval.net_edge)
                    .max();
                let trades: Vec<_> = vm.trades.iter().filter(|t| t.strategy == *s).collect();
                let pnl = UsdMicros(trades.iter().filter_map(|t| t.net_usd).map(|u| u.0).sum());
                let (status, st) = if vm.kill_engaged() {
                    ("stopped (kill)".to_string(), Style::new().fg(th.loss))
                } else if vm.replay {
                    ("recorded".to_string(), th.muted())
                } else {
                    (format!("{} running", app.glyphs.live), Style::new().fg(th.profit))
                };
                let vals = [
                    bot_name(*s).to_string(),
                    status,
                    latest.map(|o| o.label.clone()).unwrap_or_else(|| "—".into()),
                    scans.to_string(),
                    gross.to_string(),
                    trades.len().to_string(),
                    best.map(edge).unwrap_or_else(|| "—".into()),
                    latest.map(|o| age(o.detected_at.age_ms(now))).unwrap_or_else(|| "—".into()),
                    usd(pnl),
                ];
                cells(buf, inner, y, &cols, &vals, th.text());
                // status and PnL coloured
                text(buf, inner.x + 24, y, &vals[1], 13, st);
                if inner.x + 109 < inner.right() {
                    text(buf, inner.x + 109, y, &vals[8], inner.right() - inner.x - 109, th.pnl(pnl.0 as f64));
                }
            }
        }
    }
}

fn bot_name(s: StrategyKind) -> &'static str {
    match s {
        StrategyKind::RoundTrip => "Round-trip SOL⇄USDC",
        StrategyKind::CrossDex => "Cross-DEX SOL/USDC",
        StrategyKind::Triangular => "Triangular SOL→USDC→JUP",
    }
}

fn usd(u: UsdMicros) -> String {
    if u.0 != 0 && u.0.abs() < 10_000 {
        return format!("{}${:.4}", if u.0 > 0 { "+" } else { "-" }, u.0.abs() as f64 / 1e6);
    }
    if u.0 > 0 { format!("+{u}") } else { u.to_string() }
}

fn header(buf: &mut Buffer, area: Rect, cols: &[(u16, &str)], st: Style) {
    for (x, h) in cols {
        if area.x + x + width(h) <= area.right() {
            text(buf, area.x + x, area.y, h, width(h), st);
        }
    }
}

/// Values under `cols`, each clipped to its column; columns past the edge are dropped.
fn cells(buf: &mut Buffer, area: Rect, y: u16, cols: &[(u16, &str)], vals: &[String], st: Style) {
    for (i, ((x, _), v)) in cols.iter().zip(vals).enumerate() {
        let next = cols.get(i + 1).map(|(n, _)| *n).unwrap_or(area.width);
        let w = next.saturating_sub(*x + 1).min(area.width.saturating_sub(*x));
        if area.x + x < area.right() && w > 0 {
            text(buf, area.x + x, y, v, w, st);
        }
    }
}

// ───────────────────────────── ticker strip ─────────────────────────────

fn ticker_strip(buf: &mut Buffer, area: Rect, app: &App, vm: &ViewModel, cex: Option<&CexState>) {
    let th = &app.theme;
    let mut x = area.x;
    let mut item = |buf: &mut Buffer, name: &str, chg: Option<f64>, px: String| {
        let c = chg.map(|c| format!("{c:+.2}%")).unwrap_or_default();
        let need = width(name) + width(&c) + width(&px) + 5;
        if x + need > area.right() {
            return;
        }
        x += text(buf, x, area.y, name, width(name), th.text().add_modifier(Modifier::BOLD)) + 1;
        if let Some(v) = chg {
            x += text(buf, x, area.y, &c, width(&c), th.pnl(v)) + 1;
        }
        x += text(buf, x, area.y, &px, width(&px), th.muted()) + 3;
    };
    match cex.filter(|s| !s.strip.is_empty()) {
        Some(s) => {
            for t in &s.strip {
                item(buf, &t.inst.replace("-USDT", ""), Some(t.change().1), quote_px(t.last));
            }
        }
        None => {
            // on-chain oracles, change since the session's first sample
            for q in vm.quotes.iter().filter(|q| q.side == SampleSide::Oracle) {
                let chg = (q.first.micros_per_token > 0).then(|| (q.price.f64() / q.first.f64() - 1.0) * 100.0);
                item(buf, &format!("{} (Pyth)", q.pair), chg, quote_px(q.price.f64()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Page, TuiOptions};
    use crate::cex::Cex;

    /// OKX data as the poller leaves it for SOL-USDT 1m.
    fn okx() -> CexState {
        let now = Ts::now().0 / 60_000_000 * 60_000_000;
        let t = |inst: &str, last: f64| Ticker {
            inst: inst.into(),
            last,
            sod_utc0: last * 1.007,
            high24h: last * 1.02,
            low24h: last * 0.95,
            vol24h: 1_384_004.1,
            vol_ccy24h: 153_788_338.7,
            ts: Ts::now(),
        };
        CexState {
            ticker: Some(t("SOL-USDT", 111.95)),
            candles: (0..300)
                .map(|i| {
                    let o = 111.0 + (i as f64 / 9.0).sin();
                    Candle {
                        start: Ts(now - (299 - i) * 60_000_000),
                        open: o,
                        high: o + 0.2,
                        low: o - 0.2,
                        close: o + 0.05,
                        vol: 12.5,
                    }
                })
                .collect(),
            candles_for: Some((0, 1)),
            trades: (0..40)
                .map(|i| Trade {
                    ts: Ts(Ts::now().0 - i * 700_000),
                    px: 111.9 + i as f64 * 0.01,
                    sz: 0.5 + i as f64,
                    buy: i % 3 != 0,
                })
                .collect(),
            trades_for: Some(0),
            strip: vec![t("BTC-USDT", 81_275.3), t("ETH-USDT", 2641.4), t("JUP-USDT", 0.287)],
            last_ok: Some(Ts::now()),
            error: None,
        }
    }

    #[test]
    fn renders_every_tab_at_every_size_without_panicking() {
        for (w, h) in [(150, 42), (130, 40), (120, 34), (100, 30), (80, 24), (60, 16), (40, 12)] {
            for side in [SideTab::QuoteBook, SideTab::LastTrades] {
                for tab in BottomTab::ALL {
                    for live in [true, false] {
                        let mut app = App::new(&TuiOptions::default());
                        app.page = Page::Markets;
                        app.mk_side = side;
                        app.mk_tab = tab;
                        app.cex = live.then(|| Cex::fixed(crate::cex::OkxSource::default(), okx()));
                        let vm = ViewModel::new(!live);
                        let text = crate::run::buffer_text(&crate::run::snapshot(&mut app, &vm, w, h));
                        if live && w >= 100 && h >= 30 {
                            assert!(text.contains("SOL/USDT") && text.contains("111.95"), "{w}x{h}\n{text}");
                            assert!(text.contains("VWMA(20) "), "VWMA legend at {w}x{h}\n{text}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn number_formats() {
        assert_eq!(compact(1_384_004.1), "1.38M");
        assert_eq!(compact(30_238.87), "30.24K");
        assert_eq!(compact(153_788_338.7), "153.79M");
        assert_eq!(quote_px(81_275.34), "81,275.3");
        assert_eq!(quote_px(2641.42), "2,641.4");
        assert_eq!(quote_px(111.93), "111.93");
        assert_eq!(quote_px(0.287), "0.2870");
        assert_eq!(BottomTab::Bots.next(), BottomTab::OpenOrders);
    }
}
