//! Fixed-point arithmetic for pool math: 256-bit products and quotients,
//! and the square-root price of a tick in Q64.64. Written from the
//! mathematics (price = 1.0001^tick), not from any pool program's source.

/// A 256-bit unsigned integer as two halves.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct U256 {
    pub hi: u128,
    pub lo: u128,
}

const LOW: u128 = u64::MAX as u128;

impl U256 {
    pub const fn from(lo: u128) -> U256 {
        U256 { hi: 0, lo }
    }

    /// `a × b`, exact.
    pub fn mul(a: u128, b: u128) -> U256 {
        let (a1, a0, b1, b0) = (a >> 64, a & LOW, b >> 64, b & LOW);
        let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
        let mid = (p00 >> 64) + (p01 & LOW) + (p10 & LOW);
        U256 { hi: p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64), lo: (p00 & LOW) | (mid << 64) }
    }

    /// `self << n` for `n < 128`, or `None` if bits would be lost.
    pub fn checked_shl(self, n: u32) -> Option<U256> {
        if n == 0 {
            return Some(self);
        }
        if n >= 128 || self.hi >> (128 - n) != 0 {
            return None;
        }
        Some(U256 { hi: (self.hi << n) | (self.lo >> (128 - n)), lo: self.lo << n })
    }

    pub fn checked_add(self, other: U256) -> Option<U256> {
        let (lo, carry) = self.lo.overflowing_add(other.lo);
        Some(U256 { hi: self.hi.checked_add(other.hi)?.checked_add(carry as u128)?, lo })
    }

    /// Quotient and remainder of `self / d`.
    pub fn div_rem(self, d: u128) -> Option<(U256, u128)> {
        if d == 0 {
            return None;
        }
        let (mut q, mut rem) = (U256 { hi: 0, lo: 0 }, 0u128);
        for i in (0..256).rev() {
            let bit = if i >= 128 { self.hi >> (i - 128) } else { self.lo >> i } & 1;
            let carry = rem >> 127;
            rem = (rem << 1) | bit;
            if carry == 1 || rem >= d {
                rem = rem.wrapping_sub(d);
                if i >= 128 {
                    q.hi |= 1 << (i - 128);
                } else {
                    q.lo |= 1 << i;
                }
            }
        }
        Some((q, rem))
    }

    /// The value, if it fits 128 bits.
    pub fn low(self) -> Option<u128> {
        (self.hi == 0).then_some(self.lo)
    }
}

/// `⌊a × b / d⌋`, or `None` if it does not fit (or `d` is 0).
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    U256::mul(a, b).div_rem(d)?.0.low()
}

/// `⌈a × b / d⌉`.
pub fn mul_div_up(a: u128, b: u128, d: u128) -> Option<u128> {
    let (q, rem) = U256::mul(a, b).div_rem(d)?;
    q.low()?.checked_add((rem != 0) as u128)
}

pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;

/// `⌊2^128 × 1.0001^(−2^i / 2)⌋` for bit `i` of a tick, computed with exact
/// integer arithmetic (the first is an integer square root, the rest are
/// powers of 10000/10001).
const HALF_TICK_POWERS: [u128; 19] = [
    0xfffcb933bd6fad37aa2d162d1a594001,
    0xfff97272373d413259a46990580e2139,
    0xfff2e50f5f656932ef12357cf3c7fdcb,
    0xffe5caca7e10e4e61c3624eaa0941ccf,
    0xffcb9843d60f6159c9db58835c926643,
    0xff973b41fa98c081472e6896dfb254bf,
    0xff2ea16466c96a3843ec78b326b52860,
    0xfe5dee046a99a2a811c461f1969c3052,
    0xfcbe86c7900a88aedcffc83b479aa3a3,
    0xf987a7253ac413176f2b074cf7815e53,
    0xf3392b0822b70005940c7a398e4b70f2,
    0xe7159475a2c29b7443b29c7fa6e889d8,
    0xd097f3bdfd2022b8845ad8f792aa5825,
    0xa9f746462d870fdf8a65dc1f90e061e4,
    0x70d869a156d2a1b890bb3df62baf32f6,
    0x31be135f97d08fd981231505542fcfa5,
    0x09aa508b5b7a84e1c677de54f3e99bc8,
    0x005d6af8dedb81196699c329225ee604,
    0x00002216e584f5fa1ea926041bedfe97,
];

/// `√(1.0001^tick)` in Q64.64, within one unit of the exact value (a
/// relative error below 10⁻¹⁸ at the prices pools trade at). A pool program's
/// own table may differ in that last unit.
pub fn sqrt_price_at_tick(tick: i32) -> Option<u128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let mut r = u128::MAX; // 1.0 in Q0.128
    for (i, power) in HALF_TICK_POWERS.iter().enumerate() {
        if (tick.unsigned_abs() >> i) & 1 == 1 {
            r = U256::mul(r, *power).hi;
        }
    }
    if tick > 0 { U256 { hi: 1 << 64, lo: 0 }.div_rem(r)?.0.low() } else { Some(r >> 64) }
}

/// The largest tick whose square-root price is at most `sqrt_price`.
pub fn tick_at_sqrt_price(sqrt_price: u128) -> Option<i32> {
    if sqrt_price < sqrt_price_at_tick(MIN_TICK)? {
        return None;
    }
    let (mut lo, mut hi) = (MIN_TICK, MAX_TICK);
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if sqrt_price_at_tick(mid)? <= sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_products_and_quotients_are_exact() {
        assert_eq!(U256::mul(u128::MAX, u128::MAX), U256 { hi: u128::MAX - 1, lo: 1 });
        assert_eq!(U256::mul(1 << 100, 1 << 100), U256 { hi: 1 << 72, lo: 0 });
        assert_eq!(U256::mul(7, 6), U256::from(42));
        // (2^128 − 1)² / (2^128 − 1) = 2^128 − 1, nothing left over
        assert_eq!(mul_div(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(mul_div(u128::MAX, u128::MAX, 1), None, "does not fit");
        assert_eq!(mul_div(10, 10, 0), None);
        assert_eq!((mul_div(10, 10, 3), mul_div_up(10, 10, 3)), (Some(33), Some(34)));
        assert_eq!((mul_div(10, 10, 4), mul_div_up(10, 10, 4)), (Some(25), Some(25)));
        // a quotient wider than 128 bits, with a remainder
        let (q, rem) = U256 { hi: 5, lo: 7 }.div_rem(2).unwrap();
        assert_eq!((q, rem), (U256 { hi: 2, lo: (1 << 127) + 3 }, 1));
        assert_eq!(U256::from(1).checked_shl(127), Some(U256::from(1 << 127)));
        assert_eq!(U256::from(3).checked_shl(127), Some(U256 { hi: 1, lo: 1 << 127 }));
        assert_eq!(U256 { hi: 1 << 127, lo: 0 }.checked_shl(1), None);
        assert_eq!(U256 { hi: 1, lo: u128::MAX }.checked_add(U256::from(1)), Some(U256 { hi: 2, lo: 0 }));
        assert_eq!(U256 { hi: u128::MAX, lo: u128::MAX }.checked_add(U256::from(1)), None);
    }

    #[test]
    fn wide_division_agrees_with_native_division() {
        // products that fit 128 bits can be checked against u128 arithmetic
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..2_000 {
            let mut next = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u128
            };
            let (a, b, d) = (next(), next(), next() >> (next() % 60) | 1);
            assert_eq!(mul_div(a, b, d), Some(a * b / d), "{a} × {b} / {d}");
            assert_eq!(mul_div_up(a, b, d), Some((a * b).div_ceil(d)));
        }
    }

    #[test]
    fn square_root_prices_match_values_computed_exactly() {
        // ⌊2^64 × √(1.0001^tick)⌋ from big-integer arithmetic (Python, isqrt)
        for (tick, exact) in [
            (0, 18_446_744_073_709_551_615u128),
            (1, 18_447_666_387_855_959_850),
            (-1, 18_445_821_805_675_392_311),
            (-21_211, 6_387_806_290_388_874_463),
            (-21_210, 6_388_125_672_719_035_257),
        ] {
            let ours = sqrt_price_at_tick(tick).unwrap();
            assert!(ours.abs_diff(exact) <= 1, "tick {tick}: {ours} vs {exact}");
        }
        // the ends of the range: 2^-32 and 2^32 in Q64.64
        assert!(sqrt_price_at_tick(MIN_TICK).unwrap().abs_diff(1 << 32) < 1 << 17);
        assert!(sqrt_price_at_tick(MAX_TICK).unwrap().abs_diff(1 << 96) < 1 << 81);
        assert_eq!(sqrt_price_at_tick(MAX_TICK + 1), None);
        let mut last = 0;
        for tick in (MIN_TICK..=MAX_TICK).step_by(997) {
            let p = sqrt_price_at_tick(tick).unwrap();
            assert!(p > last, "increasing at {tick}");
            last = p;
        }
    }

    #[test]
    fn mainnet_pools_sit_inside_their_tick() {
        // (tick_current, sqrt_price) read from chain at slot 452,998,835:
        // Orca Whirlpool SOL/USDC and Raydium CLMM SOL/USDC
        for (tick, sqrt_price) in [(-21_211, 6_387_870_993_612_515_313u128), (-21_211, 6_387_858_338_723_441_435)] {
            assert!(sqrt_price_at_tick(tick).unwrap() <= sqrt_price);
            assert!(sqrt_price < sqrt_price_at_tick(tick + 1).unwrap());
            assert_eq!(tick_at_sqrt_price(sqrt_price), Some(tick));
        }
        assert_eq!(tick_at_sqrt_price(sqrt_price_at_tick(500).unwrap()), Some(500));
        assert_eq!(tick_at_sqrt_price(sqrt_price_at_tick(500).unwrap() - 1), Some(499));
        assert_eq!(tick_at_sqrt_price(1), None);
    }
}
