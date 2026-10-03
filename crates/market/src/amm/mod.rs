//! Pool prices worked out here, from the pool's own accounts, instead of
//! asked of a quote API: what a swap of a given size returns at the state
//! last received from the chain.

pub mod clmm;
pub mod dlmm;
pub mod math;

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
