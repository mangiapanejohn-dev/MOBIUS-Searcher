//! Smooth weighted round-robin across strategies (nginx algorithm): picks are
//! proportional to weight and evenly interleaved, so no strategy starves and
//! the request budget is spent in configured proportions.

use crate::plan::CandidatePlan;
use crate::strategies::Strategy;

pub struct Scheduler {
    entries: Vec<(Box<dyn Strategy>, i64)>,
}

impl Scheduler {
    pub fn new(strategies: Vec<Box<dyn Strategy>>) -> Self {
        Self { entries: strategies.into_iter().filter(|s| s.weight() > 0).map(|s| (s, 0)).collect() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All routes of all enabled strategies.
    pub fn universe(&self) -> Vec<CandidatePlan> {
        self.entries.iter().flat_map(|(s, _)| s.plans()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(s, _)| s.name()).collect()
    }

    pub fn next_plan(&mut self) -> Option<CandidatePlan> {
        if self.entries.is_empty() {
            return None;
        }
        let total: i64 = self.entries.iter().map(|(s, _)| s.weight() as i64).sum();
        for (s, cur) in self.entries.iter_mut() {
            *cur += s.weight() as i64;
        }
        let best = self.entries.iter().enumerate().max_by_key(|(i, (_, cur))| (*cur, -(*i as i64)))?.0;
        self.entries[best].1 -= total;
        self.entries[best].0.next_plan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::Address;
    use searcher_core::model::StrategyKind;

    struct Fixed(StrategyKind, u32);

    impl Strategy for Fixed {
        fn kind(&self) -> StrategyKind {
            self.0
        }
        fn name(&self) -> String {
            self.0.label().into()
        }
        fn weight(&self) -> u32 {
            self.1
        }
        fn plans(&self) -> Vec<CandidatePlan> {
            Vec::new()
        }
        fn next_plan(&mut self) -> Option<CandidatePlan> {
            Some(CandidatePlan {
                strategy: self.0,
                key: String::new(),
                label: String::new(),
                base_mint: Address::default(),
                amount: 1,
                legs: vec![],
            })
        }
    }

    #[test]
    fn proportional_and_interleaved() {
        let mut s = Scheduler::new(vec![
            Box::new(Fixed(StrategyKind::RoundTrip, 1)),
            Box::new(Fixed(StrategyKind::CrossDex, 3)),
            Box::new(Fixed(StrategyKind::Triangular, 1)),
            Box::new(Fixed(StrategyKind::Triangular, 0)),
        ]);
        assert_eq!(s.len(), 3, "zero weight strategies are disabled");
        let picks: Vec<_> = (0..500).map(|_| s.next_plan().unwrap().strategy).collect();
        let count = |k| picks.iter().filter(|p| **p == k).count();
        assert_eq!(count(StrategyKind::RoundTrip), 100);
        assert_eq!(count(StrategyKind::CrossDex), 300);
        assert_eq!(count(StrategyKind::Triangular), 100);
        // never more than 2 cross-dex picks in a row with weights 1/3/1
        let max_run = picks.chunk_by(|a, b| a == b).map(|c| c.len()).max().unwrap();
        assert!(max_run <= 2, "{max_run}");
    }
}
