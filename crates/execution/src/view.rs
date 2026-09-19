//! Shared runtime view used by the pipeline (not the UI): latest executable
//! SOL price, tip floor, tip accounts, ATA existence cache, paper ledger.

use parking_lot::RwLock;
use searcher_core::event::TipFloor;
use searcher_core::{Address, Ts, UsdPrice};
use std::collections::HashSet;

#[derive(Debug, Default)]
struct Inner {
    sol_price: Option<(UsdPrice, Ts, String)>,
    tip_floor: Option<TipFloor>,
    tip_accounts: Vec<Address>,
    existing_atas: HashSet<Address>,
    checked_atas: std::collections::HashMap<Address, Ts>,
    paper_net_lamports: i64,
    wallet_lamports: Option<(u64, Ts)>,
}

#[derive(Debug, Default)]
pub struct RuntimeView {
    inner: RwLock<Inner>,
}

impl RuntimeView {
    pub fn set_sol_price(&self, p: UsdPrice, ts: Ts, source: &str) {
        self.inner.write().sol_price = Some((p, ts, source.to_string()));
    }

    /// Latest SOL/USD if not older than `max_age_ms`.
    pub fn sol_price(&self, now: Ts, max_age_ms: u64) -> Option<UsdPrice> {
        self.inner.read().sol_price.as_ref().filter(|(_, t, _)| t.age_ms(now) <= max_age_ms).map(|(p, _, _)| *p)
    }

    pub fn set_tip_floor(&self, t: TipFloor) {
        self.inner.write().tip_floor = Some(t);
    }

    pub fn tip_floor(&self) -> Option<TipFloor> {
        self.inner.read().tip_floor.clone()
    }

    pub fn set_tip_accounts(&self, v: Vec<Address>) {
        if !v.is_empty() {
            self.inner.write().tip_accounts = v;
        }
    }

    /// A random tip account (spreads write-lock contention).
    pub fn pick_tip_account(&self) -> Option<Address> {
        let g = self.inner.read();
        if g.tip_accounts.is_empty() {
            return None;
        }
        let i = rand::random_range(0..g.tip_accounts.len());
        Some(g.tip_accounts[i])
    }

    /// ATAs never checked, or checked more than 60 s ago (wallets close and
    /// reopen token accounts).
    pub fn unchecked_atas(&self, candidates: &[Address]) -> Vec<Address> {
        let g = self.inner.read();
        let now = Ts::now();
        let mut v: Vec<Address> = candidates
            .iter()
            .filter(|a| g.checked_atas.get(a).is_none_or(|t| t.age_ms(now) > 60_000))
            .copied()
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn record_atas(&self, results: &[(Address, bool)]) {
        let mut g = self.inner.write();
        let now = Ts::now();
        for (a, exists) in results {
            g.checked_atas.insert(*a, now);
            if *exists {
                g.existing_atas.insert(*a);
            } else {
                g.existing_atas.remove(a);
            }
        }
    }

    pub fn existing_atas(&self) -> HashSet<Address> {
        self.inner.read().existing_atas.clone()
    }

    pub fn add_paper_net(&self, lamports: i64) -> i64 {
        let mut g = self.inner.write();
        g.paper_net_lamports += lamports;
        g.paper_net_lamports
    }

    pub fn paper_net(&self) -> i64 {
        self.inner.read().paper_net_lamports
    }

    pub fn set_wallet_lamports(&self, l: u64, ts: Ts) {
        self.inner.write().wallet_lamports = Some((l, ts));
    }

    pub fn wallet_lamports(&self, now: Ts, max_age_ms: u64) -> Option<u64> {
        self.inner.read().wallet_lamports.filter(|(_, t)| t.age_ms(now) <= max_age_ms).map(|(l, _)| l)
    }
}
