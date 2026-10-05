//! The lab's rules and its paper account. A rule looks at the candles up to
//! the one that just closed and at the account, and says what to do at the
//! next price; the account does it, pays for it and remembers every trade.
//! Rules keep no state of their own, so an account saved after a bar is all
//! that is needed to go on later.

use super::model::Model;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bar {
    /// Start of the bar, ms since the epoch.
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// What a trade costs. The fixed part does not shrink with the trade, so a
/// small account pays more of it per dollar.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Costs {
    /// Lost to the route (pool fee, spread, impact), basis points of the trade, each side.
    pub route_bps: f64,
    /// Network fee, priority fee and tip of one transaction, lamports.
    pub fixed_fee_lamports: f64,
}

impl Default for Costs {
    /// As measured on mainnet at 0.1 SOL on 2026-10-04: a SOL → USDC → SOL
    /// round trip quoted 2.29 bp under its input at the median, and a
    /// transaction cost 6,366 lamports.
    fn default() -> Self {
        Self { route_bps: 1.15, fixed_fee_lamports: 6_366.0 }
    }
}

impl Costs {
    /// Cost in USD of trading `usd` at `price` (USD per SOL).
    pub fn of(&self, usd: f64, price: f64) -> f64 {
        usd * self.route_bps / 1e4 + self.fixed_fee_lamports / 1e9 * price
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "rule", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Rule {
    /// In for the bar after a bar that closed down, out after one that did not.
    SignReversal,
    /// In when the close is `k` deviations under its `window`-bar average; out
    /// when it is back at `exit_z` deviations, or `stop` under the buy price.
    Dip {
        window: usize,
        k: f64,
        #[serde(default)]
        exit_z: f64,
        #[serde(default)]
        stop: Option<f64>,
    },
    /// One of `lots` equal parts bought each time the close is `step` under
    /// the last trade; each part sold `step` above its own buy.
    Grid { step: f64, lots: usize },
    /// In on a close above the high of the `entry` bars before; out on a
    /// close under the low of the `exit` bars before.
    Breakout { entry: usize, exit: usize },
    /// A trained model (the file `scripts/direction_model.py train` writes):
    /// in when it gives a rise at least its threshold, out its horizon after
    /// the last time it did.
    Model { file: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Order {
    Buy { usd: f64 },
    Sell { lot: usize },
}

impl Rule {
    /// Bars a rule needs before its first decision.
    pub fn warmup(&self) -> usize {
        match self {
            Rule::SignReversal => 2,
            Rule::Dip { window, .. } => *window,
            Rule::Grid { .. } => 1,
            Rule::Breakout { entry, exit } => entry.max(exit) + 1,
            Rule::Model { .. } => super::model::HISTORY,
        }
    }

    /// The prices this rule acts at next, as far as it has such prices: the
    /// close under which it buys, the close above which it sells, and the
    /// close under which it sells at a loss. Read from the same bars and
    /// account as [`Rule::decide`], and in step with it.
    pub fn levels(&self, bars: &[Bar], acct: &Account) -> (Option<f64>, Option<f64>, Option<f64>) {
        match self {
            Rule::Dip { window, k, exit_z, stop } if bars.len() >= *window => {
                let w = &bars[bars.len() - window..];
                let mean = w.iter().map(|b| b.close).sum::<f64>() / *window as f64;
                let sd = (w.iter().map(|b| (b.close - mean).powi(2)).sum::<f64>() / *window as f64).sqrt();
                let stop = acct.lots.first().zip(*stop).map(|(lot, s)| lot.price * (1.0 - s));
                (Some(mean - k * sd), Some(mean + exit_z * sd), stop)
            }
            Rule::Grid { step, lots } => {
                let buy = acct.reference.filter(|_| acct.lots.len() < *lots).map(|r| r * (1.0 - step));
                let sell = acct.lots.iter().map(|l| l.price * (1.0 + step)).min_by(f64::total_cmp);
                (buy, sell, None)
            }
            _ => (None, None, None),
        }
    }

    /// The numbers a dip rule measures the close against (the average and
    /// deviation of its window) with its settings: window, average, deviation,
    /// k, exit_z, stop. The same arithmetic as [`Rule::levels`].
    pub fn dip(&self, bars: &[Bar]) -> Option<(usize, f64, f64, f64, f64, Option<f64>)> {
        let Rule::Dip { window, k, exit_z, stop } = self else { return None };
        if bars.len() < *window {
            return None;
        }
        let w = &bars[bars.len() - window..];
        let mean = w.iter().map(|b| b.close).sum::<f64>() / *window as f64;
        let sd = (w.iter().map(|b| (b.close - mean).powi(2)).sum::<f64>() / *window as f64).sqrt();
        Some((*window, mean, sd, *k, *exit_z, *stop))
    }

    /// What the rule does, in a sentence (`zh`: in Chinese). `bar_ms` says how long its windows are.
    pub fn describe(&self, bar_ms: i64, zh: bool) -> String {
        let pct = |x: f64| format!("{}{}%", (x * 10_000.0).round() / 100.0, if zh { "" } else { " " });
        let span = |bars: usize| {
            let h = bars as f64 * bar_ms as f64 / 3_600_000.0;
            let round = |x: f64| (x * 10.0).round() / 10.0;
            match (h >= 24.0, zh) {
                (true, false) => format!("{} d", round(h / 24.0)),
                (false, false) => format!("{} h", round(h)),
                (true, true) => format!("约 {} 天", round(h / 24.0)),
                (false, true) => format!("约 {} 小时", round(h)),
            }
        };
        match (self, zh) {
            (Rule::SignReversal, false) => "Buys after a bar that closed down; sells after one that did not.".into(),
            (Rule::SignReversal, true) => "一根线收跌之后买入；一根线没有收跌之后卖出。".into(),
            (Rule::Dip { window, k, exit_z, stop }, false) => format!(
                "Buys when a bar closes {k} deviation{} under its average of {window} bars ({}); sells {}{}.",
                if *k == 1.0 { "" } else { "s" },
                span(*window),
                if *exit_z == 0.0 { "back at the average".to_string() } else { format!("{exit_z} deviations from it") },
                stop.map_or(String::new(), |s| format!(", or {} under the buy", pct(s)))
            ),
            (Rule::Dip { window, k, exit_z, stop }, true) => format!(
                "收盘价低于最近 {window} 根线（{}）平均价 {k} 个波动幅度时买入；{}卖出{}。",
                span(*window),
                if *exit_z == 0.0 { "涨回平均价".to_string() } else { format!("离平均价 {exit_z} 个波动时") },
                stop.map_or(String::new(), |s| format!("，或比买入价再跌 {} 止损", pct(s)))
            ),
            (Rule::Grid { step, lots }, false) => format!(
                "Buys one of {lots} equal parts each time the close is {} under the last trade; sells each part {} above its own buy. It never sells at a loss.",
                pct(*step),
                pct(*step)
            ),
            (Rule::Grid { step, lots }, true) => format!(
                "把预算分成 {lots} 份：收盘价比上一次成交低 {} 就买一份；每一份比自己的买入价高 {} 就卖出。从不亏本卖。",
                pct(*step),
                pct(*step)
            ),
            (Rule::Breakout { entry, exit }, false) => format!(
                "Buys on a close above the high of the {entry} bars before ({}); sells on a close under the low of the {exit} before.",
                span(*entry)
            ),
            (Rule::Breakout { entry, exit }, true) => format!(
                "收盘价高于此前 {entry} 根线（{}）的最高价时买入；低于此前 {exit} 根线的最低价时卖出。",
                span(*entry)
            ),
            (Rule::Model { .. }, false) => {
                "A trained model: in when it gives a rise at least its threshold, out its horizon after the last time it did."
                    .into()
            }
            (Rule::Model { .. }, true) => "一个训练出来的模型：它判断上涨的把握够大时买入，最后一次这样判断之后过了它的持有期就卖出。".into(),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        let bad = |what: &str| Err(format!("{what} must be greater than zero"));
        match self {
            Rule::Dip { window, k, stop, .. } if *window < 2 || *k <= 0.0 || stop.is_some_and(|s| s <= 0.0) => {
                bad("window (at least 2), k and stop")
            }
            Rule::Grid { step, lots } if *step <= 0.0 || *lots == 0 => bad("step and lots"),
            Rule::Breakout { entry, exit } if *entry == 0 || *exit == 0 => bad("entry and exit"),
            _ => Ok(()),
        }
    }

    /// Orders for the next price, decided at the close of the last of `bars`.
    /// `capital` is what the experiment started with; `may_buy` is false
    /// while a stop is in force (selling goes on).
    pub fn decide(&self, bars: &[Bar], acct: &Account, capital: f64, may_buy: bool) -> Vec<Order> {
        let n = bars.len();
        if n < self.warmup() {
            return Vec::new();
        }
        let close = bars[n - 1].close;
        let all_in = |want: bool| -> Vec<Order> {
            match (want, acct.lots.is_empty()) {
                (true, true) if may_buy && acct.cash > 0.0 => vec![Order::Buy { usd: acct.cash }],
                (false, false) => (0..acct.lots.len()).map(|lot| Order::Sell { lot }).collect(),
                _ => Vec::new(),
            }
        };
        match self {
            Rule::SignReversal => all_in(close < bars[n - 2].close),
            Rule::Dip { window, k, exit_z, stop } => {
                let w = &bars[n - window..];
                let mean = w.iter().map(|b| b.close).sum::<f64>() / *window as f64;
                let sd = (w.iter().map(|b| (b.close - mean).powi(2)).sum::<f64>() / *window as f64).sqrt();
                let z = if sd > 0.0 { (close - mean) / sd } else { 0.0 };
                match acct.lots.first() {
                    None => all_in(z < -k),
                    Some(lot) => {
                        let stopped = stop.is_some_and(|s| close < lot.price * (1.0 - s));
                        all_in(!(z > *exit_z || stopped))
                    }
                }
            }
            Rule::Grid { step, lots } => {
                let sells: Vec<Order> = acct
                    .lots
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| close >= l.price * (1.0 + step))
                    .map(|(lot, _)| Order::Sell { lot })
                    .collect();
                if !sells.is_empty() {
                    return sells;
                }
                let part = capital / *lots as f64;
                let under = acct.reference.is_some_and(|r| close <= r * (1.0 - step));
                if may_buy && under && acct.lots.len() < *lots && acct.cash >= part * 0.999 {
                    vec![Order::Buy { usd: part.min(acct.cash) }]
                } else {
                    Vec::new()
                }
            }
            Rule::Breakout { entry, exit } => {
                let before = &bars[..n - 1];
                let high = before[before.len() - entry..].iter().map(|b| b.high).fold(f64::MIN, f64::max);
                let low = before[before.len() - exit..].iter().map(|b| b.low).fold(f64::MAX, f64::min);
                if acct.lots.is_empty() { all_in(close > high) } else { all_in(close >= low) }
            }
            // decided in `step`, which has the model and the other coin's candles
            Rule::Model { .. } => Vec::new(),
        }
    }
}

/// What a step may look at besides the candles: the model of a model rule
/// and the other coin's closes, one for each bar.
#[derive(Clone, Copy, Default)]
pub struct Ctx<'a> {
    pub model: Option<&'a Model>,
    pub other: &'a [f64],
}

/// SOL bought at one time.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Lot {
    /// USD paid per SOL received, costs included.
    pub price: f64,
    pub sol: f64,
    pub usd: f64,
    pub opened: i64,
}

/// A lot bought and sold.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Trade {
    pub opened: i64,
    pub closed: i64,
    /// USD paid in.
    pub usd: f64,
    /// USD back out after every cost, less what was paid in.
    pub net: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Account {
    pub cash: f64,
    pub lots: Vec<Lot>,
    /// Close at the last trade (the grid steps from it); the first close seen before any.
    pub reference: Option<f64>,
    pub trades: Vec<Trade>,
    /// Every cost paid so far, USD.
    pub costs_paid: f64,
    /// USD bought and sold so far.
    pub turnover: f64,
    /// Equity at the start of the current UTC day, and that day (days since the epoch).
    pub day: Option<(i64, f64)>,
    /// The total-loss stop was hit: no new buying, for good.
    pub frozen: bool,
    /// A model rule holds until this bar (its start, ms).
    #[serde(default)]
    pub hold_until: Option<i64>,
}

/// What one order did, for the record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filled {
    pub buy: bool,
    pub usd: f64,
    pub sol: f64,
    pub cost_usd: f64,
}

impl Account {
    pub fn new(capital: f64) -> Self {
        Self { cash: capital, ..Self::default() }
    }

    pub fn sol(&self) -> f64 {
        // not `sum()`: of nothing it is -0.0, and prints as "-0.000000"
        self.lots.iter().fold(0.0, |held, l| held + l.sol)
    }

    /// Cash plus the SOL held, at `price`.
    pub fn equity(&self, price: f64) -> f64 {
        self.cash + self.sol() * price
    }

    /// Carries out `orders` (as [`Rule::decide`] gave them) at `buy_price` /
    /// `sell_price`; `signal_close` becomes the grid's new reference.
    pub fn fill(
        &mut self,
        orders: &[Order],
        buy_price: f64,
        sell_price: f64,
        ts: i64,
        signal_close: f64,
        costs: &Costs,
    ) -> Vec<Filled> {
        let mut done = Vec::new();
        // sells first, highest index first, so the indices stay those the rule meant
        let mut sells: Vec<usize> = orders
            .iter()
            .filter_map(|o| match o {
                Order::Sell { lot } => Some(*lot),
                _ => None,
            })
            .collect();
        sells.sort_unstable_by(|a, b| b.cmp(a));
        for i in sells {
            let lot = self.lots.remove(i);
            let gross = lot.sol * sell_price;
            let cost = costs.of(gross, sell_price);
            self.cash += gross - cost;
            self.costs_paid += cost;
            self.turnover += gross;
            self.trades.push(Trade { opened: lot.opened, closed: ts, usd: lot.usd, net: gross - cost - lot.usd });
            done.push(Filled { buy: false, usd: gross, sol: lot.sol, cost_usd: cost });
        }
        for o in orders {
            let Order::Buy { usd } = *o else { continue };
            let usd = usd.min(self.cash);
            let cost = costs.of(usd, buy_price);
            if usd <= cost {
                continue; // the fee alone is more than the order
            }
            let sol = (usd - cost) / buy_price;
            self.cash -= usd;
            self.costs_paid += cost;
            self.turnover += usd;
            self.lots.push(Lot { price: usd / sol, sol, usd, opened: ts });
            done.push(Filled { buy: true, usd, sol, cost_usd: cost });
        }
        if !done.is_empty() {
            self.reference = Some(signal_close);
        }
        done
    }

    /// A buy as the chain settled it: `usd` left and `sol` arrived, every cost inside.
    pub fn bought(&mut self, usd: f64, sol: f64, ts: i64, signal_close: f64) {
        self.cash = (self.cash - usd).max(0.0);
        self.turnover += usd;
        self.lots.push(Lot { price: usd / sol, sol, usd, opened: ts });
        self.reference = Some(signal_close);
    }

    /// The sale of lot `i` as the chain settled it: `usd` arrived, every cost inside.
    pub fn sold(&mut self, i: usize, usd: f64, ts: i64, signal_close: f64) {
        let lot = self.lots.remove(i);
        self.cash += usd;
        self.turnover += usd;
        self.trades.push(Trade { opened: lot.opened, closed: ts, usd: lot.usd, net: usd - lot.usd });
        self.reference = Some(signal_close);
    }
}

/// Stops of an experiment, as shares of its capital. `None`: no stop.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Stops {
    /// Down this much since the UTC day began: nothing is bought until the next day.
    pub daily_loss: Option<f64>,
    /// Down this much since the start: nothing is bought again.
    pub total_loss: Option<f64>,
}

/// One closed bar of an experiment: decide on it, fill at the prices given,
/// mark the account at the bar's close. Returns the fills and the equity.
pub fn step(
    rule: &Rule,
    stops: &Stops,
    costs: &Costs,
    capital: f64,
    acct: &mut Account,
    (bars, ctx): (&[Bar], Ctx),
    (buy_price, sell_price, ts): (f64, f64, i64),
) -> (Vec<Filled>, f64) {
    let Some(bar) = bars.last() else { return (Vec::new(), acct.equity(0.0)) };
    let (orders, equity) = orders(rule, stops, capital, acct, (bars, ctx));
    let fills = acct.fill(&orders, buy_price, sell_price, ts, bar.close, costs);
    (fills, equity)
}

/// What the rule wants at the close of the last of `bars`, and the equity
/// there. Keeps the account's day and stops; fills nothing.
pub fn orders(
    rule: &Rule,
    stops: &Stops,
    capital: f64,
    acct: &mut Account,
    (bars, ctx): (&[Bar], Ctx),
) -> (Vec<Order>, f64) {
    let Some(bar) = bars.last() else { return (Vec::new(), acct.equity(0.0)) };
    acct.reference.get_or_insert(bar.close);
    let equity = acct.equity(bar.close);
    let today = bar.ts.div_euclid(86_400_000);
    if acct.day.is_none_or(|(d, _)| d != today) {
        acct.day = Some((today, equity));
    }
    if stops.total_loss.is_some_and(|s| equity <= capital * (1.0 - s)) {
        acct.frozen = true;
    }
    let day_start = acct.day.map_or(equity, |(_, e)| e);
    let paused = stops.daily_loss.is_some_and(|s| equity <= day_start - capital * s);
    let may_buy = !acct.frozen && !paused;
    let orders = match (rule, ctx.model) {
        (Rule::Model { .. }, Some(model)) => {
            let on = model.signal(bars, ctx.other);
            if on && bars.len() >= 2 {
                let bar_ms = bar.ts - bars[bars.len() - 2].ts;
                acct.hold_until = Some(bar.ts + model.horizon_bars as i64 * bar_ms);
            }
            if acct.lots.is_empty() {
                if on && may_buy && acct.cash > 0.0 { vec![Order::Buy { usd: acct.cash }] } else { Vec::new() }
            } else if acct.hold_until.is_none_or(|t| bar.ts >= t) {
                (0..acct.lots.len()).map(|lot| Order::Sell { lot }).collect()
            } else {
                Vec::new()
            }
        }
        _ => rule.decide(bars, acct, capital, may_buy),
    };
    (orders, equity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_are_where_the_rule_decides() {
        // six bars around 100: the rule's own decision flips exactly at the level it names
        let rule = Rule::Dip { window: 6, k: 1.0, exit_z: 0.0, stop: Some(0.05) };
        let mut closes = vec![100.0, 101.0, 99.0, 100.5, 99.5, 100.0];
        let b = bars(&closes);
        let (buy, sell, stop) = rule.levels(&b, &Account::new(10.0));
        let (buy, sell) = (buy.unwrap(), sell.unwrap());
        assert!(buy < 100.0 && (sell - 100.0).abs() < 1e-9 && stop.is_none(), "{buy} {sell}");
        // a last close just under the level of its own window buys; just above does not
        for (last, buys) in [(98.0, true), (99.9, false)] {
            *closes.last_mut().unwrap() = last;
            let b = bars(&closes);
            let level = rule.levels(&b, &Account::new(10.0)).0.unwrap();
            let orders = rule.decide(&b, &Account::new(10.0), 10.0, true);
            assert_eq!(last < level, buys, "{last} against {level}");
            assert_eq!(!orders.is_empty(), buys, "{last}: {orders:?}");
        }
        // holding: the loss level is 5 % under what the lot cost
        let mut held = Account::new(10.0);
        held.bought(10.0, 0.1, 0, 100.0);
        assert_eq!(rule.levels(&b, &held).2, Some(95.0));
        assert_eq!(Rule::SignReversal.levels(&b, &held), (None, None, None));
        assert_eq!(
            Rule::Dip { window: 96, k: 1.0, exit_z: 0.0, stop: Some(0.05) }.describe(900_000, false),
            "Buys when a bar closes 1 deviation under its average of 96 bars (1 d); sells back at the average, or 5 % under the buy."
        );
        assert_eq!(
            Rule::Dip { window: 96, k: 1.0, exit_z: 0.0, stop: Some(0.05) }.describe(900_000, true),
            "收盘价低于最近 96 根线（约 1 天）平均价 1 个波动幅度时买入；涨回平均价卖出，或比买入价再跌 5% 止损。"
        );
        // the numbers it shows are the ones its levels come from
        let rule = Rule::Dip { window: 6, k: 1.0, exit_z: 0.0, stop: Some(0.05) };
        let (window, mean, sd, k, _, _) = rule.dip(&b).unwrap();
        assert_eq!((window, rule.levels(&b, &held).0), (6, Some(mean - k * sd)));
    }

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Bar {
                ts: i as i64 * 900_000,
                open: c,
                high: c * 1.001,
                low: c * 0.999,
                close: c,
                volume: 1.0,
            })
            .collect()
    }

    const FREE: Costs = Costs { route_bps: 0.0, fixed_fee_lamports: 0.0 };

    /// Runs `rule` over `closes`, filling each decision at the next close.
    fn run(rule: &Rule, closes: &[f64], costs: &Costs, stops: &Stops) -> Account {
        let b = bars(closes);
        let mut a = Account::new(100.0);
        for i in 0..b.len() - 1 {
            let next = b[i + 1].close;
            step(rule, stops, costs, 100.0, &mut a, (&b[..=i], Ctx::default()), (next, next, b[i + 1].ts));
        }
        a
    }

    #[test]
    fn costs_have_a_part_that_does_not_shrink_with_the_trade() {
        let c = Costs::default();
        // 12.17 USD at 121.72: 1.15 bp of the trade and 6,366 lamports
        let big = c.of(12.172, 121.72);
        assert!((big - (12.172 * 1.15e-4 + 6_366e-9 * 121.72)).abs() < 1e-12);
        let (bps_big, bps_small) = (big / 12.172 * 1e4, c.of(2.30, 121.72) / 2.30 * 1e4);
        assert!((bps_big - 1.79).abs() < 0.01, "{bps_big}");
        assert!((bps_small - 4.52).abs() < 0.01, "a 2.30 USD trade pays far more of itself: {bps_small}");
    }

    #[test]
    fn sign_reversal_buys_after_a_down_bar_and_sells_after_an_up_bar() {
        // down, down, up: bought at the bar after the first fall, kept through the second, sold after the rise
        let a = run(&Rule::SignReversal, &[100.0, 99.0, 98.0, 99.0, 99.5, 100.0], &FREE, &Stops::default());
        assert_eq!(a.trades.len(), 1);
        let t = &a.trades[0];
        // in at 98 (the price after the close of 99), out at 99.5 (the price after the close of 99)
        assert!((t.net - 100.0 * (99.5 / 98.0 - 1.0)).abs() < 1e-9, "{t:?}");
        assert!(a.lots.is_empty());
    }

    #[test]
    fn a_trade_pays_its_costs_on_both_sides() {
        let costs = Costs { route_bps: 10.0, fixed_fee_lamports: 0.0 };
        let a = run(&Rule::SignReversal, &[100.0, 99.0, 99.0, 99.0, 99.0], &costs, &Stops::default());
        // in and out at the same price: the loss is the two costs
        let t = &a.trades[0];
        assert!((t.net - (-100.0 * 0.001 - (100.0 - 0.1) * 0.001)).abs() < 1e-9, "{t:?}");
        assert!((a.costs_paid + t.net).abs() < 1e-9);
        assert!((a.turnover - (100.0 + 99.9)).abs() < 1e-9);
    }

    #[test]
    fn dip_waits_for_the_average_and_stops_out_below_its_buy() {
        let rule = Rule::Dip { window: 4, k: 1.0, exit_z: 0.0, stop: Some(0.05) };
        // flat, then a fall far under the 4-bar average: bought; back over the average: sold
        let a = run(&rule, &[100.0, 100.0, 100.0, 100.0, 96.0, 97.0, 101.0, 101.0], &FREE, &Stops::default());
        assert_eq!(a.trades.len(), 1);
        assert!(a.trades[0].net > 0.0);
        // the fall goes on: 5 % under the buy price it gives up
        let a = run(&rule, &[100.0, 100.0, 100.0, 100.0, 96.0, 97.0, 91.0, 90.0, 90.0], &FREE, &Stops::default());
        assert_eq!(a.trades.len(), 1);
        assert!(a.trades[0].net < -5.0, "{:?}", a.trades);
    }

    #[test]
    fn grid_buys_each_step_down_and_sells_each_lot_a_step_above_its_own_buy() {
        let rule = Rule::Grid { step: 0.01, lots: 4 };
        let a = run(&rule, &[100.0, 98.9, 97.8, 96.7, 97.9, 99.0, 100.2, 100.2], &FREE, &Stops::default());
        // three lots bought on the way down (each fills one bar after its signal), all sold on the way up
        assert_eq!(a.trades.len(), 3, "{a:?}");
        assert!(a.lots.is_empty());
        assert!(a.trades.iter().all(|t| t.net > 0.0));
        // lots are a quarter of the capital each
        assert!(a.trades.iter().all(|t| (t.usd - 25.0).abs() < 1e-9));
    }

    #[test]
    fn grid_stops_buying_when_its_lots_are_used_up() {
        let rule = Rule::Grid { step: 0.01, lots: 2 };
        let a = run(&rule, &[100.0, 98.9, 97.8, 96.7, 95.6, 94.5, 93.4], &FREE, &Stops::default());
        assert_eq!(a.lots.len(), 2);
        assert!(a.cash.abs() < 1e-9);
    }

    #[test]
    fn breakout_looks_only_at_the_bars_before_the_one_that_closed() {
        let rule = Rule::Breakout { entry: 3, exit: 2 };
        let b = bars(&[100.0, 100.0, 100.0, 101.0]);
        // 101 is above the three highs before it (100.1): a buy; its own high does not count
        let a = Account::new(100.0);
        assert_eq!(rule.decide(&b, &a, 100.0, true), vec![Order::Buy { usd: 100.0 }]);
        assert!(rule.decide(&bars(&[100.0, 100.0, 100.0, 100.05]), &a, 100.0, true).is_empty());
    }

    #[test]
    fn stops_keep_it_from_buying_but_not_from_selling() {
        let stops = Stops { daily_loss: None, total_loss: Some(0.02) };
        // bought at 98, the price falls to 90: down more than 2 % of capital, frozen
        let a = run(&Rule::SignReversal, &[100.0, 99.0, 98.0, 90.0, 91.0, 90.0, 89.0, 90.0], &FREE, &stops);
        assert!(a.frozen);
        assert_eq!(a.trades.len(), 1, "it sold what it held and bought nothing after: {a:?}");
        assert!(a.lots.is_empty());
        // a daily stop lifts on the next UTC day
        let day = Stops { daily_loss: Some(0.02), total_loss: None };
        let mut closes = vec![100.0, 99.0, 98.0, 90.0, 91.0];
        closes.extend(std::iter::repeat_n(90.0, 96)); // a day of 15-minute bars
        closes.extend([89.0, 88.0, 89.0, 89.0]);
        let a = run(&Rule::SignReversal, &closes, &FREE, &day);
        assert!(!a.frozen);
        assert!(a.trades.len() >= 2, "it trades again the next day: {}", a.trades.len());
    }

    #[test]
    fn an_account_survives_being_saved() {
        let a =
            run(&Rule::Grid { step: 0.01, lots: 4 }, &[100.0, 98.9, 97.8, 99.5], &Costs::default(), &Stops::default());
        let back: Account = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        // JSON keeps a float to its last digit or the one beside it: the same account for every purpose here
        assert_eq!(
            (a.lots.len(), a.trades.len(), a.reference, a.day, a.frozen),
            (2, 0, back.reference, back.day, back.frozen)
        );
        assert_eq!(back.lots.len(), 2);
        assert!((a.equity(98.0) - back.equity(98.0)).abs() < 1e-9 && (a.costs_paid - back.costs_paid).abs() < 1e-12);
    }

    #[test]
    fn rules_read_from_toml_and_refuse_what_they_do_not_know() {
        let r: Rule = toml::from_str("rule = \"dip\"\nwindow = 288\nk = 2.0\nstop = 0.05").unwrap();
        assert_eq!(r, Rule::Dip { window: 288, k: 2.0, exit_z: 0.0, stop: Some(0.05) });
        assert_eq!(toml::from_str::<Rule>("rule = \"sign-reversal\"").unwrap(), Rule::SignReversal);
        assert!(toml::from_str::<Rule>("rule = \"grid\"\nstep = 0.01\nlots = 10\nlevels = 3").is_err());
        assert!(Rule::Grid { step: 0.0, lots: 10 }.check().is_err());
    }
}
