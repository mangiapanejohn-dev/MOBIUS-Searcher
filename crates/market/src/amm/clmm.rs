//! Concentrated-liquidity pools (Orca Whirlpool, Raydium CLMM): the swap a
//! pool would execute at a given state, from its pool account and the tick
//! arrays around the price. Both follow the scheme Uniswap v3 published:
//! inside a tick range the pool is `x·y = L²`, a fee is taken from the
//! input, and liquidity changes where the price crosses an initialized tick.
//!
//! Account offsets are the programs' account structs, checked against
//! mainnet accounts in the tests.

use super::math::{U256, mul_div, mul_div_up, sqrt_price_at_tick, tick_at_sqrt_price};
use crate::accounts::{RAYDIUM_CLMM_PROGRAM, WHIRLPOOL_PROGRAM};
use searcher_core::Address;
use solana_pubkey::Pubkey;

const PPM: u128 = 1_000_000;
/// Ticks per tick array.
pub const WHIRLPOOL_ARRAY_TICKS: i32 = 88;
pub const RAYDIUM_ARRAY_TICKS: i32 = 60;
/// Anchor discriminator of Whirlpool's fixed-size tick array account.
const WHIRLPOOL_FIXED_ARRAY: [u8; 8] = [0x45, 0x61, 0xbd, 0xbe, 0x6e, 0x07, 0x42, 0xbb];

/// An initialized tick: liquidity added when the price crosses it upwards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    pub index: i32,
    pub liquidity_net: i128,
}

/// A pool at one moment. Token A is the pool's first mint; the price is B per A.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clmm {
    /// Square root of the price in raw units, Q64.64.
    pub sqrt_price: u128,
    pub tick: i32,
    pub liquidity: u128,
    /// Swap fee taken from the input, in millionths.
    pub fee_ppm: u32,
    /// Initialized ticks inside `known`, ascending.
    pub ticks: Vec<Tick>,
    /// Ticks `known.0 .. known.1` are covered by the tick arrays at hand;
    /// what lies beyond is not known.
    pub known: (i32, i32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Swap {
    pub amount_out: u64,
    /// Fee paid, in the input token.
    pub fee: u64,
    /// The pool's price afterwards.
    pub sqrt_price: u128,
    pub ticks_crossed: u32,
}

/// Token A between two prices at liquidity `l`: `l · (hi − lo) / (hi · lo)`.
fn amount_a(lo: u128, hi: u128, l: u128, up: bool) -> Option<u128> {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    let (q, r1) = U256::mul(l, hi - lo).checked_shl(64)?.div_rem(hi)?;
    let (q, r2) = q.div_rem(lo)?;
    q.low()?.checked_add((up && (r1 != 0 || r2 != 0)) as u128)
}

/// Token B between two prices at liquidity `l`: `l · (hi − lo)`.
fn amount_b(lo: u128, hi: u128, l: u128, up: bool) -> Option<u128> {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    let p = U256::mul(l, hi - lo);
    if p.hi >> 64 != 0 {
        return None;
    }
    ((p.hi << 64) | (p.lo >> 64)).checked_add((up && p.lo as u64 != 0) as u128)
}

/// The price after `amount` of the input token (fee already taken) is added
/// at liquidity `l`, rounded against the trader.
fn price_after(sqrt_price: u128, l: u128, amount: u128, a_to_b: bool) -> Option<u128> {
    if amount == 0 {
        return Some(sqrt_price);
    }
    if a_to_b {
        // l·√P / (l + amount·√P), rounded up
        let l_x64 = U256::from(l).checked_shl(64)?.low()?;
        let denominator = U256::from(l_x64).checked_add(U256::mul(amount, sqrt_price))?.low()?;
        mul_div_up(l_x64, sqrt_price, denominator)
    } else {
        // √P + amount / l, rounded down
        sqrt_price.checked_add(U256::from(amount).checked_shl(64)?.div_rem(l)?.0.low()?)
    }
}

struct Step {
    sqrt_price: u128,
    amount_in: u128,
    amount_out: u128,
    fee: u128,
}

/// One stretch of a swap at constant liquidity: towards `target`, as far as
/// `remaining` input (fee included) goes.
fn step(sqrt_price: u128, target: u128, l: u128, remaining: u128, fee_ppm: u128, a_to_b: bool) -> Option<Step> {
    let input = |to: u128| if a_to_b { amount_a(to, sqrt_price, l, true) } else { amount_b(sqrt_price, to, l, true) };
    let to_target = input(target)?;
    let after_fee = mul_div(remaining, PPM - fee_ppm, PPM)?;
    let next = if after_fee >= to_target { target } else { price_after(sqrt_price, l, after_fee, a_to_b)? };
    let reached = next == target;
    let amount_in = if reached { to_target } else { input(next)? };
    let amount_out = if a_to_b { amount_b(next, sqrt_price, l, false)? } else { amount_a(sqrt_price, next, l, false)? };
    let fee =
        if reached { mul_div_up(amount_in, fee_ppm, PPM - fee_ppm)? } else { remaining.checked_sub(amount_in)? };
    Some(Step { sqrt_price: next, amount_in, amount_out, fee })
}

impl Clmm {
    /// Sell exactly `amount_in` of token A (`a_to_b`) or of token B.
    ///
    /// `None` when the answer is not known: the swap would run past the tick
    /// arrays at hand, or the numbers are outside what this handles
    /// (liquidity of 2^64 or more).
    pub fn swap(&self, a_to_b: bool, amount_in: u64) -> Option<Swap> {
        let (mut sqrt_price, mut tick, mut liquidity) = (self.sqrt_price, self.tick, self.liquidity);
        let (mut remaining, mut out, mut fees, mut crossed) = (amount_in as u128, 0u128, 0u128, 0);
        while remaining > 0 {
            // the next initialized tick in the swap's direction, else the edge of what is known
            let next = if a_to_b {
                self.ticks.iter().rev().find(|t| t.index <= tick)
            } else {
                self.ticks.iter().find(|t| t.index > tick)
            };
            let edge = if a_to_b { self.known.0 } else { self.known.1 };
            let target_tick = next.map_or(edge, |t| t.index);
            let target = sqrt_price_at_tick(target_tick)?;
            let s = step(sqrt_price, target, liquidity, remaining, self.fee_ppm as u128, a_to_b)?;
            remaining = remaining.checked_sub(s.amount_in + s.fee)?;
            (out, fees, sqrt_price) = (out + s.amount_out, fees + s.fee, s.sqrt_price);
            if sqrt_price != target {
                break; // the input ran out inside the range
            }
            match next {
                Some(t) => {
                    let net = if a_to_b { t.liquidity_net.checked_neg()? } else { t.liquidity_net };
                    liquidity = liquidity.checked_add_signed(net)?;
                    crossed += 1;
                }
                // at the edge of the known ticks with input left: unknown from here
                None if remaining > 0 => return None,
                None => {}
            }
            tick = if a_to_b { target_tick - 1 } else { target_tick };
        }
        Some(Swap {
            amount_out: u64::try_from(out).ok()?,
            fee: u64::try_from(fees).ok()?,
            sqrt_price,
            ticks_crossed: crossed,
        })
    }

    /// Tick arrays `(start, ticks)` joined into the ticks and the range they cover.
    fn with_arrays(mut self, mut arrays: Vec<(i32, Vec<Tick>)>, span: i32) -> Option<Clmm> {
        arrays.sort_by_key(|a| a.0);
        let (first, last) = (arrays.first()?.0, arrays.last()?.0);
        // side by side, and the current tick inside them
        if arrays.windows(2).any(|w| w[1].0 != w[0].0 + span) || !(first..last + span).contains(&self.tick) {
            return None;
        }
        self.known = (first, last + span);
        self.ticks = arrays.into_iter().flat_map(|a| a.1).collect();
        // the stored tick must be the one the stored price is in
        (tick_at_sqrt_price(self.sqrt_price)?.abs_diff(self.tick) <= 1).then_some(self)
    }
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn i32_at(d: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn u128_at(d: &[u8], at: usize) -> Option<u128> {
    Some(u128::from_le_bytes(d.get(at..at + 16)?.try_into().ok()?))
}

/// First tick of the tick array holding `tick`.
pub fn array_start(tick: i32, tick_spacing: u16, ticks_per_array: i32) -> i32 {
    let span = tick_spacing as i32 * ticks_per_array;
    tick.div_euclid(span) * span
}

fn pda(seeds: &[&[u8]], program: &str) -> Option<Address> {
    let program: Address = program.parse().ok()?;
    Some(Address(Pubkey::find_program_address(seeds, &Pubkey::new_from_array(program.0)).0.to_bytes()))
}

/// Orca Whirlpool. Pool account: tick_spacing u16 @41, fee tier index u16 @43,
/// fee_rate u16 @45 (millionths), liquidity u128 @49, sqrt_price u128 @65,
/// tick_current_index i32 @81.
pub mod whirlpool {
    use super::*;

    pub fn tick_spacing(pool: &[u8]) -> Option<u16> {
        u16_at(pool, 41)
    }

    pub fn current_tick(pool: &[u8]) -> Option<i32> {
        i32_at(pool, 81)
    }

    /// Address of the tick array starting at `start` (seed: the start as decimal text).
    pub fn tick_array(pool: &Address, start: i32) -> Option<Address> {
        pda(&[b"tick_array".as_slice(), pool.0.as_slice(), start.to_string().as_bytes()], WHIRLPOOL_PROGRAM)
    }

    /// The pool with the given tick arrays (any order; they must be side by
    /// side and include the current tick). `None` for what this does not
    /// handle: a pool on an adaptive fee tier (its fee is not the stored
    /// rate) and variable-size tick arrays.
    pub fn decode(pool: &[u8], arrays: &[&[u8]]) -> Option<Clmm> {
        let spacing = tick_spacing(pool)?;
        if u16_at(pool, 43)? != spacing {
            return None; // fee tier index ≠ tick spacing: not a fixed-fee pool
        }
        let span = spacing as i32 * WHIRLPOOL_ARRAY_TICKS;
        let mut decoded = Vec::new();
        for a in arrays {
            // FixedTickArray: start_tick_index i32 @8, then 88 ticks of 113
            // bytes: initialized u8, liquidity_net i128 @1
            if a.len() != 9_988 || a[..8] != WHIRLPOOL_FIXED_ARRAY {
                return None;
            }
            let start = i32_at(a, 8)?;
            let ticks = (0..WHIRLPOOL_ARRAY_TICKS)
                .filter(|i| a[12 + *i as usize * 113] != 0)
                .map(|i| {
                    Some(Tick {
                        index: start + i * spacing as i32,
                        liquidity_net: u128_at(a, 13 + i as usize * 113)? as i128,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            decoded.push((start, ticks));
        }
        Clmm {
            sqrt_price: u128_at(pool, 65)?,
            tick: current_tick(pool)?,
            liquidity: u128_at(pool, 49)?,
            fee_ppm: u16_at(pool, 45)? as u32,
            ticks: vec![],
            known: (0, 0),
        }
        .with_arrays(decoded, span)
    }
}

/// Raydium CLMM. PoolState: amm_config @9, tick_spacing u16 @235, liquidity
/// u128 @237, sqrt_price_x64 u128 @253, tick_current i32 @269. The fee is in
/// the pool's AmmConfig: trade_fee_rate u32 @47 (millionths).
pub mod raydium {
    use super::*;

    pub fn tick_spacing(pool: &[u8]) -> Option<u16> {
        u16_at(pool, 235)
    }

    pub fn current_tick(pool: &[u8]) -> Option<i32> {
        i32_at(pool, 269)
    }

    pub fn amm_config(pool: &[u8]) -> Option<Address> {
        Some(Address(pool.get(9..41)?.try_into().ok()?))
    }

    /// Address of the tick array starting at `start` (seed: the start, big-endian).
    pub fn tick_array(pool: &Address, start: i32) -> Option<Address> {
        pda(&[b"tick_array".as_slice(), pool.0.as_slice(), start.to_be_bytes().as_slice()], RAYDIUM_CLMM_PROGRAM)
    }

    pub fn decode(pool: &[u8], amm_config: &[u8], arrays: &[&[u8]]) -> Option<Clmm> {
        let spacing = tick_spacing(pool)?;
        let span = spacing as i32 * RAYDIUM_ARRAY_TICKS;
        let mut decoded = Vec::new();
        for a in arrays {
            // TickArrayState: start_tick_index i32 @40, then 60 ticks of 168
            // bytes: tick i32, liquidity_net i128 @4, liquidity_gross u128 @20
            if a.len() != 10_240 {
                return None;
            }
            let ticks = (0..RAYDIUM_ARRAY_TICKS as usize)
                .map(|i| 44 + i * 168)
                .filter(|at| u128_at(a, at + 20).is_some_and(|gross| gross != 0))
                .map(|at| Some(Tick { index: i32_at(a, at)?, liquidity_net: u128_at(a, at + 4)? as i128 }))
                .collect::<Option<Vec<_>>>()?;
            decoded.push((i32_at(a, 40)?, ticks));
        }
        Clmm {
            sqrt_price: u128_at(pool, 253)?,
            tick: current_tick(pool)?,
            liquidity: u128_at(pool, 237)?,
            fee_ppm: u32::from_le_bytes(amm_config.get(47..51)?.try_into().ok()?),
            ticks: vec![],
            known: (0, 0),
        }
        .with_arrays(decoded, span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::fixture::samples;

    /// A pool at price 1 (tick 0) with one position of liquidity `l` between
    /// ticks −100 and 100, and a second one, twice as deep, from −200 to −100.
    fn pool(fee_ppm: u32) -> Clmm {
        let l = 1_000_000_000_000i128;
        Clmm {
            sqrt_price: 1 << 64,
            tick: 0,
            liquidity: l as u128,
            fee_ppm,
            ticks: vec![
                Tick { index: -200, liquidity_net: 2 * l },
                Tick { index: -100, liquidity_net: -l },
                Tick { index: 100, liquidity_net: -l },
            ],
            known: (-300, 300),
        }
    }

    #[test]
    fn a_small_swap_at_price_one_returns_the_input_less_fee_and_impact() {
        // 1,000,000 in at 0.3 %: 997,000 trades; impact at L = 1e12 is about one part in a million
        for a_to_b in [true, false] {
            let s = pool(3_000).swap(a_to_b, 1_000_000).unwrap();
            assert_eq!(s.fee, 3_000);
            assert!((996_990..997_000).contains(&s.amount_out), "{}", s.amount_out);
            assert_eq!(s.ticks_crossed, 0);
            assert_eq!(s.sqrt_price < 1 << 64, a_to_b, "selling A lowers the price");
        }
        // nothing in, nothing out
        assert_eq!(pool(3_000).swap(true, 0).unwrap().amount_out, 0);
    }

    #[test]
    fn without_a_fee_there_and_back_never_gains() {
        let p = pool(0);
        for amount in [1u64, 999, 1_000_000, 4_000_000_000] {
            let there = p.swap(true, amount).unwrap();
            let mut moved = p.clone();
            (moved.sqrt_price, moved.tick) = (there.sqrt_price, tick_at_sqrt_price(there.sqrt_price).unwrap());
            let back = moved.swap(false, there.amount_out).unwrap();
            assert!(back.amount_out <= amount, "{amount} → {} → {}", there.amount_out, back.amount_out);
            assert!(amount - back.amount_out <= 3, "only rounding is lost: {amount} → {}", back.amount_out);
        }
    }

    #[test]
    fn crossing_a_tick_changes_the_liquidity_the_rest_trades_at() {
        let p = pool(0);
        // token A needed to move the price from tick 0 down to tick −100 at L = 1e12
        let to_edge = amount_a(sqrt_price_at_tick(-100).unwrap(), 1 << 64, 1_000_000_000_000, true).unwrap() as u64;
        let at_edge = p.swap(true, to_edge).unwrap();
        // lands on the tick and has crossed it: no input was left for the next range
        assert_eq!((at_edge.sqrt_price, at_edge.ticks_crossed), (sqrt_price_at_tick(-100).unwrap(), 1));
        // past it the pool is twice as deep: the same extra input moves the price half as far
        let further = p.swap(true, to_edge + 1_000_000_000).unwrap();
        assert_eq!(further.ticks_crossed, 1);
        let shallow = Clmm { ticks: vec![], ..p.clone() }.swap(true, to_edge + 1_000_000_000).unwrap();
        assert!(further.amount_out > shallow.amount_out, "deeper liquidity past the tick gives a better price");
        // upwards the position ends at tick 100: beyond it there is no liquidity
        let up = p.swap(false, u64::MAX / 4);
        assert_eq!(up, None, "ran past the known ticks");
    }

    #[test]
    fn a_swap_that_runs_past_the_known_ticks_has_no_answer() {
        let mut p = pool(0);
        p.ticks.clear(); // constant liquidity, known only for ticks −300..300
        assert!(p.swap(true, 1_000_000).is_some());
        assert_eq!(p.swap(true, 100_000_000_000), None);
        assert_eq!(p.swap(false, 100_000_000_000), None);
    }

    #[test]
    fn whirlpool_swaps_match_what_the_program_paid_on_mainnet() {
        let samples = samples(include_str!("../../../../fixtures/amm/whirlpool.json"));
        assert!(samples.len() >= 10);
        let mut crossed = 0;
        for s in &samples {
            let arrays: Vec<&[u8]> = s.others.iter().map(Vec::as_slice).collect();
            let pool = whirlpool::decode(&s.pool, &arrays).expect("decodes");
            let swap = pool.swap(s.a_to_b, s.amount_in).expect("inside the known ticks");
            eprintln!(
                "{} in {:>13} ours {:>13} chain {:>13} diff {:>3} crossed {}",
                if s.a_to_b { "A→B" } else { "B→A" },
                s.amount_in,
                swap.amount_out,
                s.amount_out,
                swap.amount_out as i64 - s.amount_out as i64,
                swap.ticks_crossed
            );
            crossed += swap.ticks_crossed;
            assert_eq!(swap.amount_out, s.amount_out, "{} in, a_to_b {}", s.amount_in, s.a_to_b);
        }
        assert!(crossed > 0, "the samples include swaps that cross initialized ticks");
    }

    #[test]
    fn raydium_swaps_match_what_the_program_paid_on_mainnet() {
        let samples = samples(include_str!("../../../../fixtures/amm/raydium.json"));
        assert!(samples.len() >= 10);
        let (mut crossed, mut worst) = (0, 0f64);
        for s in &samples {
            let config = s.others.iter().find(|a| a.len() == 117).expect("AmmConfig");
            let arrays: Vec<&[u8]> = s.others.iter().filter(|a| a.len() != 117).map(Vec::as_slice).collect();
            let pool = raydium::decode(&s.pool, config, &arrays).expect("decodes");
            let swap = pool.swap(s.a_to_b, s.amount_in).expect("inside the known ticks");
            eprintln!(
                "{} in {:>13} ours {:>13} chain {:>13} diff {:>3} crossed {}",
                if s.a_to_b { "A→B" } else { "B→A" },
                s.amount_in,
                swap.amount_out,
                s.amount_out,
                swap.amount_out as i64 - s.amount_out as i64,
                swap.ticks_crossed
            );
            crossed += swap.ticks_crossed;
            // Exact inside a tick range. Across a tick the program's own
            // tick-to-price table differs from the exact value in its last
            // bits: within one part in ten million (0.001 bp).
            let off = swap.amount_out.abs_diff(s.amount_out);
            let allowed = if swap.ticks_crossed == 0 { 0 } else { s.amount_out / 10_000_000 };
            assert!(off <= allowed, "{} in, a_to_b {}: off by {off}", s.amount_in, s.a_to_b);
            worst = worst.max(off as f64 / s.amount_out as f64);
        }
        assert!(crossed > 0, "the samples include swaps that cross initialized ticks");
        eprintln!("largest relative difference: {worst:.1e}");
    }

    #[test]
    fn tick_arrays_start_on_multiples_of_their_span() {
        assert_eq!(array_start(-21_211, 4, WHIRLPOOL_ARRAY_TICKS), -21_472);
        assert_eq!(array_start(-21_211, 1, RAYDIUM_ARRAY_TICKS), -21_240);
        assert_eq!(array_start(0, 4, WHIRLPOOL_ARRAY_TICKS), 0);
        assert_eq!(array_start(351, 4, WHIRLPOOL_ARRAY_TICKS), 0);
        assert_eq!(array_start(-1, 4, WHIRLPOOL_ARRAY_TICKS), -352);
    }

    #[test]
    fn tick_array_addresses_are_the_ones_on_chain() {
        // read from mainnet 2026-10-03: each account exists and stores this start
        let orca: Address = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE".parse().unwrap();
        assert_eq!(
            whirlpool::tick_array(&orca, -21_472).unwrap().to_string(),
            "6hA1LN1fzCiXqymDiQXeBFn5da1b7STP1L7JmDc6hR3M"
        );
        assert_eq!(
            whirlpool::tick_array(&orca, -21_824).unwrap().to_string(),
            "D3461zSTVPNdBFPRk2b6zpqQ93g2LW5Kw2potgMdxNJP"
        );
    }
}
