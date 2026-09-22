//! Render every page at the required terminal sizes (and below the minimum)
//! with a populated view model; nothing may panic, key content must be
//! visible, and ASCII mode must emit pure ASCII.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use searcher_core::costs::CostBreakdown;
use searcher_core::event::{SessionInfo, Stage, StageEvent};
use searcher_core::metrics::MetricId;
use searcher_core::model::*;
use searcher_core::profit::ProfitEval;
use searcher_core::{Address, Event, Ppm, Ts, UsdMicros, UsdPrice};
use searcher_tui::app::{App, TuiOptions};
use searcher_tui::theme::{Depth, Glyphs, Theme};
use searcher_tui::{ViewModel, buffer_text, snapshot};

const T0: i64 = 1_789_700_000_000_000;

fn leg(i: u8, dex: &str, inp: u64, out: u64) -> Leg {
    let sol: Address = searcher_core::address::well_known::addr(searcher_core::address::well_known::WSOL_MINT);
    let usdc: Address = searcher_core::address::well_known::addr(searcher_core::address::well_known::USDC_MINT);
    let (a, b) = if i == 0 { (sol, usdc) } else { (usdc, sol) };
    Leg {
        index: i,
        input_mint: a,
        output_mint: b,
        in_amount: inp,
        out_amount: out,
        min_out: out - out / 1000,
        slippage_bps: 15,
        slippage_spec: SlippageSpec::Rtse,
        price_impact: Ppm(12),
        hops: vec![Hop {
            amm_key: Address([9; 32]),
            label: dex.into(),
            input_mint: a,
            output_mint: b,
            in_amount: inp,
            out_amount: out,
            bps: 10_000,
        }],
        mode: RoutingMode::Fast,
        dex_filter: DexFilter::Only(vec![dex.into()]),
        quoted_at: Ts(T0),
        latency_ms: 812,
        cu_price_micro: Some(2_719),
        last_valid_block_height: 426_039_712,
        request_id: None,
    }
}

fn opp(id: u64, t: i64, gross: i64, status: OppStatus) -> Opportunity {
    let input = 1_000_000_000u64;
    let out = (input as i64 + gross) as u64;
    let costs = CostBreakdown {
        base_fee: 5_000,
        priority_fee: 350,
        jito_tip: 1_126,
        expected_slippage: 30_000,
        safety_buffer: 105_000,
        compute_units_used: Some(107_719),
        compute_units_limit: 129_263,
        compute_unit_price_micro: 2_719,
        signatures: 1,
        tx_count: 1,
        swap_fees_embedded: true,
        ..Default::default()
    };
    let net = gross - costs.total() as i64;
    Opportunity {
        id: OpportunityId(id),
        key: "xd".into(),
        strategy: if id.is_multiple_of(3) { StrategyKind::RoundTrip } else { StrategyKind::CrossDex },
        label: if id.is_multiple_of(3) { "SOL→USDC→SOL".into() } else { "Raydium CLMM → Whirlpool".into() },
        detected_at: Ts(t),
        slot: Some(448_011_563),
        base_mint: searcher_core::address::well_known::addr(searcher_core::address::well_known::WSOL_MINT),
        input,
        gross_output: out,
        route: Route { legs: vec![leg(0, "Raydium CLMM", input, 105_598_041), leg(1, "Whirlpool", 105_598_041, out)] },
        costs,
        eval: ProfitEval {
            gross_pnl: gross,
            expected_net: net,
            gross_edge: Ppm::ratio(gross as i128, input as i128).unwrap(),
            net_edge: Ppm::ratio(net as i128, input as i128).unwrap(),
            expected_net_usd: UsdPrice::new(105_360_000).value(net as i128, 9),
            gross_usd: None,
            simulated_net: Some(net - 1_000),
        },
        status,
        updated_at: Ts(t),
        sol_price: Some(UsdPrice::new(105_360_000)),
        simulation: Some(SimulationResult {
            opportunity: OpportunityId(id),
            plan: PlanKind::SingleTx,
            fidelity: SimFidelity::Exact,
            ok: true,
            failure: None,
            txs: vec![],
            latency_ms: 362,
            simulated_at: Ts(t + 500_000),
            context_slot: Some(448_011_563),
        }),
        risk: None,
        guard: None,
    }
}

pub fn populated() -> ViewModel {
    let mut vm = ViewModel::new(true);
    vm.apply(&Event::Session(SessionInfo {
        session_id: "20260918-061200-ab12".into(),
        started_at: Ts(T0),
        mode: Mode::Paper,
        version: "0.1.0".into(),
        config_summary: "mode=PAPER".into(),
        taker: Some("sim taker F7p3…gmNe (shadow)".into()),
        paper_equity_lamports: Some(1_000_000_000),
        limits: vec![("max_trade_size".into(), "1.0 SOL".into()), ("max_daily_loss".into(), "$5.00".into())],
    }));
    for i in 0..600i64 {
        let t = T0 + i * 1_000_000;
        vm.apply(&Event::Metric { ts: Ts(t), metric: MetricId::Price, value: 105.3 + (i as f64 / 40.0).sin() * 0.2 });
        if i % 3 == 0 {
            vm.apply(&Event::Metric {
                ts: Ts(t),
                metric: MetricId::NetEdge,
                value: -8.0 + (i as f64 / 9.0).cos() * 3.0,
            });
            vm.apply(&Event::Metric {
                ts: Ts(t),
                metric: MetricId::JupiterLatency,
                value: 700.0 + (i % 17) as f64 * 40.0,
            });
            vm.apply(&Event::Metric { ts: Ts(t), metric: MetricId::Spread, value: 2.4 + (i % 5) as f64 * 0.3 });
        }
        if i % 10 == 0 {
            let id = (i / 10) as u64 + 1;
            let gross = if i % 50 == 0 { 45_000 } else { -250_000 + (i * 97 % 90_000) };
            let status =
                if i % 50 == 0 { OppStatus::PaperFilled } else { OppStatus::Skipped(SkipReason::EdgeTooSmall) };
            vm.apply(&Event::Opportunity(Box::new(opp(id, t, gross, status.clone()))));
            vm.apply(&Event::Stage(StageEvent {
                ts: Ts(t),
                opportunity: OpportunityId(id),
                stage: Stage::Opportunity,
                ok: true,
                subject: "Raydium CLMM → Whirlpool".into(),
                value: "-.08%".into(),
                detail: String::new(),
            }));
            vm.apply(&Event::Stage(StageEvent {
                ts: Ts(t),
                opportunity: OpportunityId(id),
                stage: Stage::Build,
                ok: true,
                subject: "Jupiter".into(),
                value: "812ms".into(),
                detail: String::new(),
            }));
            if status == OppStatus::PaperFilled {
                vm.apply(&Event::Execution(ExecutionAttempt {
                    opportunity: OpportunityId(id),
                    mode: Mode::Paper,
                    plan: PlanKind::SingleTx,
                    state: ExecState::PaperFilled,
                    bundle_id: None,
                    signatures: vec![],
                    tip_lamports: 1_126,
                    created_at: Ts(t),
                    updated_at: Ts(t + 600_000),
                    latency_ms: None,
                }));
                vm.apply(&Event::Trade(TradeResult {
                    opportunity: OpportunityId(id),
                    strategy: StrategyKind::CrossDex,
                    label: "Raydium CLMM → Whirlpool".into(),
                    mode: Mode::Paper,
                    paper: true,
                    entry_ts: Ts(t),
                    exit_ts: Ts(t + 600_000),
                    input: 1_000_000_000,
                    output: 1_000_045_000,
                    fees_lamports: 5_350,
                    tip_lamports: 1_126,
                    expected_net: 3_524,
                    net: 2_524,
                    net_usd: Some(UsdMicros(266)),
                }));
                vm.apply(&Event::Stage(StageEvent {
                    ts: Ts(t + 600_000),
                    opportunity: OpportunityId(id),
                    stage: Stage::Paper,
                    ok: true,
                    subject: "simulated fill".into(),
                    value: "+$0.00".into(),
                    detail: String::new(),
                }));
            } else {
                vm.apply(&Event::Stage(StageEvent {
                    ts: Ts(t),
                    opportunity: OpportunityId(id),
                    stage: Stage::Skip,
                    ok: false,
                    subject: "EDGE_TOO_SMALL".into(),
                    value: "-.08%".into(),
                    detail: String::new(),
                }));
            }
        }
    }
    vm.apply(&Event::Health {
        ts: Ts(T0),
        service: ServiceId::Jupiter,
        snapshot: ServiceSnapshot {
            service: Some(ServiceId::Jupiter),
            state: ServiceState::RateLimited,
            last_latency_ms: Some(812),
            p50_latency_ms: Some(790),
            requests: 1200,
            errors: 3,
            rate_limited: 2,
            error_rate: Ppm(2500),
            backoff_until: Some(Ts(T0 + 700_000_000)),
            last_success: Some(Ts(T0)),
            last_error: Some("429 rate limited".into()),
            quota_remaining: Some(0),
            recent_latency: vec![700, 800, 750, 900, 820],
        },
    });
    vm.apply(&Event::KillSwitch { ts: Ts(T0 + 400_000_000), engaged: true, reason: "operator (K)".into() });
    vm
}

fn app(ascii: bool) -> App {
    let mut a = App::new(&TuiOptions::default());
    a.theme = Theme::with_depth(Depth::TrueColor);
    a.glyphs = if ascii { Glyphs::ascii() } else { Glyphs::unicode() };
    a
}

const SIZES: [(u16, u16); 6] = [(80, 24), (100, 30), (120, 40), (160, 50), (40, 12), (30, 8)];

#[test]
fn every_page_renders_at_every_size() {
    let vm = populated();
    for ascii in [false, true] {
        let mut a = app(ascii);
        for p in '1'..='8' {
            a.on_key(KeyEvent::new(KeyCode::Char(p), KeyModifiers::NONE), &vm);
            for (w, h) in SIZES {
                let buf = snapshot(&mut a, &vm, w, h);
                let out = buffer_text(&buf);
                if w < 40 || h < 12 {
                    assert!(out.contains("terminal too small"), "{w}x{h}");
                    continue;
                }
                let name = if ascii { "MOBIUS-Searcher" } else { "MØBIUS-Searcher" };
                assert!(out.contains(name), "page {p} {w}x{h}\n{out}");
                assert!(out.contains("PAPER"), "page {p} {w}x{h}");
                if ascii {
                    assert!(out.is_ascii(), "non-ascii in ascii mode page {p} {w}x{h}:\n{out}");
                }
            }
        }
    }
}

#[test]
fn overview_shows_the_four_regions_and_kill_state() {
    let vm = populated();
    let mut a = app(false);
    for (w, h) in [(120, 40), (160, 50)] {
        let out = buffer_text(&snapshot(&mut a, &vm, w, h));
        for s in [
            "OPPORTUNITIES",
            "GRAPH WORKSPACE",
            "INSPECTOR",
            "EVENT STREAM",
            "KILL SWITCH",
            "Equity",
            "Session",
            "Simulated",
        ] {
            assert!(out.contains(s), "{s} missing at {w}x{h}\n{out}");
        }
        assert!(out.contains("EDGE_TOO_SMALL") || out.contains("PAPER FILL"), "skip reasons visible");
        assert!(out.contains("Route"), "inspector route tree");
    }
    let small = buffer_text(&snapshot(&mut a, &vm, 80, 24));
    assert!(small.contains("OPPORTUNITIES") && small.contains("GRAPH WORKSPACE"), "{small}");
}

#[test]
fn ab_inspector_shows_deltas() {
    let vm = populated();
    let mut a = app(false);
    a.on_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE), &vm);
    a.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &vm);
    for _ in 0..30 {
        a.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &vm);
    }
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &vm);
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("Δ time"), "{out}");
    assert!(out.contains("Δ SOL price"), "{out}");
    assert!(out.contains("Between A and B"), "{out}");
}

#[test]
fn system_page_shows_429_state() {
    let vm = populated();
    let mut a = app(false);
    a.on_key(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE), &vm);
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("Jupiter API"), "{out}");
    assert!(out.contains("429"), "{out}");
}

#[test]
fn resize_sequence_is_stable() {
    let vm = populated();
    let mut a = app(false);
    let mut w = 40u16;
    let mut h = 12u16;
    for _ in 0..40 {
        let _ = snapshot(&mut a, &vm, w, h);
        w = if w >= 200 { 41 } else { w + 7 };
        h = if h >= 60 { 13 } else { h + 3 };
    }
}

#[test]
fn incomplete_routes_never_show_an_edge() {
    // Leg 1 (SOL→USDC) quoted, leg 2 failed: the "output" is USDC, not SOL.
    let mut vm = ViewModel::new(true);
    let mut o = opp(1, T0, 0, OppStatus::Skipped(SkipReason::BuildFailed));
    o.route.legs.truncate(1);
    o.gross_output = 105_598_041; // what the old code priced: USDC atoms vs SOL input
    o.eval.gross_edge = Ppm(-894_101);
    o.eval.gross_pnl = -894_401_959;
    vm.apply(&Event::Opportunity(Box::new(o)));
    let mut a = app(false);
    a.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE), &vm);
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(!out.contains("89."), "no fake edge for an incomplete route:\n{out}");
    assert!(out.contains("BUILD_FAILED"));
    assert!(out.contains("Route incomplete"), "{out}");
}

#[test]
fn startup_empty_state_shows_the_brand_not_empty_axes() {
    let vm = ViewModel::new(false);
    let mut a = app(false);
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("MØBIUS-Searcher"), "{out}");
    assert!(out.contains("waiting for the first quotes"), "{out}");
    assert!(!out.contains("no samples in window"), "{out}");
    let mut a = app(true);
    let out = buffer_text(&snapshot(&mut a, &vm, 80, 24));
    assert!(out.contains("MOBIUS-Searcher") && out.is_ascii(), "{out}");
}

#[test]
fn help_overlay_fits_without_wrapping() {
    let vm = ViewModel::new(false);
    for ascii in [true, false] {
        for (w, h) in [(80, 24), (120, 40)] {
            let mut a = app(ascii);
            a.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &vm);
            let out = buffer_text(&snapshot(&mut a, &vm, w, h));
            let name = if ascii { "MOBIUS-Searcher . keys" } else { "MØBIUS-Searcher · keys" };
            assert!(out.contains(name), "{w}x{h} ascii={ascii}: {out}");
            // first and last help lines intact: nothing wrapped, nothing clipped
            assert!(out.contains("Opportunities Graphs Trades Risk System Logs"), "{w}x{h} ascii={ascii}: {out}");
            assert!(out.contains("quit (graceful; recording is flushed)"), "{w}x{h} ascii={ascii}: {out}");
            if ascii {
                assert!(out.contains("^/v") && !out.contains("?/?"), "arrows fold to ^/v: {out}");
            }
        }
    }
}

// ───────────────────────────── mouse ─────────────────────────────

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use searcher_tui::app::{Focus, Hit, Page};

fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
    MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
}

/// Render, then return a cell inside the first region matching `pred`.
fn spot(a: &mut App, vm: &ViewModel, pred: impl Fn(&Hit) -> bool) -> (u16, u16) {
    snapshot(a, vm, 120, 40);
    let hits = a.hits.borrow();
    let (r, _) = hits.iter().find(|(_, h)| pred(h)).expect("region registered");
    (r.x + r.width / 2, r.y)
}

#[test]
fn clicking_tabs_rows_and_the_inspector_follows_the_keyboard_model() {
    let vm = populated();
    let mut a = app(false);
    let at = spot(&mut a, &vm, |h| *h == Hit::Page(Page::Graphs));
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm);
    assert_eq!(a.page, Page::Graphs);
    a.on_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE), &vm);
    // third opportunity row
    let rows: Vec<_> = {
        snapshot(&mut a, &vm, 120, 40);
        a.hits.borrow().iter().filter_map(|(r, h)| matches!(h, Hit::Opp(_)).then_some((*r, *h))).collect()
    };
    let (r, Hit::Opp(id)) = rows[2] else { unreachable!() };
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (r.x + 3, r.y)), &vm);
    assert_eq!(a.opp_selected, Some(id));
    assert_eq!(a.focus, Focus::Opportunities);
    assert!(a.timeline.cursor.is_some(), "selection moves the shared cursor");
    // clicking the selected row again opens it in the inspector
    snapshot(&mut a, &vm, 120, 40);
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (r.x + 3, r.y)), &vm);
    assert_eq!(a.focus, Focus::Inspector);
}

#[test]
fn graphs_take_clicks_drags_right_click_marks_and_wheel_zoom() {
    let vm = populated();
    let mut a = app(false);
    let (_, y) = spot(&mut a, &vm, |h| matches!(h, Hit::Graph { index: Some(0), .. }));
    let plot = a
        .hits
        .borrow()
        .iter()
        .find_map(|(_, h)| match h {
            Hit::Graph { index: Some(0), plot, .. } => Some(*plot),
            _ => None,
        })
        .unwrap();
    let y = y + 2;
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (plot.x + 2, y)), &vm);
    assert_eq!(a.focus, Focus::Graphs);
    assert!(!a.timeline.follow, "a click freezes the window at the cursor");
    let early = a.timeline.cursor.expect("cursor on a sample");
    a.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), (plot.right() - 2, y)), &vm);
    let late = a.timeline.cursor.unwrap();
    assert!(late > early, "dragging right scrubs forward");
    a.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), (plot.right() - 2, y)), &vm);
    // right-click: A, then B
    snapshot(&mut a, &vm, 120, 40);
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Right), (plot.x + 2, y)), &vm);
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Right), (plot.right() - 2, y)), &vm);
    let (ta, tb) = (a.timeline.a.unwrap(), a.timeline.b.unwrap());
    assert!(ta < tb, "A then B");
    // wheel over a graph zooms the shared timeframe
    let tf = a.timeline.tf;
    a.on_mouse(mouse(MouseEventKind::ScrollDown, (plot.x + 5, y)), &vm);
    assert_ne!(a.timeline.tf, tf);
}

#[test]
fn stream_lines_open_details_and_overlays_close_on_outside_clicks() {
    let vm = populated();
    let mut a = app(false);
    let at = spot(&mut a, &vm, |h| matches!(h, Hit::Stream(_)));
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm);
    assert!(a.detail.is_some(), "a stream line opens its detail");
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("opportunity"), "{out}");
    // inside the overlay: stays open; outside: closes
    let inside =
        a.hits.borrow().iter().rev().find(|(_, h)| *h == Hit::Overlay).map(|(r, _)| (r.x + 5, r.y + 3)).unwrap();
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), inside), &vm);
    assert!(a.detail.is_some());
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (0, 39)), &vm);
    assert!(a.detail.is_none());
    // wheel up over the stream scrolls back in time
    let at = spot(&mut a, &vm, |h| matches!(h, Hit::Stream(_)));
    a.on_mouse(mouse(MouseEventKind::ScrollUp, at), &vm);
    assert_eq!(a.stream_offset, 3);
}

#[test]
fn picker_items_toggle_graphs_by_click() {
    let vm = populated();
    let mut a = app(false);
    a.on_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE), &vm);
    let at = spot(&mut a, &vm, |h| *h == Hit::PickerItem(4)); // PnL
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm);
    assert!(a.workspace.contains(MetricId::Pnl));
}

#[test]
fn kill_switch_click_engages_but_release_stays_on_the_keyboard() {
    let mut vm = populated();
    // the fixture ends with the switch engaged: start from released
    vm.apply(&Event::KillSwitch { ts: Ts(T0 + 500_000_000), engaged: false, reason: "released".into() });
    let mut a = app(false);
    a.on_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE), &vm); // Risk: always-visible row
    let at = spot(&mut a, &vm, |h| *h == Hit::Kill);
    let cmds = a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm);
    assert!(matches!(cmds.as_slice(), [searcher_core::event::Command::KillSwitch { engage: true, .. }]));
    vm.apply(&Event::KillSwitch { ts: Ts(T0), engaged: true, reason: "click".into() });
    let at = spot(&mut a, &vm, |h| *h == Hit::Kill);
    assert!(a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm).is_empty());
    assert!(a.kill_release_prompt, "second click asks for confirmation");
    // any click cancels the prompt; only the y key releases
    assert!(a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (60, 20)), &vm).is_empty());
    assert!(!a.kill_release_prompt);
}
