//! Terminal loop. Runs on its own OS thread; reads the view model under a
//! short read lock per frame and never blocks the engine. Terminal state is
//! restored on exit and on panic (ratatui installs the panic hook).

use crate::app::{App, TuiOptions};
use crate::hub::ViewModel;
use crate::ui;
use parking_lot::RwLock;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use searcher_core::event::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub type CommandSink = Box<dyn Fn(Command) + Send>;

pub fn run(
    vm: Arc<RwLock<ViewModel>>,
    send: CommandSink,
    opts: TuiOptions,
    stop: Arc<AtomicBool>,
    startup_keys: Vec<ratatui::crossterm::event::KeyEvent>,
) -> std::io::Result<()> {
    use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    let mut terminal = ratatui::try_init()?;
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::terminal::SetTitle(crate::brand::NAME));
    let mut app = App::new(&opts);
    if !vm.read().replay {
        app.cex = opts
            .okx
            .clone()
            .filter(|s| !s.markets.is_empty())
            .map(|s| crate::cex::Cex::start(s, app.mk_pair, app.mk_bar));
    }
    // graphics query must run in the alternate screen, before events are read
    if crate::brand::can_draw_logo(&app.theme, &app.glyphs) {
        app.logo_image = crate::brand::terminal_logo();
        // A terminal that echoed the query leaves text behind: wipe it. Nothing
        // is drawn yet (ratatui's buffer is blank), so a raw clear matches it;
        // `Terminal::clear` would query the cursor and fail on a silent terminal.
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::terminal::Clear(ratatui::crossterm::terminal::ClearType::All)
        );
    }
    if opts.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        // ratatui's hook restores the screen on panic; mouse reporting too
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
            prev(info);
        }));
    }
    {
        let g = vm.read();
        for k in startup_keys {
            for c in app.on_key(k, &g) {
                send(c);
            }
        }
    }
    let frame = Duration::from_millis(1000 / opts.fps.clamp(1, 60) as u64);
    let mut last_rev = u64::MAX;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let result = (|| -> std::io::Result<()> {
        loop {
            if stop.load(Ordering::Relaxed) || app.quit {
                return Ok(());
            }
            let mut dirty = false;
            if event::poll(frame)? {
                match event::read()? {
                    TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        let cmds = {
                            let g = vm.read();
                            app.on_key(k, &g)
                        };
                        for c in cmds {
                            send(c);
                        }
                        dirty = true;
                    }
                    TermEvent::Mouse(m) => {
                        let cmds = {
                            let g = vm.read();
                            app.on_mouse(m, &g)
                        };
                        for c in cmds {
                            send(c);
                        }
                        dirty = !matches!(m.kind, event::MouseEventKind::Moved);
                    }
                    TermEvent::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
            let rev = vm.read().revision;
            // Redraw on new data, input, resize — and at least 4×/s for clocks/ages.
            if dirty || rev != last_rev || last_draw.elapsed() >= Duration::from_millis(250) {
                let g = vm.read();
                terminal.draw(|f| ui::draw(f, &mut app, &g))?;
                last_rev = rev;
                last_draw = Instant::now();
            }
        }
    })();
    if opts.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

/// Render one frame off-screen (tests, `--snapshot`).
pub fn snapshot(app: &mut App, vm: &ViewModel, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    ui::render(&mut buf, area, app, vm);
    buf
}

pub fn buffer_text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in a.y..a.bottom() {
        let mut line = String::new();
        let mut skip = 0usize;
        for x in a.x..a.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let s = buf[(x, y)].symbol();
            skip = unicode_width::UnicodeWidthStr::width(s).saturating_sub(1);
            line.push_str(s);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn css(c: Color, default: &str) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => {
            // xterm-256 approximation
            let (r, g, b) = match i {
                16..=231 => {
                    let i = i - 16;
                    let s = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                    (s(i / 36), s((i / 6) % 6), s(i % 6))
                }
                232..=255 => {
                    let v = 8 + (i - 232) * 10;
                    (v, v, v)
                }
                _ => (200, 200, 200),
            };
            format!("#{r:02x}{g:02x}{b:02x}")
        }
        Color::Red => "#cd3131".into(),
        Color::LightRed => "#f14c4c".into(),
        Color::Green => "#0dbc79".into(),
        Color::Yellow => "#e5e510".into(),
        Color::Gray => "#a0a0a0".into(),
        Color::DarkGray => "#666666".into(),
        _ => default.into(),
    }
}

/// Self-contained HTML rendering of a frame (for screenshots in reports).
pub fn buffer_html(buf: &Buffer, title: &str) -> String {
    let a = buf.area;
    let (bg0, fg0, name) = ("#1a1917", "#d6d3cc", crate::brand::NAME);
    let mut s = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{name} · {title}</title><style>body{{margin:0;background:{bg0}}}\
         pre{{margin:0;padding:14px 16px;font:13px/1.28 'SF Mono','JetBrains Mono',Menlo,monospace;color:{fg0};background:{bg0}}}\
         span{{white-space:pre;display:inline-block;width:1ch;overflow:hidden;vertical-align:top}}</style></head><body><pre>"
    );
    for y in a.y..a.bottom() {
        let mut x = a.x;
        while x < a.right() {
            let c = &buf[(x, y)];
            let mut fg = css(c.fg, fg0);
            let mut bg = css(c.bg, bg0);
            if c.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let bold = c.modifier.contains(Modifier::BOLD);
            let sym = c.symbol();
            // Quadrant blocks (logo, candles) are drawn as exact cell quadrants,
            // as terminals do; font glyphs leave gaps at this line height.
            // (fg quadrants over a bg base, so edges blend the cell's own two
            // colours, never the page)
            let quadrant = " ▘▝▀▖▌▞▛▗▚▐▜▄▙▟█".chars().position(|q| sym.starts_with(q) && sym.len() == q.len_utf8());
            if let Some(m) = quadrant.filter(|&m| m > 0) {
                let layers: String = [(0, "left top"), (1, "right top"), (2, "left bottom"), (3, "right bottom")]
                    .iter()
                    .filter(|(bit, _)| m < 15 && m >> bit & 1 == 1)
                    .map(|(_, pos)| format!("linear-gradient({fg},{fg}) {pos}/50% 50% no-repeat,"))
                    .collect();
                let base = if m == 15 { &fg } else { &bg };
                s.push_str(&format!("<span style=\"background:{layers}{base}\"> </span>"));
                x += 1;
                continue;
            }
            let esc = sym.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            s.push_str(&format!(
                "<span style=\"color:{fg};background:{bg}{}\">{esc}</span>",
                if bold { ";font-weight:600" } else { "" }
            ));
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        s.push('\n');
    }
    s.push_str("</pre></body></html>");
    s
}

/// Parse a key script: plain characters plus `<left>`, `<right>`, `<up>`,
/// `<down>`, `<tab>`, `<enter>`, `<esc>`, `<home>`, `<end>`, with optional
/// repetition `<left*30>`.
pub fn parse_keys(script: &str) -> Result<Vec<ratatui::crossterm::event::KeyEvent>, String> {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut out = Vec::new();
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            continue;
        }
        let tok: String = chars.by_ref().take_while(|c| *c != '>').collect();
        let (name, n) = match tok.split_once('*') {
            Some((a, b)) => (a.to_string(), b.parse::<usize>().map_err(|_| format!("bad repeat in <{tok}>"))?),
            None => (tok.clone(), 1),
        };
        let code = match name.as_str() {
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "tab" => KeyCode::Tab,
            "enter" => KeyCode::Enter,
            "esc" => KeyCode::Esc,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            other => return Err(format!("unknown key <{other}>")),
        };
        for _ in 0..n {
            out.push(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn key_script() {
        let k = parse_keys("4a<left*3>b<tab>").unwrap();
        assert_eq!(k.len(), 7);
        assert_eq!(k[2].code, KeyCode::Left);
        assert_eq!(k[6].code, KeyCode::Tab);
        assert!(parse_keys("<bogus>").is_err());
    }
}
