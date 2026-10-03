//! Pool prices worked out here, from the pool's own accounts, instead of
//! asked of a quote API: what a swap of a given size returns at the state
//! last received from the chain.

pub mod clmm;
pub mod dlmm;
pub mod math;

use searcher_core::Address;
use searcher_core::config::PoolKind;

/// How many tick or bin arrays are read on each side of the current price.
const ARRAYS_EACH_SIDE: i32 = 2;

/// One pool at one moment, whichever kind it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pool {
    Clmm(clmm::Clmm),
    Dlmm(dlmm::Dlmm),
}

impl Pool {
    /// What selling exactly `amount_in` of the pool's first token (`a_to_b`)
    /// or of its second returns, at unix time `now`. `None`: not known from
    /// the accounts at hand.
    pub fn swap(&self, a_to_b: bool, amount_in: u64, now: i64) -> Option<u64> {
        match self {
            Pool::Clmm(p) => p.swap(a_to_b, amount_in).map(|s| s.amount_out),
            Pool::Dlmm(p) => p.swap(a_to_b, amount_in, now).map(|s| s.amount_out),
        }
    }
}

/// The accounts a pool's swaps depend on besides the pool account, as of the
/// pool account `pool`: the tick or bin arrays around the current price (and
/// Raydium's fee configuration, first).
pub fn dependencies(kind: PoolKind, address: &Address, pool: &[u8]) -> Option<Vec<Address>> {
    let around = -ARRAYS_EACH_SIDE..=ARRAYS_EACH_SIDE;
    match kind {
        PoolKind::Whirlpool => {
            let spacing = clmm::whirlpool::tick_spacing(pool)?;
            let span = spacing as i32 * clmm::WHIRLPOOL_ARRAY_TICKS;
            let start = clmm::array_start(clmm::whirlpool::current_tick(pool)?, spacing, clmm::WHIRLPOOL_ARRAY_TICKS);
            around.map(|k| clmm::whirlpool::tick_array(address, start + k * span)).collect()
        }
        PoolKind::RaydiumClmm => {
            let spacing = clmm::raydium::tick_spacing(pool)?;
            let span = spacing as i32 * clmm::RAYDIUM_ARRAY_TICKS;
            let start = clmm::array_start(clmm::raydium::current_tick(pool)?, spacing, clmm::RAYDIUM_ARRAY_TICKS);
            let arrays = around.map(|k| clmm::raydium::tick_array(address, start + k * span));
            std::iter::once(clmm::raydium::amm_config(pool)).chain(arrays).collect()
        }
        PoolKind::MeteoraDlmm => {
            let index = dlmm::array_index(dlmm::active_id(pool)?);
            around.map(|k| dlmm::bin_array(address, index + k as i64)).collect()
        }
    }
}

/// The arrays around the middle one (the current price) that exist without a gap.
fn run(arrays: &[Option<Vec<u8>>]) -> Vec<&[u8]> {
    if arrays.is_empty() {
        return Vec::new();
    }
    let mid = arrays.len() / 2;
    let below = (0..mid).rev().take_while(|i| arrays[*i].is_some()).count();
    let above = (mid + 1..arrays.len()).take_while(|i| arrays[*i].is_some()).count();
    arrays[mid - below..=mid + above].iter().flatten().map(Vec::as_slice).collect()
}

/// The pool from its account and the data of its [`dependencies`], in that
/// order (`None` for an account that does not exist: an array nobody has
/// put liquidity in). Arrays not next to the current one are left out.
pub fn decode(kind: PoolKind, pool: &[u8], deps: &[Option<Vec<u8>>]) -> Option<Pool> {
    match kind {
        PoolKind::Whirlpool => clmm::whirlpool::decode(pool, &run(deps)).map(Pool::Clmm),
        PoolKind::RaydiumClmm => {
            let (config, arrays) = deps.split_first()?;
            clmm::raydium::decode(pool, config.as_deref()?, &run(arrays)).map(Pool::Clmm)
        }
        PoolKind::MeteoraDlmm => dlmm::decode(pool, &run(deps)).map(Pool::Dlmm),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    /// Samples written by `scripts/amm_parity.py`: a pool's accounts and
    /// what the pool program itself paid for a swap simulated at that state.
    pub struct Sample {
        pub pool: Vec<u8>,
        /// Every other account of the sample.
        pub others: Vec<Vec<u8>>,
        pub a_to_b: bool,
        pub amount_in: u64,
        pub amount_out: u64,
        pub unix_time: i64,
    }

    pub fn samples(fixture: &str) -> Vec<Sample> {
        use base64::Engine;
        let v: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let blob = |key: &serde_json::Value| {
            base64::engine::general_purpose::STANDARD
                .decode(v["blobs"][key.as_str().unwrap()].as_str().unwrap())
                .unwrap()
        };
        v["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let pool = s["pool"].as_str().unwrap();
                let accounts = s["accounts"].as_object().unwrap();
                Sample {
                    pool: blob(&accounts[pool]),
                    others: accounts.iter().filter(|(a, _)| *a != pool).map(|(_, k)| blob(k)).collect(),
                    a_to_b: s["a_to_b"].as_bool().unwrap(),
                    amount_in: s["amount_in"].as_u64().unwrap(),
                    amount_out: s["amount_out"].as_u64().unwrap(),
                    unix_time: s["unix_time"].as_i64().unwrap(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_names_the_accounts_it_depends_on_and_decodes_from_them() {
        // the Whirlpool fixture holds exactly these five arrays, by address
        let fixture = include_str!("../../../../fixtures/amm/whirlpool.json");
        let v: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let sample = &v["samples"][0];
        let address: Address = sample["pool"].as_str().unwrap().parse().unwrap();
        let data = |a: &str| -> Option<Vec<u8>> {
            use base64::Engine;
            let key = sample["accounts"].get(a)?.as_str()?;
            base64::engine::general_purpose::STANDARD.decode(v["blobs"][key].as_str()?).ok()
        };
        let pool = data(&address.to_string()).unwrap();
        let deps = dependencies(PoolKind::Whirlpool, &address, &pool).unwrap();
        assert_eq!(deps.len(), 5);
        let datas: Vec<Option<Vec<u8>>> = deps.iter().map(|a| data(&a.to_string())).collect();
        assert!(datas.iter().all(Option::is_some), "every array the pool names is in the fixture");
        let decoded = decode(PoolKind::Whirlpool, &pool, &datas).unwrap();
        let (a_to_b, amount) = (sample["a_to_b"].as_bool().unwrap(), sample["amount_in"].as_u64().unwrap());
        assert_eq!(decoded.swap(a_to_b, amount, 0), sample["amount_out"].as_u64());

        // an array that does not exist ends what is known on that side
        let mut gap = datas.clone();
        gap[1] = None; // the array just below the current one
        let Pool::Clmm(p) = decode(PoolKind::Whirlpool, &pool, &gap).unwrap() else { panic!() };
        let Pool::Clmm(full) = decoded else { panic!() };
        assert!(p.known.0 > full.known.0 && p.known.1 == full.known.1);
        // without the current array there is no pool
        gap[2] = None;
        assert_eq!(decode(PoolKind::Whirlpool, &pool, &gap), None);
    }
}
