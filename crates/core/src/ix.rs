//! Provider-neutral raw instructions (what an adapter hands to the assembler).

use crate::address::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawAccountMeta {
    pub pubkey: Address,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawInstruction {
    pub program_id: Address,
    pub accounts: Vec<RawAccountMeta>,
    pub data: Vec<u8>,
}

/// Everything needed to put one swap leg into a transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegInstructions {
    pub compute_budget: Vec<RawInstruction>,
    pub setup: Vec<RawInstruction>,
    pub swap: RawInstruction,
    pub cleanup: Option<RawInstruction>,
    pub other: Vec<RawInstruction>,
    /// Address lookup tables: (table address, addresses in table order).
    pub lookup_tables: Vec<(Address, Vec<Address>)>,
    pub blockhash: [u8; 32],
    pub last_valid_block_height: u64,
}

/// Decode `SetComputeUnitPrice` (discriminator 3, u64 LE micro-lamports).
pub fn decode_cu_price(ix: &RawInstruction) -> Option<u64> {
    if ix.program_id.to_string() != crate::address::well_known::COMPUTE_BUDGET_PROGRAM {
        return None;
    }
    match ix.data.as_slice() {
        [3, rest @ ..] if rest.len() == 8 => Some(u64::from_le_bytes(rest.try_into().ok()?)),
        _ => None,
    }
}

/// Decode `SetComputeUnitLimit` (discriminator 2, u32 LE).
pub fn decode_cu_limit(ix: &RawInstruction) -> Option<u32> {
    if ix.program_id.to_string() != crate::address::well_known::COMPUTE_BUDGET_PROGRAM {
        return None;
    }
    match ix.data.as_slice() {
        [2, rest @ ..] if rest.len() == 4 => Some(u32::from_le_bytes(rest.try_into().ok()?)),
        _ => None,
    }
}

pub fn compute_budget_ix(data: Vec<u8>) -> RawInstruction {
    RawInstruction {
        program_id: crate::address::well_known::addr(crate::address::well_known::COMPUTE_BUDGET_PROGRAM),
        accounts: vec![],
        data,
    }
}

pub fn set_cu_limit_ix(units: u32) -> RawInstruction {
    let mut d = vec![2u8];
    d.extend_from_slice(&units.to_le_bytes());
    compute_budget_ix(d)
}

pub fn set_cu_price_ix(micro_lamports: u64) -> RawInstruction {
    let mut d = vec![3u8];
    d.extend_from_slice(&micro_lamports.to_le_bytes());
    compute_budget_ix(d)
}

/// System program `Transfer { lamports }` (instruction index 2, u64 LE).
pub fn system_transfer_ix(from: Address, to: Address, lamports: u64) -> RawInstruction {
    let mut d = 2u32.to_le_bytes().to_vec();
    d.extend_from_slice(&lamports.to_le_bytes());
    RawInstruction {
        program_id: crate::address::well_known::addr(crate::address::well_known::SYSTEM_PROGRAM),
        accounts: vec![
            RawAccountMeta { pubkey: from, is_signer: true, is_writable: true },
            RawAccountMeta { pubkey: to, is_signer: false, is_writable: true },
        ],
        data: d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_budget_roundtrip() {
        assert_eq!(decode_cu_price(&set_cu_price_ix(2_719)), Some(2_719));
        assert_eq!(decode_cu_limit(&set_cu_limit_ix(360_000)), Some(360_000));
        assert_eq!(decode_cu_limit(&set_cu_price_ix(1)), None);
        // Jupiter fixture data "A58KAAAAAAAA" = [3, 0x9f, 0x0a, 0...]
        let jup = compute_budget_ix(vec![3, 0x9f, 0x0a, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_cu_price(&jup), Some(2_719));
    }

    #[test]
    fn transfer_layout() {
        let ix = system_transfer_ix(Address([1; 32]), Address([2; 32]), 10_000);
        assert_eq!(&ix.data[..4], &[2, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(ix.data[4..].try_into().unwrap()), 10_000);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable && !ix.accounts[1].is_signer);
    }
}
