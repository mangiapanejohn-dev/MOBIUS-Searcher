//! Exchange venues next to the Solana stack: signed REST clients (OKX first),
//! order types, paper fills against the live order book, and the permit that
//! gates every order.
//!
//! Safety model: an order can only be sent with a [`TradePermit`]. A demo
//! permit (the venue's simulated-funds environment) needs `trading = true`;
//! a real-account permit also needs a sending mode (CONFIRM/LIVE) and
//! `execution.live_enabled = true`. A client refuses a permit of the other kind.

pub mod binance;
pub mod book;
pub mod evm;
pub mod evm_sign;
pub mod evm_trade;
pub mod okx;

pub use binance::{BinanceClient, BinanceCredentials, BinanceError};
pub use book::{Book, Level, PaperFill, fill_against_book};
pub use okx::{Credentials, OkxClient, OkxError};

use searcher_core::config::VenueConfig;
use searcher_core::model::Mode;
use searcher_core::units::{format_atoms, parse_decimal};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// Trading rules of one instrument. Steps are kept as the exchange's decimal
/// strings so order sizes and prices are formatted exactly on the grid.
#[derive(Clone, Debug, PartialEq)]
pub struct Instrument {
    pub inst_id: String,
    pub base: String,
    pub quote: String,
    pub tick_sz: String,
    pub lot_sz: String,
    pub min_sz: String,
    /// Smallest order value in quote currency (0 = none published).
    pub min_notional: f64,
    /// Open for trading.
    pub live: bool,
}

/// `atoms / 10^d` as a plain decimal without trailing zeros (`1`, `0.3`).
fn plain(atoms: u128, d: u8) -> String {
    let s = format_atoms(atoms as i128, d, d);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// Digits after the decimal point of a step such as `0.000001`.
fn decimals(step: &str) -> u8 {
    step.split_once('.').map_or(0, |(_, f)| f.trim_end_matches('0').len() as u8)
}

/// `0.00100000` → `0.001` (Binance pads steps with zeros).
fn trim_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

impl Instrument {
    /// `qty` rounded down to the lot size, or `None` below the minimum size.
    pub fn size(&self, qty: f64) -> Option<String> {
        let d = decimals(&self.lot_sz);
        let lot = parse_decimal(trim_zeros(&self.lot_sz), d).ok()?.max(1) as u128;
        let min = parse_decimal(trim_zeros(&self.min_sz), d).ok()? as u128;
        if !qty.is_finite() || qty <= 0.0 {
            return None;
        }
        // tolerance for binary floating point just below a lot boundary
        let atoms = (qty * 10f64.powi(d as i32) * (1.0 + 1e-12)).floor() as u128;
        let atoms = atoms / lot * lot;
        (atoms >= min && atoms > 0).then(|| plain(atoms, d))
    }

    /// Price on the tick grid, rounded against us: a buy limit up (we accept
    /// paying up to it), a sell limit down.
    pub fn price(&self, px: f64, side: Side) -> Option<String> {
        let d = decimals(&self.tick_sz);
        let tick = parse_decimal(trim_zeros(&self.tick_sz), d).ok()?.max(1) as f64;
        if !px.is_finite() || px <= 0.0 {
            return None;
        }
        let steps = px * 10f64.powi(d as i32) / tick;
        let steps = match side {
            Side::Buy => (steps * (1.0 - 1e-12)).ceil(),
            Side::Sell => (steps * (1.0 + 1e-12)).floor(),
        };
        (steps >= 1.0).then(|| plain((steps * tick) as u128, d))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OrderKind {
    Market,
    Limit,
    /// Immediate-or-cancel at a limit price: never rests on the book.
    Ioc,
    PostOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    pub inst_id: String,
    pub side: Side,
    pub kind: OrderKind,
    /// Base-currency size on the lot grid ([`Instrument::size`]).
    pub size: String,
    /// Required for every kind except `Market` ([`Instrument::price`]).
    pub price: Option<String>,
    /// Our id for idempotency and lookups (OKX: ≤ 32 alphanumerics).
    pub client_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderState {
    Live,
    PartiallyFilled,
    Filled,
    Canceled,
    Other(String),
}

impl OrderState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, OrderState::Filled | OrderState::Canceled)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderStatus {
    pub order_id: String,
    pub client_id: String,
    pub state: OrderState,
    /// Filled base size.
    pub filled: f64,
    pub avg_px: f64,
    /// Fee as reported (negative = paid).
    pub fee: f64,
    pub fee_ccy: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Balance {
    pub ccy: String,
    pub available: f64,
    pub total: f64,
}

/// What a read-only account check found (`--doctor`).
#[derive(Clone, Debug, PartialEq)]
pub struct AccountProbe {
    pub demo: bool,
    /// Venue clock − ours, ms (signed requests fail beyond a few seconds).
    pub offset_ms: i64,
    /// Non-zero balances, largest amount first.
    pub balances: Vec<Balance>,
}

/// Check a venue's credentials without trading (clock sync + a signed
/// balance read). `Ok(None)` when the venue has no credentials set or no
/// account (EVM chains: the address is public, nothing to prove).
pub async fn probe_account(v: &VenueConfig) -> Result<Option<AccountProbe>, String> {
    use searcher_core::config::VenueKind;
    match v.kind {
        VenueKind::Okx => okx::probe_account(v).await.map_err(|e| e.to_string()),
        VenueKind::Binance => binance::probe_account(v).await.map_err(|e| e.to_string()),
        VenueKind::Evm => Ok(None),
    }
}

/// Proof that orders may be sent to a venue. Only [`TradePermit::check`]
/// makes one.
#[derive(Debug)]
pub struct TradePermit {
    demo: bool,
}

impl TradePermit {
    pub fn check(mode: Mode, live_enabled: bool, venue: &VenueConfig) -> Result<TradePermit, String> {
        if !venue.trading {
            return Err("venue has trading = false".into());
        }
        if venue.demo {
            return Ok(TradePermit { demo: true });
        }
        if !mode.sends_transactions() {
            return Err(format!("real-account orders need CONFIRM or LIVE mode (now {})", mode.label()));
        }
        if !live_enabled {
            return Err("real-account orders need execution.live_enabled = true".into());
        }
        Ok(TradePermit { demo: false })
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sol_usdt() -> Instrument {
        Instrument {
            inst_id: "SOL-USDT".into(),
            base: "SOL".into(),
            quote: "USDT".into(),
            tick_sz: "0.01".into(),
            lot_sz: "0.000001".into(),
            min_sz: "0.01".into(),
            min_notional: 0.0,
            live: true,
        }
    }

    #[test]
    fn sizes_round_down_to_the_lot_and_respect_the_minimum() {
        let i = sol_usdt();
        assert_eq!(i.size(0.0123456789).as_deref(), Some("0.012345"));
        assert_eq!(i.size(1.0).as_deref(), Some("1"));
        assert_eq!(i.size(0.3).as_deref(), Some("0.3"), "no float artefacts: 0.3 stays 0.3");
        assert_eq!(i.size(0.0099999), None, "below minSz 0.01");
        assert_eq!(i.size(-1.0), None);
        assert_eq!(i.size(f64::NAN), None);
    }

    #[test]
    fn prices_round_against_us_on_the_tick_grid() {
        let i = sol_usdt();
        assert_eq!(i.price(111.372, Side::Buy).as_deref(), Some("111.38"));
        assert_eq!(i.price(111.378, Side::Sell).as_deref(), Some("111.37"));
        assert_eq!(i.price(111.37, Side::Buy).as_deref(), Some("111.37"), "on-grid prices stay");
        assert_eq!(i.price(111.37, Side::Sell).as_deref(), Some("111.37"));
        assert_eq!(i.price(0.0, Side::Buy), None);
    }

    #[test]
    fn a_real_account_permit_needs_every_gate() {
        let mut v = VenueConfig::okx();
        assert!(TradePermit::check(Mode::Live, true, &v).is_err(), "trading = false");
        v.trading = true;
        let p = TradePermit::check(Mode::Paper, false, &v).unwrap();
        assert!(p.is_demo(), "demo needs nothing else");
        v.demo = false;
        assert!(TradePermit::check(Mode::Paper, true, &v).unwrap_err().contains("CONFIRM or LIVE"));
        assert!(TradePermit::check(Mode::Live, false, &v).unwrap_err().contains("live_enabled"));
        assert!(!TradePermit::check(Mode::Live, true, &v).unwrap().is_demo());
    }
}
