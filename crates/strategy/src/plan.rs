use searcher_core::Address;
use searcher_core::model::{DexFilter, RoutingMode, StrategyKind};

/// One leg to request from the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegSpec {
    pub input: Address,
    pub output: Address,
    pub dex_filter: DexFilter,
    pub mode: RoutingMode,
    pub max_accounts: Option<u8>,
}

/// A cycle to evaluate: legs are requested in order, each leg's input amount
/// is the previous leg's quoted output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidatePlan {
    pub strategy: StrategyKind,
    /// Stable identity over time (for lifetime / episode statistics).
    pub key: String,
    pub label: String,
    pub base_mint: Address,
    pub amount: u64,
    pub legs: Vec<LegSpec>,
}

impl CandidatePlan {
    /// Router requests needed to price this plan (one per leg).
    pub fn requests(&self) -> usize {
        self.legs.len()
    }
}
