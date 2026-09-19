//! Inline first-run setup in the style of OpenClaw's Clack wizard: the real
//! logo on top, then one symbol rail in the normal scrollback (no alternate
//! screen). Every line is word-wrapped before it is printed, so the redraw of
//! the active prompt always knows exactly how many rows to erase.

use anyhow::Result;
use ratatui::crossterm::cursor::{MoveToColumn, MoveUp};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size};
use searcher_tui::brand::{InlineArt, InlineLogo};
use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Lines never run wider than this, however wide the terminal is.
const MAX_WIDTH: usize = 84;

// Primary text uses the terminal's own foreground so light themes stay
// readable; the accent and the greys work on dark and light backgrounds.
const ACCENT: &str = "\x1b[38;2;194;139;91m";
const MUTED: &str = "\x1b[38;2;150;140;128m";
const FAINT: &str = "\x1b[38;2;112;103;93m";
const GREEN: &str = "\x1b[38;2;118;178;138m";
const AMBER: &str = "\x1b[38;2;217;165;91m";
const RED: &str = "\x1b[38;2;204;105;91m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

thread_local! {
    static ACTIVE: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("setup cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The error every prompt returns on Esc / Ctrl-C.
pub fn cancelled() -> anyhow::Error {
    Cancelled.into()
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

struct Ui {
    step: Option<Step>,
    saved: bool,
    finished: bool,
    /// The last committed line was an empty rail (`│`), so a new block
    /// needs no extra spacing.
    spaced: bool,
}

struct Step {
    current: usize,
    total: usize,
    title: String,
    details: Vec<String>,
}

/// Owns the rail: prints the header on start and the closing `└` line when
/// setup ends without `finish` (Esc, Ctrl-C or an error).
pub struct Session {
    visual: bool,
}

impl Session {
    /// `rerun`: a config already exists (it is backed up before replacing).
    pub fn start(rerun: bool) -> Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Ok(Self { visual: false });
        }
        let (width, height) = size().map_or((80, 24), |(w, h)| (w as usize, h as usize));
        // the first question must still fit under the header
        let logo_rows = height.saturating_sub(13).min(24) as u16;
        let logo = searcher_tui::brand::inline_logo(width.saturating_sub(4).min(60) as u16, logo_rows);
        let mut out = header(logo.as_ref(), width);
        let (title, hint) = if rerun {
            ("Setup", "your current settings stay unless you change them")
        } else {
            ("First-run setup", "about 2 minutes · nothing is written until you save")
        };
        out.push_str(&format!("{}  {}", paint(ACCENT, "┌"), paint(BOLD, title)));
        if 3 + title.width() + 2 + hint.width() <= content_width() {
            out.push_str(&format!("  {}\r\n", paint(FAINT, hint)));
        } else {
            out.push_str("\r\n");
            for part in wrap(hint, content_width().saturating_sub(3)) {
                out.push_str(&format!("{}  {}\r\n", paint(FAINT, "│"), paint(FAINT, &part)));
            }
        }
        out.push_str(&format!("{}\r\n", paint(FAINT, "│")));
        emit(&out)?;
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Ui { step: None, saved: false, finished: false, spaced: true }));
        Ok(Self { visual: true })
    }

    pub fn visual(&self) -> bool {
        self.visual
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.visual {
            return;
        }
        ACTIVE.with(|slot| {
            if let Some(ui) = slot.borrow_mut().take()
                && !ui.finished
            {
                let text =
                    if ui.saved { "Setup saved · not started" } else { "Setup cancelled · nothing was written" };
                let gap = if ui.spaced { String::new() } else { format!("{}\r\n", paint(FAINT, "│")) };
                let _ = emit(&format!(
                    "{gap}{}  {}\r\n\r\n",
                    paint(FAINT, "└"),
                    paint(if ui.saved { MUTED } else { RED }, text)
                ));
            }
        });
    }
}

pub fn active() -> bool {
    ACTIVE.with(|slot| slot.borrow().is_some())
}

/// The next step's heading; printed right before its first prompt or line.
pub fn step(current: usize, total: usize, title: &str, details: &[&str]) {
    with_ui(|ui| {
        ui.step = Some(Step {
            current,
            total,
            title: title.to_string(),
            details: details.iter().map(|d| (*d).to_string()).collect(),
        });
    });
}

/// Nothing after this point can be lost by cancelling.
pub fn mark_saved() {
    with_ui(|ui| ui.saved = true);
}

/// A boxed block attached to the rail. Rows are `(key, value)`; an empty key
/// makes a plain line and `("", "")` a blank one.
pub fn note(title: &str, rows: &[(&str, String)]) -> Result<()> {
    print_lines(&note_lines(title, rows, content_width(), (MUTED, "")))
}

/// Like `note`, but the keys lead (default colour) and the values are muted:
/// an outline of steps rather than a table of settings.
pub fn outline(title: &str, rows: &[(&str, String)]) -> Result<()> {
    print_lines(&note_lines(title, rows, content_width(), ("", MUTED)))
}

pub fn success(message: &str) -> Result<()> {
    rail_message(&paint(GREEN, "✓"), message, None)
}

pub fn info(message: &str) -> Result<()> {
    rail_message(" ", message, Some(MUTED))
}

/// One connection-test result: ✓ ok, ✗ failed, ! failed but optional.
pub fn check_line(ok: bool, optional: bool, name: &str, detail: &str) -> Result<()> {
    let mark = match (ok, optional) {
        (true, _) => paint(GREEN, "✓"),
        (false, false) => paint(RED, "✗"),
        (false, true) => paint(AMBER, "!"),
    };
    let name_width = 17;
    let text_width = content_width().saturating_sub(5 + name_width + 1).max(12);
    let mut lines = with_step_only();
    for (i, part) in wrap(detail, text_width).iter().enumerate() {
        let (lead, label) =
            if i == 0 { (mark.clone(), format!("{name:<name_width$}")) } else { (" ".into(), " ".repeat(name_width)) };
        lines.push(format!("{}  {lead} {} {}", paint(FAINT, "│"), label, paint(MUTED, part)));
    }
    print_lines(&lines)
}

/// Runs `work` while a spinner line turns on the rail; the line is erased
/// afterwards. Plain output (no terminal) just runs `work`.
pub fn with_spinner<T>(label: &str, work: impl FnOnce() -> T) -> T {
    if !active() {
        return work();
    }
    let _ = print_lines(&block_start());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let label = label.to_string();
    let spinner = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = emit(&format!("\r{}  {} {}", paint(FAINT, "│"), paint(ACCENT, FRAMES[i % 10]), label));
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(90));
            }
            let _ = emit("\r\x1b[2K");
        })
    };
    let out = work();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = spinner.join();
    out
}

/// The QR code of `data` with a two-module quiet zone; `true` = dark module.
pub fn qr_matrix(data: &str) -> Option<Vec<Vec<bool>>> {
    let code = qrcode::QrCode::with_error_correction_level(data, qrcode::EcLevel::M).ok()?;
    let (width, quiet) = (code.width(), 2);
    let size = width + 2 * quiet;
    Some(
        (0..size)
            .map(|y| {
                (0..size)
                    .map(|x| {
                        let inside = (quiet..quiet + width).contains(&x) && (quiet..quiet + width).contains(&y);
                        inside && code[(x - quiet, y - quiet)] == qrcode::Color::Dark
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Two modules per cell (`▀` = upper). With colour the modules are painted
/// black on white explicitly, so the code scans on dark and light themes;
/// without colour a dark terminal background is assumed.
fn qr_rows(matrix: &[Vec<bool>], colour: bool) -> Vec<String> {
    let blank = vec![false; matrix.first().map_or(0, Vec::len)];
    matrix
        .chunks(2)
        .map(|pair| {
            let (top, bottom) = (&pair[0], pair.get(1).unwrap_or(&blank));
            let mut row = String::new();
            for (&t, &b) in top.iter().zip(bottom) {
                if colour {
                    let shade = |dark: bool| if dark { "0;0;0" } else { "255;255;255" };
                    row.push_str(&format!("\x1b[38;2;{}m\x1b[48;2;{}m▀", shade(t), shade(b)));
                } else {
                    row.push(match (t, b) {
                        (false, false) => '█',
                        (true, false) => '▄',
                        (false, true) => '▀',
                        (true, true) => ' ',
                    });
                }
            }
            if colour {
                row.push_str(RESET);
            }
            row
        })
        .collect()
}

/// A scannable QR code of `data` on the rail between a caption and `label`
/// (what to copy by hand); the code is left out when the terminal is too
/// narrow for it.
pub fn qr(data: &str, label: &str, caption: &str) -> Result<()> {
    let mut lines = with_step_only();
    for part in wrap(caption, content_width().saturating_sub(5)) {
        lines.push(format!("{}    {}", paint(FAINT, "│"), paint(MUTED, &part)));
    }
    if let Some(matrix) = qr_matrix(data)
        && matrix.len() + 5 <= content_width()
    {
        for row in qr_rows(&matrix, colors()) {
            lines.push(format!("{}    {row}", paint(FAINT, "│")));
        }
    }
    lines.push(format!("{}    {}", paint(FAINT, "│"), paint(BOLD, label)));
    print_lines(&lines)
}

/// A warning on the rail itself (`▲`).
pub fn warn(message: &str) -> Result<()> {
    let mut lines = block_start();
    let wrapped = wrap(message, content_width().saturating_sub(3));
    for (i, part) in wrapped.iter().enumerate() {
        let lead = if i == 0 { paint(AMBER, "▲") } else { paint(FAINT, "│") };
        lines.push(format!("{lead}  {}", paint(AMBER, part)));
    }
    lines.push(paint(FAINT, "│"));
    print_lines(&lines)
}

/// Closes the rail.
pub fn finish(title: &str, hint: &str) -> Result<()> {
    let mut lines = block_start();
    lines.push(format!("{}  {}  {}", paint(FAINT, "└"), paint(GREEN, title), paint(FAINT, hint)));
    lines.push(String::new());
    print_lines(&lines)?;
    with_ui(|ui| ui.finished = true);
    Ok(())
}

fn rail_message(mark: &str, message: &str, style: Option<&str>) -> Result<()> {
    let mut lines = with_step_only();
    for (i, part) in wrap(message, content_width().saturating_sub(5)).iter().enumerate() {
        let lead = if i == 0 { format!("{mark} ") } else { "  ".into() };
        let text = style.map_or_else(|| part.clone(), |s| paint(s, part));
        lines.push(format!("{}  {lead}{text}", paint(FAINT, "│")));
    }
    print_lines(&lines)
}

// ---------------------------------------------------------------- prompts

pub struct MenuItem<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub badge: Option<&'a str>,
}

pub fn prompt_menu(prompt: &str, items: &[MenuItem<'_>], default: usize) -> Result<Option<usize>> {
    if !active() {
        return Ok(None);
    }
    let mut selected = default.min(items.len().saturating_sub(1));
    interact(
        &mut selected,
        |selected| menu_frame(prompt, items, *selected),
        |selected, key| match key.code {
            KeyCode::Enter => Key::Done(items[*selected].title.to_string()),
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab | KeyCode::Char('k') => {
                *selected = selected.checked_sub(1).unwrap_or(items.len() - 1);
                Key::Redraw
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::Char('j') => {
                *selected = (*selected + 1) % items.len();
                Key::Redraw
            }
            KeyCode::Char(c) => match c.to_digit(10).and_then(|n| (n as usize).checked_sub(1)) {
                Some(i) if i < items.len() => {
                    *selected = i;
                    Key::Redraw
                }
                _ => Key::Ignore,
            },
            _ => Key::Ignore,
        },
        prompt,
    )?;
    Ok(Some(selected))
}

pub fn prompt_bool(prompt: &str, default: bool) -> Result<Option<bool>> {
    if !active() {
        return Ok(None);
    }
    let mut yes = default;
    interact(
        &mut yes,
        |yes| confirm_frame(prompt, *yes),
        |yes, key| match key.code {
            KeyCode::Enter => Key::Done(if *yes { "Yes" } else { "No" }.into()),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                *yes = !*yes;
                Key::Redraw
            }
            KeyCode::Char('y' | 'Y') => {
                *yes = true;
                Key::Done("Yes".into())
            }
            KeyCode::Char('n' | 'N') => {
                *yes = false;
                Key::Done("No".into())
            }
            _ => Key::Ignore,
        },
        prompt,
    )?;
    Ok(Some(yes))
}

pub type Validator<'a> = &'a dyn Fn(&str) -> std::result::Result<(), String>;

pub struct TextPrompt<'a> {
    pub label: &'a str,
    /// Shown faint while the input is empty.
    pub placeholder: &'a str,
    /// Shown as the answer when Enter is pressed on an empty input.
    pub empty_answer: &'a str,
    pub hidden: bool,
    /// Checked on Enter; an error keeps the prompt open and shows the reason.
    pub validate: Option<Validator<'a>>,
}

struct Input {
    value: String,
    error: Option<String>,
}

pub fn prompt_text(spec: &TextPrompt<'_>) -> Result<Option<String>> {
    if !active() {
        return Ok(None);
    }
    let mut input = Input { value: String::new(), error: None };
    interact(
        &mut input,
        |input| text_frame(spec, &input.value, input.error.as_deref()),
        |input, key| {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => {
                    let value = input.value.trim();
                    if let Some(validate) = spec.validate
                        && let Err(reason) = validate(value)
                    {
                        input.error = Some(reason);
                        return Key::Redraw;
                    }
                    Key::Done(if value.is_empty() {
                        spec.empty_answer.into()
                    } else if spec.hidden {
                        "saved (hidden)".into()
                    } else {
                        value.to_string()
                    })
                }
                KeyCode::Char('u') if ctrl => {
                    input.value.clear();
                    input.error = None;
                    Key::Redraw
                }
                KeyCode::Char('w') if ctrl => {
                    let kept = input.value.trim_end().rsplit_once(' ').map_or("", |(head, _)| head).len();
                    input.value.truncate(kept);
                    Key::Redraw
                }
                KeyCode::Backspace => {
                    input.value.pop();
                    input.error = None;
                    Key::Redraw
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    input.value.push(c);
                    input.error = None;
                    Key::Redraw
                }
                _ => Key::Ignore,
            }
        },
        spec.label,
    )?;
    Ok(Some(input.value))
}

enum Key {
    Redraw,
    Ignore,
    /// Committed; the string is the answer shown under the finished prompt.
    Done(String),
}

/// Draws `frame(state)` under the rail and feeds keys to `on_key` until it
/// returns `Done`; the live frame is then replaced by `◇ prompt / answer`.
fn interact<S>(
    state: &mut S,
    frame: impl Fn(&S) -> Vec<String>,
    mut on_key: impl FnMut(&mut S, &KeyEvent) -> Key,
    prompt: &str,
) -> Result<()> {
    print_lines(&block_start())?;
    let raw = RawMode::new()?;
    let mut drawn = draw(&frame(state))?;
    loop {
        let key = read_key()?;
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || ctrl_c {
            erase(drawn)?;
            print_lines(&[
                format!("{}  {}", paint(RED, "■"), paint(MUTED, prompt)),
                format!("{}  {}", paint(FAINT, "│"), paint(FAINT, "cancelled")),
            ])?;
            drop(raw);
            return Err(cancelled());
        }
        match on_key(state, &key) {
            Key::Ignore => {}
            Key::Redraw => {
                erase(drawn)?;
                drawn = draw(&frame(state))?;
            }
            Key::Done(answer) => {
                erase(drawn)?;
                drop(raw);
                let width = content_width().saturating_sub(3);
                let mut lines = Vec::new();
                for (i, part) in wrap(prompt, width).iter().enumerate() {
                    let lead = if i == 0 { paint(ACCENT, "◇") } else { paint(FAINT, "│") };
                    lines.push(format!("{lead}  {part}"));
                }
                for part in wrap(&answer, width) {
                    lines.push(format!("{}  {}", paint(FAINT, "│"), paint(MUTED, &part)));
                }
                lines.push(paint(FAINT, "│"));
                return print_lines(&lines);
            }
        }
    }
}

fn prompt_head(prompt: &str, width: usize) -> Vec<String> {
    wrap(prompt, width.saturating_sub(3))
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let lead = if i == 0 { paint(ACCENT, "◆") } else { paint(FAINT, "│") };
            format!("{lead}  {}", paint(BOLD, part))
        })
        .collect()
}

fn hint_lines(text: &str) -> Vec<String> {
    wrap(text, content_width().saturating_sub(3))
        .iter()
        .enumerate()
        .map(|(i, part)| format!("{}  {}", paint(FAINT, if i == 0 { "└" } else { " " }), paint(FAINT, part)))
        .collect()
}

fn menu_frame(prompt: &str, items: &[MenuItem<'_>], selected: usize) -> Vec<String> {
    let width = content_width();
    let rail = paint(FAINT, "│");
    let mut lines = prompt_head(prompt, width);
    for (i, item) in items.iter().enumerate() {
        let on = i == selected;
        let badge = item.badge.map_or(String::new(), |b| format!("  {}", paint(ACCENT, b)));
        let title_width = width.saturating_sub(5 + item.badge.map_or(0, |b| b.width() + 2));
        for (j, part) in wrap(item.title, title_width).iter().enumerate() {
            let mark = match (j, on) {
                (0, true) => paint(ACCENT, "●"),
                (0, false) => paint(FAINT, "○"),
                _ => " ".into(),
            };
            let title = if on { paint(BOLD, part) } else { paint(MUTED, part) };
            lines.push(format!("{rail}  {mark} {title}{}", if j == 0 { badge.as_str() } else { "" }));
        }
        if !item.description.is_empty() {
            for part in wrap(item.description, width.saturating_sub(5)) {
                lines.push(format!("{rail}    {}", paint(if on { MUTED } else { FAINT }, &part)));
            }
        }
    }
    let jump = if items.len() > 2 { format!(" · 1–{} jump", items.len().min(9)) } else { String::new() };
    lines.extend(hint_lines(&format!("↑↓ move{jump} · enter select · esc cancel")));
    lines
}

fn confirm_frame(prompt: &str, yes: bool) -> Vec<String> {
    let mut lines = prompt_head(prompt, content_width());
    let option = |label: &str, on: bool| {
        if on {
            format!("{} {}", paint(ACCENT, "●"), paint(BOLD, label))
        } else {
            format!("{} {}", paint(FAINT, "○"), paint(MUTED, label))
        }
    };
    lines.push(format!("{}  {} {} {}", paint(FAINT, "│"), option("Yes", yes), paint(FAINT, "/"), option("No", !yes)));
    lines.extend(hint_lines("←→ switch · y / n · enter select · esc cancel"));
    lines
}

fn text_frame(spec: &TextPrompt<'_>, value: &str, error: Option<&str>) -> Vec<String> {
    let width = content_width();
    let rail = paint(FAINT, "│");
    let mut lines = prompt_head(spec.label, width);
    let shown = if spec.hidden { "•".repeat(value.chars().count()) } else { value.to_string() };
    let cursor = paint(ACCENT, "▌");
    if shown.is_empty() {
        let placeholder = if spec.placeholder.is_empty() { String::new() } else { paint(FAINT, spec.placeholder) };
        lines.push(format!("{rail}  {} {cursor}{placeholder}", paint(ACCENT, "›")));
    } else {
        let parts = wrap_hard(&shown, width.saturating_sub(6));
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let lead = if i == 0 { paint(ACCENT, "›") } else { " ".into() };
            lines.push(format!("{rail}  {lead} {part}{}", if i == last { cursor.as_str() } else { "" }));
        }
    }
    if let Some(error) = error {
        for part in wrap(error, width.saturating_sub(5)) {
            lines.push(format!("{rail}  {}", paint(RED, &part)));
        }
    }
    let hint = if spec.hidden {
        "hidden input · enter confirm · ctrl-u clear · esc cancel"
    } else {
        "enter confirm · ctrl-u clear · esc cancel"
    };
    lines.extend(hint_lines(hint));
    lines
}

// ----------------------------------------------------------------- header

/// Logo beside the wordmark and a two-line promise; stacked when narrow.
fn header(logo: Option<&InlineLogo>, width: usize) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut text: Vec<String> = vec![
        format!("{}{}{}{}", paint(BOLD, "M"), paint(&format!("{BOLD}{ACCENT}"), "Ø"), paint(BOLD, "BIUS"), "-Searcher"),
        paint(MUTED, "Solana · Jupiter arbitrage searcher"),
        paint(FAINT, &format!("v{version} · research first")),
        String::new(),
    ];
    let promise = "Nothing can sign or send a transaction until you explicitly unlock it.";
    let (cols, rows) = logo.map_or((0, 0), |l| (l.cols as usize, l.rows as usize));
    let side = logo.is_some() && width >= 2 + cols + 3 + 34;
    let text_width = if side { (width - 2 - cols - 3 - 1).min(46) } else { width.saturating_sub(3).min(60) };
    text.extend(wrap(promise, text_width).iter().map(|l| paint(MUTED, l)));

    let mut out = String::from("\r\n");
    let Some(logo) = logo else {
        for line in &text {
            out.push_str(&format!("  {line}\r\n"));
        }
        out.push_str("\r\n");
        return out;
    };
    if !side {
        out.push_str(&logo_block(logo, &[], 0));
        out.push_str("\r\n");
        for line in &text {
            out.push_str(&format!("  {line}\r\n"));
        }
        out.push_str("\r\n");
        return out;
    }
    let top = rows.saturating_sub(text.len()) / 2;
    let mut beside = vec![String::new(); top];
    beside.extend(text);
    out.push_str(&logo_block(logo, &beside, 2 + cols + 3));
    out.push_str("\r\n");
    out
}

/// The logo at column 2 with `beside[i]` printed from column `text_col` on
/// row i. Graphics are drawn into rows reserved first, so the terminal never
/// scrolls under a half-drawn image.
fn logo_block(logo: &InlineLogo, beside: &[String], text_col: usize) -> String {
    let rows = (logo.rows as usize).max(beside.len());
    let mut out = String::new();
    match &logo.art {
        InlineArt::Graphics { escape, .. } => {
            out.push_str(&"\r\n".repeat(rows));
            out.push_str(&format!("\x1b[{rows}A\r\x1b7  {escape}\x1b8"));
            for i in 0..rows {
                if let Some(line) = beside.get(i).filter(|l| !l.is_empty()) {
                    out.push_str(&format!("\x1b[{text_col}C{line}"));
                }
                out.push_str("\r\n");
            }
        }
        InlineArt::Cells(cells) => {
            let blank = " ".repeat(logo.cols as usize);
            for i in 0..rows {
                out.push_str("  ");
                out.push_str(cells.get(i).map_or(blank.as_str(), String::as_str));
                if let Some(line) = beside.get(i).filter(|l| !l.is_empty()) {
                    out.push_str(&format!("   {line}"));
                }
                out.push_str("\r\n");
            }
        }
    }
    out
}

// ------------------------------------------------------------------ boxes

fn note_lines(title: &str, rows: &[(&str, String)], width: usize, style: (&str, &str)) -> Vec<String> {
    let key_width = rows.iter().filter(|(k, _)| !k.is_empty()).map(|(k, _)| k.width()).max().unwrap_or(0);
    let longest =
        rows.iter().map(|(k, v)| if k.is_empty() { v.width() } else { key_width + 2 + v.width() }).max().unwrap_or(0);
    let box_width = (longest + 6).max(title.width() + 8).min(width);
    let inner = box_width.saturating_sub(6);
    let rail = paint(FAINT, "│");
    let mut lines = block_start();
    let dashes = box_width.saturating_sub(5 + title.width());
    lines.push(format!(
        "{}  {} {}",
        paint(ACCENT, "◇"),
        paint(BOLD, title),
        paint(FAINT, &format!("{}╮", "─".repeat(dashes)))
    ));
    let pad = |content: &str, visible: usize| {
        format!("{rail}  {content}{}  {rail}", " ".repeat(inner.saturating_sub(visible)))
    };
    lines.push(pad("", 0));
    for (key, value) in rows {
        if key.is_empty() {
            let parts = if value.is_empty() { vec![String::new()] } else { wrap(value, inner) };
            for part in parts {
                lines.push(pad(&part, part.width()));
            }
            continue;
        }
        let styled = |text: &str, s: &str| if s.is_empty() { text.to_string() } else { paint(s, text) };
        if inner < key_width + 2 + 16 {
            // too narrow for two columns: key above its value
            lines.push(pad(&styled(key, style.0), key.width()));
            for part in wrap(value, inner.saturating_sub(2)) {
                lines.push(pad(&format!("  {}", styled(&part, style.1)), 2 + part.width()));
            }
            continue;
        }
        for (i, part) in wrap(value, inner - key_width - 2).iter().enumerate() {
            let key_cell = if i == 0 { format!("{key:<key_width$}") } else { " ".repeat(key_width) };
            let content = format!("{}  {}", styled(&key_cell, style.0), styled(part, style.1));
            lines.push(pad(&content, key_width + 2 + part.width()));
        }
    }
    lines.push(pad("", 0));
    lines.push(paint(FAINT, &format!("├{}╯", "─".repeat(box_width.saturating_sub(2)))));
    lines.push(rail);
    lines
}

/// A pending step heading (with its gap), but no gap for plain rail lines
/// that belong to the block above.
fn with_step_only() -> Vec<String> {
    let pending = ACTIVE.with(|slot| slot.borrow().as_ref().is_some_and(|ui| ui.step.is_some()));
    if pending { block_start() } else { Vec::new() }
}

/// What must precede a new block: an empty rail line if the previous output
/// did not end with one, then a pending step heading (itself followed by one).
fn block_start() -> Vec<String> {
    let mut lines = Vec::new();
    with_ui(|ui| {
        if !ui.spaced {
            lines.push(paint(FAINT, "│"));
        }
    });
    lines.extend(flush_step());
    lines
}

fn flush_step() -> Vec<String> {
    let mut lines = Vec::new();
    with_ui(|ui| {
        let Some(step) = ui.step.take() else { return };
        let progress = if step.total > 1 {
            let dots: String = (1..=step.total)
                .map(|i| if i <= step.current { paint(ACCENT, "●") } else { paint(FAINT, "○") })
                .collect();
            format!("  {dots} {}", paint(FAINT, &format!("{}/{}", step.current, step.total)))
        } else {
            String::new()
        };
        lines.push(format!("{}  {}{progress}", paint(ACCENT, "◇"), paint(&format!("{BOLD}{ACCENT}"), &step.title)));
        for detail in &step.details {
            for part in wrap(detail, content_width().saturating_sub(3)) {
                lines.push(format!("{}  {}", paint(FAINT, "│"), paint(MUTED, &part)));
            }
        }
        lines.push(paint(FAINT, "│"));
    });
    lines
}

// ----------------------------------------------------------------- output

/// Every line is wrapped to this, so none of them is ever wrapped again by
/// the terminal (which would break the redraw's row count).
fn content_width() -> usize {
    size().map_or(80, |(w, _)| w as usize).saturating_sub(1).clamp(24, MAX_WIDTH)
}

/// Commits lines to the scrollback.
fn print_lines(lines: &[String]) -> Result<()> {
    let Some(last) = lines.last() else { return Ok(()) };
    let spaced = matches!(last.trim_start_matches(FAINT).trim_end_matches(RESET), "│" | "");
    with_ui(|ui| ui.spaced = spaced);
    emit(&format!("{}\r\n", lines.join("\r\n")))
}

/// Prints a live frame (erased again before anything is committed) and
/// returns how many rows it occupies.
fn draw(lines: &[String]) -> Result<usize> {
    emit(&format!("{}\r\n", lines.join("\r\n")))?;
    Ok(lines.len())
}

fn erase(rows: usize) -> Result<()> {
    if rows == 0 {
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    queue!(stdout, MoveUp(rows as u16), MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    stdout.flush()?;
    Ok(())
}

fn emit(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// Word wrap on spaces; words longer than `width` (paths, addresses) are
/// broken hard.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let gap = usize::from(!line.is_empty());
        if line.width() + gap + word.width() <= width {
            if gap == 1 {
                line.push(' ');
            }
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if word.width() <= width {
            line.push_str(word);
        } else {
            let mut parts = wrap_hard(word, width);
            line = parts.pop().unwrap_or_default();
            lines.extend(parts);
        }
    }
    lines.push(line);
    lines
}

fn wrap_hard(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width.max(1) {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().expect("at least one line").push(c);
        used += w;
    }
    lines
}

fn with_ui(f: impl FnOnce(&mut Ui)) {
    ACTIVE.with(|slot| {
        if let Some(ui) = slot.borrow_mut().as_mut() {
            f(ui);
        }
    });
}

fn read_key() -> Result<KeyEvent> {
    loop {
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            return Ok(key);
        }
    }
}

struct RawMode;

impl RawMode {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn colors() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

fn paint(style: &str, text: &str) -> String {
    if colors() && !text.is_empty() { format!("{style}{text}{RESET}") } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                output.push(character);
            }
        }
        output
    }

    fn items() -> Vec<MenuItem<'static>> {
        vec![
            MenuItem { title: "Public access", description: "works immediately", badge: Some("recommended") },
            MenuItem { title: "My provider", description: "private RPC", badge: None },
        ]
    }

    #[test]
    fn menu_is_a_single_column_symbol_flow() {
        let frame: Vec<String> =
            menu_frame("How should MØBIUS connect?", &items(), 0).iter().map(|l| plain(l)).collect();
        assert_eq!(frame[0], "◆  How should MØBIUS connect?");
        assert_eq!(frame[1], "│  ● Public access  recommended");
        assert_eq!(frame[2], "│    works immediately");
        assert_eq!(frame[3], "│  ○ My provider");
        assert_eq!(frame[4], "│    private RPC");
        assert!(frame[5].starts_with("└  ↑↓ move"), "{frame:?}");
        assert!(frame.iter().all(|l| !l.contains('╭')));
    }

    #[test]
    fn every_line_fits_the_width_it_was_wrapped_for() {
        let long = "Public endpoints are enough for research. Assisted trading benefits from private RPC capacity.";
        for width in [40, 57, 84] {
            for line in wrap(long, width) {
                assert!(line.width() <= width, "{line:?} > {width}");
            }
        }
        // no word is split while it fits on a line of its own
        assert!(wrap(long, 40).iter().all(|l| !l.ends_with("ca")));
        let address = "9N67XSEmZkYMtrRHvBLn3fBycGDGh47o2opNANJeHr7p";
        assert_eq!(wrap(address, 20).concat(), address);
    }

    #[test]
    fn hidden_input_never_renders_the_secret() {
        let spec = TextPrompt { label: "API key", placeholder: "", empty_answer: "", hidden: true, validate: None };
        let frame = plain(&text_frame(&spec, "secret-value", None).join("\n"));
        assert!(!frame.contains("secret-value"), "{frame}");
        assert!(frame.contains("••••••••••••"), "{frame}");
    }

    #[test]
    fn notes_are_closed_boxes_of_one_width() {
        let rows = [("Mode", "PAPER · sending stays locked".to_string()), ("", String::new()), ("", "plain".into())];
        let lines: Vec<String> = note_lines("Review", &rows, 60, (MUTED, "")).iter().map(|l| plain(l)).collect();
        assert!(lines[0].starts_with("◇  Review ─") && lines[0].ends_with('╮'), "{lines:?}");
        let body = &lines[1..lines.len() - 2];
        let widths: Vec<usize> = body.iter().map(|l| l.width()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{lines:?}");
        assert_eq!(lines[0].width(), widths[0]);
        assert!(body.iter().all(|l| l.starts_with('│') && l.ends_with('│')));
        assert!(lines[lines.len() - 2].starts_with('├') && lines[lines.len() - 2].ends_with('╯'));
        assert!(lines.iter().any(|l| l.contains("Mode  PAPER")));
    }

    #[test]
    fn the_funding_qr_code_decodes_back_to_the_address() {
        let uri = "solana:9N67XSEmZkYMtrRHvBLn3fBycGDGh47o2opNANJeHr7p";
        let matrix = qr_matrix(uri).expect("encodes");
        // read the rendered cells back into pixels: each glyph is one module
        // wide and two tall, light = '█' in the no-colour form
        let rows = qr_rows(&matrix, false);
        let width = rows[0].chars().count();
        let mut dark = vec![vec![false; width]; rows.len() * 2];
        for (y, row) in rows.iter().enumerate() {
            for (x, glyph) in row.chars().enumerate() {
                let (top, bottom) = match glyph {
                    '█' => (false, false),
                    '▄' => (true, false),
                    '▀' => (false, true),
                    _ => (true, true),
                };
                dark[2 * y][x] = top;
                dark[2 * y + 1][x] = bottom;
            }
        }
        let scale = 6;
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(width * scale, dark.len() * scale, |x, y| {
            if dark[y / scale][x / scale] { 0 } else { 255 }
        });
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].decode().expect("decodes").1, uri);
        // the colour form paints the very same modules, one cell per pair
        let coloured = qr_rows(&matrix, true);
        assert_eq!(coloured.len(), rows.len());
        assert!(coloured.iter().all(|r| r.matches('▀').count() == width));
    }
}
