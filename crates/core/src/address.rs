//! A 32-byte Solana address with base58 (de)serialization. Kept independent of
//! the solana crates so the domain model stays light; adapters convert.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 32]);

impl Address {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// `AbCd…WxYz` for compact display.
    pub fn short(&self) -> String {
        let s = self.to_string();
        if s.len() <= 10 { s } else { format!("{}…{}", &s[..4], &s[s.len() - 4..]) }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("invalid base58 address: {0}")]
    Base58(String),
    #[error("address must decode to 32 bytes, got {0}")]
    Length(usize),
}

impl FromStr for Address {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v = bs58::decode(s.trim()).into_vec().map_err(|e| AddressError::Base58(e.to_string()))?;
        let arr: [u8; 32] = v.as_slice().try_into().map_err(|_| AddressError::Length(v.len()))?;
        Ok(Address(arr))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(self.0).into_string())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({self})")
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub mod well_known {
    use super::Address;

    /// Parse a compile-time known base58 constant.
    pub fn addr(s: &str) -> Address {
        s.parse().expect("well-known address constant")
    }

    pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
    pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
    pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
    pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    pub const JUPITER_V6_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_base58_and_json() {
        let a: Address = well_known::USDC_MINT.parse().unwrap();
        assert_eq!(a.to_string(), well_known::USDC_MINT);
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(j, format!("\"{}\"", well_known::USDC_MINT));
        let b: Address = serde_json::from_str(&j).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.short(), "EPjF…Dt1v");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!("0OIl".parse::<Address>(), Err(AddressError::Base58(_))));
        assert!(matches!("abc".parse::<Address>(), Err(AddressError::Length(_))));
    }
}
