//! The shadow maker: what an order resting in a bin of a Meteora pool would
//! have met, worked out afterwards from the snapshots `--research-pools`
//! keeps (the pool's active bin and the exchange price, once a second).
//!
//! In a bin pool, liquidity in one bin is a limit order: base token in a bin
//! above the price is sold when the price goes up through it, quote token in
//! a bin below buys on the way down, and the swap's fee is paid to it. The
//! question is the usual one for a maker: after a fill, was the exchange
//! price on the right side of it? And does it depend on whether the pool was
//! below or above the exchange when the order went in? Nothing is placed.

use searcher_storage::research::PoolSnap;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;

/// A pair of imagined orders goes in this often …
const PLACE_EVERY_S: f64 = 5.0;
/// … and one not filled within this long is withdrawn.
const HORIZON_S: f64 = 60.0;
/// How many bins from the active one.
const DISTANCES: [i32; 3] = [1, 2, 5];
/// Seconds after a fill at which the exchange price is read.
pub const MARKOUTS_S: [f64; 4] = [0.0, 5.0, 15.0, 30.0];
/// The pool counts as below or above the exchange beyond this many bp.
const GAP_BPS: f64 = 2.0;
/// An exchange price that comes this much later than wanted is not used.
const LATE_S: f64 = 5.0;

#[derive(Serialize, Default, Debug, Clone, PartialEq)]
pub struct MakerRow {
    /// `sell` (base token above the price) or `buy` (quote token below it).
    pub side: &'static str,
    pub bins: i32,
    /// `always`, or where the pool's price was against the exchange when the order went in.
    pub when: &'static str,
    pub placed: usize,
    pub filled: usize,
    /// Mean result of the fills against the exchange mid after each of
    /// [`MARKOUTS_S`], in bp of the price; positive = the maker's fill
    /// (with the fee it earned) was better than the exchange price then.
    pub markout_bps: [Option<f64>; 4],
    /// Standard error of the 5-second figure, as if fills were independent.
    pub se_5s: Option<f64>,
}

#[derive(Serialize, Default, Debug)]
pub struct MakerReport {
    pub snapshots: usize,
    /// Snapshots with a fresh exchange price.
    pub with_exchange: usize,
    pub hours: f64,
    pub rows: Vec<MakerRow>,
}

#[derive(Default)]
struct Acc {
    placed: usize,
    filled: usize,
    sum: [f64; 4],
    n: [usize; 4],
    sq_5s: f64,
}

/// The imagined orders of one pool's snapshots (one run, in time order).
fn orders(v: &[PoolSnap], acc: &mut BTreeMap<(usize, i32, usize), Acc>) {
    let secs = |a: &PoolSnap, b: &PoolSnap| (b.ts - a.ts) as f64 / 1e6;
    let mut last_placed: Option<usize> = None;
    for i in 0..v.len() {
        let Some(mid) = v[i].cex_mid else { continue };
        if last_placed.is_some_and(|l| secs(&v[l], &v[i]) < PLACE_EVERY_S) {
            continue;
        }
        last_placed = Some(i);
        let gap = (v[i].price - mid) / mid * 1e4;
        let when = if gap < -GAP_BPS {
            1
        } else if gap > GAP_BPS {
            3
        } else {
            2
        };
        let step = 1.0 + v[i].bin_step as f64 / 1e4;
        for (side, sign) in [(0usize, 1i32), (1, -1)] {
            for d in DISTANCES {
                let bin = v[i].active_id + sign * d;
                let price = v[i].price * step.powi(sign * d);
                // filled when the price has gone through the whole bin
                let fill = (i + 1..v.len())
                    .take_while(|j| secs(&v[i], &v[*j]) <= HORIZON_S)
                    .find(|j| if sign > 0 { v[*j].active_id > bin } else { v[*j].active_id < bin });
                let mut marks = [None; 4];
                if let Some(j) = fill {
                    // the maker receives the fee on top of the bin's price
                    let got = price * (1.0 + sign as f64 * v[j - 1].lp_fee_bps / 1e4);
                    for (m, after) in MARKOUTS_S.iter().enumerate() {
                        let at = (j..v.len()).find(|k| secs(&v[j], &v[*k]) >= *after);
                        let later = at.filter(|k| secs(&v[j], &v[*k]) <= after + LATE_S).and_then(|k| v[k].cex_mid);
                        marks[m] = later.map(|mid| sign as f64 * (got - mid) / mid * 1e4);
                    }
                }
                for bucket in [0, when] {
                    let a = acc.entry((side, d, bucket)).or_default();
                    a.placed += 1;
                    a.filled += fill.is_some() as usize;
                    for (m, mark) in marks.iter().enumerate() {
                        if let Some(bp) = mark {
                            a.sum[m] += bp;
                            a.n[m] += 1;
                            if m == 1 {
                                a.sq_5s += bp * bp;
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn build(snaps: &[PoolSnap]) -> MakerReport {
    let mut r = MakerReport { snapshots: snaps.len(), ..Default::default() };
    r.with_exchange = snaps.iter().filter(|s| s.cex_mid.is_some()).count();
    let mut acc = BTreeMap::new();
    // rows arrive by run, pool and time: one series at a time
    for series in snaps.chunk_by(|a, b| a.run == b.run && a.pool == b.pool) {
        r.hours += (series[series.len() - 1].ts - series[0].ts) as f64 / 3.6e9;
        orders(series, &mut acc);
    }
    const SIDES: [&str; 2] = ["sell", "buy"];
    const WHEN: [&str; 4] = ["always", "pool below the exchange", "in line with it", "pool above the exchange"];
    for ((side, bins, when), a) in acc {
        let mean = |m: usize| (a.n[m] > 0).then(|| a.sum[m] / a.n[m] as f64);
        let n = a.n[1] as f64;
        let se_5s = (a.n[1] > 1).then(|| ((a.sq_5s / n - (a.sum[1] / n).powi(2)).max(0.0) / (n - 1.0)).sqrt());
        r.rows.push(MakerRow {
            side: SIDES[side],
            bins,
            when: WHEN[when],
            placed: a.placed,
            filled: a.filled,
            markout_bps: [mean(0), mean(1), mean(2), mean(3)],
            se_5s,
        });
    }
    r
}

pub fn render(r: &MakerReport) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "RESTING ORDERS IN THE BIN POOL (imagined) · {} snapshots over {:.1} h · exchange price at {} of them",
        r.snapshots, r.hours, r.with_exchange
    );
    let _ = writeln!(
        o,
        "  Every {PLACE_EVERY_S:.0} s two orders are imagined in the Meteora pool: base token to sell some bins above the"
    );
    let _ =
        writeln!(o, "  active bin, quote token to buy some bins below. One is filled when the price goes through its");
    let _ = writeln!(
        o,
        "  whole bin within {HORIZON_S:.0} s. Its result: the bin's price plus the fee the bin earns, against the exchange"
    );
    let _ = writeln!(o, "  mid 0, 5, 15 and 30 s after the fill (bp; positive = better than the exchange price then).");
    if r.rows.is_empty() {
        let _ = writeln!(o, "  No orders: no snapshot of a bin pool had an exchange price.");
        return o;
    }
    let _ = writeln!(
        o,
        "  {:<12} {:<24} {:>7} {:>7} {:>6}  {:>8} {:>8} {:>8} {:>8}  {:>8}",
        "order", "when it went in", "placed", "filled", "", "0 s", "5 s", "15 s", "30 s", "± at 5 s"
    );
    let f = |v: Option<f64>| v.map(|x| format!("{x:+.2}")).unwrap_or_else(|| "-".into());
    for row in &r.rows {
        let _ = writeln!(
            o,
            "  {:<12} {:<24} {:>7} {:>7} {:>5.1}%  {:>8} {:>8} {:>8} {:>8}  {:>8}",
            format!("{} {} bin{}", row.side, row.bins, if row.bins == 1 { "" } else { "s" }),
            row.when,
            row.placed,
            row.filled,
            row.filled as f64 / row.placed.max(1) as f64 * 100.0,
            f(row.markout_bps[0]),
            f(row.markout_bps[1]),
            f(row.markout_bps[2]),
            f(row.markout_bps[3]),
            row.se_5s.map(|s| format!("{s:.2}")).unwrap_or_else(|| "-".into())
        );
    }
    let _ =
        writeln!(o, "  Not counted: a partial fill in the bin where the price stopped; the two transactions that put");
    let _ =
        writeln!(o, "  the liquidity in and take it out; what it costs to turn the filled token back. Orders overlap");
    let _ = writeln!(o, "  in time, so fills are not independent and the ± is smaller than the real uncertainty.");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One snapshot a second: (active bin, exchange mid). The pool's price is
    /// 100 at bin 0 and 1 bp more per bin; liquidity earns 1 bp.
    fn series(points: &[(i32, f64)]) -> Vec<PoolSnap> {
        points
            .iter()
            .enumerate()
            .map(|(t, (active, mid))| PoolSnap {
                run: "r".into(),
                ts: t as i64 * 1_000_000,
                slot: t as u64,
                pool: "Meteora DLMM".into(),
                active_id: *active,
                bin_step: 1,
                price: 100.0 * 1.0001f64.powi(*active),
                lp_fee_bps: 1.0,
                cex_mid: Some(*mid),
                cex_src: Some("okx".into()),
            })
            .collect()
    }

    fn row<'a>(r: &'a MakerReport, side: &str, bins: i32, when: &str) -> &'a MakerRow {
        r.rows.iter().find(|x| x.side == side && x.bins == bins && x.when == when).unwrap()
    }

    #[test]
    fn a_sell_order_is_filled_when_the_price_goes_through_its_bin() {
        // the pool sits at bin 0, then jumps to bin 2 and stays; the exchange is at 100.02 throughout
        let mut points = vec![(0, 100.02); 3];
        points.extend(vec![(2, 100.02); 40]);
        let r = build(&series(&points));
        // orders go in at 0 s (pool at bin 0, 2 bp below the exchange) and every 5 s after
        let sell1 = row(&r, "sell", 1, "always");
        assert_eq!((sell1.placed, sell1.filled), (9, 1), "only the order placed before the jump is passed");
        // sold at bin 1 = 100.01 plus 1 bp fee = 100.020001 against 100.02: +0.0001 bp… about nothing
        let got = 100.0 * 1.0001 * 1.0001;
        let expect = (got - 100.02) / 100.02 * 1e4;
        for m in 0..4 {
            assert!((sell1.markout_bps[m].unwrap() - expect).abs() < 1e-9, "{:?}", sell1.markout_bps);
        }
        assert_eq!(sell1.se_5s, None, "one fill has no spread");
        // the price stopped in bin 2: an order there is touched, not passed
        assert_eq!(row(&r, "sell", 2, "always").filled, 0);
        // nothing went down: no buy order filled
        assert_eq!(row(&r, "buy", 1, "always").filled, 0);
        // the first order went in with the pool 2 bp below the exchange: not beyond the 2 bp line
        assert_eq!(row(&r, "sell", 1, "in line with it").filled, 1);
    }

    #[test]
    fn a_buy_order_filled_before_the_exchange_falls_further_loses() {
        // the pool drops from bin 0 to bin −3; the exchange, at 100 before, is at 99.90 afterwards
        let mut points = vec![(0, 100.0); 2];
        points.extend(vec![(-3, 99.90); 40]);
        let r = build(&series(&points));
        let buy = row(&r, "buy", 1, "always");
        assert_eq!(buy.filled, 1);
        // bought at bin −1 = 99.99, less the 1 bp fee earned = 99.980001; the exchange says 99.90: −8 bp
        let paid = 100.0 / 1.0001 * (1.0 - 1e-4);
        let expect = -(paid - 99.90) / 99.90 * 1e4;
        assert!((buy.markout_bps[1].unwrap() - expect).abs() < 1e-9 && expect < -7.9, "{expect}");
        assert_eq!(row(&r, "buy", 2, "always").filled, 1);
        assert_eq!(row(&r, "buy", 5, "always").filled, 0);
    }

    #[test]
    fn where_the_pool_stood_against_the_exchange_is_kept_apart() {
        // pool at 100, exchange at 100.05: the pool is 5 bp below
        let r = build(&series(&[(0, 100.05); 12]));
        assert_eq!(row(&r, "sell", 1, "pool below the exchange").placed, 3);
        assert!(r.rows.iter().all(|x| x.when != "pool above the exchange"));
        // without an exchange price no order is imagined
        let mut blind = series(&[(0, 100.0); 12]);
        blind.iter_mut().for_each(|s| s.cex_mid = None);
        let r = build(&blind);
        assert!(r.rows.is_empty() && r.with_exchange == 0);
        assert!(render(&r).contains("No orders"));
        assert!(render(&build(&series(&[(0, 100.0); 12]))).contains("sell 1 bin"));
    }
}
