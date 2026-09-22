//! Size ladder: each round takes the next configured route (the enabled
//! strategies' cycles, in turn) and quotes it at every configured size, legs
//! chained exactly as the trading engine chains them (a leg's input is the
//! previous leg's quoted output).

use super::{Ctx, Priority, shuffle};
use anyhow::Result;
use searcher_core::Ts;
use searcher_core::config::Config;
use searcher_core::model::Leg;
use searcher_storage::research::LadderRow;
use searcher_strategy::{CandidatePlan, Strategy};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Compute units per leg for the priority-fee estimate: the recorded LIVE
/// simulations consumed ≈ 150k CU for a two-leg transaction, so this errs high.
const CU_PER_LEG: u64 = 150_000;

/// The enabled strategies' cycles; SOL → USDC → SOL when none is enabled.
pub fn plans(cfg: &Config) -> Result<Vec<CandidatePlan>> {
    let mut v: Vec<CandidatePlan> = crate::engine::build_strategies(cfg)?.iter().flat_map(|s| s.plans()).collect();
    if v.is_empty() {
        let t = cfg.tokens();
        let (sol, usdc) = (t.sol().clone(), t.get("USDC").cloned().ok_or_else(|| anyhow::anyhow!("no USDC token"))?);
        v.push(
            searcher_strategy::RoundTrip {
                base: sol,
                quote: usdc,
                amount: 0,
                weight: 1,
                fast: searcher_strategy::FastPairs(cfg.jupiter.fast_mode_pairs.clone()),
                max_accounts: cfg.jupiter.max_accounts,
            }
            .plans()
            .remove(0),
        );
    }
    Ok(v)
}

pub async fn run(ctx: Arc<Ctx>, plans: Vec<CandidatePlan>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(ctx.cfg.research.ladder_every_s));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut round: i64 = 0;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {}
        }
        let plan = &plans[round as usize % plans.len()];
        round += 1;
        let mut sizes = ctx.cfg.research.ladder_sizes_lamports.clone();
        shuffle(&mut sizes, Ts::now().0 as u64);
        for size in sizes {
            if *shutdown.borrow() {
                return;
            }
            let row = quote_cycle(&ctx, plan, size, round).await;
            if row.err.as_deref() == Some("stopped") {
                return;
            }
            ctx.record("ladder", |s| s.insert_ladder(&ctx.run, &row));
        }
    }
}

async fn quote_cycle(ctx: &Ctx, plan: &CandidatePlan, size: u64, round: i64) -> LadderRow {
    let mut row = LadderRow {
        round,
        ts: Ts::now().0,
        route_key: plan.key.clone(),
        route_label: plan.label.clone(),
        size,
        ..Default::default()
    };
    let mut legs: Vec<Leg> = Vec::new();
    let mut amount = size;
    for (i, spec) in plan.legs.iter().enumerate() {
        let req = ctx.request(spec.input, spec.output, amount, Some(spec));
        match ctx.gate.build(req, Priority::Normal).await {
            Ok(b) => {
                amount = b.leg.out_amount;
                legs.push(b.leg);
            }
            Err(e) if e == "stopped" => {
                row.err = Some(e);
                return row;
            }
            Err(e) => {
                row.err = Some(format!("leg {}: {e}", i + 1));
                break;
            }
        }
    }
    let (Some(first), Some(last)) = (legs.first(), legs.last()) else { return row };
    row.out1 = Some(first.out_amount);
    if legs.len() == plan.legs.len() {
        row.out2 = Some(last.out_amount);
        row.gross = Some(last.out_amount as i64 - size as i64);
        row.impact2_ppm = Some(last.price_impact.0);
        row.dexes2 = Some(legs[1..].iter().map(|l| l.dex_labels().join("+")).collect::<Vec<_>>().join(" | "));
        row.gap_ms = Some((last.quoted_at.0 - first.quoted_at.0) / 1_000);
        row.priority_est =
            first.cu_price_micro.map(|p| (p as u128 * (CU_PER_LEG * legs.len() as u64) as u128 / 1_000_000) as u64);
    }
    row.impact1_ppm = Some(first.price_impact.0);
    row.dexes1 = Some(first.dex_labels().join("+"));
    row
}
