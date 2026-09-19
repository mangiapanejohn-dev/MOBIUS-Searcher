//! Integer money math. On-chain base units (lamports, token atoms) are always
//! integers; floats appear only at the display edge (`*_f64` helpers).

use serde::{Deserialize, Serialize};
use std::fmt;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
/// Base fee per signature (Solana fee structure).
pub const BASE_FEE_LAMPORTS_PER_SIGNATURE: u64 = 5_000;
pub const MICRO_LAMPORTS_PER_LAMPORT: u128 = 1_000_000;
/// Max compute units a single transaction may request.
pub const MAX_COMPUTE_UNITS_PER_TX: u32 = 1_400_000;
/// Rent-exempt minimum of a 165-byte SPL token account: (165 + 128) * 3480 * 2.
pub const TOKEN_ACCOUNT_RENT_LAMPORTS: u64 = 2_039_280;
/// Jito enforces a minimum bundle tip of 1000 lamports.
pub const JITO_MIN_TIP_LAMPORTS: u64 = 1_000;
/// v0 / legacy packet limit.
pub const MAX_TX_BYTES: usize = 1_232;
/// Account lock limit per transaction (128 is feature-gated and inactive).
pub const MAX_TX_ACCOUNTS: usize = 64;

pub const PPM_ONE: i64 = 1_000_000;

/// Signed ratio in parts-per-million. 1 bp = 100 ppm, 1% = 10_000 ppm.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ppm(pub i64);

impl Ppm {
    pub const ZERO: Ppm = Ppm(0);

    /// `num / den` in ppm, rounded toward negative infinity (conservative for
    /// edges: a positive edge is never rounded up). `None` if `den <= 0`.
    pub fn ratio(num: i128, den: i128) -> Option<Ppm> {
        if den <= 0 {
            return None;
        }
        let scaled = num.checked_mul(PPM_ONE as i128)?;
        let q = scaled.div_euclid(den);
        i64::try_from(q).ok().map(Ppm)
    }

    pub const fn from_bps(bps: i64) -> Ppm {
        Ppm(bps * 100)
    }

    /// Apply to an amount, rounding toward negative infinity.
    pub fn of(self, amount: u64) -> i128 {
        (amount as i128 * self.0 as i128).div_euclid(PPM_ONE as i128)
    }

    pub fn bps_f64(self) -> f64 {
        self.0 as f64 / 100.0
    }

    pub fn pct_f64(self) -> f64 {
        self.0 as f64 / 10_000.0
    }
}

impl fmt::Display for Ppm {
    /// Signed percent with 2 decimals, e.g. `+0.27%`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "+" };
        let abs = self.0.unsigned_abs();
        // ppm -> hundredths of a percent, round half away from zero
        let hundredths = (abs + 50) / 100;
        write!(f, "{sign}{}.{:02}%", hundredths / 100, hundredths % 100)
    }
}

/// Micro-USD (1e-6 USD). USDC atoms map 1:1.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UsdMicros(pub i64);

impl UsdMicros {
    pub const ZERO: UsdMicros = UsdMicros(0);

    pub fn f64(self) -> f64 {
        self.0 as f64 / 1e6
    }

    pub fn saturating_add(self, o: UsdMicros) -> UsdMicros {
        UsdMicros(self.0.saturating_add(o.0))
    }
}

impl fmt::Display for UsdMicros {
    /// `$1,234.56` / `-$0.04` (cents, round half away from zero).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.0 < 0;
        let cents = (self.0.unsigned_abs() + 5_000) / 10_000;
        let dollars = cents / 100;
        let c = cents % 100;
        let mut d = dollars.to_string();
        let mut i = d.len() as isize - 3;
        while i > 0 {
            d.insert(i as usize, ',');
            i -= 3;
        }
        if neg && cents > 0 { write!(f, "-${d}.{c:02}") } else { write!(f, "${d}.{c:02}") }
    }
}

/// USD price of one *whole* token, in micro-USD.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UsdPrice {
    pub micros_per_token: u64,
}

impl UsdPrice {
    pub fn new(micros_per_token: u64) -> Self {
        Self { micros_per_token }
    }

    /// Value of `atoms` of a token with `decimals`, rounded toward -inf.
    pub fn value(self, atoms: i128, decimals: u8) -> Option<UsdMicros> {
        let scale = 10i128.checked_pow(decimals as u32)?;
        let v = atoms.checked_mul(self.micros_per_token as i128)?.div_euclid(scale);
        i64::try_from(v).ok().map(UsdMicros)
    }

    /// Price implied by trading `in_atoms` of token A (decimals `in_dec`) for
    /// `out_atoms` of a USD stable (decimals `out_dec`, 1 atom-unit = $1).
    pub fn from_trade(in_atoms: u64, in_dec: u8, out_atoms: u64, out_dec: u8) -> Option<UsdPrice> {
        if in_atoms == 0 {
            return None;
        }
        // micros_per_token = out_atoms / 10^out_dec * 1e6 / (in_atoms / 10^in_dec)
        let num = (out_atoms as u128).checked_mul(1_000_000)?.checked_mul(10u128.checked_pow(in_dec as u32)?)?;
        let den = (in_atoms as u128).checked_mul(10u128.checked_pow(out_dec as u32)?)?;
        u64::try_from(num / den).ok().map(UsdPrice::new)
    }

    pub fn f64(self) -> f64 {
        self.micros_per_token as f64 / 1e6
    }
}

/// Priority fee in lamports: `ceil(cu_limit * cu_price_micro_lamports / 1e6)`.
pub fn priority_fee_lamports(cu_limit: u32, cu_price_micro_lamports: u64) -> u64 {
    let prod = cu_limit as u128 * cu_price_micro_lamports as u128;
    let fee = prod.div_ceil(MICRO_LAMPORTS_PER_LAMPORT);
    u64::try_from(fee).unwrap_or(u64::MAX)
}

/// CU limit to request after simulation: `ceil(used * (1 + margin))`, clamped to
/// `[used, MAX_COMPUTE_UNITS_PER_TX]`.
pub fn cu_limit_with_margin(units_used: u32, margin: Ppm) -> u32 {
    let m = margin.0.max(0) as u128;
    let want = (units_used as u128 * (PPM_ONE as u128 + m)).div_ceil(PPM_ONE as u128);
    want.clamp(units_used as u128, MAX_COMPUTE_UNITS_PER_TX as u128) as u32
}

/// Parse a non-negative decimal string (e.g. `"0.01"`, `"1.5"`) into an integer
/// with `decimals` fractional digits, without going through floats.
pub fn parse_decimal(s: &str, decimals: u8) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty decimal".into());
    }
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid decimal `{s}`"));
    }
    if frac.len() > decimals as usize {
        return Err(format!("`{s}` has more than {decimals} fractional digits"));
    }
    let scale = 10u128.pow(decimals as u32);
    let int_v: u128 = if int.is_empty() { 0 } else { int.parse().map_err(|e| format!("{e}"))? };
    let mut frac_v: u128 = if frac.is_empty() { 0 } else { frac.parse().map_err(|e| format!("{e}"))? };
    frac_v *= 10u128.pow((decimals as usize - frac.len()) as u32);
    let v = int_v.checked_mul(scale).and_then(|x| x.checked_add(frac_v)).ok_or_else(|| format!("`{s}` overflows"))?;
    u64::try_from(v).map_err(|_| format!("`{s}` overflows u64"))
}

/// Format atoms as a decimal token amount with up to `max_frac` digits.
pub fn format_atoms(atoms: i128, decimals: u8, max_frac: u8) -> String {
    let neg = atoms < 0;
    let a = atoms.unsigned_abs();
    let scale = 10u128.pow(decimals as u32);
    let int = a / scale;
    let frac = a % scale;
    let mut frac_s = format!("{:0width$}", frac, width = decimals as usize);
    frac_s.truncate(max_frac.min(decimals) as usize);
    let sign = if neg { "-" } else { "" };
    if frac_s.is_empty() { format!("{sign}{int}") } else { format!("{sign}{int}.{frac_s}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_fee_rounds_up() {
        assert_eq!(priority_fee_lamports(200_000, 1), 1); // 0.2 -> 1
        assert_eq!(priority_fee_lamports(1_000_000, 1), 1);
        assert_eq!(priority_fee_lamports(1_000_001, 1), 2);
        assert_eq!(priority_fee_lamports(300_000, 2_719), 816); // 815.7
        assert_eq!(priority_fee_lamports(0, 1_000_000), 0);
        assert!(priority_fee_lamports(u32::MAX, u64::MAX) > 0);
    }

    #[test]
    fn cu_margin() {
        assert_eq!(cu_limit_with_margin(100_000, Ppm(200_000)), 120_000);
        assert_eq!(cu_limit_with_margin(100_001, Ppm(200_000)), 120_002); // ceil(120001.2)
        assert_eq!(cu_limit_with_margin(1_300_000, Ppm(200_000)), MAX_COMPUTE_UNITS_PER_TX);
        assert_eq!(cu_limit_with_margin(50_000, Ppm(-5)), 50_000);
    }

    #[test]
    fn ppm_ratio_and_display() {
        assert_eq!(Ppm::ratio(27, 10_000), Some(Ppm(2_700)));
        assert_eq!(Ppm::ratio(-1, 3), Some(Ppm(-333_334))); // floor
        assert_eq!(Ppm::ratio(1, 0), None);
        assert_eq!(Ppm(2_700).to_string(), "+0.27%");
        assert_eq!(Ppm(-300).to_string(), "-0.03%");
        assert_eq!(Ppm::from_bps(5), Ppm(500));
        assert_eq!(Ppm(500_000).of(3), 1);
        assert_eq!(Ppm(-500_000).of(3), -2);
    }

    #[test]
    fn usd_display() {
        assert_eq!(UsdMicros(104_210_000).to_string(), "$104.21");
        assert_eq!(UsdMicros(-40_000).to_string(), "-$0.04");
        assert_eq!(UsdMicros(1_234_567_890).to_string(), "$1,234.57");
        assert_eq!(UsdMicros(-4_000).to_string(), "$0.00");
    }

    #[test]
    fn price_from_trade_and_value() {
        // 0.1 SOL -> 10.536494 USDC  => $105.36494 / SOL
        let p = UsdPrice::from_trade(100_000_000, 9, 10_536_494, 6).unwrap();
        assert_eq!(p.micros_per_token, 105_364_940);
        assert_eq!(p.value(1_000_000_000, 9), Some(UsdMicros(105_364_940)));
        assert_eq!(p.value(-5_000, 9), Some(UsdMicros(-527)));
        assert_eq!(UsdPrice::from_trade(0, 9, 1, 6), None);
    }

    #[test]
    fn decimal_parse() {
        assert_eq!(parse_decimal("0.01", 6), Ok(10_000));
        assert_eq!(parse_decimal("1", 9), Ok(1_000_000_000));
        assert_eq!(parse_decimal(".5", 2), Ok(50));
        assert!(parse_decimal("1.234", 2).is_err());
        assert!(parse_decimal("-1", 2).is_err());
        assert!(parse_decimal("1e3", 2).is_err());
        assert!(parse_decimal("99999999999999999999", 0).is_err());
    }

    #[test]
    fn atoms_format() {
        assert_eq!(format_atoms(1_500_000_000, 9, 4), "1.5000");
        assert_eq!(format_atoms(-2_039_280, 9, 6), "-0.002039");
        assert_eq!(format_atoms(12, 0, 3), "12");
    }
}
