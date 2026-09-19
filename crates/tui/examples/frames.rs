//! Print rendered frames of a synthetic session: `cargo run -p searcher-tui --example frames -- 120 40 1`
#[path = "../tests/render.rs"]
#[allow(dead_code, unused_imports)]
mod render;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use searcher_tui::app::{App, TuiOptions};
use searcher_tui::theme::{Depth, Glyphs, Theme};
use searcher_tui::{buffer_text, snapshot};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let w: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let h: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let pages = args.get(3).cloned().unwrap_or("1".into());
    let vm = render::populated();
    let mut a = App::new(&TuiOptions::default());
    a.theme = Theme::with_depth(Depth::TrueColor);
    a.glyphs = if args.iter().any(|x| x == "ascii") { Glyphs::ascii() } else { Glyphs::unicode() };
    for p in pages.chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(p), KeyModifiers::NONE), &vm);
        if args.iter().any(|x| x == "ab") {
            a.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &vm);
            for _ in 0..40 {
                a.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &vm);
            }
            a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &vm);
        }
        println!("{}", buffer_text(&snapshot(&mut a, &vm, w, h)));
    }
}
