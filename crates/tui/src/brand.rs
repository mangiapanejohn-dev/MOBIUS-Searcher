//! MØBIUS-Searcher brand: the wordmark and the logo.
//!
//! On terminals with a graphics protocol (kitty/Ghostty, iTerm2/WezTerm,
//! sixel) the logo is the real image, `assets/mobius-logo.png`. Elsewhere it
//! is drawn as quadrant-block cells (2×2 pixels per cell, each cell with its
//! own foreground/background tone between the logo's ink and paper colours),
//! generated from the same PNG by `scripts/logo_to_cells.py`. When the art is
//! missing, the terminal is too small, glyphs are ASCII or the colour depth
//! cannot show tones (16 colours / none), only the wordmark is drawn.
//!
//! First-run setup prints into the scrollback instead: `inline_logo` sends the
//! PNG itself over kitty / iTerm2 / sixel, or resamples it into true-colour
//! quadrant cells on terminals without an image protocol.

use crate::chart::{text, width};
use crate::theme::{Depth, Glyphs, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};
use std::sync::OnceLock;
use std::time::Duration;

pub const NAME: &str = "MØBIUS-Searcher";

const LOGO: &str = include_str!("../assets/logo.cells");
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/mobius-logo.png");

struct Logo {
    ink: [u8; 3],
    paper: [u8; 3],
    /// (glyph, fg tone, bg tone; `None` = transparent)
    rows: Vec<Vec<(char, u8, Option<u8>)>>,
}

fn logo() -> Option<&'static Logo> {
    static PARSED: OnceLock<Option<Logo>> = OnceLock::new();
    PARSED.get_or_init(|| parse(LOGO)).as_ref()
}

fn parse(src: &str) -> Option<Logo> {
    let mut lines = src.lines();
    let (mut ink, mut paper) = (None, None);
    for kv in lines.next()?.split_whitespace() {
        let (k, v) = kv.split_once('=')?;
        let n = u32::from_str_radix(v, 16).ok()?;
        let rgb = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        match k {
            "ink" => ink = Some(rgb),
            "paper" => paper = Some(rgb),
            _ => {}
        }
    }
    let hex = |s: &str| u8::from_str_radix(s, 16).ok();
    let mut rows = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let chars: Vec<char> = line.chars().collect();
        let mut row = Vec::new();
        for c in chars.chunks(5) {
            let [g, f1, f2, b1, b2] = c else { return None };
            let bg: String = [*b1, *b2].iter().collect();
            row.push((
                *g,
                hex(&[*f1, *f2].iter().collect::<String>())?,
                if bg == ".." { None } else { Some(hex(&bg)?) },
            ));
        }
        rows.push(row);
    }
    if rows.is_empty() || rows.iter().any(|r| r.len() != rows[0].len()) {
        return None;
    }
    Some(Logo { ink: ink?, paper: paper?, rows })
}

/// (columns, rows) of the logo art; (0, 0) when it has not been generated.
pub fn logo_size() -> (u16, u16) {
    logo().map_or((0, 0), |l| (l.rows[0].len() as u16, l.rows.len() as u16))
}

/// Whether this terminal can show the logo art (Unicode blocks + tones).
pub fn can_draw_logo(th: &Theme, g: &Glyphs) -> bool {
    g.unicode && logo().is_some() && matches!(th.depth, Depth::TrueColor | Depth::Ansi256)
}

/// The real logo image, sized to the logo box, when the terminal answers the
/// graphics query with kitty, iTerm2 or sixel support, or half-block image
/// rendering elsewhere. Talks to the terminal: call after entering the
/// alternate screen and before reading events.
pub fn terminal_logo() -> Option<Protocol> {
    let opts = QueryStdioOptions { timeout: Duration::from_millis(500), ..Default::default() };
    let picker = Picker::from_query_stdio_with_options(opts).ok()?;
    logo_protocol(&picker)
}

fn logo_protocol(picker: &Picker) -> Option<Protocol> {
    let (w, h) = logo_size();
    let img = image::load_from_memory_with_format(LOGO_PNG, image::ImageFormat::Png).ok()?;
    picker.new_protocol(img, ratatui::layout::Size::new(w, h), Resize::Fit(Some(FilterType::Lanczos3))).ok()
}

/// Preferred inline logo width in cells: with a graphics protocol the PNG is
/// sharp at any size; character cells need more of them to stay recognisable.
const INLINE_GRAPHICS_COLS: u16 = 24;
const INLINE_CELL_COLS: u16 = 40;

/// The logo for an inline flow that stays in the scrollback (first-run setup).
pub struct InlineLogo {
    pub art: InlineArt,
    pub cols: u16,
    pub rows: u16,
}

pub enum InlineArt {
    /// One kitty / iTerm2 / sixel sequence that draws the PNG itself into
    /// `cols × rows` cells from the cursor. Protocols disagree on where the
    /// cursor ends up, so print it between save- and restore-cursor.
    Graphics { protocol: &'static str, escape: String },
    /// The terminal speaks no image protocol: the PNG resampled into quadrant
    /// cells (2×2 pixels each, true colour), one string per row.
    Cells(Vec<String>),
}

/// Asks the terminal (at most 500 ms, before any other output or key reads)
/// which image protocol it speaks and prepares the logo within
/// `max_cols × max_rows` cells. `None` when it would not be recognisable:
/// no room, a dumb terminal, or cells without 256 colours.
pub fn inline_logo(max_cols: u16, max_rows: u16) -> Option<InlineLogo> {
    if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
        return None;
    }
    // Terminal.app answers no image query; don't make the user wait for one.
    let picker = if std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "Apple_Terminal") {
        Picker::halfblocks()
    } else {
        let opts = QueryStdioOptions { timeout: Duration::from_millis(500), ..Default::default() };
        Picker::from_query_stdio_with_options(opts).unwrap_or_else(|_| Picker::halfblocks())
    };
    let depth = Theme::detect(searcher_core::config::ColorMode::Auto).depth;
    inline_logo_for(&picker, depth, max_cols, max_rows)
}

fn inline_logo_for(picker: &Picker, depth: Depth, max_cols: u16, max_rows: u16) -> Option<InlineLogo> {
    let protocol = picker.protocol_type();
    let graphics = protocol != ProtocolType::Halfblocks;
    if !graphics && !matches!(depth, Depth::TrueColor | Depth::Ansi256) {
        return None;
    }
    let font = picker.font_size();
    let (fw, fh) = (u32::from(font.width.max(1)), u32::from(font.height.max(1)));
    // the logo is square: rows = cols · cell width / cell height
    let rows_for = |cols: u16| ((u32::from(cols) * fw + fh / 2) / fh).max(1) as u16;
    let mut cols = if graphics { INLINE_GRAPHICS_COLS } else { INLINE_CELL_COLS }.min(max_cols);
    while cols > 0 && rows_for(cols) > max_rows {
        cols -= 1;
    }
    let min_cols = if graphics { 12 } else { 24 };
    if cols < min_cols {
        return None;
    }
    let rows = rows_for(cols);
    let logo = image::load_from_memory_with_format(LOGO_PNG, image::ImageFormat::Png).ok()?;
    let art = if graphics {
        let canvas = fit_logo(&logo, u32::from(cols) * fw, u32::from(rows) * fh, (1, 1));
        let tmux = picker.tmux_detected();
        let (name, escape) = match protocol {
            ProtocolType::Kitty => ("kitty", kitty_escape(&png_bytes(&canvas)?, cols, rows, tmux)),
            ProtocolType::Iterm2 => ("iterm2", iterm2_escape(&png_bytes(&canvas)?, cols, rows, tmux)),
            _ => {
                let size = ratatui::layout::Size::new(cols, rows);
                ("sixel", ratatui_image::protocol::sixel::Sixel::new(canvas, size, tmux).ok()?.data)
            }
        };
        InlineArt::Graphics { protocol: name, escape }
    } else {
        let grid = fit_logo(&logo, u32::from(cols) * 2, u32::from(rows) * 2, (fw, fh));
        InlineArt::Cells(quadrant_cells(&grid, depth))
    };
    Some(InlineLogo { art, cols, rows })
}

/// The artwork (cropped to its opaque pixels) scaled to fit `w × h` pixels
/// and centred on a transparent canvas of exactly that size. `pixel` is the
/// on-screen size of one canvas pixel, so a grid of non-square pixels
/// (quadrant cells are twice as tall as wide) keeps the logo round.
fn fit_logo(logo: &image::DynamicImage, w: u32, h: u32, pixel: (u32, u32)) -> image::DynamicImage {
    let rgba = logo.to_rgba8();
    let (mut x0, mut y0, mut x1, mut y1) = (rgba.width(), rgba.height(), 0, 0);
    for (x, y, px) in rgba.enumerate_pixels() {
        if px[3] > 0 {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
        }
    }
    let art = if x1 > x0 && y1 > y0 { logo.crop_imm(x0, y0, x1 - x0, y1 - y0) } else { logo.clone() };
    let (pw, ph) = (f64::from(pixel.0.max(1)), f64::from(pixel.1.max(1)));
    let (aw, ah) = (f64::from(art.width()), f64::from(art.height()));
    let scale = (f64::from(w) * pw / aw).min(f64::from(h) * ph / ah);
    let fit_w = ((aw * scale / pw).round() as u32).clamp(1, w.max(1));
    let fit_h = ((ah * scale / ph).round() as u32).clamp(1, h.max(1));
    let fitted = art.resize_exact(fit_w, fit_h, FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(w.max(1), h.max(1));
    let (dx, dy) = ((w.saturating_sub(fitted.width())) / 2, (h.saturating_sub(fitted.height())) / 2);
    image::imageops::overlay(&mut canvas, &fitted.to_rgba8(), i64::from(dx), i64::from(dy));
    image::DynamicImage::ImageRgba8(canvas)
}

fn png_bytes(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// tmux forwards an escape to the outer terminal only inside a DCS
/// passthrough with every ESC doubled (needs `allow-passthrough on`).
fn passthrough(seq: &str, tmux: bool) -> String {
    if tmux { format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")) } else { seq.to_string() }
}

/// kitty graphics: transmit and place the PNG in one go (`a=T`, `f=100`),
/// scaled into `c × r` cells, no replies (`q=2`), cursor left alone (`C=1`).
fn kitty_escape(png: &[u8], cols: u16, rows: u16, tmux: bool) -> String {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    let mut out = String::with_capacity(data.len() + chunks.len() * 16 + 64);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let control =
            if i == 0 { format!("a=T,f=100,t=d,q=2,C=1,c={cols},r={rows},m={more}") } else { format!("m={more}") };
        out.push_str(&passthrough(&format!("\x1b_G{control};{chunk}\x1b\\"), tmux));
    }
    out
}

/// iTerm2 inline image (also WezTerm, VS Code, Warp, Tabby, mintty, rio).
fn iterm2_escape(png: &[u8], cols: u16, rows: u16, tmux: bool) -> String {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let seq = format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1;doNotMoveCursor=1:{data}\x07",
        png.len()
    );
    passthrough(&seq, tmux)
}

/// Quadrant cells from a `2·cols × 2·rows` pixel grid: each cell shows its
/// four pixels with the best two-colour split (glyph = which quadrants take the
/// foreground). At the same cell count this is twice the horizontal detail of
/// half blocks. Transparent pixels keep the terminal's own background.
fn quadrant_cells(img: &image::DynamicImage, depth: Depth) -> Vec<String> {
    const GLYPHS: [char; 16] = [' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█'];
    let rgba = img.to_rgba8();
    let colour = |c: [u32; 3], layer: u8| match depth {
        Depth::TrueColor => format!("\x1b[{layer}8;2;{};{};{}m", c[0], c[1], c[2]),
        _ => format!("\x1b[{layer}8;5;{}m", ansi256(c[0] as u8, c[1] as u8, c[2] as u8)),
    };
    let mean = |px: &[[u32; 3]]| -> [u32; 3] {
        let n = px.len().max(1) as u32;
        let sum = px.iter().fold([0; 3], |a, p| [a[0] + p[0], a[1] + p[1], a[2] + p[2]]);
        [sum[0] / n, sum[1] / n, sum[2] / n]
    };
    let mut rows = Vec::new();
    for cy in 0..rgba.height() / 2 {
        let mut line = String::new();
        for cx in 0..rgba.width() / 2 {
            // upper-left, upper-right, lower-left, lower-right = bits 0..3
            let quad = [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dy)| *rgba.get_pixel(2 * cx + dx, 2 * cy + dy));
            let opaque: u8 = (0..4).filter(|&i| quad[i][3] >= 128).fold(0, |m, i| m | 1 << i);
            let rgb = quad.map(|p| [u32::from(p[0]), u32::from(p[1]), u32::from(p[2])]);
            if opaque == 0 {
                line.push_str("\x1b[0m ");
                continue;
            }
            if opaque != 0b1111 {
                let fg: Vec<[u32; 3]> = (0..4).filter(|&i| opaque >> i & 1 == 1).map(|i| rgb[i]).collect();
                line.push_str(&format!("\x1b[0m{}{}", colour(mean(&fg), 3), GLYPHS[opaque as usize]));
                continue;
            }
            let split = |mask: u8| {
                let (a, b): (Vec<usize>, Vec<usize>) = (0..4).partition(|&i| mask >> i & 1 == 1);
                let (ma, mb) = (
                    mean(&a.iter().map(|&i| rgb[i]).collect::<Vec<_>>()),
                    mean(&b.iter().map(|&i| rgb[i]).collect::<Vec<_>>()),
                );
                let err: u32 = (0..4)
                    .map(|i| {
                        let m = if mask >> i & 1 == 1 { ma } else { mb };
                        (0..3).map(|k| rgb[i][k].abs_diff(m[k]).pow(2)).sum::<u32>()
                    })
                    .sum();
                (err, mask, ma, mb)
            };
            let (_, mask, fg, bg) = (1..8u8).map(split).min_by_key(|s| s.0).unwrap_or((0, 0, rgb[0], rgb[0]));
            line.push_str(&format!("{}{}{}", colour(fg, 3), colour(bg, 4), GLYPHS[mask as usize]));
        }
        line.push_str("\x1b[0m");
        rows.push(line);
    }
    rows
}

/// Nearest xterm-256 colour: the 6×6×6 cube or the 24-step grey ramp.
fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near =
        |v: u8| STEPS.iter().enumerate().min_by_key(|(_, s)| (**s as i32 - v as i32).abs()).map_or(0, |(i, _)| i);
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (STEPS[ri], STEPS[gi], STEPS[bi]);
    let grey_i = ((u32::from(r) + u32::from(g) + u32::from(b)) / 3).saturating_sub(8).div_ceil(10).min(23) as u8;
    let grey = 8 + 10 * grey_i;
    let dist = |c: (u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    if dist((grey, grey, grey)) < dist(cube) { 232 + grey_i } else { 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8 }
}

fn tone(l: &Logo, depth: Depth, t: u8) -> Color {
    let mix = |i: usize| (l.ink[i] as u32 * (255 - t as u32) + l.paper[i] as u32 * t as u32) / 255;
    let (r, g, b) = (mix(0), mix(1), mix(2));
    match depth {
        Depth::TrueColor => Color::Rgb(r as u8, g as u8, b as u8),
        // 256 colours: nearest step of the 24-step grey ramp (232 = 8 … 255 = 238)
        _ => Color::Indexed(232 + ((r * 299 + g * 587 + b * 114) / 1000).saturating_sub(8).div_ceil(10).min(23) as u8),
    }
}

/// "MØBIUS-Searcher": MØBIUS bold with the Ø in the accent colour; returns
/// columns written.
pub fn wordmark(buf: &mut Buffer, x: u16, y: u16, max_w: u16, th: &Theme) -> u16 {
    let bold = th.text().add_modifier(Modifier::BOLD);
    let mut w = text(buf, x, y, "M", max_w, bold);
    w += text(buf, x + w, y, "Ø", max_w.saturating_sub(w), th.accent_bold());
    w += text(buf, x + w, y, "BIUS", max_w.saturating_sub(w), bold);
    w += text(buf, x + w, y, "-Searcher", max_w.saturating_sub(w), th.text());
    w
}

/// Logo centred in `area` with the wordmark and an optional caption under it:
/// the real image when `image` is given (graphics terminal), else the block
/// art. Returns true when the logo itself (image or art) was drawn.
pub fn draw_logo(
    buf: &mut Buffer,
    area: Rect,
    th: &Theme,
    g: &Glyphs,
    image: Option<&Protocol>,
    caption: &str,
) -> bool {
    let (lw, lh) = logo_size();
    let extra = if caption.is_empty() { 2 } else { 3 };
    let art = can_draw_logo(th, g) && lw <= area.width && lh + extra <= area.height;
    let block_h = if art { lh + extra } else { extra };
    let mut y = area.y + area.height.saturating_sub(block_h) / 2;
    if let (true, Some(img)) = (art, image) {
        // fitted inside the logo box; centre it there
        let s = img.size();
        let r = Rect::new(area.x + (area.width - s.width) / 2, y + (lh - s.height.min(lh)) / 2, s.width, s.height);
        Image::new(img).render(r, buf);
        return true; // supplied artwork is the complete logo; add no duplicate wordmark
    } else if let (true, Some(l)) = (art, logo()) {
        let x0 = area.x + (area.width - lw) / 2;
        for row in &l.rows {
            for (i, &(glyph, fg, bg)) in row.iter().enumerate() {
                if glyph == ' ' && bg.is_none() {
                    continue; // fully transparent: keep what is underneath
                }
                if let Some(c) = buf.cell_mut((x0 + i as u16, y)) {
                    c.set_char(glyph).set_fg(tone(l, th.depth, fg));
                    if let Some(bg) = bg {
                        c.set_bg(tone(l, th.depth, bg));
                    }
                }
            }
            y += 1;
        }
        y += 1;
    }
    let name_w = width(NAME);
    if area.width >= name_w {
        wordmark(buf, area.x + (area.width - name_w) / 2, y, name_w, th);
    }
    if !caption.is_empty() {
        let cw = width(caption).min(area.width);
        text(buf, area.x + (area.width - cw) / 2, y + 1, caption, cw, th.faint());
    }
    art
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_asset_parses_into_quadrant_cells() {
        let l = parse(LOGO).expect("logo.cells parses");
        let (w, h) = logo_size();
        assert!(w >= 16 && h >= 8, "{w}x{h}");
        for row in &l.rows {
            assert!(row.iter().all(|&(g, _, _)| " ▘▝▀▖▌▞▛▗▚▐▜▄▙▟█".contains(g)), "quadrant glyphs only");
        }
        assert!(parse("ink=000000 paper=ffffff\n▀00ff▀00").is_none(), "truncated cell is rejected");
        assert!(parse("").is_none());
    }

    #[test]
    fn tones_follow_colour_depth() {
        let l = logo().unwrap();
        assert_eq!(tone(l, Depth::TrueColor, 0), Color::Rgb(l.ink[0], l.ink[1], l.ink[2]));
        assert_eq!(tone(l, Depth::TrueColor, 255), Color::Rgb(l.paper[0], l.paper[1], l.paper[2]));
        let (Color::Indexed(dark), Color::Indexed(light)) = (tone(l, Depth::Ansi256, 0), tone(l, Depth::Ansi256, 255))
        else {
            panic!("256-colour tones are indexed")
        };
        assert!((232..=255).contains(&dark) && (232..=255).contains(&light) && dark < light);
    }

    #[test]
    fn wordmark_and_ascii_fallback() {
        let th = Theme::with_depth(Depth::TrueColor);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        crate::theme::set_ascii(false);
        assert_eq!(wordmark(&mut buf, 0, 0, 20, &th), 15);
        let s: String = (0..15).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(s, "MØBIUS-Searcher");
        let mut buf = Buffer::empty(area);
        crate::theme::set_ascii(true);
        wordmark(&mut buf, 0, 0, 20, &th);
        let s: String = (0..15).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(s, "MOBIUS-Searcher");
        crate::theme::set_ascii(false);
    }

    #[test]
    fn logo_draws_in_colour_and_degrades_to_wordmark() {
        let (lw, lh) = logo_size();
        let big = Rect::new(0, 0, lw + 4, lh + 4);
        let dump = |buf: &Buffer, r: Rect| -> String {
            (r.y..r.bottom()).flat_map(|y| (r.x..r.right()).map(move |x| (x, y))).map(|p| buf[p].symbol()).collect()
        };
        let blocks = |s: &str| s.chars().any(|c| "▘▝▀▖▌▞▛▗▚▐▜▄▙▟█".contains(c));
        crate::theme::set_ascii(false);
        // truecolor, room: art (quadrant blocks in logo tones) + wordmark
        let th = Theme::with_depth(Depth::TrueColor);
        let mut buf = Buffer::empty(big);
        assert!(draw_logo(&mut buf, big, &th, &Glyphs::unicode(), None, ""));
        assert!(blocks(&dump(&buf, big)) && dump(&buf, big).contains("MØBIUS"));
        assert!(buf.content.iter().any(|c| matches!(c.bg, Color::Rgb(..))));
        // no colour, 16 colours, ASCII glyphs or a small area: wordmark only
        for (depth, g, area) in [
            (Depth::None, Glyphs::unicode(), big),
            (Depth::Ansi16, Glyphs::unicode(), big),
            (Depth::TrueColor, Glyphs::ascii(), big),
            (Depth::TrueColor, Glyphs::unicode(), Rect::new(0, 0, 18, 3)),
        ] {
            let th = Theme::with_depth(depth);
            let mut buf = Buffer::empty(area);
            assert!(!draw_logo(&mut buf, area, &th, &g, None, ""), "{depth:?} {area:?}");
            let s = dump(&buf, area);
            assert!(s.contains("MØBIUS") && !blocks(&s), "{s}");
        }
    }

    #[test]
    fn real_image_fits_the_logo_box() {
        // what a graphics terminal gets (protocol-independent: sized by font)
        let (lw, lh) = logo_size();
        let p = logo_protocol(&Picker::halfblocks()).expect("embedded PNG decodes");
        let s = p.size();
        assert!(s.width <= lw && s.height <= lh && s.width >= lw - 2, "{s:?} in {lw}x{lh}");
        let th = Theme::with_depth(Depth::TrueColor);
        let area = Rect::new(0, 0, lw + 4, lh + 4);
        let mut buf = Buffer::empty(area);
        crate::theme::set_ascii(false);
        assert!(draw_logo(&mut buf, area, &th, &Glyphs::unicode(), Some(&p), ""));
        let all: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!all.contains("MØBIUS-Searcher"), "image is the complete logo; no duplicate wordmark");
        assert!(all.chars().any(|c| !c.is_whitespace()), "image protocol rendered cells");
    }

    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|c| c.is_ascii_alphabetic());
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn kitty_sends_the_png_itself_sized_in_cells() {
        use base64::Engine as _;
        let mut picker = Picker::halfblocks(); // 10×20 px cells
        picker.set_protocol_type(ProtocolType::Kitty);
        let logo = inline_logo_for(&picker, Depth::TrueColor, 60, 30).expect("room for the logo");
        assert_eq!((logo.cols, logo.rows), (INLINE_GRAPHICS_COLS, INLINE_GRAPHICS_COLS / 2));
        let InlineArt::Graphics { protocol: "kitty", escape } = &logo.art else { panic!("kitty graphics") };
        assert!(escape.starts_with("\x1b_Ga=T,f=100,t=d,q=2,C=1,c=24,r=12,m=1;"), "{}", &escape[..60]);
        assert!(escape.ends_with("\x1b\\") && escape.contains("\x1b_Gm=0;"));
        let payload: String = escape
            .split("\x1b_G")
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| chunk.split_once(';').expect("control;payload").1.trim_end_matches("\x1b\\"))
            .collect();
        let png = base64::engine::general_purpose::STANDARD.decode(payload).expect("base64");
        let img = image::load_from_memory(&png).expect("a PNG");
        assert_eq!((img.width(), img.height()), (240, 240), "24×12 cells of 10×20 px");
    }

    #[test]
    fn without_an_image_protocol_the_png_becomes_quadrant_cells() {
        let picker = Picker::halfblocks();
        let logo = inline_logo_for(&picker, Depth::TrueColor, 60, 17).expect("room for cells");
        assert_eq!((logo.cols, logo.rows), (34, 17), "limited by the rows, still round");
        let InlineArt::Cells(rows) = &logo.art else { panic!("cells") };
        assert_eq!(rows.len(), 17);
        for row in rows {
            let row = visible(row);
            assert_eq!(row.chars().count(), 34, "{row:?}");
            assert!(row.chars().all(|c| " ▘▝▀▖▌▞▛▗▚▐▜▄▙▟█".contains(c)));
        }
        assert!(rows.iter().any(|r| r.contains("\x1b[48;2;")), "true-colour backgrounds");
        assert!(inline_logo_for(&picker, Depth::Ansi16, 60, 17).is_none(), "16 colours cannot show it");
        assert!(inline_logo_for(&picker, Depth::TrueColor, 60, 8).is_none(), "too small to recognise");
    }

    #[test]
    fn quadrants_take_the_best_two_colour_split() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
        img.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, 0, 255]));
        let rows = quadrant_cells(&image::DynamicImage::ImageRgba8(img), Depth::TrueColor);
        assert_eq!(visible(&rows[0]), "▌");
        assert!(rows[0].contains("\x1b[38;2;0;0;0m") && rows[0].contains("\x1b[48;2;255;255;255m"));
        let clear = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
        assert_eq!(visible(&quadrant_cells(&image::DynamicImage::ImageRgba8(clear), Depth::TrueColor)[0]), " ");
    }

    #[test]
    fn fitting_keeps_the_logo_round_on_tall_pixels() {
        let logo = image::load_from_memory_with_format(LOGO_PNG, image::ImageFormat::Png).unwrap();
        // 68×34 grid of 5×10 px pixels is a square area: the fitted art spans it
        let grid = fit_logo(&logo, 68, 34, (5, 10)).to_rgba8();
        let opaque_cols = (0..68).filter(|&x| (0..34).any(|y| grid.get_pixel(x, y)[3] > 0)).count();
        let opaque_rows = (0..34).filter(|&y| (0..68).any(|x| grid.get_pixel(x, y)[3] > 0)).count();
        assert!(opaque_cols >= 64 && opaque_rows >= 31, "{opaque_cols}×{opaque_rows}");
    }
}
