//! Meteora DLMM: liquidity sits in bins, each at one fixed price. A swap
//! empties bins one after another, starting at the active bin, and pays a
//! fee that grows with how many bins recent swaps have moved through.
//!
//! Written from Meteora's published description of the pool (bin prices,
//! base and variable fee, volatility accumulator). Account offsets are the
//! program's account structs, checked against mainnet accounts in the tests.

use crate::accounts::METEORA_DLMM_PROGRAM;
use searcher_core::Address;
use solana_pubkey::Pubkey;
use std::collections::BTreeMap;

use super::math::{U256, mul_div_up};

/// Fee rates are in billionths.
const FEE_PRECISION: u128 = 1_000_000_000;
/// The program's cap on the total fee: 10 %.
const MAX_FEE_RATE: u128 = 100_000_000;
pub const BINS_PER_ARRAY: i32 = 70;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    /// Y per X in raw units, Q64.64.
    pub price: u128,
}

/// A pool at one moment. Token X is the pool's first mint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dlmm {
    pub active_id: i32,
    /// Price step between bins, in basis points.
    pub bin_step: u16,
    pub base_factor: u16,
    pub base_fee_power: u8,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    /// Share of the fee the protocol keeps, in ten-thousandths; the rest
    /// goes to the liquidity in the bin.
    pub protocol_share: u16,
    /// Seconds: a swap sooner than this after the last one keeps the references.
    pub filter_period: u16,
    /// Seconds after which the volatility reference is forgotten.
    pub decay_period: u16,
    /// Share of the accumulator kept as the reference, in ten-thousandths.
    pub reduction_factor: u16,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    /// Unix time of the last swap.
    pub last_update: i64,
    /// Bins of the bin arrays at hand, by id; what lies beyond is not known.
    pub bins: BTreeMap<i32, Bin>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Swap {
    pub amount_out: u64,
    /// Fee paid, in the input token.
    pub fee: u64,
    /// The active bin afterwards.
    pub active_id: i32,
}

impl Dlmm {
    /// The fee rate (billionths) while the active bin is `id`, given the
    /// volatility reference in force.
    fn fee_rate(&self, id: i32, index_reference: i32, volatility_reference: u32) -> u128 {
        let moved = index_reference.abs_diff(id) as u128 * 10_000;
        let accumulator = (volatility_reference as u128 + moved).min(self.max_volatility_accumulator as u128);
        let base = self.base_factor as u128 * self.bin_step as u128 * 10 * 10u128.pow(self.base_fee_power as u32);
        let variable = if self.variable_fee_control == 0 {
            0
        } else {
            let v = (accumulator * self.bin_step as u128).pow(2) * self.variable_fee_control as u128;
            v.div_ceil(100_000_000_000)
        };
        (base + variable).min(MAX_FEE_RATE)
    }

    /// The bin and volatility a swap at unix time `now` counts from: a swap
    /// after a quiet spell starts from where the price is now.
    fn references(&self, now: i64) -> (i32, u32) {
        let elapsed = now - self.last_update;
        if elapsed < self.filter_period as i64 {
            return (self.index_reference, self.volatility_reference);
        }
        let kept = if elapsed < self.decay_period as i64 {
            (self.volatility_accumulator as u64 * self.reduction_factor as u64 / 10_000) as u32
        } else {
            0
        };
        (self.active_id, kept)
    }

    /// What a swap arriving at bin `id` at unix time `now` pays, in billionths
    /// of its input.
    pub fn fee_rate_at(&self, id: i32, now: i64) -> u128 {
        let (index_reference, volatility_reference) = self.references(now);
        self.fee_rate(id, index_reference, volatility_reference)
    }

    /// Sell exactly `amount_in` of token X (`x_to_y`) or of token Y at unix
    /// time `now` (the fee depends on the time since the last swap).
    ///
    /// `None` when the answer is not known: the swap would run past the bins
    /// at hand.
    pub fn swap(&self, x_to_y: bool, amount_in: u64, now: i64) -> Option<Swap> {
        let (index_reference, volatility_reference) = self.references(now);

        let (mut id, mut left, mut out, mut fees) = (self.active_id, amount_in as u128, 0u128, 0u128);
        while left > 0 {
            let bin = self.bins.get(&id)?;
            let rate = self.fee_rate(id, index_reference, volatility_reference);
            let available = if x_to_y { bin.amount_y } else { bin.amount_x } as u128;
            if available > 0 {
                // the input that empties the bin, and the fee on top of it
                let empties = if x_to_y {
                    let (q, rem) = U256::from(available).checked_shl(64)?.div_rem(bin.price)?;
                    q.low()?.checked_add((rem != 0) as u128)?
                } else {
                    let p = U256::mul(available, bin.price);
                    ((p.hi << 64) | (p.lo >> 64)).checked_add((p.lo as u64 != 0) as u128)?
                };
                let with_fee = empties + mul_div_up(empties, rate, FEE_PRECISION - rate)?;
                if left >= with_fee {
                    (left, out, fees) = (left - with_fee, out + available, fees + with_fee - empties);
                } else {
                    // the fee comes out of what is left; the rest trades at the bin's price
                    let fee = mul_div_up(left, rate, FEE_PRECISION)?;
                    let trades = left - fee;
                    let got = if x_to_y {
                        let p = U256::mul(trades, bin.price);
                        (p.hi << 64) | (p.lo >> 64)
                    } else {
                        U256::from(trades).checked_shl(64)?.div_rem(bin.price)?.0.low()?
                    };
                    (left, out, fees) = (0, out + got.min(available), fees + fee);
                }
            }
            if left > 0 {
                id += if x_to_y { -1 } else { 1 };
            }
        }
        Some(Swap { amount_out: u64::try_from(out).ok()?, fee: u64::try_from(fees).ok()?, active_id: id })
    }
}

fn at<const N: usize>(d: &[u8], offset: usize) -> Option<[u8; N]> {
    d.get(offset..offset + N)?.try_into().ok()
}

pub fn active_id(pool: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(at(pool, 76)?))
}

/// Index of the bin array holding bin `id`.
pub fn array_index(id: i32) -> i64 {
    id.div_euclid(BINS_PER_ARRAY) as i64
}

/// Address of bin array `index` (seed: the index, little-endian).
pub fn bin_array(pool: &Address, index: i64) -> Option<Address> {
    let program: Address = METEORA_DLMM_PROGRAM.parse().ok()?;
    let index = index.to_le_bytes();
    let seeds = [b"bin_array".as_slice(), pool.0.as_slice(), index.as_slice()];
    Some(Address(Pubkey::find_program_address(&seeds, &Pubkey::new_from_array(program.0)).0.to_bytes()))
}

/// The pool with the bins of the given bin arrays.
///
/// LbPair: static parameters @8 (base_factor u16, filter_period u16,
/// decay_period u16, reduction_factor u16, variable_fee_control u32,
/// max_volatility_accumulator u32 @20, protocol_share u16 @32,
/// base_fee_power_factor u8 @34),
/// variable parameters @40 (volatility_accumulator u32, volatility_reference
/// u32, index_reference i32, last_update_timestamp i64 @56), active_id i32
/// @76, bin_step u16 @80. BinArray: index i64 @8, then 70 bins of 144 bytes
/// @56: amount_x u64, amount_y u64, price u128.
pub fn decode(pool: &[u8], arrays: &[&[u8]]) -> Option<Dlmm> {
    let u16_at = |o| at(pool, o).map(u16::from_le_bytes);
    let u32_at = |o| at(pool, o).map(u32::from_le_bytes);
    let mut bins = BTreeMap::new();
    for a in arrays {
        if a.len() != 10_136 {
            return None;
        }
        let index = i64::from_le_bytes(at(a, 8)?);
        let first = i32::try_from(index.checked_mul(BINS_PER_ARRAY as i64)?).ok()?;
        for i in 0..BINS_PER_ARRAY {
            let o = 56 + i as usize * 144;
            bins.insert(
                first + i,
                Bin {
                    amount_x: u64::from_le_bytes(at(a, o)?),
                    amount_y: u64::from_le_bytes(at(a, o + 8)?),
                    price: u128::from_le_bytes(at(a, o + 16)?),
                },
            );
        }
    }
    let pool = Dlmm {
        active_id: active_id(pool)?,
        bin_step: u16_at(80)?,
        base_factor: u16_at(8)?,
        base_fee_power: *pool.get(34)?,
        variable_fee_control: u32_at(16)?,
        max_volatility_accumulator: u32_at(20)?,
        protocol_share: u16_at(32)?,
        filter_period: u16_at(10)?,
        decay_period: u16_at(12)?,
        reduction_factor: u16_at(14)?,
        volatility_accumulator: u32_at(40)?,
        volatility_reference: u32_at(44)?,
        index_reference: i32::from_le_bytes(at(pool, 48)?),
        last_update: i64::from_le_bytes(at(pool, 56)?),
        bins,
    };
    pool.bins.contains_key(&pool.active_id).then_some(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::fixture::samples;

    /// Bins at price 2.0 (id 0) and 2.0002 (id 1), 1 bp apart, 10 bp base fee.
    fn pool() -> Dlmm {
        let two = 2u128 << 64;
        Dlmm {
            active_id: 0,
            bin_step: 1,
            base_factor: 10_000,
            base_fee_power: 1,
            variable_fee_control: 0,
            max_volatility_accumulator: 100_000,
            protocol_share: 1_000,
            filter_period: 10,
            decay_period: 120,
            reduction_factor: 5_000,
            volatility_accumulator: 0,
            volatility_reference: 0,
            index_reference: 0,
            last_update: 0,
            bins: BTreeMap::from([
                (-1, Bin { amount_x: 0, amount_y: 1_000_000, price: two - two / 10_000 }),
                (0, Bin { amount_x: 500_000, amount_y: 1_000_000, price: two }),
                (1, Bin { amount_x: 500_000, amount_y: 0, price: two + two / 10_000 }),
            ]),
        }
    }

    #[test]
    fn inside_a_bin_the_price_is_fixed_and_the_fee_comes_off_the_input() {
        let p = pool();
        // base fee = 10000 × 1 × 10 × 10 = 1,000,000 billionths = 0.1 %
        assert_eq!(p.fee_rate(0, 0, 0), 1_000_000);
        // 100,000 X in: 100 fee, 99,900 trade at 2.0 → 199,800 Y
        assert_eq!(p.swap(true, 100_000, 0), Some(Swap { amount_out: 199_800, fee: 100, active_id: 0 }));
        // 100,000 Y in: 100 fee, 99,900 / 2.0 → 49,950 X
        assert_eq!(p.swap(false, 100_000, 0), Some(Swap { amount_out: 49_950, fee: 100, active_id: 0 }));
        assert_eq!(p.swap(true, 0, 0).unwrap().amount_out, 0);
    }

    #[test]
    fn a_swap_empties_a_bin_and_goes_on_in_the_next() {
        let p = pool();
        // bin 0 holds 1,000,000 Y: 500,000 X empties it, plus 501 fee (0.1 % on top, rounded up)
        let s = p.swap(true, 500_501, 0).unwrap();
        assert_eq!((s.amount_out, s.fee, s.active_id), (1_000_000, 501, 0));
        // one more X goes to bin −1 at 1.9998
        let s = p.swap(true, 500_501 + 1_000, 0).unwrap();
        assert_eq!(s.active_id, -1);
        assert_eq!(s.amount_out, 1_000_000 + 1_997); // 999 after fee × 1.9998, rounded down
        // past the bins at hand: no answer
        assert_eq!(p.swap(true, 10_000_000, 0), None);
    }

    #[test]
    fn the_fee_grows_with_the_bins_a_swap_moves_through() {
        let mut p = pool();
        p.variable_fee_control = 2_000_000;
        // at the reference bin there is no variable fee; one bin away the
        // accumulator is 10,000: (10,000 × 1)² × 2,000,000 / 10¹¹ = 2,000 billionths
        assert_eq!(p.fee_rate(0, 0, 0), 1_000_000);
        assert_eq!(p.fee_rate(-1, 0, 0), 1_002_000);
        // the accumulator is capped
        assert_eq!(p.fee_rate(-50, 0, 0), 1_000_000 + 100_000u128.pow(2) * 2_000_000 / 100_000_000_000);
        // a swap soon after the last keeps the old reference; after the
        // filter period it starts from the current bin with part of the accumulator
        p.volatility_accumulator = 30_000;
        p.volatility_reference = 20_000;
        p.index_reference = 3;
        let soon = p.swap(true, 100_000, 5).unwrap();
        let later = p.swap(true, 100_000, 60).unwrap();
        let long_after = p.swap(true, 100_000, 600).unwrap();
        assert!(soon.fee > later.fee && later.fee > long_after.fee, "{soon:?} {later:?} {long_after:?}");
        assert_eq!(long_after.fee, 100);
        assert_eq!(
            (p.fee_rate_at(0, 5), p.fee_rate_at(0, 60), p.fee_rate_at(0, 600)),
            (1_050_000, 1_004_500, 1_000_000)
        );
    }

    #[test]
    fn swaps_match_what_the_program_paid_on_mainnet() {
        let samples = samples(include_str!("../../../../fixtures/amm/dlmm.json"));
        assert!(samples.len() >= 8);
        let mut moved = 0;
        for s in &samples {
            let arrays: Vec<&[u8]> = s.others.iter().map(Vec::as_slice).collect();
            let pool = decode(&s.pool, &arrays).expect("decodes");
            let swap = pool.swap(s.a_to_b, s.amount_in, s.unix_time).expect("inside the bins at hand");
            eprintln!(
                "{} in {:>13} ours {:>13} chain {:>13} diff {:>3} bins {}",
                if s.a_to_b { "X→Y" } else { "Y→X" },
                s.amount_in,
                swap.amount_out,
                s.amount_out,
                swap.amount_out as i64 - s.amount_out as i64,
                swap.active_id.abs_diff(pool.active_id)
            );
            moved += swap.active_id.abs_diff(pool.active_id);
            assert_eq!(swap.amount_out, s.amount_out, "{} in, x_to_y {}", s.amount_in, s.a_to_b);
        }
        assert!(moved > 0, "the samples include swaps that go through several bins");
    }
}
