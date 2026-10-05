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
use searcher_tui::bots::{BotAction, BotPort, BotState, BotView, Bots, BotsView, Calc, Levels};
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
        // the Bots page with bots on it, as every other page has its content
        a.bots = Some(Bots::fixed(bots_view()));
        for p in '1'..='9' {
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
                // the way to the key list is on screen at every size
                let footer = out.lines().last().unwrap_or_default();
                assert!(footer.ends_with("? help") || footer.ends_with("q quit"), "page {p} {w}x{h}: {footer}");
                assert!(footer.contains("? help"), "page {p} {w}x{h}: {footer}");
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

#[test]
fn threshold_panel_renders_and_warns_before_allowing_losses() {
    use searcher_tui::thresholds::{Mode, Panel};
    let mut vm = ViewModel::new(false);
    vm.thresholds = searcher_core::thresholds::values(&searcher_core::config::Config::default());
    let mut a = app(false);
    let mut p = Panel::default();
    p.staged.insert("profit.protect_min_out".into(), "false".into());
    a.thresholds = Some(p.clone());
    for (w, h) in [(120, 40), (80, 24)] {
        let out = buffer_text(&snapshot(&mut a, &vm, w, h));
        assert!(out.contains("Thresholds"), "{w}x{h}:\n{out}");
        assert!(out.contains("true  →  false"), "staged change shown at {w}x{h}:\n{out}");
    }
    p.mode = Mode::Review;
    a.thresholds = Some(p);
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("LOSE money"), "{out}");
    // once allowed, the header says so on every page
    vm.loss_possible = true;
    a.thresholds = None;
    assert!(buffer_text(&snapshot(&mut a, &vm, 120, 40)).contains("LOSS ALLOWED"));
}

// ───────────────────────────── keys act on what is on the page ─────────────────────────────

fn press(a: &mut App, vm: &ViewModel, c: KeyCode) {
    a.on_key(KeyEvent::new(c, KeyModifiers::NONE), vm);
}

#[test]
fn keys_do_not_move_lists_that_are_not_on_the_page() {
    let vm = populated();
    let mut a = app(false);
    // Risk and System show neither the stream nor the opportunity list
    for page in ['6', '7'] {
        press(&mut a, &vm, KeyCode::Char(page));
        for k in [KeyCode::Char('j'), KeyCode::Char('k'), KeyCode::PageUp, KeyCode::Enter, KeyCode::Char('f')] {
            press(&mut a, &vm, k);
        }
        assert_eq!((a.stream_offset, a.log_offset), (0, 0), "page {page}");
        assert!(a.detail.is_none(), "page {page}: Enter opened a line of a stream that is not shown");
    }
    // Trades: the opportunity selection and its filter stay as they were
    press(&mut a, &vm, KeyCode::Char('5'));
    for k in [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('f'), KeyCode::PageDown] {
        press(&mut a, &vm, k);
    }
    assert_eq!(a.opp_selected, None);
    assert_eq!(a.opp_filter, searcher_tui::app::OppFilter::All);
    // where the list is shown the same keys work
    press(&mut a, &vm, KeyCode::Char('3'));
    press(&mut a, &vm, KeyCode::Char('j'));
    press(&mut a, &vm, KeyCode::Char('f'));
    assert_eq!(a.opp_filter, searcher_tui::app::OppFilter::GrossPositive);
    press(&mut a, &vm, KeyCode::Char('8'));
    press(&mut a, &vm, KeyCode::Char('k'));
    assert_eq!(a.log_offset, 1);
}

#[test]
fn footer_keys_belong_to_the_page() {
    let vm = populated();
    let mut a = app(false);
    let footer = |a: &mut App| buffer_text(&snapshot(a, &vm, 160, 50)).lines().last().unwrap_or_default().to_string();
    assert!(footer(&mut a).contains("f filter"));
    for page in ['5', '6', '7'] {
        press(&mut a, &vm, KeyCode::Char(page));
        let f = footer(&mut a);
        assert!(!f.contains("j/k") && !f.contains("f filter") && !f.contains("detail"), "page {page}: {f}");
    }
    // narrow: the page is named, the others keep their digit, the help stays
    press(&mut a, &vm, KeyCode::Char('3'));
    let f = buffer_text(&snapshot(&mut a, &vm, 80, 24)).lines().last().unwrap_or_default().to_string();
    assert!(f.contains("3 Opportunities") && f.contains(" 2  3") && f.ends_with("? help"), "{f}");
}

#[test]
fn a_long_detail_scrolls_and_a_short_one_takes_only_its_lines() {
    let mut vm = populated();
    vm.apply(&Event::Stage(StageEvent {
        ts: Ts(T0 + 700_000_000),
        opportunity: OpportunityId(1),
        stage: Stage::Simulation,
        ok: false,
        subject: "fail".into(),
        value: "200ms".into(),
        detail: (1..=60).map(|i| format!("log line {i}")).collect::<Vec<_>>().join("\n"),
    }));
    let mut a = app(false);
    press(&mut a, &vm, KeyCode::Tab);
    press(&mut a, &vm, KeyCode::Tab);
    press(&mut a, &vm, KeyCode::Tab);
    press(&mut a, &vm, KeyCode::Enter);
    assert!(a.detail.is_some());
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("log line 1") && !out.contains("log line 60"), "{out}");
    assert!(out.contains("↑↓ scroll · Esc close"), "{out}");
    press(&mut a, &vm, KeyCode::PageDown);
    for _ in 0..200 {
        press(&mut a, &vm, KeyCode::Char('j'));
    }
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("log line 60"), "the end is reachable and the scroll stops there:\n{out}");
    assert!((a.detail_scroll as usize) < 60, "clamped to the text: {}", a.detail_scroll);
    press(&mut a, &vm, KeyCode::Esc);
    assert!(a.detail.is_none());
    // a short detail: a small box, the page stays visible around it
    press(&mut a, &vm, KeyCode::Char('k'));
    press(&mut a, &vm, KeyCode::Char('k'));
    press(&mut a, &vm, KeyCode::Enter);
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("Esc close") && !out.contains("↑↓ scroll"), "{out}");
    assert!(out.contains("GRAPH WORKSPACE"), "the page above the box is still shown:\n{out}");
}

#[test]
fn routes_and_statuses_fit_the_opportunities_page_at_120_columns() {
    let mut vm = populated();
    let mut o = opp(900, T0 + 650_000_000, -40_000, OppStatus::Skipped(SkipReason::EdgeTooSmall));
    o.label = "Raydium CLMM → Meteora DLMM".into();
    vm.apply(&Event::Opportunity(Box::new(o)));
    let mut a = app(false);
    press(&mut a, &vm, KeyCode::Char('3'));
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    let row = out.lines().find(|l| l.contains("Raydium CLMM → Meteora DLMM")).expect("the longest route, whole");
    assert!(row.contains("EDGE_TOO_SMALL"), "{row}");
    // narrower, the route gives way with an ellipsis instead of a cut word
    let out = buffer_text(&snapshot(&mut a, &vm, 100, 30));
    assert!(out.contains("Raydium CLMM → Meteora DLMM"), "one column at 100: room for the whole route\n{out}");
    press(&mut a, &vm, KeyCode::Char('1'));
    let out = buffer_text(&snapshot(&mut a, &vm, 100, 30));
    assert!(!out.contains("Raydium CLMM → Meteora DLM "), "{out}");
}

// ───────────────────────────── the Bots page ─────────────────────────────

/// A real bot that waits to buy, as the first real run stood, and a paper one in SOL.
fn bots_view() -> BotsView {
    let bar = 900_000i64;
    let t0 = 1_791_180_000_000i64;
    // a day of closes that sink, bounce and sit over the average
    let closes: Vec<(i64, f64)> = (0..96)
        .map(|i| {
            let x = i as f64;
            (t0 + i * bar, 121.3 + (x / 9.0).sin() * 0.55 - if (60..70).contains(&i) { 0.5 } else { 0.0 })
        })
        .collect();
    let real = BotView {
        id: "trade-417971eff3174c0b".into(),
        name: "dip-1d".into(),
        real: true,
        inst: "SOL-USDT".into(),
        bar: "15m".into(),
        bar_ms: bar,
        rule: "Buys when a bar closes 1 deviation under its average of 96 bars (1 d); sells back at the average, or 5 % under the buy.".into(),
        state: BotState::Running,
        funded: true,
        pending: false,
        budget: 2.0,
        stop_at: Some(1.0),
        cash: 2.0,
        sol: 0.0,
        paid: 0.0,
        worth: Some(2.0),
        trades: vec![(t0 + 70 * bar, 2.0, -0.0035)],
        levels: Levels { buy: Some(120.93), sell: Some(121.33), stop: None },
        calc: Some(Calc { window: 96, mean: 121.33, sd: 0.40, k: 1.0, exit_z: 0.0, stop: Some(0.05) }),
        closes: closes.clone(),
        fills: vec![(t0 + 62 * bar - 1, true), (t0 + 70 * bar - 1, false)],
        journal: vec![
            (t0, "the wallet holds 2.0051 USDC: 2.0000 of it is the rule's budget".into()),
            (t0 + 62 * bar, "bought 0.016464 SOL for 2.0000 USDC (121.48 a SOL, every cost inside)".into()),
            (t0 + 70 * bar, "sold 0.016464 SOL for 1.9965 USDC; this trade -0.0035 USD".into()),
        ],
        file: Some("/Users/me/.config/mobius/trade-fast.toml".into()),
        wish: None,
        opened: None,
        equity: Vec::new(),
    };
    let paper = BotView {
        id: "803bcf65/reversal".into(),
        name: "reversal".into(),
        real: false,
        rule: "Buys after a bar that closed down; sells after one that did not.".into(),
        state: BotState::Paper,
        budget: 23.0,
        stop_at: None,
        cash: 0.0,
        sol: 0.19,
        paid: 23.0,
        worth: Some(23.05),
        trades: Vec::new(),
        levels: Levels::default(),
        calc: None,
        fills: Vec::new(),
        journal: Vec::new(),
        file: None,
        ..real.clone()
    };
    BotsView { bots: vec![real, paper], error: None }
}

fn bots_app(view: BotsView) -> App {
    let mut a = app(false);
    a.bots = Some(Bots::fixed(view));
    a
}

#[test]
fn the_bots_page_says_what_a_bot_holds_and_what_it_waits_for() {
    let vm = populated();
    let mut a = bots_app(bots_view());
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    if std::env::var_os("SHOW").is_some() {
        println!("{out}");
    }
    for needle in [
        "DIP-1D · REAL MONEY · SOL-USDT 15m",
        "Waiting to buy: when a 15m bar closes under 120.93",
        // the ruler: the two prices and where the price stands between them
        "buys under 120.93",
        "sells above 121.33",
        "now 120.80  under its buy price",
        // beside the chart on a screen this wide: the account, what it waits for, what it did so far, what is next
        "ACCOUNT",
        "2.0000 USD   +0.00 %",
        "all sold at 1.00 USD or less",
        "WAITING TO BUY",
        "120.80   under it already",
        "SO FAR",
        "1, 0 of them won",
        "-0.0035 USD",
        "NEXT",
        "Decides at",
        // and its trades as a table, beside its record
        "ITS TRADES",
        "0.016464    2.0000     121.48",
        "0.016464    1.9965     121.26     -0.0035 USD",
        // the arithmetic behind the prices
        "HOW ITS PRICES ARE WORKED OUT",
        "From the closes of the last 96 15m bars (1 d): their average 121.33, their deviation 0.40",
        "buy price = average − 1 × deviation = 120.93",
        "sold 0.016464 SOL for 1.9965 USDC",
        "x stop",
        // a paper experiment is not the operator's money: there, and out of sight until asked for
        "p paper runs (1)",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    assert!(!out.contains("reversal"), "{out}");
    // its buy and its sale are marked on the price
    assert!(out.contains('▲') && out.contains('▼'), "{out}");
    // the header counts the wallet's USDC and names the bot that runs
    let kpi = out.lines().nth(1).unwrap_or_default().to_string();
    assert!(kpi.contains("Bot dip-1d · waiting, buys under 120.93"), "{kpi}");
    // the footer's keys are this page's
    let footer = out.lines().last().unwrap_or_default().to_string();
    assert!(footer.contains("9 Bots") && footer.contains("s start") && footer.contains("c close & sell"), "{footer}");
    assert!(footer.contains("b budget") && footer.contains("n new") && footer.contains("[ ] bar"), "{footer}");
    // j does not leave the real ones while the paper ones are hidden
    press(&mut a, &vm, KeyCode::Char('j'));
    assert_eq!(a.bot_selected, 0);
    // the paper one: shown with p and selected with j, it says so and offers nothing
    press(&mut a, &vm, KeyCode::Char('p'));
    press(&mut a, &vm, KeyCode::Char('j'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("p hide paper runs"), "{out}");
    assert!(out.contains("REVERSAL · PAPER") && out.contains("Holds 0.190000 SOL bought for 23.0000 USD."), "{out}");
    // a rule that names no prices has no ruler; its sentence stands where the arithmetic would
    assert!(!out.contains("sells above") && out.contains("Buys after a bar that closed down"), "{out}");
    // narrow: the list above, the detail under it, nothing lost that matters
    press(&mut a, &vm, KeyCode::Char('k'));
    let out = buffer_text(&snapshot(&mut a, &vm, 100, 30));
    assert!(out.contains("DIP-1D · REAL MONEY") && out.contains("Waiting to buy"), "{out}");
}

#[test]
fn the_bots_page_reads_in_chinese_for_an_operator_who_does() {
    let vm = populated();
    let mut a = bots_app(bots_view());
    a.zh = true;
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    if std::env::var_os("SHOW").is_some() {
        println!("{out}");
    }
    for needle in [
        "DIP-1D · 真钱 · SOL-USDT 15m",
        "等待买入：上一根收盘价 120.80 已经低于买入线 120.93",
        "买入线 120.93",
        "卖出线 121.33",
        "已在买入线下方",
        "市值",
        "账户",
        "市值跌到 1.00 美元就全部卖出",
        "等待买入",
        "战绩",
        "1 笔，其中赚 0 笔",
        "接下来",
        "下一次判断",
        "成交记录",
        "这些线是怎么算出来的",
        "取最近 96 根15 分钟线（约 1 天）的收盘价：平均价 121.33，波动幅度（标准差）0.40",
        "买入线 = 平均价 − 1 × 波动 = 120.93",
        "卖出 0.016464 SOL，得到 1.9965 USDC；这一笔 -0.0035 美元",
        "x 停止",
        "运行中",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    let footer = out.lines().last().unwrap_or_default().to_string();
    assert!(footer.contains("s 启动") && footer.contains("c 卖出并结束"), "{footer}");
    // the question before real money, in Chinese too
    let mut view = bots_view();
    view.bots[0].state = BotState::Stopped;
    let mut a = bots_app(view);
    a.zh = true;
    press(&mut a, &vm, KeyCode::Char('9'));
    press(&mut a, &vm, KeyCode::Char('s'));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(
        out.contains("用真钱启动 dip-1d 吗？") && out.contains("可能亏钱") && out.contains("y 启动 · 其他键取消"),
        "{out}"
    );
}

#[test]
fn a_bot_is_started_stopped_and_closed_only_after_a_y() {
    use std::sync::Mutex;
    let vm = populated();
    let asked: std::sync::Arc<Mutex<Vec<(String, BotAction)>>> = Default::default();
    let log = asked.clone();
    let mut view = bots_view();
    let port = BotPort {
        view: std::sync::Arc::new({
            let v = view.clone();
            move || v.clone()
        }),
        act: std::sync::Arc::new(move |id, action| {
            log.lock().unwrap().push((id.to_string(), action));
            Ok(format!("{} done", action.verb(false)))
        }),
        create: std::sync::Arc::new(|_| Err("not here".into())),
    };
    let mut a = App::new(&TuiOptions { bots: Some(port.clone()), ..TuiOptions::default() });
    a.theme = Theme::with_depth(Depth::TrueColor);
    a.glyphs = Glyphs::unicode();
    a.bots = Some(Bots::start(port));
    press(&mut a, &vm, KeyCode::Char('9'));
    // running: it can be stopped, not started or closed
    press(&mut a, &vm, KeyCode::Char('s'));
    assert!(a.bot_prompt.is_none() && a.status_text().is_some_and(|s| s.contains("running already")));
    press(&mut a, &vm, KeyCode::Char('c'));
    assert!(a.bot_prompt.is_none() && a.status_text().is_some_and(|s| s.contains("stop it first")));
    press(&mut a, &vm, KeyCode::Char('x'));
    assert_eq!(a.bot_prompt, Some(("trade-417971eff3174c0b".to_string(), BotAction::Stop)));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("Stop dip-1d?") && out.contains("y stop · any other key cancels"), "{out}");
    assert!(out.contains("nothing is sold"), "{out}");
    // any other key: nothing happens
    press(&mut a, &vm, KeyCode::Char('n'));
    assert!(a.bot_prompt.is_none() && asked.lock().unwrap().is_empty());
    press(&mut a, &vm, KeyCode::Char('x'));
    press(&mut a, &vm, KeyCode::Char('y'));
    assert_eq!(*asked.lock().unwrap(), vec![("trade-417971eff3174c0b".to_string(), BotAction::Stop)]);
    assert_eq!(a.status_text(), Some("stop done"));

    // stopped: the question before real money names the budget and says that it can lose
    view.bots[0].state = BotState::Stopped;
    let mut a = bots_app(view);
    press(&mut a, &vm, KeyCode::Char('9'));
    press(&mut a, &vm, KeyCode::Char('s'));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("Start dip-1d with real money?") && out.contains("Budget 2.00 USD"), "{out}");
    assert!(out.contains("can lose money") && out.contains("y start · any other key cancels"), "{out}");
    // a click anywhere is not a yes
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), (80, 25)), &vm);
    assert!(a.bot_prompt.is_none());
    // the paper bot takes no action from here
    press(&mut a, &vm, KeyCode::Char('p'));
    press(&mut a, &vm, KeyCode::Char('j'));
    press(&mut a, &vm, KeyCode::Char('s'));
    assert!(a.bot_prompt.is_none() && a.status_text().is_some_and(|s| s.contains("--lab")));
}

#[test]
fn without_bots_the_page_says_how_to_begin_and_the_markets_tabs_count_them() {
    let vm = populated();
    let mut a = bots_app(BotsView::default());
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("No bot has run yet.") && out.contains("n makes one here"), "{out}");
    let mut a = bots_app(bots_view());
    press(&mut a, &vm, KeyCode::Char('2'));
    a.mk_tab = searcher_tui::markets::BottomTab::Bots;
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(
        out.contains("Bots (4)") && out.contains("dip-1d · real money") && !out.contains("reversal · paper"),
        "{out}"
    );
    // the paper ones are counted and listed once they are asked for
    a.bot_paper = true;
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("Bots (5)") && out.contains("reversal · paper"), "{out}");
    a.wallet = Some(Wallet::fixed(wallet_view()));
    a.mk_tab = searcher_tui::markets::BottomTab::Assets;
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("USDC") && out.contains("2.0051"), "the wallet's USDC is among its assets:\n{out}");
}

// ───────────────────────────── the Wallet page ─────────────────────────────

use searcher_tui::wallet::{Asset, Moved, Review, SendForm, SendRequest, Sending, Wallet, WalletView};

const MINE: &str = "Examp1eWa11etAddressForTestsNotARea1Wa11et11";
const THEIRS: &str = "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU";

/// The wallet as it stood while the first real bot waited: its budget in USDC, the rest in SOL.
fn wallet_view() -> WalletView {
    WalletView {
        address: Some(MINE.into()),
        sol: Some(0.171412),
        usdc: Some(2.005052),
        read_at: Some(Ts::now().millis() - 3_000),
        // not a real code: enough modules to see that one is drawn
        qr: (0..37).map(|y| (0..37).map(|x| (x + y) % 3 == 0).collect()).collect(),
        moved: vec![
            Moved {
                at: 1_791_190_000_000,
                sol: 0.016535,
                usdc: -2.0,
                fee: 0.000007,
                failed: false,
                signature: "3Examp1eSignatureForTestsNotARea1TransactionAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAxyQ1"
                    .into(),
            },
            Moved {
                at: 1_791_180_000_000,
                sol: 0.2,
                usdc: 0.0,
                fee: 0.0,
                failed: false,
                signature: "4Examp1eSignatureForTestsNotARea1TransactionBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBabZ2"
                    .into(),
            },
        ],
        reserve: 0.02,
        ..Default::default()
    }
}

fn wallet_app(view: WalletView, zh: bool) -> App {
    let mut a = bots_app(bots_view());
    a.zh = zh;
    a.wallet = Some(Wallet::fixed(view));
    a
}

fn review() -> Review {
    Review {
        request: SendRequest { asset: Asset::Sol, to: THEIRS.into(), amount: "0.05".into() },
        to: THEIRS.into(),
        amount: 0.05,
        recipient: "a wallet that holds 1.200000 SOL".into(),
        fee: 0.000007,
        opens_account: None,
        left_sol: 0.121405,
        left_usdc: 2.005052,
        notes: Vec::new(),
    }
}

fn show(out: &str) {
    if std::env::var_os("SHOW").is_some() {
        println!("{out}");
    }
}

#[test]
fn the_wallet_page_says_what_it_holds_where_it_receives_and_what_moved() {
    let vm = populated();
    let mut a = wallet_app(wallet_view(), false);
    press(&mut a, &vm, KeyCode::Char('0'));
    assert_eq!(a.page, Page::Wallet);
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "WALLET",
        "Exam…et11 · read 3 s ago",
        "0.171412 SOL",
        "2.005052 USDC",
        // what the bot holds is part of it, and is not offered for sending
        "Of that, 2.0000 USDC is what the bot dip-1d holds",
        "Free to send    SOL 0.151412 (0.02 kept for fees)  ·  USDC 0.005052",
        // receiving: the address whole, in two halves under its code
        "RECEIVE",
        "Examp1eWa11etAddressFo",
        "rTestsNotARea1Wa11et11",
        "Solana network only",
        "SEND",
        "s send SOL · u send USDC",
        "IN AND OUT",
        "swap",
        "-2.0000 USDC",
        "+0.016542 SOL",
        "received",
        "+0.200000 SOL",
        "3Exa…xyQ1",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    assert!(out.contains('▀'), "the code is drawn:\n{out}");
    let footer = out.lines().last().unwrap_or_default().to_string();
    assert!(
        footer.contains("0 Wallet") && footer.contains("s send SOL") && footer.contains("c copy address"),
        "{footer}"
    );
    // narrow: no room for the code, the address is there whole all the same
    let out = buffer_text(&snapshot(&mut a, &vm, 100, 30));
    show(&out);
    assert!(out.contains(MINE) && out.contains("RECEIVE") && out.contains("IN AND OUT"), "{out}");
    // ⏎ on a transaction: all of it, its signature whole
    press(&mut a, &vm, KeyCode::Char('j'));
    press(&mut a, &vm, KeyCode::Enter);
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(
        out.contains("4Examp1eSignatureForTestsNotARea1TransactionBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBabZ2"),
        "{out}"
    );
}

#[test]
fn the_wallet_page_reads_in_chinese_for_an_operator_who_does() {
    let vm = populated();
    let mut a = wallet_app(wallet_view(), true);
    press(&mut a, &vm, KeyCode::Char('0'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "钱包",
        "3 秒前读取",
        "其中机器人 dip-1d 持有 2.0000 USDC",
        "可以转出    SOL 0.151412（已留 0.02 付手续费）  ·  USDC 0.005052",
        "收款",
        "你的地址（Solana 网络）",
        "只收 Solana 网络上的 SOL 和 USDC",
        "转出",
        "s 转出 SOL · u 转出 USDC",
        "最近进出",
        "兑换",
        "收到",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    let footer = out.lines().last().unwrap_or_default().to_string();
    assert!(footer.contains("s 转出 SOL") && footer.contains("c 复制地址"), "{footer}");
}

#[test]
fn a_transfer_is_written_checked_and_confirmed_by_the_end_of_its_address() {
    let vm = populated();
    let mut a = wallet_app(wallet_view(), false);
    press(&mut a, &vm, KeyCode::Char('0'));
    // s: the form, on the address; what is typed goes into it and nowhere else
    press(&mut a, &vm, KeyCode::Char('s'));
    for c in THEIRS.chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    press(&mut a, &vm, KeyCode::Enter);
    press(&mut a, &vm, KeyCode::Char('m'));
    assert_eq!(a.page, Page::Wallet, "keys typed into the form do not act on the page under it");
    // nor does a click: on another page's tab it neither leaves the page nor throws away what was written
    let _ = snapshot(&mut a, &vm, 200, 58);
    let tab = spot(&mut a, &vm, |h| *h == Hit::Page(Page::Overview));
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), tab), &vm);
    assert!(a.page == Page::Wallet && a.wallet_form.is_some());
    let form = a.wallet_form.clone().expect("the form is open");
    assert_eq!((form.to.as_str(), form.amount.as_str()), (THEIRS, "0.151412"), "m: all that is free to send");
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "Send",
        "What",
        " SOL ",
        "To",
        THEIRS,
        "Amount",
        "0.151412▏ SOL",
        "0.151412 free to send (m fills it in)",
        "⏎ check it",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }

    // the application's answer: shown back in full, and nothing is sent before the address's end is typed
    let mut a = wallet_app(WalletView { sending: Some(Sending::Reviewed(review())), ..wallet_view() }, false);
    press(&mut a, &vm, KeyCode::Char('0'));
    a.wallet_form = Some(SendForm::new(Asset::Sol));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "Check this transfer",
        "0.05 SOL",
        "7xKX tg2C W87d 97TX JSDp bD5j Bkhe TqA8 3TZR uJos gAsU",
        "a wallet that holds 1.200000 SOL",
        "0.000007 SOL",
        "0.121405 SOL · 2.0051 USDC",
        "Once sent it cannot be taken back",
        "Type the last 4 characters of the address to confirm",
        "Esc back",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    assert!(!out.contains("⏎ send it"), "{out}");
    // the bot that runs keeps its account by the wallet's balances: said before a transfer is confirmed
    assert!(out.contains("The bot dip-1d is running") && out.contains("do not send in the minute it trades"), "{out}");
    for c in "gAsX".chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("not the end of this address") && !out.contains("⏎ send it"), "{out}");
    press(&mut a, &vm, KeyCode::Backspace);
    press(&mut a, &vm, KeyCode::Char('U'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("⏎ send it"), "{out}");

    // sending what a bot holds is said before it is done
    let taken = Review { left_usdc: 0.5, request: SendRequest { asset: Asset::Usdc, ..review().request }, ..review() };
    let mut a = wallet_app(WalletView { sending: Some(Sending::Reviewed(taken)), ..wallet_view() }, true);
    a.wallet_form = Some(SendForm::new(Asset::Usdc));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    assert!(out.contains("这会动用机器人 dip-1d 持有的 USDC"), "{out}");
    assert!(out.contains("核对这笔转出") && out.contains("输入收款地址的最后 4 位来确认"), "{out}");

    // how it ended
    let signature = "4Examp1eSignatureForTestsNotARea1TransactionBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBabZ2";
    let done = Sending::Done { review: review(), signature: signature.into() };
    let mut a = wallet_app(WalletView { sending: Some(done), ..wallet_view() }, false);
    a.wallet_form = Some(SendForm::new(Asset::Sol));
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("0.05 SOL went to 7xKX…gAsU: confirmed on the chain."), "{out}");
    // its signature whole, over two lines: it is the receipt
    assert!(out.contains(&signature[..80]) && out.contains(&signature[80..]), "{out}");
    press(&mut a, &vm, KeyCode::Char(' '));
    assert!(a.wallet_form.is_none(), "any key closes it");
    let failed =
        Sending::Failed { review: review(), why: "Its simulation failed, so it was not sent".into(), signature: None };
    let mut a = wallet_app(WalletView { sending: Some(failed), ..wallet_view() }, false);
    a.wallet_form = Some(SendForm::new(Asset::Sol));
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("Not sent") && out.contains("Its simulation failed"), "{out}");
}

#[test]
fn a_wallet_that_cannot_send_says_why_and_opens_no_form() {
    let vm = populated();
    let why = "Nothing can be sent from here: your config names no key file.";
    let mut a = wallet_app(WalletView { cannot_send: Some(why.into()), ..wallet_view() }, false);
    press(&mut a, &vm, KeyCode::Char('0'));
    press(&mut a, &vm, KeyCode::Char('s'));
    assert!(a.wallet_form.is_none());
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains(why) && !out.contains("s send SOL · u send USDC"), "{out}");
    // no wallet configured: the page says what to do
    let mut a = wallet_app(WalletView::default(), false);
    press(&mut a, &vm, KeyCode::Char('0'));
    let out = buffer_text(&snapshot(&mut a, &vm, 120, 40));
    assert!(out.contains("No wallet is configured yet"), "{out}");
}

#[test]
fn the_budget_of_a_bot_is_changed_from_the_page_after_it_says_what_that_does() {
    let vm = populated();
    let mut a = wallet_app(wallet_view(), false);
    press(&mut a, &vm, KeyCode::Char('9'));
    press(&mut a, &vm, KeyCode::Char('b'));
    let form = a.bot_budget.clone().expect("b: the form, for the bot that is selected");
    assert_eq!((form.id.as_str(), form.text.as_str()), ("trade-417971eff3174c0b", ""));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "The budget of dip-1d",
        "Budget now",
        "2.00 USD (it holds 2.00 USDC, worth 2.00 USD now)",
        "Wallet, free",
        "USDC 0.01 · SOL 0.1514",
        "New budget",
        "Write a number of USD between 1 and 25.",
        "Esc cancel",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    // only a number goes in, two decimals at most; one that is no budget does not go on
    for c in "3x0".chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    press(&mut a, &vm, KeyCode::Enter);
    assert_eq!(
        a.bot_budget.as_ref().map(|f| f.text.as_str()),
        Some("30"),
        "30 is over the most: it stays to be corrected"
    );
    assert!(a.bot_prompt.is_none() && a.page == Page::Bots);
    press(&mut a, &vm, KeyCode::Backspace);
    press(&mut a, &vm, KeyCode::Backspace);
    // nor does what it has already
    press(&mut a, &vm, KeyCode::Char('2'));
    press(&mut a, &vm, KeyCode::Enter);
    assert!(a.bot_prompt.is_none() && a.bot_budget.is_some());
    press(&mut a, &vm, KeyCode::Backspace);
    for c in "5.257".chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    assert_eq!(a.bot_budget.as_ref().map(|f| f.text.as_str()), Some("5.25"));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    // more than the wallet holds free in USDC: said, with what is sold for the rest
    assert!(out.contains("3.25 USD more: the wallet's free USDC first (0.01), and SOL sold for the 3.24 USD"), "{out}");
    assert!(
        out.contains("That is a real swap, with its fee.") && out.contains("It is then sold out at 2.62 USD."),
        "{out}"
    );
    assert!(out.contains("⏎ next (it asks once more)"), "{out}");
    // ⏎: the question, and only y does it
    press(&mut a, &vm, KeyCode::Enter);
    assert_eq!(a.bot_prompt, Some(("trade-417971eff3174c0b".to_string(), BotAction::Budget { cents: 525 })));
    assert!(a.bot_budget.is_none());
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("Change the budget of dip-1d from 2.00 to 5.25 USD?") && out.contains("y change it"), "{out}");
    press(&mut a, &vm, KeyCode::Char('n'));
    assert!(a.bot_prompt.is_none());

    // lowered while it is in SOL: said that it waits; in Chinese for who reads it
    let mut view = bots_view();
    (view.bots[0].cash, view.bots[0].sol, view.bots[0].paid, view.bots[0].wish) = (0.0, 0.0165, 2.0, Some(1.5));
    let mut a = bots_app(view);
    (a.zh, a.wallet) = (true, Some(Wallet::fixed(wallet_view())));
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("已登记：预算从 2.00 调到 1.50 美元，它手里的 USDC 够退时生效"), "{out}");
    assert!(out.contains("b 调预算"), "{out}");
    press(&mut a, &vm, KeyCode::Char('b'));
    for c in "1.5".chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "调整 dip-1d 的预算",
        "现在的预算",
        "新的预算",
        "调低 0.50 美元：这部分以 USDC 还给钱包，不再归它用。不用兑换。",
        "它现在手里只有 0.00 USDC，其余是 SOL：要等它卖出后才会调低",
        "清仓线变成 0.75 美元",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    press(&mut a, &vm, KeyCode::Esc);
    assert!(a.bot_budget.is_none() && a.bot_prompt.is_none(), "Esc: nothing asked, nothing changed");
    // what it has now, written while a change is noted, takes the change back
    press(&mut a, &vm, KeyCode::Char('b'));
    press(&mut a, &vm, KeyCode::Char('2'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("这会取消已登记的调整（调到 1.50 美元）"), "{out}");
    press(&mut a, &vm, KeyCode::Enter);
    assert_eq!(a.bot_prompt.as_ref().map(|p| p.1), Some(BotAction::Budget { cents: 200 }));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("取消 dip-1d 已登记的预算调整吗？"), "{out}");
    press(&mut a, &vm, KeyCode::Char('n'));
    // a paper run has no budget to change
    press(&mut a, &vm, KeyCode::Char('p'));
    press(&mut a, &vm, KeyCode::Char('j'));
    press(&mut a, &vm, KeyCode::Char('b'));
    assert!(a.bot_budget.is_none() && a.status_text().is_some_and(|s| s.contains("--lab")));
}

#[test]
fn a_bots_chart_is_looked_at_in_any_bar_while_its_rule_keeps_its_own() {
    use searcher_tui::cex::{Candle, Cex, CexState, OkxSource, Ticker};
    let vm = populated();
    let mut a = bots_app(bots_view());
    // the exchange's 15-minute candles of the bot's market, as the running program has them
    let candles: Vec<Candle> = (0..60)
        .map(|i| {
            let o = 120.0 + (i as f64 / 7.0).sin();
            Candle {
                start: Ts((1_791_180_000 + i * 900) * 1_000_000),
                open: o,
                high: o + 0.3,
                low: o - 0.2,
                close: o + 0.1,
                vol: 100.0 + (i % 9) as f64 * 40.0,
            }
        })
        .collect();
    let ticker = Ticker { inst: "SOL-USDT".into(), last: 120.8, ..Default::default() };
    let state = CexState { ticker: Some(ticker), candles, candles_for: Some((0, 3)), ..Default::default() };
    a.cex = Some(Cex::fixed(OkxSource::default(), state));
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    // the bars as tabs, the rule's own marked; volume under the price; the footer's key
    assert!(out.contains("1s   1m   5m   15m●   1h   4h   1D"), "{out}");
    assert!(out.contains("● the rule's own bar · [ ] other bars") && out.contains("live · OKX SOL-USDT"), "{out}");
    assert!(out.contains("vol 420") && out.contains('▇'), "volume, to the largest bar shown:\n{out}");
    assert!(out.lines().last().is_some_and(|l| l.contains("[ ] bar")), "{out}");
    // ] and [ and a click on a tab change what is looked at, never what the rule decides by
    press(&mut a, &vm, KeyCode::Char(']'));
    assert_eq!(a.bot_bar, Some(4));
    press(&mut a, &vm, KeyCode::Char('['));
    press(&mut a, &vm, KeyCode::Char('['));
    assert_eq!(a.bot_bar, Some(2));
    a.bot_bar = None;
    let _ = snapshot(&mut a, &vm, 200, 58);
    let at = {
        let hits = a.hits.borrow();
        let (r, _) = hits.iter().find(|(_, h)| *h == Hit::BotBar(1)).expect("the 1m tab is there to click");
        (r.x, r.y)
    };
    a.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), &vm);
    assert_eq!(a.bot_bar, Some(1));
    // in Chinese the readout is 开 高 低 收
    a.bot_bar = None;
    a.zh = true;
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("开 ") && out.contains("收 ") && out.contains("● 是规则自己用的周期 · [ ] 换周期"), "{out}");
    assert!(out.contains("量 420"), "{out}");
}

#[test]
fn for_who_reads_chinese_the_frame_is_in_it_and_the_help_explains_the_page() {
    let vm = populated();
    let mut a = wallet_app(wallet_view(), true);
    press(&mut a, &vm, KeyCode::Char('9'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    let (kpi, footer) = (out.lines().nth(1).unwrap_or_default(), out.lines().last().unwrap_or_default());
    assert!(kpi.contains("总资产") && kpi.contains("机会 60") && kpi.contains("已成交 12"), "{kpi}");
    for needle in ["1 总览", "9 机器人", "0 钱包", "n 新建", "K 急停", "? 说明"] {
        assert!(footer.contains(needle), "missing `{needle}`: {footer}");
    }
    // ?: what this page is, what each part of it means, what each key does
    press(&mut a, &vm, KeyCode::Char('?'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "说明 · 机器人",
        "一个机器人就是一条规则加一笔预算",
        "清仓线",
        "调预算：给它加钱或减钱（1 到 25 美元）",
        "只是换个周期看价格，规则仍按它自己的周期判断",
        "机器人可能亏钱，没有任何保证",
        "每一页都能用的键",
        "任意键关闭",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    press(&mut a, &vm, KeyCode::Char(' '));
    press(&mut a, &vm, KeyCode::Char('0'));
    press(&mut a, &vm, KeyCode::Char('?'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    for needle in ["说明 · 钱包", "转出的步骤", "输入收款地址的最后 4 位", "链上转账发出后无法撤回"]
    {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    // a small window says that there is more
    let out = buffer_text(&snapshot(&mut a, &vm, 100, 30));
    assert!(out.contains("说明 · 钱包") && out.contains("窗口再高一些可以看到全部"), "{out}");
    // the question of the kill switch can still be clicked in the footer
    press(&mut a, &vm, KeyCode::Char(' '));
    let _ = snapshot(&mut a, &vm, 200, 58);
    assert!(
        a.hits.borrow().iter().any(|(_, h)| *h == Hit::Help) && a.hits.borrow().iter().any(|(_, h)| *h == Hit::Kill)
    );
}

#[test]
fn a_new_bot_is_written_on_the_page_and_made_only_with_the_operators_word() {
    use searcher_tui::bots::NewBot;
    use std::sync::Mutex;
    let vm = populated();
    let made: std::sync::Arc<Mutex<Vec<NewBot>>> = Default::default();
    let log = made.clone();
    let view = bots_view();
    let port = BotPort {
        view: std::sync::Arc::new(move || view.clone()),
        act: std::sync::Arc::new(|_, _| Err("not here".into())),
        create: std::sync::Arc::new(move |spec| {
            log.lock().unwrap().push(spec.clone());
            Ok(format!("{} is made", spec.name))
        }),
    };
    let mut a = App::new(&TuiOptions { bots: Some(port.clone()), ..TuiOptions::default() });
    (a.theme, a.glyphs) = (Theme::with_depth(Depth::TrueColor), Glyphs::unicode());
    a.bots = Some(Bots::start(port));
    press(&mut a, &vm, KeyCode::Char('9'));
    press(&mut a, &vm, KeyCode::Char('n'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    show(&out);
    for needle in [
        "A new bot",
        "What it does: when a 15-minute bar of SOL closes under its average by k deviations",
        "busy: the average of 1 day",
        "calmer: the average of 3 days",
        "Buys at k",
        "Budget",
        // what it has cost before is said before it is made
        "the 1-day rule lost about 72 %",
        "type ALLOW LOSS to say you know it can lose money",
        "⏎ make it (it is not started)",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    // the other kind, its numbers; a budget written over
    press(&mut a, &vm, KeyCode::Right);
    for _ in 0..4 {
        press(&mut a, &vm, KeyCode::Tab);
    }
    press(&mut a, &vm, KeyCode::Backspace);
    press(&mut a, &vm, KeyCode::Char('4'));
    // ⏎ down to the last field, and on it: without the words it is not made, and it says why
    press(&mut a, &vm, KeyCode::Enter);
    press(&mut a, &vm, KeyCode::Enter);
    press(&mut a, &vm, KeyCode::Enter);
    assert!(made.lock().unwrap().is_empty());
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    assert!(out.contains("the last field takes the words ALLOW LOSS"), "{out}");
    for c in "ALLOW LOSS".chars() {
        press(&mut a, &vm, KeyCode::Char(c));
    }
    assert_eq!(a.page, Page::Bots, "what is typed goes into the form, not to the page");
    press(&mut a, &vm, KeyCode::Enter);
    assert!(a.bot_new.is_none() && a.status_text().is_some_and(|s| s.contains("dip-3d is made")));
    let spec = made.lock().unwrap()[0].clone();
    assert_eq!(
        (spec.name.as_str(), spec.window, spec.k, spec.stop, spec.budget, spec.total_stop),
        ("dip-3d", 288, 2.0, 0.05, 4.0, 0.5)
    );
    // a number out of its bounds is said, not made; Esc throws the form away
    press(&mut a, &vm, KeyCode::Char('n'));
    for _ in 0..4 {
        press(&mut a, &vm, KeyCode::Tab);
    }
    press(&mut a, &vm, KeyCode::Char('9'));
    press(&mut a, &vm, KeyCode::Tab);
    press(&mut a, &vm, KeyCode::Tab);
    press(&mut a, &vm, KeyCode::Enter);
    assert!(a.bot_new.as_ref().is_some_and(|f| f.error.as_deref() == Some("the budget (USD) is between 1 and 25")));
    press(&mut a, &vm, KeyCode::Esc);
    assert!(a.bot_new.is_none() && made.lock().unwrap().len() == 1);
    // in Chinese
    a.zh = true;
    press(&mut a, &vm, KeyCode::Char('n'));
    let out = buffer_text(&snapshot(&mut a, &vm, 200, 58));
    for needle in [
        "新建机器人",
        "勤快：看 1 天的平均价",
        "买入倍数 k",
        "只用亏得起的小钱",
        "输入 ALLOW LOSS 表示你知道它可能亏钱",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
}

#[test]
fn a_bots_buys_and_sales_are_trades_of_the_wallet_and_listed_as_such() {
    let vm = populated();
    let mut a = bots_app(bots_view());
    // the Trades page: the bots' own first, each with its bot, its side, what for what, at what price, what it made
    press(&mut a, &vm, KeyCode::Char('5'));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    show(&out);
    for needle in [
        "BOT TRADES (buy low, sell high)",
        "2 · page 9 has the bots",
        "dip-1d",
        "sell    0.016464     1.9965      121.26      -0.0035 USD",
        "buy     0.016464     2.0000      121.48",
        // the arbitrage's own are under them, as before
        "TRADES",
        "Session PnL",
    ] {
        assert!(out.contains(needle), "missing `{needle}`:\n{out}");
    }
    assert!(out.find("sell    0.016464") < out.find("buy     0.016464"), "newest first:\n{out}");
    a.zh = true;
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("机器人成交（低买高卖）") && out.contains("卖出") && out.contains("这笔盈亏"), "{out}");
    // a real bot that has not traded yet: said, so that the place is known
    let mut view = bots_view();
    view.bots[0].journal.clear();
    let mut a = bots_app(view);
    press(&mut a, &vm, KeyCode::Char('5'));
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("none yet: a bot's buys and sales are listed here as they happen"), "{out}");
    // without a real bot the page is the arbitrage's alone
    let mut view = bots_view();
    view.bots.remove(0);
    let mut a = bots_app(view);
    press(&mut a, &vm, KeyCode::Char('5'));
    assert!(!buffer_text(&snapshot(&mut a, &vm, 160, 50)).contains("BOT TRADES"));
    // the Markets page's order history has them too, among the arbitrage's
    let mut a = bots_app(bots_view());
    press(&mut a, &vm, KeyCode::Char('2'));
    a.mk_tab = searcher_tui::markets::BottomTab::OrderHistory;
    let out = buffer_text(&snapshot(&mut a, &vm, 160, 50));
    assert!(out.contains("bot") && out.contains("sell SOL @ 121.26") && out.contains("buy SOL @ 121.48"), "{out}");
}
