//! Metrics that can be registered in the Graph Workspace. Every metric is a
//! time series on the shared session timeline.

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricId {
    /// SOL/USD, executable sell price observed on Jupiter `/build` (base → USDC).
    Price,
    /// Round-trip spread: sell vs. buy executable price (bps).
    Spread,
    GrossEdge,
    NetEdge,
    /// Cumulative session PnL (USD), simulated + realized.
    Pnl,
    Equity,
    JupiterLatency,
    RpcLatency,
    SimLatency,
    BundleLatency,
    ComputeUnits,
    JitoTip,
    Slippage,
    /// Pyth SOL/USD from the on-chain price account (real time, not executable).
    OraclePrice,
    /// Mean of the fresh on-chain pool mids (SOL/USDC, not executable).
    PoolMid,
    /// Spread between the highest and lowest on-chain pool mid (bps).
    PoolSpread,
    PriorityFee,
    NetworkTps,
}

impl MetricId {
    pub const ALL: [MetricId; 18] = [
        MetricId::Price,
        MetricId::Spread,
        MetricId::GrossEdge,
        MetricId::NetEdge,
        MetricId::Pnl,
        MetricId::Equity,
        MetricId::JupiterLatency,
        MetricId::RpcLatency,
        MetricId::SimLatency,
        MetricId::BundleLatency,
        MetricId::ComputeUnits,
        MetricId::JitoTip,
        MetricId::Slippage,
        MetricId::OraclePrice,
        MetricId::PoolMid,
        MetricId::PoolSpread,
        MetricId::PriorityFee,
        MetricId::NetworkTps,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MetricId::Price => "Price",
            MetricId::Spread => "Spread",
            MetricId::GrossEdge => "Gross Edge",
            MetricId::NetEdge => "Net Edge",
            MetricId::Pnl => "PnL",
            MetricId::Equity => "Equity",
            MetricId::JupiterLatency => "Jupiter latency",
            MetricId::RpcLatency => "RPC latency",
            MetricId::SimLatency => "Simulation latency",
            MetricId::BundleLatency => "Bundle landing latency",
            MetricId::ComputeUnits => "CU",
            MetricId::JitoTip => "Jito tip",
            MetricId::Slippage => "Slippage",
            MetricId::OraclePrice => "Oracle price",
            MetricId::PoolMid => "Pool mid",
            MetricId::PoolSpread => "Pool spread",
            MetricId::PriorityFee => "Priority fee",
            MetricId::NetworkTps => "Network TPS",
        }
    }

    pub fn unit(self) -> Unit {
        match self {
            MetricId::Price | MetricId::Pnl | MetricId::Equity | MetricId::OraclePrice | MetricId::PoolMid => Unit::Usd,
            MetricId::Spread | MetricId::GrossEdge | MetricId::NetEdge | MetricId::Slippage | MetricId::PoolSpread => {
                Unit::Bps
            }
            MetricId::JupiterLatency | MetricId::RpcLatency | MetricId::SimLatency | MetricId::BundleLatency => {
                Unit::Millis
            }
            MetricId::ComputeUnits | MetricId::PriorityFee | MetricId::NetworkTps => Unit::Count,
            MetricId::JitoTip => Unit::Lamports,
        }
    }

    /// Short description of where the numbers come from (shown under graphs).
    pub fn source(self) -> &'static str {
        match self {
            MetricId::Price => "Jupiter /swap/v2/build SOL→USDC executable price",
            MetricId::Spread => "sell vs buy executable price, same size",
            MetricId::GrossEdge => "(gross out − in) / in per evaluated cycle",
            MetricId::NetEdge => "expected net / in per evaluated cycle",
            MetricId::Pnl => "session PnL (paper fills are simulated)",
            MetricId::Equity => "wallet or paper equity × SOL price",
            MetricId::JupiterLatency => "HTTP round-trip per /build",
            MetricId::RpcLatency => "JSON-RPC round-trip",
            MetricId::SimLatency => "simulateTransaction round-trip",
            MetricId::BundleLatency => "sendBundle → landed",
            MetricId::ComputeUnits => "simulated units consumed",
            MetricId::JitoTip => "tip policy: tip if sent now",
            MetricId::Slippage => "slippage tolerance applied (RTSE/fixed)",
            MetricId::OraclePrice => "Pyth SOL/USD on-chain price account · reference, not executable",
            MetricId::PoolMid => {
                "SOL/USDC mean on-chain pool mid (Whirlpool · Raydium CLMM · Meteora DLMM) · not executable"
            }
            MetricId::PoolSpread => "max − min on-chain pool mid across DEXes · not executable",
            MetricId::PriorityFee => "µlamports/CU p75 · recent slots touching the watched pools",
            MetricId::NetworkTps => "Solana transactions/s · getRecentPerformanceSamples",
        }
    }

    pub fn key(self) -> char {
        match self {
            MetricId::Price => 'p',
            MetricId::Spread => 's',
            MetricId::GrossEdge => 'g',
            MetricId::NetEdge => 'n',
            MetricId::Pnl => '$',
            MetricId::Equity => 'e',
            MetricId::JupiterLatency => 'j',
            MetricId::RpcLatency => 'r',
            MetricId::SimLatency => 'm',
            MetricId::BundleLatency => 'l',
            MetricId::ComputeUnits => 'c',
            MetricId::JitoTip => 't',
            MetricId::Slippage => 'w',
            MetricId::OraclePrice => 'o',
            MetricId::PoolMid => 'i',
            MetricId::PoolSpread => 'v',
            MetricId::PriorityFee => 'f',
            MetricId::NetworkTps => 'x',
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    Usd,
    Bps,
    Millis,
    Count,
    Lamports,
}

impl Unit {
    pub fn format(self, v: f64) -> String {
        match self {
            Unit::Usd => {
                if v.abs() >= 1000.0 {
                    format!("{v:.1}")
                } else if v.abs() >= 1.0 {
                    format!("{v:.3}")
                } else {
                    format!("{v:.4}")
                }
            }
            Unit::Bps => format!("{v:.1}bp"),
            Unit::Millis => format!("{v:.0}ms"),
            Unit::Count => {
                if v.abs() >= 1e6 {
                    format!("{:.2}M", v / 1e6)
                } else if v.abs() >= 1e3 {
                    format!("{:.1}k", v / 1e3)
                } else {
                    format!("{v:.0}")
                }
            }
            Unit::Lamports => {
                if v.abs() >= 1e6 {
                    format!("{:.2}M◎", v / 1e6)
                } else {
                    format!("{v:.0}")
                }
            }
        }
    }

    /// Format a delta with explicit sign.
    pub fn format_delta(self, d: f64) -> String {
        let s = self.format(d.abs());
        if d > 0.0 {
            format!("+{s}")
        } else if d < 0.0 {
            format!("-{s}")
        } else {
            s
        }
    }
}
