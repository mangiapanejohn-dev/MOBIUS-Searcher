//! Profit engine. `final_out > initial_in` is never the criterion: expected net
//! PnL is gross PnL minus the full cost breakdown, and three independent guards
//! (lamports, edge, USD) must all pass.

use crate::costs::CostBreakdown;
use crate::units::{Ppm, UsdMicros, UsdPrice};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfitEval {
    /// gross_output − input (base atoms).
    pub gross_pnl: i64,
    /// gross_pnl − costs.total().
    pub expected_net: i64,
    pub gross_edge: Ppm,
    pub net_edge: Ppm,
    pub gross_usd: Option<UsdMicros>,
    pub expected_net_usd: Option<UsdMicros>,
    /// Net lamport change of the taker in an exact simulation, minus modeled
    /// costs the simulation cannot see (tip if not included, slippage, buffer).
    pub simulated_net: Option<i64>,
}

pub fn evaluate(
    input: u64,
    gross_output: u64,
    costs: &CostBreakdown,
    base_decimals: u8,
    base_price: Option<UsdPrice>,
) -> ProfitEval {
    let gross = gross_output as i128 - input as i128;
    let net = gross - costs.total() as i128;
    let clamp = |v: i128| v.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    ProfitEval {
        gross_pnl: clamp(gross),
        expected_net: clamp(net),
        gross_edge: Ppm::ratio(gross, input as i128).unwrap_or(Ppm::ZERO),
        net_edge: Ppm::ratio(net, input as i128).unwrap_or(Ppm::ZERO),
        gross_usd: base_price.and_then(|p| p.value(gross, base_decimals)),
        expected_net_usd: base_price.and_then(|p| p.value(net, base_decimals)),
        simulated_net: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfitGuards {
    pub min_profit_lamports: i64,
    pub min_profit_edge: Ppm,
    pub min_profit_usd: UsdMicros,
}

impl Default for ProfitGuards {
    fn default() -> Self {
        Self { min_profit_lamports: 10_000, min_profit_edge: Ppm::from_bps(5), min_profit_usd: UsdMicros(10_000) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "guard")]
pub enum GuardFailure {
    Lamports {
        net: i64,
        min: i64,
    },
    Edge {
        net_edge: Ppm,
        min: Ppm,
    },
    Usd {
        net_usd: UsdMicros,
        min: UsdMicros,
    },
    /// USD guard configured but no price available to evaluate it.
    PriceUnavailable,
}

/// All three guards must pass. The PnL used is `simulated_net` when present
/// (it is the more grounded number), otherwise `expected_net`; the smaller of
/// the two is used if both exist.
pub fn check_guards(eval: &ProfitEval, input: u64, g: &ProfitGuards) -> Result<(), GuardFailure> {
    let net = match eval.simulated_net {
        Some(s) => s.min(eval.expected_net),
        None => eval.expected_net,
    };
    if net < g.min_profit_lamports {
        return Err(GuardFailure::Lamports { net, min: g.min_profit_lamports });
    }
    let edge = Ppm::ratio(net as i128, input as i128).unwrap_or(Ppm::ZERO);
    if edge < g.min_profit_edge {
        return Err(GuardFailure::Edge { net_edge: edge, min: g.min_profit_edge });
    }
    if g.min_profit_usd.0 > 0 {
        let usd = match (eval.expected_net_usd, eval.expected_net) {
            (Some(usd), en) if en != 0 => {
                // rescale USD to the chosen `net` (same price)
                UsdMicros(((usd.0 as i128 * net as i128) / en as i128) as i64)
            }
            (Some(usd), _) => usd,
            (None, _) => return Err(GuardFailure::PriceUnavailable),
        };
        if usd < g.min_profit_usd {
            return Err(GuardFailure::Usd { net_usd: usd, min: g.min_profit_usd });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn costs(total_parts: [u64; 7]) -> CostBreakdown {
        CostBreakdown {
            base_fee: total_parts[0],
            priority_fee: total_parts[1],
            jito_tip: total_parts[2],
            ata_rent: total_parts[3],
            expected_slippage: total_parts[4],
            safety_buffer: total_parts[5],
            platform_fee: total_parts[6],
            ..Default::default()
        }
    }

    #[test]
    fn positive_gross_negative_net_is_not_profitable() {
        // out > in by 20k lamports, but fees+tip eat it.
        let c = costs([5_000, 1_632, 10_000, 0, 3_000, 5_000, 0]);
        let e = evaluate(1_000_000_000, 1_000_020_000, &c, 9, Some(UsdPrice::new(105_000_000)));
        assert_eq!(e.gross_pnl, 20_000);
        assert_eq!(e.expected_net, 20_000 - 24_632);
        assert!(e.net_edge < Ppm::ZERO);
        assert!(check_guards(&e, 1_000_000_000, &ProfitGuards::default()).is_err());
    }

    #[test]
    fn guards_each_layer() {
        let price = Some(UsdPrice::new(100_000_000)); // $100/SOL
        let g = ProfitGuards {
            min_profit_lamports: 50_000,
            min_profit_edge: Ppm::from_bps(1),
            min_profit_usd: UsdMicros(10_000),
        };
        // lamports guard
        let e = evaluate(1_000_000_000, 1_000_040_000, &CostBreakdown::default(), 9, price);
        assert!(matches!(check_guards(&e, 1_000_000_000, &g), Err(GuardFailure::Lamports { .. })));
        // edge guard: 60k on 1000 SOL = 0.006bp
        let e = evaluate(1_000_000_000_000, 1_000_000_060_000, &CostBreakdown::default(), 9, price);
        assert!(matches!(check_guards(&e, 1_000_000_000_000, &g), Err(GuardFailure::Edge { .. })));
        // usd guard: 60k lamports @ $100 = $0.006 < $0.01
        let e = evaluate(100_000_000, 100_060_000, &CostBreakdown::default(), 9, price);
        assert!(matches!(check_guards(&e, 100_000_000, &g), Err(GuardFailure::Usd { .. })));
        // no price
        let e = evaluate(100_000_000, 100_200_000, &CostBreakdown::default(), 9, None);
        assert_eq!(check_guards(&e, 100_000_000, &g), Err(GuardFailure::PriceUnavailable));
        // pass: 200k lamports @ $100 = $0.02, 20bp
        let e = evaluate(100_000_000, 100_200_000, &CostBreakdown::default(), 9, price);
        assert_eq!(check_guards(&e, 100_000_000, &g), Ok(()));
    }

    #[test]
    fn simulated_net_overrides_when_worse() {
        let price = Some(UsdPrice::new(100_000_000));
        let mut e = evaluate(100_000_000, 100_200_000, &CostBreakdown::default(), 9, price);
        e.simulated_net = Some(1_000);
        assert!(matches!(check_guards(&e, 100_000_000, &ProfitGuards::default()), Err(GuardFailure::Lamports { .. })));
    }

    proptest! {
        #[test]
        fn net_is_exact_integer_identity(
            input in 1u64..=u64::MAX / 4,
            out in 0u64..=u64::MAX / 4,
            parts in proptest::array::uniform7(0u64..=1_000_000_000u64),
        ) {
            let c = costs(parts);
            let e = evaluate(input, out, &c, 9, None);
            // parts[3] is the deposit (rent of accounts left created): capital, not a cost
            let expect = out as i128 - input as i128
                - parts.iter().enumerate().filter(|(i, _)| *i != 3).map(|(_, p)| *p as i128).sum::<i128>();
            prop_assert_eq!(e.expected_net as i128, expect);
            prop_assert_eq!(e.gross_pnl as i128, out as i128 - input as i128);
            prop_assert!(e.net_edge <= e.gross_edge);
        }

        #[test]
        fn each_cost_reduces_net_one_for_one(
            input in 1u64..1_000_000_000_000u64,
            out in 0u64..1_000_000_000_000u64,
            idx in 0usize..7,
            extra in 0u64..1_000_000_000u64,
        ) {
            let base = [0u64; 7];
            let mut more = base;
            more[idx] = extra;
            let a = evaluate(input, out, &costs(base), 9, None);
            let b = evaluate(input, out, &costs(more), 9, None);
            // every cost lowers net one for one; the deposit (idx 3) does not
            let expect = if idx == 3 { 0 } else { extra as i64 };
            prop_assert_eq!(a.expected_net - b.expected_net, expect);
        }

        #[test]
        fn guard_never_passes_below_min_lamports(
            input in 1u64..1_000_000_000_000u64,
            out in 0u64..1_000_000_000_000u64,
            min in 0i64..1_000_000i64,
        ) {
            let e = evaluate(input, out, &CostBreakdown::default(), 9, Some(UsdPrice::new(1)));
            let g = ProfitGuards { min_profit_lamports: min, min_profit_edge: Ppm(i64::MIN), min_profit_usd: UsdMicros(0) };
            let ok = check_guards(&e, input, &g).is_ok();
            prop_assert_eq!(ok, e.expected_net >= min);
        }
    }
}
