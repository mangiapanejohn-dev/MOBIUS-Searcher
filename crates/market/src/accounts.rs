//! Decoders for the on-chain accounts the chain feed watches: DEX pool state
//! (mid price) and Pyth price updates. Offsets follow the programs' account
//! structs and are pinned by tests on mainnet snapshots (`fixtures/accounts`).

use searcher_core::Address;
use searcher_core::config::PoolKind;

pub const WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
pub const METEORA_DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
/// Pyth Solana receiver: owner of `PriceUpdateV2` accounts.
pub const PYTH_RECEIVER_PROGRAM: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

pub fn pool_program(kind: PoolKind) -> &'static str {
    match kind {
        PoolKind::Whirlpool => WHIRLPOOL_PROGRAM,
        PoolKind::RaydiumClmm => RAYDIUM_CLMM_PROGRAM,
        PoolKind::MeteoraDlmm => METEORA_DLMM_PROGRAM,
    }
}

/// Pool state reduced to its mid price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoolMid {
    pub mint_a: Address,
    pub mint_b: Address,
    /// Whole B per whole A (decimal-adjusted).
    pub price_a_in_b: f64,
}

impl PoolMid {
    /// Price of `base` in `quote`, whichever order the pool stores them in.
    pub fn price_of(&self, base: &Address, quote: &Address) -> Option<f64> {
        if (&self.mint_a, &self.mint_b) == (base, quote) {
            Some(self.price_a_in_b)
        } else if (&self.mint_b, &self.mint_a) == (base, quote) {
            Some(1.0 / self.price_a_in_b)
        } else {
            None
        }
    }
}

fn addr(d: &[u8], at: usize) -> Option<Address> {
    Some(Address(d.get(at..at + 32)?.try_into().ok()?))
}

fn sqrt_x64_price(d: &[u8], at: usize, dec_a: u8, dec_b: u8) -> Option<f64> {
    let sp = u128::from_le_bytes(d.get(at..at + 16)?.try_into().ok()?);
    let r = sp as f64 / 18_446_744_073_709_551_616.0; // 2^64
    Some(r * r * 10f64.powi(dec_a as i32 - dec_b as i32))
}

/// Mid price from pool account data. `decimals` resolves the pool's mints
/// (Whirlpool and DLMM do not store decimals); `None` for unknown mints.
pub fn decode_pool(kind: PoolKind, d: &[u8], decimals: impl Fn(&Address) -> Option<u8>) -> Option<PoolMid> {
    let (mint_a, mint_b, price) = match kind {
        // Whirlpool: sqrt_price u128 @65, token_mint_a @101, token_mint_b @181
        PoolKind::Whirlpool => {
            let (a, b) = (addr(d, 101)?, addr(d, 181)?);
            (a, b, sqrt_x64_price(d, 65, decimals(&a)?, decimals(&b)?)?)
        }
        // Raydium CLMM PoolState: token_mint_0 @73, token_mint_1 @105,
        // mint_decimals_0/1 @233/234, sqrt_price_x64 u128 @253
        PoolKind::RaydiumClmm => {
            let (a, b) = (addr(d, 73)?, addr(d, 105)?);
            (a, b, sqrt_x64_price(d, 253, *d.get(233)?, *d.get(234)?)?)
        }
        // Meteora DLMM LbPair: active_id i32 @76, bin_step u16 @80,
        // token_x_mint @88, token_y_mint @120; price = (1 + step/1e4)^id
        PoolKind::MeteoraDlmm => {
            let (a, b) = (addr(d, 88)?, addr(d, 120)?);
            let id = i32::from_le_bytes(d.get(76..80)?.try_into().ok()?);
            let step = u16::from_le_bytes(d.get(80..82)?.try_into().ok()?);
            let adj = 10f64.powi(decimals(&a)? as i32 - decimals(&b)? as i32);
            (a, b, (1.0 + step as f64 / 10_000.0).powi(id) * adj)
        }
    };
    (price.is_finite() && price > 0.0).then_some(PoolMid { mint_a, mint_b, price_a_in_b: price })
}

/// Latest price from a Pyth `PriceUpdateV2` account.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OracleQuote {
    pub feed_id: [u8; 32],
    pub price: f64,
    pub conf: f64,
    pub publish_time: i64,
    pub posted_slot: u64,
}

/// `PriceUpdateV2`: discriminator (8), write_authority (32), verification
/// level (Borsh enum: 1 byte for `Full`, 2 for `Partial { num_signatures }`),
/// then `PriceFeedMessage` and `posted_slot`.
pub fn decode_pyth_price_update(d: &[u8]) -> Option<OracleQuote> {
    let off = match *d.get(40)? {
        0 => 42,
        1 => 41,
        _ => return None,
    };
    let i64_at = |at: usize| Some(i64::from_le_bytes(d.get(off + at..off + at + 8)?.try_into().ok()?));
    let feed_id: [u8; 32] = d.get(off..off + 32)?.try_into().ok()?;
    let price = i64_at(32)?;
    let conf = u64::from_le_bytes(d.get(off + 40..off + 48)?.try_into().ok()?);
    let expo = i32::from_le_bytes(d.get(off + 48..off + 52)?.try_into().ok()?);
    let publish_time = i64_at(52)?;
    let posted_slot = u64::from_le_bytes(d.get(off + 84..off + 92)?.try_into().ok()?);
    let scale = 10f64.powi(expo);
    let price = price as f64 * scale;
    (price.is_finite() && price > 0.0).then_some(OracleQuote {
        feed_id,
        price,
        conf: conf as f64 * scale,
        publish_time,
        posted_slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use searcher_core::config::parse_feed_id;
    use searcher_core::token::TokenRegistry;

    const SOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn fixture(json: &str) -> (Vec<u8>, String, f64) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let data = base64::engine::general_purpose::STANDARD.decode(v["data_base64"].as_str().unwrap()).unwrap();
        (data, v["owner"].as_str().unwrap().to_string(), v["expected_price"].as_f64().unwrap())
    }

    fn close(a: f64, b: f64) -> bool {
        ((a - b) / b).abs() < 1e-9
    }

    #[test]
    fn pools_decode_mainnet_snapshots() {
        let reg = TokenRegistry::defaults();
        let well_known_decimals = |m: &Address| reg.decimals(m);
        let (sol, usdc): (Address, Address) = (SOL.parse().unwrap(), USDC.parse().unwrap());
        for (kind, json) in [
            (PoolKind::Whirlpool, include_str!("../../../fixtures/accounts/whirlpool.json")),
            (PoolKind::RaydiumClmm, include_str!("../../../fixtures/accounts/raydium_clmm.json")),
            (PoolKind::MeteoraDlmm, include_str!("../../../fixtures/accounts/meteora_dlmm.json")),
        ] {
            let (data, owner, expected) = fixture(json);
            assert_eq!(owner, pool_program(kind), "{kind:?} owner");
            let mid = decode_pool(kind, &data, well_known_decimals).unwrap_or_else(|| panic!("{kind:?} decodes"));
            let p = mid.price_of(&sol, &usdc).expect("SOL/USDC pool");
            assert!(close(p, expected), "{kind:?}: {p} vs {expected}");
            assert!(close(mid.price_of(&usdc, &sol).unwrap(), 1.0 / expected), "inverse");
            assert!(decode_pool(kind, &data[..60], well_known_decimals).is_none(), "truncated data");
        }
    }

    #[test]
    fn unknown_mints_and_other_pairs_are_rejected() {
        let reg = TokenRegistry::defaults();
        let well_known_decimals = |m: &Address| reg.decimals(m);
        let (data, _, _) = fixture(include_str!("../../../fixtures/accounts/whirlpool.json"));
        assert!(decode_pool(PoolKind::Whirlpool, &data, |_| None).is_none(), "decimals unknown");
        let mid = decode_pool(PoolKind::Whirlpool, &data, well_known_decimals).unwrap();
        assert_eq!(mid.price_of(&Address([7; 32]), &Address([8; 32])), None);
    }

    #[test]
    fn pyth_decodes_mainnet_snapshot() {
        let (data, owner, expected) = fixture(include_str!("../../../fixtures/accounts/pyth_sol_usd.json"));
        assert_eq!(owner, PYTH_RECEIVER_PROGRAM);
        let q = decode_pyth_price_update(&data).unwrap();
        assert_eq!(Some(q.feed_id), parse_feed_id("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"));
        assert!(close(q.price, expected), "{} vs {expected}", q.price);
        assert!(q.conf > 0.0 && q.conf < q.price / 100.0, "confidence is small: {}", q.conf);
        assert!(q.publish_time > 1_700_000_000 && q.posted_slot > 300_000_000);
        let mut bad = data.clone();
        bad[40] = 9;
        assert!(decode_pyth_price_update(&bad).is_none(), "unknown verification level");
    }
}
