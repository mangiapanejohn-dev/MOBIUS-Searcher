//! Cost model. Everything is in base-asset atoms (lamports: every strategy
//! starts and ends in SOL, and network fees are paid in SOL).
//!
//! What is *not* subtracted: AMM LP fees. Jupiter's quoted `outAmount` is
//! already net of pool fees, so subtracting them again would double-count.
//! They are flagged as `swap_fees_embedded` and shown as such in the UI.

use crate::model::Leg;
use crate::units::{
    BASE_FEE_LAMPORTS_PER_SIGNATURE, MAX_COMPUTE_UNITS_PER_TX, PPM_ONE, Ppm, TOKEN_ACCOUNT_RENT_LAMPORTS,
    cu_limit_with_margin, priority_fee_lamports,
};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// Pre-simulation estimate (CU per leg estimated).
    #[default]
    Quote,
    /// CU limits derived from actual simulated consumption.
    Simulated,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub source: CostSource,
    pub tx_count: u8,
    pub signatures: u8,
    /// 5000 lamports × signatures.
    pub base_fee: u64,
    /// Requested CU limit summed over transactions.
    pub compute_units_limit: u32,
    /// Simulated CU consumption, when known.
    pub compute_units_used: Option<u32>,
    pub compute_unit_price_micro: u64,
    /// Σ ceil(cu_limit × cu_price / 1e6). This is the compute cost.
    pub priority_fee: u64,
    pub jito_tip: u64,
    /// Rent locked by token accounts the cycle has to create (not refunded in-tx).
    pub ata_rent: u64,
    pub atas_created: u8,
    /// Modeled adverse fill vs. quote on the final leg (share × tolerance).
    pub expected_slippage: u64,
    pub safety_buffer: u64,
    /// Platform fee we pay to Jupiter integrators (0 unless configured).
    pub platform_fee: u64,
    /// AMM LP fees are already inside the quoted output.
    pub swap_fees_embedded: bool,
}

impl CostBreakdown {
    /// Everything subtracted from gross PnL.
    pub fn total(&self) -> u64 {
        self.base_fee
            .saturating_add(self.priority_fee)
            .saturating_add(self.jito_tip)
            .saturating_add(self.ata_rent)
            .saturating_add(self.expected_slippage)
            .saturating_add(self.safety_buffer)
            .saturating_add(self.platform_fee)
    }

    /// Fees actually paid to the network/validators if the bundle lands.
    pub fn landing_cost(&self) -> u64 {
        self.base_fee.saturating_add(self.priority_fee).saturating_add(self.jito_tip)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostParams {
    /// Headroom over simulated CU (default 20%).
    pub cu_margin: Ppm,
    /// CU assumed per leg before simulation.
    pub est_cu_per_leg: u32,
    /// Hard cap on the CU price we will pay (Jupiter suggestions can spike).
    pub max_cu_price_micro: u64,
    /// Fraction of the final leg's slippage tolerance treated as expected loss.
    pub expected_slippage_share: Ppm,
    pub safety_buffer_lamports: u64,
    /// Additional buffer proportional to trade input.
    pub safety_buffer: Ppm,
}

impl Default for CostParams {
    fn default() -> Self {
        Self {
            cu_margin: Ppm(200_000),
            est_cu_per_leg: 300_000,
            max_cu_price_micro: 200_000,
            expected_slippage_share: Ppm(250_000),
            safety_buffer_lamports: 5_000,
            safety_buffer: Ppm::from_bps(1),
        }
    }
}

/// How the cycle will be packaged into transactions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TxShape {
    pub tx_count: u8,
    pub signatures_per_tx: u8,
}

impl TxShape {
    pub const SINGLE: TxShape = TxShape { tx_count: 1, signatures_per_tx: 1 };

    pub fn bundle(legs: usize) -> TxShape {
        TxShape { tx_count: legs.max(1) as u8, signatures_per_tx: 1 }
    }
}

pub struct CostInputs<'a> {
    pub legs: &'a [Leg],
    pub input: u64,
    pub shape: TxShape,
    /// Per-transaction simulated CU consumption; `None` → estimate.
    pub cu_used: Option<&'a [u32]>,
    /// CU price the transactions will carry (before clamping).
    pub cu_price_micro: u64,
    pub jito_tip: u64,
    pub atas_to_create: u8,
    pub platform_fee: u64,
}

pub fn compute_costs(inp: &CostInputs<'_>, p: &CostParams) -> CostBreakdown {
    let tx_count = inp.shape.tx_count.max(1);
    let signatures = tx_count.saturating_mul(inp.shape.signatures_per_tx.max(1));
    let cu_price = inp.cu_price_micro.min(p.max_cu_price_micro);

    // Per-transaction CU limits.
    let (limits, used, source): (Vec<u32>, Option<u32>, CostSource) = match inp.cu_used {
        Some(used) if !used.is_empty() => (
            used.iter().map(|u| cu_limit_with_margin(*u, p.cu_margin)).collect(),
            Some(used.iter().copied().fold(0u32, u32::saturating_add)),
            CostSource::Simulated,
        ),
        _ => {
            let legs = inp.legs.len().max(1) as u32;
            let total_est = p.est_cu_per_leg.saturating_mul(legs);
            let per_tx = (total_est / tx_count as u32).min(MAX_COMPUTE_UNITS_PER_TX);
            (vec![per_tx; tx_count as usize], None, CostSource::Quote)
        }
    };
    let priority_fee = limits.iter().map(|l| priority_fee_lamports(*l, cu_price)).fold(0u64, u64::saturating_add);
    let compute_units_limit = limits.iter().copied().fold(0u32, u32::saturating_add);

    CostBreakdown {
        source,
        tx_count,
        signatures,
        base_fee: BASE_FEE_LAMPORTS_PER_SIGNATURE * signatures as u64,
        compute_units_limit,
        compute_units_used: used,
        compute_unit_price_micro: cu_price,
        priority_fee,
        jito_tip: inp.jito_tip,
        ata_rent: TOKEN_ACCOUNT_RENT_LAMPORTS * inp.atas_to_create as u64,
        atas_created: inp.atas_to_create,
        expected_slippage: expected_slippage(inp.legs, p.expected_slippage_share),
        safety_buffer: safety_buffer(inp.input, p),
        platform_fee: inp.platform_fee,
        swap_fees_embedded: true,
    }
}

/// `share × (out − min_out)` of the final leg (whose output is the base asset).
/// Intermediate legs have fixed downstream inputs, so a shortfall there makes
/// the next swap fail (atomic revert) rather than realizing a loss.
pub fn expected_slippage(legs: &[Leg], share: Ppm) -> u64 {
    let Some(last) = legs.last() else { return 0 };
    let tol = last.out_amount.saturating_sub(last.min_out) as u128;
    let share = share.0.clamp(0, PPM_ONE) as u128;
    (tol * share).div_ceil(PPM_ONE as u128) as u64
}

pub fn safety_buffer(input: u64, p: &CostParams) -> u64 {
    let prop = (input as u128 * p.safety_buffer.0.max(0) as u128).div_ceil(PPM_ONE as u128) as u64;
    p.safety_buffer_lamports.saturating_add(prop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Address;
    use crate::model::{DexFilter, RoutingMode, SlippageSpec};
    use crate::time::Ts;

    pub fn leg(in_amount: u64, out_amount: u64, min_out: u64) -> Leg {
        Leg {
            index: 0,
            input_mint: Address([1; 32]),
            output_mint: Address([2; 32]),
            in_amount,
            out_amount,
            min_out,
            slippage_bps: 0,
            slippage_spec: SlippageSpec::Rtse,
            price_impact: Ppm::ZERO,
            hops: vec![],
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            quoted_at: Ts(0),
            latency_ms: 0,
            cu_price_micro: Some(2_719),
            last_valid_block_height: 0,
            request_id: None,
        }
    }

    #[test]
    fn quote_stage_costs() {
        let legs = [leg(1_000_000_000, 105_000_000, 104_800_000), leg(105_000_000, 1_001_000_000, 999_500_000)];
        let p = CostParams::default();
        let c = compute_costs(
            &CostInputs {
                legs: &legs,
                input: 1_000_000_000,
                shape: TxShape::SINGLE,
                cu_used: None,
                cu_price_micro: 2_719,
                jito_tip: 10_000,
                atas_to_create: 0,
                platform_fee: 0,
            },
            &p,
        );
        assert_eq!(c.source, CostSource::Quote);
        assert_eq!(c.base_fee, 5_000);
        assert_eq!(c.compute_units_limit, 600_000);
        assert_eq!(c.priority_fee, 1_632); // ceil(600000*2719/1e6)=1631.4
        assert_eq!(c.expected_slippage, 375_000); // 25% of 1.5M tolerance
        assert_eq!(c.safety_buffer, 5_000 + 100_000); // 1bp of 1 SOL
        assert_eq!(c.total(), 5_000 + 1_632 + 10_000 + 375_000 + 105_000);
        assert_eq!(c.landing_cost(), 5_000 + 1_632 + 10_000);
    }

    #[test]
    fn simulated_costs_use_actual_cu_and_bundle_shape() {
        let legs = [leg(10, 10, 10), leg(10, 10, 10)];
        let p = CostParams::default();
        let used = [180_000u32, 95_000];
        let c = compute_costs(
            &CostInputs {
                legs: &legs,
                input: 0,
                shape: TxShape::bundle(2),
                cu_used: Some(&used),
                cu_price_micro: 10_000_000, // clamped to 200_000
                jito_tip: 0,
                atas_to_create: 1,
                platform_fee: 0,
            },
            &p,
        );
        assert_eq!(c.source, CostSource::Simulated);
        assert_eq!(c.base_fee, 10_000);
        assert_eq!(c.compute_units_used, Some(275_000));
        assert_eq!(c.compute_units_limit, 216_000 + 114_000);
        assert_eq!(c.compute_unit_price_micro, 200_000);
        assert_eq!(c.priority_fee, 43_200 + 22_800);
        assert_eq!(c.ata_rent, TOKEN_ACCOUNT_RENT_LAMPORTS);
    }
}
