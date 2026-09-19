//! Scheduler-independent latency probes: the same definitions measure the
//! round-robin baseline and the event-driven scheduler.
//!
//! * market change → usable quote: a pool's mid moves ≥ `CHANGE_BP` from its
//!   last reference (an *episode* starts at the first such move); the episode
//!   ends when a Jupiter quote that observes that pool and was **sent after**
//!   the move arrives.
//! * market change → decision: same, ending at the strategy decision.
//! * duplicate request: the same request key sent again while none of the
//!   pools it observes moved since the previous send.

use parking_lot::Mutex;
use searcher_core::Address;
use searcher_core::model::DexFilter;
use searcher_telemetry::LatencyBook;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A pool mid move smaller than this is not a market change.
pub const CHANGE_BP: f64 = 1.0;
const DUP_WINDOW: Duration = Duration::from_secs(30);

/// Which watched pools a quote reflects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observes {
    /// Dex-constrained quote on a watched pair.
    Dexes(Vec<Arc<str>>),
    /// Best route on the watched pair: reflects every watched pool.
    AllPools,
    /// Nothing we can observe on-chain (other pairs, undecodable venues).
    Unobservable,
}

#[derive(Default)]
struct Inner {
    reference_mid: HashMap<Arc<str>, f64>,
    last_change: HashMap<Arc<str>, Instant>,
    dirty_quote: HashMap<Arc<str>, Instant>,
    dirty_decision: HashMap<Arc<str>, Instant>,
    last_sent: HashMap<String, Instant>,
}

pub struct Probe {
    book: Arc<LatencyBook>,
    /// Watched pool venues (Jupiter DEX labels) and the pair they quote.
    dexes: HashSet<Arc<str>>,
    pair: (Address, Address),
    inner: Mutex<Inner>,
}

impl Probe {
    pub fn new(book: Arc<LatencyBook>, dexes: impl IntoIterator<Item = String>, pair: (Address, Address)) -> Self {
        Self {
            book,
            dexes: dexes.into_iter().map(|d| Arc::from(d.as_str())).collect(),
            pair,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn book(&self) -> &Arc<LatencyBook> {
        &self.book
    }

    /// What a quote for this leg reflects.
    pub fn observes(&self, input: &Address, output: &Address, filter: &DexFilter) -> Observes {
        let on_pair =
            (input, output) == (&self.pair.0, &self.pair.1) || (output, input) == (&self.pair.0, &self.pair.1);
        if !on_pair {
            return Observes::Unobservable;
        }
        match filter {
            DexFilter::Any => Observes::AllPools,
            DexFilter::Only(d) => {
                let v: Vec<Arc<str>> = d.iter().filter_map(|x| self.dexes.get(x.as_str()).cloned()).collect();
                if v.len() == d.len() { Observes::Dexes(v) } else { Observes::Unobservable }
            }
            DexFilter::Exclude(_) => Observes::Unobservable,
        }
    }

    fn set<'a>(&'a self, o: &'a Observes) -> Vec<Arc<str>> {
        match o {
            Observes::Dexes(d) => d.clone(),
            Observes::AllPools => self.dexes.iter().cloned().collect(),
            Observes::Unobservable => Vec::new(),
        }
    }

    /// A decoded pool mid arrived.
    pub fn on_pool(&self, dex: &Arc<str>, mid: f64, received: Instant) {
        if !self.dexes.contains(dex) {
            return;
        }
        let mut g = self.inner.lock();
        match g.reference_mid.get(dex) {
            None => {
                g.reference_mid.insert(dex.clone(), mid);
            }
            Some(r) if ((mid / r - 1.0).abs() * 10_000.0) >= CHANGE_BP => {
                g.reference_mid.insert(dex.clone(), mid);
                g.last_change.insert(dex.clone(), received);
                g.dirty_quote.entry(dex.clone()).or_insert(received);
                g.dirty_decision.entry(dex.clone()).or_insert(received);
                drop(g);
                self.book.count("market.changes", 1);
            }
            Some(_) => {}
        }
    }

    /// A quote request was sent (called with its send time).
    pub fn on_quote_sent(&self, key: &str, o: &Observes, sent: Instant) {
        let pools = self.set(o);
        let mut g = self.inner.lock();
        if let Some(prev) = g.last_sent.get(key).copied()
            && sent.saturating_duration_since(prev) < DUP_WINDOW
        {
            let moved = pools.iter().any(|d| g.last_change.get(d).is_some_and(|t| *t > prev));
            match (o, moved) {
                (Observes::Unobservable, _) => self.book.count("jupiter.repeat_unobservable", 1),
                (_, false) => self.book.count("jupiter.duplicate", 1),
                (_, true) => {}
            }
        }
        g.last_sent.insert(key.to_string(), sent);
    }

    /// A quote arrived: it serves market changes on its pools that happened
    /// before it was sent.
    pub fn on_quote_received(&self, o: &Observes, sent: Instant, received: Instant) {
        let pools = self.set(o);
        let mut g = self.inner.lock();
        for d in pools {
            if let Some(t0) = g.dirty_quote.get(&d).copied()
                && t0 <= sent
            {
                g.dirty_quote.remove(&d);
                self.book.duration_us("market.change_to_usable_quote_us", received.saturating_duration_since(t0));
            }
        }
    }

    /// A strategy decision on a route whose first quote was sent at `first_sent`.
    pub fn on_decision(&self, o: &Observes, first_sent: Instant, decided: Instant) {
        let pools = self.set(o);
        let mut g = self.inner.lock();
        for d in pools {
            if let Some(t0) = g.dirty_decision.get(&d).copied()
                && t0 <= first_sent
            {
                g.dirty_decision.remove(&d);
                self.book.duration_us("market.change_to_decision_us", decided.saturating_duration_since(t0));
            }
        }
    }

    /// Episodes still waiting at the end of a run (never served).
    pub fn finish(&self) {
        let g = self.inner.lock();
        self.book.count("market.unserved_quote_episodes", g.dirty_quote.len() as u64);
        self.book.count("market.unserved_decision_episodes", g.dirty_decision.len() as u64);
    }
}

/// Union of what several legs observe.
pub fn union(parts: &[Observes]) -> Observes {
    if parts.contains(&Observes::AllPools) {
        return Observes::AllPools;
    }
    let mut v: Vec<Arc<str>> = Vec::new();
    for p in parts {
        if let Observes::Dexes(d) = p {
            for x in d {
                if !v.contains(x) {
                    v.push(x.clone());
                }
            }
        }
    }
    if v.is_empty() { Observes::Unobservable } else { Observes::Dexes(v) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> (Probe, Arc<str>, Arc<str>) {
        let book = Arc::new(LatencyBook::default());
        let p = Probe::new(
            book,
            ["Whirlpool".to_string(), "Raydium CLMM".to_string()],
            (Address([1; 32]), Address([2; 32])),
        );
        (p, Arc::from("Whirlpool"), Arc::from("Raydium CLMM"))
    }

    #[test]
    fn episodes_end_at_the_first_quote_sent_after_the_change() {
        let (p, w, r) = probe();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        p.on_pool(&w, 100.0, ms(0)); // reference
        p.on_pool(&w, 100.005, ms(10)); // 0.5 bp: not a change
        p.on_pool(&w, 100.02, ms(100)); // 2 bp: episode starts at 100
        p.on_pool(&w, 100.05, ms(200)); // still the same episode
        let o = Observes::Dexes(vec![w.clone()]);
        // a quote sent before the change does not serve it
        p.on_quote_received(&o, ms(50), ms(400));
        // a quote on another pool does not either
        p.on_quote_received(&Observes::Dexes(vec![r.clone()]), ms(150), ms(420));
        p.on_quote_received(&o, ms(300), ms(900));
        let rep = p.book().report();
        let s = &rep.series["market.change_to_usable_quote_us"];
        assert_eq!((s.count, s.p50), (1, 800_000), "900 − 100 ms, one episode");
        // best route observes every pool
        assert_eq!(p.observes(&Address([1; 32]), &Address([2; 32]), &DexFilter::Any), Observes::AllPools);
        assert_eq!(
            p.observes(&Address([1; 32]), &Address([2; 32]), &DexFilter::Only(vec!["HumidiFi".into()])),
            Observes::Unobservable
        );
    }

    #[test]
    fn duplicates_are_repeats_without_market_movement() {
        let (p, w, _) = probe();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let o = Observes::Dexes(vec![w.clone()]);
        p.on_pool(&w, 100.0, ms(0));
        p.on_quote_sent("k", &o, ms(10));
        p.on_quote_sent("k", &o, ms(2_000)); // nothing moved → duplicate
        p.on_pool(&w, 100.1, ms(2_500)); // 10 bp move
        p.on_quote_sent("k", &o, ms(3_000)); // moved → not a duplicate
        p.on_quote_sent("u", &Observes::Unobservable, ms(3_000));
        p.on_quote_sent("u", &Observes::Unobservable, ms(3_500));
        assert_eq!(p.book().counter("jupiter.duplicate"), 1);
        assert_eq!(p.book().counter("jupiter.repeat_unobservable"), 1);
        assert_eq!(union(&[o.clone(), Observes::Unobservable]), o);
    }
}
