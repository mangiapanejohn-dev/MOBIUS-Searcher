use ratatui::{buffer::Buffer, layout::Rect};
use searcher_core::{Ts, metrics::Unit, series::TimeSeries};
use searcher_tui::chart::{ChartInput, render_chart};
use searcher_tui::hub::{Marker, MarkerKind};
use searcher_tui::theme::{Depth, Glyphs, Theme};
use searcher_tui::workspace::{ChartStyle, LineStyle};

fn main() {
    let mut s = TimeSeries::default();
    for i in 0..300 {
        s.push(
            Ts(1_789_700_000_000_000 + i * 1_000_000),
            105.3 + (i as f64 / 23.0).sin() * 0.12 + (i as f64 / 7.0).cos() * 0.03,
        );
    }
    let t0 = Ts(1_789_700_000_000_000);
    let markers = vec![
        Marker { ts: Ts(t0.0 + 80_000_000), kind: MarkerKind::Opportunity, opportunity: Default::default() },
        Marker { ts: Ts(t0.0 + 81_000_000), kind: MarkerKind::Execution, opportunity: Default::default() },
    ];
    for (style, line, g) in [
        (ChartStyle::Line, LineStyle::Box, Glyphs::unicode()),
        (ChartStyle::Line, LineStyle::Braille, Glyphs::unicode()),
        (ChartStyle::Candle, LineStyle::Box, Glyphs::unicode()),
        (ChartStyle::Line, LineStyle::Box, Glyphs::ascii()),
    ] {
        let area = Rect::new(0, 0, 96, 12);
        let mut buf = Buffer::empty(area);
        let inp = ChartInput {
            title: "Price",
            subtitle: "SOL/USD · jupiter /build best route · 1 SOL",
            series: Some(&s),
            unit: Unit::Usd,
            t0,
            t1: Ts(t0.0 + 299_000_000),
            style,
            line,
            cursor: Some(Ts(t0.0 + 150_000_000)),
            a: Some(Ts(t0.0 + 60_000_000)),
            b: Some(Ts(t0.0 + 240_000_000)),
            markers: &markers,
            active: true,
            y_label_w: 9,
            candle_min_samples: 3,
            empty_note: "",
        };
        render_chart(area, &mut buf, &inp, &Theme::with_depth(Depth::None), &g);
        for y in 0..area.height {
            println!("{}", (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>());
        }
        println!();
    }
}
