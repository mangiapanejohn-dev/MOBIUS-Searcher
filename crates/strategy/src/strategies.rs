//! Strategy trait and the three first-phase strategies. Strategies only
//! *propose* cycles; pricing, simulation and risk live elsewhere.

use crate::plan::{CandidatePlan, LegSpec};
use searcher_core::model::{DexFilter, RoutingMode, StrategyKind};
use searcher_core::token::Token;

pub trait Strategy: Send {
    fn kind(&self) -> StrategyKind;
    fn name(&self) -> String;
    fn weight(&self) -> u32;
    /// Next cycle to evaluate; strategies rotate through their variants.
    fn next_plan(&mut self) -> Option<CandidatePlan>;
    /// Every cycle this strategy can evaluate (the event-driven scheduler's
    /// route universe).
    fn plans(&self) -> Vec<CandidatePlan>;
}

/// Pairs ("SOL/USDC") liquid enough for `mode=fast`; everything else routes normally.
#[derive(Clone, Debug, Default)]
pub struct FastPairs(pub Vec<String>);

impl FastPairs {
    pub fn mode(&self, a: &Token, b: &Token) -> RoutingMode {
        let p1 = format!("{}/{}", a.symbol, b.symbol);
        let p2 = format!("{}/{}", b.symbol, a.symbol);
        if self.0.iter().any(|p| *p == p1 || *p == p2) { RoutingMode::Fast } else { RoutingMode::Normal }
    }
}

/// Baseline: base → quote → base with unconstrained best routes. Expected to
/// be net-negative (spread + fees); used to validate the whole pipeline.
pub struct RoundTrip {
    pub base: Token,
    pub quote: Token,
    pub amount: u64,
    pub weight: u32,
    pub fast: FastPairs,
    pub max_accounts: Option<u8>,
}

impl Strategy for RoundTrip {
    fn kind(&self) -> StrategyKind {
        StrategyKind::RoundTrip
    }

    fn name(&self) -> String {
        format!("{}⇄{}", self.base.symbol, self.quote.symbol)
    }

    fn weight(&self) -> u32 {
        self.weight
    }

    fn plans(&self) -> Vec<CandidatePlan> {
        vec![self.plan()]
    }

    fn next_plan(&mut self) -> Option<CandidatePlan> {
        Some(self.plan())
    }
}

impl RoundTrip {
    fn plan(&self) -> CandidatePlan {
        let mode = self.fast.mode(&self.base, &self.quote);
        CandidatePlan {
            strategy: StrategyKind::RoundTrip,
            key: format!("rt:{}:{}:{}", self.base.symbol, self.quote.symbol, self.amount),
            label: format!("{}→{}→{}", self.base.symbol, self.quote.symbol, self.base.symbol),
            base_mint: self.base.mint,
            amount: self.amount,
            legs: vec![
                LegSpec {
                    input: self.base.mint,
                    output: self.quote.mint,
                    dex_filter: DexFilter::Any,
                    mode,
                    max_accounts: self.max_accounts,
                },
                LegSpec {
                    input: self.quote.mint,
                    output: self.base.mint,
                    dex_filter: DexFilter::Any,
                    mode,
                    max_accounts: self.max_accounts,
                },
            ],
        }
    }
}

/// Buy on one DEX, sell on another: leg 1 restricted to DEX A, leg 2 to DEX B,
/// for every ordered pair (A, B), A ≠ B.
pub struct CrossDex {
    pub base: Token,
    pub quote: Token,
    pub amount: u64,
    pub weight: u32,
    pub dexes: Vec<String>,
    pub fast: FastPairs,
    pub max_accounts: Option<u8>,
    cursor: usize,
}

impl CrossDex {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base: Token,
        quote: Token,
        amount: u64,
        weight: u32,
        dexes: Vec<String>,
        fast: FastPairs,
        max_accounts: Option<u8>,
    ) -> Self {
        Self { base, quote, amount, weight, dexes, fast, max_accounts, cursor: 0 }
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for a in &self.dexes {
            for b in &self.dexes {
                if a != b {
                    v.push((a.clone(), b.clone()));
                }
            }
        }
        v
    }
}

impl Strategy for CrossDex {
    fn kind(&self) -> StrategyKind {
        StrategyKind::CrossDex
    }

    fn name(&self) -> String {
        format!("cross-dex {}/{} ({} dexes)", self.base.symbol, self.quote.symbol, self.dexes.len())
    }

    fn weight(&self) -> u32 {
        self.weight
    }

    fn plans(&self) -> Vec<CandidatePlan> {
        self.pairs().into_iter().map(|(a, b)| self.plan_for(a, b)).collect()
    }

    fn next_plan(&mut self) -> Option<CandidatePlan> {
        let pairs = self.pairs();
        if pairs.is_empty() {
            return None;
        }
        let (a, b) = pairs[self.cursor % pairs.len()].clone();
        self.cursor = (self.cursor + 1) % pairs.len();
        Some(self.plan_for(a, b))
    }
}

impl CrossDex {
    fn plan_for(&self, a: String, b: String) -> CandidatePlan {
        let mode = self.fast.mode(&self.base, &self.quote);
        CandidatePlan {
            strategy: StrategyKind::CrossDex,
            key: format!("xd:{}:{}:{a}>{b}:{}", self.base.symbol, self.quote.symbol, self.amount),
            label: format!("{a} → {b}"),
            base_mint: self.base.mint,
            amount: self.amount,
            legs: vec![
                LegSpec {
                    input: self.base.mint,
                    output: self.quote.mint,
                    dex_filter: DexFilter::Only(vec![a]),
                    mode,
                    max_accounts: self.max_accounts,
                },
                LegSpec {
                    input: self.quote.mint,
                    output: self.base.mint,
                    dex_filter: DexFilter::Only(vec![b]),
                    mode,
                    max_accounts: self.max_accounts,
                },
            ],
        }
    }
}

/// Cycle through ≥ 3 tokens starting and ending at the base (e.g.
/// SOL → USDC → JUP → SOL). Illiquid hops use normal routing.
pub struct Triangular {
    pub cycle: Vec<Token>,
    pub amount: u64,
    pub weight: u32,
    pub fast: FastPairs,
    pub max_accounts: Option<u8>,
}

impl Strategy for Triangular {
    fn kind(&self) -> StrategyKind {
        StrategyKind::Triangular
    }

    fn name(&self) -> String {
        self.label()
    }

    fn weight(&self) -> u32 {
        self.weight
    }

    fn plans(&self) -> Vec<CandidatePlan> {
        self.plan().into_iter().collect()
    }

    fn next_plan(&mut self) -> Option<CandidatePlan> {
        self.plan()
    }
}

impl Triangular {
    fn plan(&self) -> Option<CandidatePlan> {
        if self.cycle.len() < 3 {
            return None;
        }
        let n = self.cycle.len();
        let legs = (0..n)
            .map(|i| {
                let a = &self.cycle[i];
                let b = &self.cycle[(i + 1) % n];
                LegSpec {
                    input: a.mint,
                    output: b.mint,
                    dex_filter: DexFilter::Any,
                    mode: self.fast.mode(a, b),
                    max_accounts: self.max_accounts,
                }
            })
            .collect();
        Some(CandidatePlan {
            strategy: StrategyKind::Triangular,
            key: format!("tri:{}:{}", self.label(), self.amount),
            label: self.label(),
            base_mint: self.cycle[0].mint,
            amount: self.amount,
            legs,
        })
    }

    fn label(&self) -> String {
        let mut s: Vec<&str> = self.cycle.iter().map(|t| t.symbol.as_str()).collect();
        s.push(&self.cycle[0].symbol);
        s.join("→")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::token::TokenRegistry;

    fn t(sym: &str) -> Token {
        TokenRegistry::defaults().get(sym).unwrap().clone()
    }

    fn fast() -> FastPairs {
        FastPairs(vec!["SOL/USDC".into()])
    }

    #[test]
    fn round_trip_is_closed_and_fast_on_majors() {
        let mut s =
            RoundTrip { base: t("SOL"), quote: t("USDC"), amount: 1, weight: 1, fast: fast(), max_accounts: Some(30) };
        let p = s.next_plan().unwrap();
        assert_eq!(p.legs.len(), 2);
        assert_eq!(p.legs[0].input, p.legs[1].output);
        assert!(p.legs.iter().all(|l| l.mode == RoutingMode::Fast));
        assert_eq!(p.label, "SOL→USDC→SOL");
    }

    #[test]
    fn cross_dex_rotates_all_ordered_pairs_with_dex_constraints() {
        let dexes = vec!["Raydium CLMM".to_string(), "Whirlpool".into(), "Meteora DLMM".into()];
        let mut s = CrossDex::new(t("SOL"), t("USDC"), 5, 3, dexes, fast(), None);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..6 {
            let p = s.next_plan().unwrap();
            let (DexFilter::Only(a), DexFilter::Only(b)) = (&p.legs[0].dex_filter, &p.legs[1].dex_filter) else {
                panic!("cross-dex legs must be dex-constrained")
            };
            assert_ne!(a, b);
            seen.insert(p.key);
        }
        assert_eq!(seen.len(), 6, "3 dexes → 6 ordered pairs");
        assert_eq!(s.next_plan().unwrap().label, "Raydium CLMM → Whirlpool", "wraps around");
    }

    #[test]
    fn plans_enumerate_every_route_once() {
        let dexes = vec!["Raydium CLMM".to_string(), "Whirlpool".into(), "Meteora DLMM".into(), "HumidiFi".into()];
        let s = CrossDex::new(t("SOL"), t("USDC"), 5, 3, dexes, fast(), None);
        let keys: std::collections::BTreeSet<_> = s.plans().into_iter().map(|p| p.key).collect();
        assert_eq!(keys.len(), 12, "4 dexes → 12 ordered pairs");
        let rt = RoundTrip { base: t("SOL"), quote: t("USDC"), amount: 1, weight: 1, fast: fast(), max_accounts: None };
        assert_eq!(rt.plans().len(), 1);
    }

    #[test]
    fn triangular_uses_normal_routing_for_illiquid_hops() {
        let mut s = Triangular {
            cycle: vec![t("SOL"), t("USDC"), t("JUP")],
            amount: 1,
            weight: 1,
            fast: fast(),
            max_accounts: None,
        };
        let p = s.next_plan().unwrap();
        assert_eq!(p.label, "SOL→USDC→JUP→SOL");
        assert_eq!(p.legs.len(), 3);
        assert_eq!(p.legs[0].mode, RoutingMode::Fast);
        assert_eq!(p.legs[1].mode, RoutingMode::Normal);
        assert_eq!(p.legs[2].mode, RoutingMode::Normal);
        assert_eq!(p.legs[2].output, p.legs[0].input);
    }
}
