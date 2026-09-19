//! Dynamic tip policies. PAPER uses the same policy to report "the tip we
//! would pay if we sent now".

use searcher_core::config::{TipPolicyConfig, TipPolicyKind};
use searcher_core::event::TipFloor;
use searcher_core::units::JITO_MIN_TIP_LAMPORTS;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TipQuote {
    pub lamports: u64,
    /// How the number was derived (shown in the inspector).
    pub basis: String,
    /// The policy wanted more than `max_lamports` (→ TIP_TOO_HIGH if it matters).
    pub capped: bool,
}

#[derive(Clone, Debug)]
pub struct TipPolicy {
    cfg: TipPolicyConfig,
}

impl TipPolicy {
    pub fn new(cfg: TipPolicyConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &TipPolicyConfig {
        &self.cfg
    }

    fn pick(floor: &TipFloor, which: &str) -> u64 {
        match which {
            "p25" => floor.p25,
            "p75" => floor.p75,
            "p95" => floor.p95,
            "p99" => floor.p99,
            "ema50" => floor.ema_p50,
            _ => floor.p50,
        }
    }

    /// `profit_before_tip`: expected net PnL excluding the tip (lamports).
    pub fn quote(&self, floor: Option<&TipFloor>, profit_before_tip: i64) -> TipQuote {
        let c = &self.cfg;
        let (raw, basis) = match c.kind {
            TipPolicyKind::Fixed => (c.fixed_lamports, format!("fixed {}", c.fixed_lamports)),
            TipPolicyKind::Percentile => match floor {
                Some(f) => {
                    let v = Self::pick(f, &c.percentile);
                    (v, format!("landed tips {} = {v}", c.percentile))
                }
                None => (c.fixed_lamports, format!("no tip floor yet → fixed {}", c.fixed_lamports)),
            },
            TipPolicyKind::ProfitShare => {
                let share = (profit_before_tip.max(0) as u128 * c.profit_share_bps as u128 / 10_000) as u64;
                (share, format!("{}% of {} expected", c.profit_share_bps as f64 / 100.0, profit_before_tip.max(0)))
            }
        };
        let floor_min = c.min_lamports.max(JITO_MIN_TIP_LAMPORTS);
        let capped = raw > c.max_lamports;
        let lamports = raw.clamp(floor_min, c.max_lamports.max(floor_min));
        TipQuote { lamports, basis, capped }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::Ts;

    fn floor() -> TipFloor {
        TipFloor { ts: Ts(0), p25: 1_000, p50: 1_126, p75: 12_000, p95: 555_000, p99: 1_000_000, ema_p50: 2_300 }
    }

    fn cfg(kind: TipPolicyKind) -> TipPolicyConfig {
        TipPolicyConfig { kind, ..TipPolicyConfig::default() }
    }

    #[test]
    fn fixed() {
        let p = TipPolicy::new(TipPolicyConfig { fixed_lamports: 7_000, ..cfg(TipPolicyKind::Fixed) });
        assert_eq!(p.quote(None, 0).lamports, 7_000);
    }

    #[test]
    fn percentile_with_and_without_floor() {
        let mut c = cfg(TipPolicyKind::Percentile);
        c.percentile = "p75".into();
        let p = TipPolicy::new(c.clone());
        assert_eq!(p.quote(Some(&floor()), 0).lamports, 12_000);
        assert_eq!(p.quote(None, 0).lamports, c.fixed_lamports);
        c.percentile = "p99".into();
        let q = TipPolicy::new(c).quote(Some(&floor()), 0);
        assert_eq!(q.lamports, 200_000);
        assert!(q.capped);
    }

    #[test]
    fn profit_share_is_clamped_to_min_and_max() {
        let p = TipPolicy::new(cfg(TipPolicyKind::ProfitShare)); // 50%
        assert_eq!(p.quote(None, 40_000).lamports, 20_000);
        assert_eq!(p.quote(None, -5).lamports, 1_000, "never below Jito minimum");
        assert_eq!(p.quote(None, 10_000_000).lamports, 200_000);
    }
}
