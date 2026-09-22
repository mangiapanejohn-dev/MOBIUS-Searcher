//! Turns quoted legs into a fully-costed `Opportunity`, and re-prices it after
//! simulation. Pure functions: no IO.

use crate::plan::CandidatePlan;
use searcher_core::costs::{CostInputs, CostParams, TxShape, compute_costs};
use searcher_core::model::{Leg, OppStatus, Opportunity, OpportunityId, Route, SkipReason};
use searcher_core::profit::{GuardFailure, ProfitGuards, check_guards, evaluate};
use searcher_core::{Ts, UsdPrice};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TipInfo {
    pub lamports: u64,
    /// The tip policy wanted more than its cap.
    pub capped: bool,
}

pub struct PricingEnv<'a> {
    pub cost_params: &'a CostParams,
    pub guards: &'a ProfitGuards,
    /// Tip for a given expected profit before tip (lamports).
    pub tip: &'a dyn Fn(i64) -> TipInfo,
}

pub struct PricingInput<'a> {
    pub id: OpportunityId,
    pub plan: &'a CandidatePlan,
    pub legs: Vec<Leg>,
    pub now: Ts,
    pub slot: Option<u64>,
    pub sol_price: Option<UsdPrice>,
    pub base_decimals: u8,
    pub shape: TxShape,
    pub atas_to_create: u8,
}

/// Validate that the legs form the planned closed cycle with chained amounts.
pub fn validate_chain(plan: &CandidatePlan, legs: &[Leg]) -> Result<(), String> {
    if legs.len() != plan.legs.len() || legs.is_empty() {
        return Err(format!("expected {} legs, got {}", plan.legs.len(), legs.len()));
    }
    if legs[0].in_amount != plan.amount {
        return Err(format!("leg 0 input {} ≠ plan amount {}", legs[0].in_amount, plan.amount));
    }
    for (i, (l, s)) in legs.iter().zip(&plan.legs).enumerate() {
        if l.input_mint != s.input || l.output_mint != s.output {
            return Err(format!("leg {i} mints differ from plan"));
        }
        if i > 0 && l.in_amount != legs[i - 1].out_amount {
            return Err(format!("leg {i} input {} ≠ leg {} output {}", l.in_amount, i - 1, legs[i - 1].out_amount));
        }
    }
    if legs.last().map(|l| l.output_mint) != Some(plan.base_mint) {
        return Err("cycle does not close on the base mint".into());
    }
    Ok(())
}

fn guard_skip(g: &GuardFailure) -> SkipReason {
    match g {
        GuardFailure::PriceUnavailable => SkipReason::PriceUnavailable,
        _ => SkipReason::EdgeTooSmall,
    }
}

fn costs_for(
    legs: &[Leg],
    input: u64,
    shape: TxShape,
    cu_used: Option<&[u32]>,
    tip: u64,
    atas: u8,
    p: &CostParams,
) -> searcher_core::CostBreakdown {
    let cu_price = legs.iter().filter_map(|l| l.cu_price_micro).max().unwrap_or(0);
    compute_costs(
        &CostInputs {
            legs,
            input,
            shape,
            cu_used,
            cu_price_micro: cu_price,
            jito_tip: tip,
            atas_to_create: atas,
            platform_fee: 0,
        },
        p,
    )
}

/// Quote-stage pricing (CU estimated). Every result is returned — including
/// unprofitable ones — with the reason it would be skipped.
pub fn price(inp: PricingInput<'_>, env: &PricingEnv<'_>) -> Opportunity {
    let chain_ok = validate_chain(inp.plan, &inp.legs);
    let input = inp.plan.amount;
    let gross_output = inp.legs.last().map(|l| l.out_amount).unwrap_or(0);

    let pre_tip = costs_for(&inp.legs, input, inp.shape, None, 0, inp.atas_to_create, env.cost_params);
    let profit_before_tip = gross_output as i64 - input as i64 - pre_tip.total() as i64;
    let tip = (env.tip)(profit_before_tip);
    let costs = costs_for(&inp.legs, input, inp.shape, None, tip.lamports, inp.atas_to_create, env.cost_params);
    let eval = evaluate(input, gross_output, &costs, inp.base_decimals, inp.sol_price);

    let guard = check_guards(&eval, input, env.guards).err();
    let status = if chain_ok.is_err() {
        OppStatus::Skipped(SkipReason::BuildFailed)
    } else if let Some(g) = &guard {
        OppStatus::Skipped(guard_skip(g))
    } else if tip.capped {
        OppStatus::Skipped(SkipReason::TipTooHigh)
    } else {
        OppStatus::Quoted
    };

    Opportunity {
        id: inp.id,
        key: inp.plan.key.clone(),
        strategy: inp.plan.strategy,
        label: inp.plan.label.clone(),
        detected_at: inp.now,
        slot: inp.slot,
        base_mint: inp.plan.base_mint,
        input,
        gross_output,
        route: Route { legs: inp.legs },
        costs,
        eval,
        status,
        updated_at: inp.now,
        sol_price: inp.sol_price,
        simulation: None,
        risk: None,
        guard,
    }
}

/// Re-price with simulated CU (per tx) and the actual transaction shape.
/// `simulated_delta`: taker's net lamport change measured in an exact
/// simulation, already corrected to the final CU limit. Slippage and buffer
/// are still subtracted (the simulation executed at the quoted state).
#[allow(clippy::too_many_arguments)]
pub fn reprice_after_simulation(
    opp: &mut Opportunity,
    shape: TxShape,
    cu_used: &[u32],
    simulated_delta: Option<i64>,
    atas_to_create: u8,
    env: &PricingEnv<'_>,
    base_decimals: u8,
    now: Ts,
) {
    let legs = opp.route.legs.clone();
    let pre_tip = costs_for(&legs, opp.input, shape, Some(cu_used), 0, atas_to_create, env.cost_params);
    let profit_before_tip = opp.gross_output as i64 - opp.input as i64 - pre_tip.total() as i64;
    let tip = (env.tip)(profit_before_tip);
    opp.costs = costs_for(&legs, opp.input, shape, Some(cu_used), tip.lamports, atas_to_create, env.cost_params);
    let mut eval = evaluate(opp.input, opp.gross_output, &opp.costs, base_decimals, opp.sol_price);
    eval.simulated_net =
        simulated_delta.map(|d| d - opp.costs.expected_slippage as i64 - opp.costs.safety_buffer as i64);
    opp.eval = eval;
    opp.updated_at = now;
    opp.guard = check_guards(&opp.eval, opp.input, env.guards).err();
    opp.status = match &opp.guard {
        Some(g) => OppStatus::Skipped(guard_skip(g)),
        None if tip.capped => OppStatus::Skipped(SkipReason::TipTooHigh),
        None => OppStatus::Quoted,
    };
}

/// Explicit `slippageBps` for the final leg such that its min-out is at least
/// `required` (input + costs + min profit). `None` when the quote cannot meet
/// it at all.
pub fn protective_slippage_bps(final_out: u64, required: u64) -> Option<u16> {
    if final_out == 0 || required > final_out {
        return None;
    }
    let bps = (final_out - required) as u128 * 10_000 / final_out as u128;
    Some(bps.min(10_000) as u16)
}

/// Minimum final output that still meets the lamport guard after all costs
/// except modeled slippage (slippage is what the tolerance absorbs).
pub fn required_final_out(opp: &Opportunity, guards: &ProfitGuards) -> u64 {
    let c = &opp.costs;
    let non_slippage = c.total().saturating_sub(c.expected_slippage);
    (opp.input as i128 + non_slippage as i128 + guards.min_profit_lamports.max(0) as i128).clamp(0, u64::MAX as i128)
        as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::LegSpec;
    use searcher_core::model::{DexFilter, RoutingMode, SlippageSpec, StrategyKind};
    use searcher_core::{Address, Ppm, UsdMicros};

    const SOL: Address = Address([1; 32]);
    const USDC: Address = Address([2; 32]);

    fn plan() -> CandidatePlan {
        let spec = |i, o| LegSpec {
            input: i,
            output: o,
            dex_filter: DexFilter::Any,
            mode: RoutingMode::Normal,
            max_accounts: None,
        };
        CandidatePlan {
            strategy: StrategyKind::CrossDex,
            key: "k".into(),
            label: "A → B".into(),
            base_mint: SOL,
            amount: 1_000_000_000,
            legs: vec![spec(SOL, USDC), spec(USDC, SOL)],
        }
    }

    fn leg(i: u8, input: Address, output: Address, ina: u64, out: u64) -> Leg {
        Leg {
            index: i,
            input_mint: input,
            output_mint: output,
            in_amount: ina,
            out_amount: out,
            min_out: out - out / 1_000,
            slippage_bps: 10,
            slippage_spec: SlippageSpec::Rtse,
            price_impact: Ppm::ZERO,
            hops: vec![],
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            quoted_at: Ts(0),
            latency_ms: 20,
            cu_price_micro: Some(10_000),
            last_valid_block_height: 0,
            request_id: None,
        }
    }

    fn env<'a>(p: &'a CostParams, g: &'a ProfitGuards, tip: &'a dyn Fn(i64) -> TipInfo) -> PricingEnv<'a> {
        PricingEnv { cost_params: p, guards: g, tip }
    }

    fn input(legs: Vec<Leg>) -> PricingInput<'static> {
        let plan: &'static CandidatePlan = Box::leak(Box::new(plan()));
        PricingInput {
            id: OpportunityId(1),
            plan,
            legs,
            now: Ts(5),
            slot: Some(9),
            sol_price: Some(UsdPrice::new(105_000_000)),
            base_decimals: 9,
            shape: TxShape::SINGLE,
            atas_to_create: 0,
        }
    }

    #[test]
    fn profitable_gross_but_not_net_is_skipped_with_reason() {
        let p = CostParams::default();
        let g = ProfitGuards::default();
        let tip = |_| TipInfo { lamports: 10_000, capped: false };
        let legs = vec![leg(0, SOL, USDC, 1_000_000_000, 105_000_000), leg(1, USDC, SOL, 105_000_000, 1_000_050_000)];
        let o = price(input(legs), &env(&p, &g, &tip));
        assert_eq!(o.eval.gross_pnl, 50_000);
        assert!(o.eval.expected_net < 0);
        assert_eq!(o.status, OppStatus::Skipped(SkipReason::EdgeTooSmall));
        assert_eq!(o.costs.jito_tip, 10_000);
    }

    #[test]
    fn clearly_profitable_is_quoted_and_tip_sees_pre_tip_profit() {
        let p = CostParams { expected_slippage_share: Ppm(0), ..CostParams::default() };
        let g = ProfitGuards { min_profit_usd: UsdMicros(0), ..ProfitGuards::default() };
        let seen = std::cell::Cell::new(0i64);
        let tip = |pre: i64| {
            seen.set(pre);
            TipInfo { lamports: (pre / 2).max(1_000) as u64, capped: false }
        };
        let legs = vec![leg(0, SOL, USDC, 1_000_000_000, 105_000_000), leg(1, USDC, SOL, 105_000_000, 1_010_000_000)];
        let o = price(input(legs), &env(&p, &g, &tip));
        assert_eq!(o.status, OppStatus::Quoted);
        assert!(seen.get() > 0);
        assert_eq!(o.costs.jito_tip as i64, seen.get() / 2);
        assert_eq!(o.eval.expected_net, 10_000_000 - o.costs.total() as i64);
    }

    #[test]
    fn broken_chain_is_build_failed() {
        let p = CostParams::default();
        let g = ProfitGuards::default();
        let tip = |_| TipInfo { lamports: 1_000, capped: false };
        let legs = vec![leg(0, SOL, USDC, 1_000_000_000, 105_000_000), leg(1, USDC, SOL, 104_000_000, 2_000_000_000)];
        let o = price(input(legs), &env(&p, &g, &tip));
        assert_eq!(o.status, OppStatus::Skipped(SkipReason::BuildFailed));
    }

    #[test]
    fn capped_tip_is_tip_too_high() {
        let p = CostParams { expected_slippage_share: Ppm(0), ..CostParams::default() };
        let g = ProfitGuards { min_profit_usd: UsdMicros(0), ..ProfitGuards::default() };
        let tip = |_| TipInfo { lamports: 200_000, capped: true };
        let legs = vec![leg(0, SOL, USDC, 1_000_000_000, 105_000_000), leg(1, USDC, SOL, 105_000_000, 1_010_000_000)];
        let o = price(input(legs), &env(&p, &g, &tip));
        assert_eq!(o.status, OppStatus::Skipped(SkipReason::TipTooHigh));
    }

    #[test]
    fn reprice_uses_simulated_cu_and_worse_of_model_and_sim() {
        let p = CostParams { expected_slippage_share: Ppm(0), ..CostParams::default() };
        let g = ProfitGuards { min_profit_usd: UsdMicros(0), ..ProfitGuards::default() };
        let tip = |_| TipInfo { lamports: 5_000, capped: false };
        let legs = vec![leg(0, SOL, USDC, 1_000_000_000, 105_000_000), leg(1, USDC, SOL, 105_000_000, 1_010_000_000)];
        let mut o = price(input(legs), &env(&p, &g, &tip));
        reprice_after_simulation(&mut o, TxShape::SINGLE, &[250_000], Some(20_000), 0, &env(&p, &g, &tip), 9, Ts(9));
        assert_eq!(o.costs.compute_units_used, Some(250_000));
        assert_eq!(o.costs.compute_units_limit, 300_000);
        assert_eq!(o.eval.simulated_net, Some(20_000 - o.costs.safety_buffer as i64));
        // simulated net (~ -85k) is far below the model → guard fails
        assert_eq!(o.status, OppStatus::Skipped(SkipReason::EdgeTooSmall));
    }

    #[test]
    fn protective_slippage() {
        assert_eq!(protective_slippage_bps(1_000_000, 999_000), Some(10));
        assert_eq!(protective_slippage_bps(1_000_000, 1_000_000), Some(0));
        assert_eq!(protective_slippage_bps(1_000_000, 1_000_001), None);
        assert_eq!(protective_slippage_bps(0, 0), None);
    }
}
