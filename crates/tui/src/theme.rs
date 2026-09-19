//! Colors and glyphs. Quiet neutral palette; Claude orange for focus and
//! selection; green/red only for profit/loss. Every glyph has an ASCII
//! fallback and nothing depends on a Nerd Font.

use ratatui::style::{Color, Modifier, Style};
use searcher_core::config::{ColorMode, GlyphMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Ansi256,
    Ansi16,
    None,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub depth: Depth,
    pub accent: Color,
    pub accent_dim: Color,
    pub fg: Color,
    pub muted: Color,
    pub faint: Color,
    pub rule: Color,
    pub profit: Color,
    pub loss: Color,
    pub warn: Color,
    pub select_bg: Color,
}

impl Theme {
    pub fn detect(mode: ColorMode) -> Theme {
        let depth = match mode {
            ColorMode::Truecolor => Depth::TrueColor,
            ColorMode::Ansi256 => Depth::Ansi256,
            ColorMode::None => Depth::None,
            ColorMode::Auto => {
                if std::env::var_os("NO_COLOR").is_some() {
                    Depth::None
                } else {
                    let ct = std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
                    let term = std::env::var("TERM").unwrap_or_default();
                    if ct.contains("truecolor") || ct.contains("24bit") {
                        Depth::TrueColor
                    } else if term.contains("256") {
                        Depth::Ansi256
                    } else {
                        Depth::Ansi16
                    }
                }
            }
        };
        Theme::with_depth(depth)
    }

    pub fn with_depth(depth: Depth) -> Theme {
        match depth {
            Depth::TrueColor => Theme {
                depth,
                accent: Color::Rgb(217, 119, 87), // Claude orange
                accent_dim: Color::Rgb(140, 84, 66),
                fg: Color::Rgb(214, 211, 204),
                muted: Color::Rgb(140, 137, 130),
                faint: Color::Rgb(88, 86, 82),
                rule: Color::Rgb(58, 56, 53),
                profit: Color::Rgb(106, 176, 110),
                loss: Color::Rgb(214, 96, 84),
                warn: Color::Rgb(214, 170, 90),
                select_bg: Color::Rgb(45, 40, 37),
            },
            Depth::Ansi256 => Theme {
                depth,
                accent: Color::Indexed(173),
                accent_dim: Color::Indexed(95),
                fg: Color::Indexed(252),
                muted: Color::Indexed(245),
                faint: Color::Indexed(240),
                rule: Color::Indexed(237),
                profit: Color::Indexed(71),
                loss: Color::Indexed(167),
                warn: Color::Indexed(179),
                select_bg: Color::Indexed(236),
            },
            Depth::Ansi16 => Theme {
                depth,
                accent: Color::LightRed,
                accent_dim: Color::Red,
                fg: Color::Reset,
                muted: Color::Gray,
                faint: Color::DarkGray,
                rule: Color::DarkGray,
                profit: Color::Green,
                loss: Color::Red,
                warn: Color::Yellow,
                select_bg: Color::Reset,
            },
            Depth::None => Theme {
                depth,
                accent: Color::Reset,
                accent_dim: Color::Reset,
                fg: Color::Reset,
                muted: Color::Reset,
                faint: Color::Reset,
                rule: Color::Reset,
                profit: Color::Reset,
                loss: Color::Reset,
                warn: Color::Reset,
                select_bg: Color::Reset,
            },
        }
    }

    pub fn text(&self) -> Style {
        Style::new().fg(self.fg)
    }
    pub fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }
    pub fn faint(&self) -> Style {
        Style::new().fg(self.faint)
    }
    pub fn rule(&self) -> Style {
        Style::new().fg(self.rule)
    }
    pub fn accent(&self) -> Style {
        Style::new().fg(self.accent)
    }
    pub fn accent_bold(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }
    pub fn header(&self, focused: bool) -> Style {
        if focused { self.accent_bold() } else { Style::new().fg(self.muted).add_modifier(Modifier::BOLD) }
    }
    pub fn selected(&self) -> Style {
        match self.depth {
            Depth::None | Depth::Ansi16 => Style::new().add_modifier(Modifier::REVERSED),
            _ => Style::new().fg(self.accent).bg(self.select_bg),
        }
    }
    /// Profit/loss coloring for signed values.
    pub fn pnl(&self, v: f64) -> Style {
        if v > 0.0 {
            Style::new().fg(self.profit)
        } else if v < 0.0 {
            Style::new().fg(self.loss)
        } else {
            self.muted()
        }
    }
    pub fn warn(&self) -> Style {
        Style::new().fg(self.warn)
    }
}

/// Glyph set. `ascii` must stay readable on any terminal/font.
#[derive(Clone, Debug)]
pub struct Glyphs {
    pub unicode: bool,
    pub h: &'static str,
    pub v: &'static str,
    pub up_turn: (&'static str, &'static str), // (at old row, at new row) when rising
    pub down_turn: (&'static str, &'static str), // when falling
    pub axis_v: &'static str,
    pub axis_corner: &'static str,
    pub axis_tick: &'static str,
    pub guide: &'static str,
    pub cursor: &'static str,
    pub dot: &'static str,
    pub marker_opp: &'static str,
    pub marker_exec: &'static str,
    pub marker_entry: &'static str,
    pub marker_exit: &'static str,
    pub candle_body: &'static str,
    pub candle_wick: &'static str,
    pub candle_flat: &'static str,
    pub bullet: &'static str,
    pub live: &'static str,
    pub off: &'static str,
    pub arrow: &'static str,
    pub tree_mid: &'static str,
    pub tree_end: &'static str,
    pub spark: [&'static str; 8],
    pub ellipsis: &'static str,
}

impl Glyphs {
    pub fn unicode() -> Glyphs {
        Glyphs {
            unicode: true,
            h: "─",
            v: "│",
            up_turn: ("╯", "╭"),
            down_turn: ("╮", "╰"),
            axis_v: "│",
            axis_corner: "└",
            axis_tick: "┤",
            guide: "┊",
            cursor: "│",
            dot: "·",
            marker_opp: "◆",
            marker_exec: "▲",
            marker_entry: "▸",
            marker_exit: "◂",
            candle_body: "█",
            candle_wick: "│",
            candle_flat: "━",
            bullet: "●",
            live: "●",
            off: "○",
            arrow: "→",
            tree_mid: "├─",
            tree_end: "└─",
            spark: ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
            ellipsis: "…",
        }
    }

    pub fn ascii() -> Glyphs {
        Glyphs {
            unicode: false,
            h: "-",
            v: "|",
            up_turn: ("+", "+"),
            down_turn: ("+", "+"),
            axis_v: "|",
            axis_corner: "+",
            axis_tick: "+",
            guide: ":",
            cursor: "|",
            dot: ".",
            marker_opp: "o",
            marker_exec: "^",
            marker_entry: ">",
            marker_exit: "<",
            candle_body: "#",
            candle_wick: "|",
            candle_flat: "=",
            bullet: "*",
            live: "*",
            off: "o",
            arrow: "->",
            tree_mid: "|-",
            tree_end: "`-",
            spark: ["_", "_", ".", ".", "-", "-", "=", "#"],
            ellipsis: "~",
        }
    }

    pub fn detect(mode: GlyphMode) -> Glyphs {
        match mode {
            GlyphMode::Unicode => Glyphs::unicode(),
            GlyphMode::Ascii => Glyphs::ascii(),
            GlyphMode::Auto => {
                let loc = ["LC_ALL", "LC_CTYPE", "LANG"]
                    .iter()
                    .filter_map(|k| std::env::var(k).ok())
                    .find(|v| !v.is_empty())
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                if loc.contains("UTF-8") || loc.contains("UTF8") || cfg!(windows) {
                    Glyphs::unicode()
                } else {
                    Glyphs::ascii()
                }
            }
        }
    }

    pub fn spark(&self, frac: f64) -> &'static str {
        let i = (frac.clamp(0.0, 1.0) * 7.0).round() as usize;
        self.spark[i.min(7)]
    }
}

thread_local! {
    static ASCII: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Set per render pass (the renderer runs on one thread).
pub fn set_ascii(on: bool) {
    ASCII.with(|a| a.set(on));
}

pub fn ascii_mode() -> bool {
    ASCII.with(|a| a.get())
}

/// Fold text to ASCII when the glyph mode is ASCII, so labels coming from
/// data (routes, arrows, separators) never leak unsupported characters.
pub fn fold(s: &str) -> std::borrow::Cow<'_, str> {
    if !ascii_mode() || s.is_ascii() {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            c if c.is_ascii() => o.push(c),
            '·' | '•' => o.push('.'),
            '→' | '▸' | '›' => o.push('>'),
            '←' | '◂' | '‹' => o.push('<'),
            '⇄' => o.push_str("<>"),
            '≈' | '…' => o.push('~'),
            '─' | '━' | '–' | '—' => o.push('-'),
            '│' | '┊' | '┃' => o.push('|'),
            '●' => o.push('*'),
            '○' => o.push('o'),
            '◆' => o.push('o'),
            '▲' | '↑' => o.push('^'),
            '↓' => o.push('v'),
            '−' => o.push('-'),
            '≥' => o.push_str(">="),
            '¬' => o.push('!'),
            'ø' => o.push('o'),
            'Ø' => o.push('O'),
            'µ' => o.push('u'),
            '◎' => o.push('@'),
            '⏎' => o.push_str("enter"),
            '×' => o.push('x'),
            'Δ' => o.push('d'),
            '├' | '└' | '┤' | '┴' | '┬' | '┼' | '╭' | '╮' | '╯' | '╰' => o.push('+'),
            _ => o.push('?'),
        }
    }
    std::borrow::Cow::Owned(o)
}
