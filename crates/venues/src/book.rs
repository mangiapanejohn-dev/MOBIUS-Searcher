//! Order book snapshot and paper fills: what a market or IOC order of a given
//! size would have received against the book as it was, including fees. Used
//! in PAPER mode; it cannot see hidden liquidity or other traders racing us.

use crate::Side;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Level {
    pub px: f64,
    pub sz: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Book {
    /// Best (lowest) first.
    pub asks: Vec<Level>,
    /// Best (highest) first.
    pub bids: Vec<Level>,
    /// Exchange timestamp, ms.
    pub ts_ms: i64,
}

impl Book {
    pub fn mid(&self) -> Option<f64> {
        Some((self.asks.first()?.px + self.bids.first()?.px) / 2.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaperFill {
    /// Base size filled.
    pub filled: f64,
    pub avg_px: f64,
    /// Quote amount before fees.
    pub notional: f64,
    /// Fee in quote currency (positive = paid).
    pub fee: f64,
    /// Levels consumed (1 = top of book only).
    pub levels: usize,
}

/// Walk the opposite side of `book` for `size` base units, stopping at
/// `limit` (a buy never pays above it, a sell never receives below it).
pub fn fill_against_book(book: &Book, side: Side, size: f64, limit: Option<f64>, taker_fee_bps: u32) -> PaperFill {
    let levels = match side {
        Side::Buy => &book.asks,
        Side::Sell => &book.bids,
    };
    let within = |px: f64| match (side, limit) {
        (_, None) => true,
        (Side::Buy, Some(l)) => px <= l,
        (Side::Sell, Some(l)) => px >= l,
    };
    let (mut left, mut filled, mut notional, mut used) = (size.max(0.0), 0.0, 0.0, 0);
    for l in levels.iter().take_while(|l| within(l.px)) {
        if left <= 0.0 {
            break;
        }
        let take = left.min(l.sz);
        filled += take;
        notional += take * l.px;
        left -= take;
        used += 1;
    }
    PaperFill {
        filled,
        avg_px: if filled > 0.0 { notional / filled } else { 0.0 },
        notional,
        fee: notional * taker_fee_bps as f64 / 10_000.0,
        levels: used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Book {
        let l = |px, sz| Level { px, sz };
        Book {
            asks: vec![l(100.0, 1.0), l(101.0, 2.0), l(103.0, 5.0)],
            bids: vec![l(99.0, 1.0), l(98.0, 3.0)],
            ts_ms: 0,
        }
    }

    #[test]
    fn a_market_buy_walks_the_asks_and_pays_the_fee() {
        let f = fill_against_book(&book(), Side::Buy, 2.5, None, 10);
        assert_eq!((f.filled, f.levels), (2.5, 2));
        assert!((f.notional - (100.0 + 1.5 * 101.0)).abs() < 1e-9);
        assert!((f.avg_px - 251.5 / 2.5).abs() < 1e-9);
        assert!((f.fee - 0.2515).abs() < 1e-9, "10 bps of 251.5");
        assert_eq!(book().mid(), Some(99.5));
    }

    #[test]
    fn a_limit_stops_the_walk_and_a_thin_book_fills_partially() {
        let f = fill_against_book(&book(), Side::Buy, 10.0, Some(101.0), 0);
        assert_eq!(f.filled, 3.0, "103 is above the limit");
        let f = fill_against_book(&book(), Side::Sell, 10.0, None, 0);
        assert_eq!(f.filled, 4.0, "only 4 SOL of bids");
        let f = fill_against_book(&book(), Side::Sell, 1.0, Some(99.5), 0);
        assert_eq!(f.filled, 0.0, "best bid 99 is below the limit");
    }
}
