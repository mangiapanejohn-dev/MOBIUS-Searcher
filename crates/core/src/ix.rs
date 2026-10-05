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

/// SPL Token `TransferChecked` (instruction 12: amount u64 LE, decimals):
/// `amount` atoms of `mint` from the token account `from` to the token
/// account `to`, signed by `owner`.
pub fn token_transfer_checked_ix(
    from: Address,
    mint: Address,
    to: Address,
    owner: Address,
    amount: u64,
    decimals: u8,
) -> RawInstruction {
    let mut d = vec![12u8];
    d.extend_from_slice(&amount.to_le_bytes());
    d.push(decimals);
    let meta = |pubkey, is_signer, is_writable| RawAccountMeta { pubkey, is_signer, is_writable };
    RawInstruction {
        program_id: crate::address::well_known::addr(crate::address::well_known::TOKEN_PROGRAM),
        accounts: vec![
            meta(from, false, true),
            meta(mint, false, false),
            meta(to, false, true),
            meta(owner, true, false),
        ],
        data: d,
    }
}

/// Associated-token-account `CreateIdempotent` (data `[1]`): opens `ata`, the
/// account of `owner` for `mint`, at `payer`'s expense; does nothing when it exists.
pub fn ata_create_idempotent_ix(payer: Address, ata: Address, owner: Address, mint: Address) -> RawInstruction {
    use crate::address::well_known::{ASSOCIATED_TOKEN_PROGRAM, SYSTEM_PROGRAM, TOKEN_PROGRAM, addr};
    let meta = |pubkey, is_signer, is_writable| RawAccountMeta { pubkey, is_signer, is_writable };
    RawInstruction {
        program_id: addr(ASSOCIATED_TOKEN_PROGRAM),
        accounts: vec![
            meta(payer, true, true),
            meta(ata, false, true),
            meta(owner, false, false),
            meta(mint, false, false),
            meta(addr(SYSTEM_PROGRAM), false, false),
            meta(addr(TOKEN_PROGRAM), false, false),
        ],
        data: vec![1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_transfer_and_account_opening_layouts() {
        let a = |n| Address([n; 32]);
        let ix = token_transfer_checked_ix(a(1), a(2), a(3), a(4), 2_005_052, 6);
        assert_eq!(ix.data[0], 12);
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 2_005_052);
        assert_eq!(ix.data[9], 6);
        let who: Vec<_> = ix.accounts.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect();
        assert_eq!(who, [(a(1), false, true), (a(2), false, false), (a(3), false, true), (a(4), true, false)]);
        let ix = ata_create_idempotent_ix(a(1), a(5), a(3), a(2));
        assert_eq!(ix.data, [1]);
        assert_eq!(ix.accounts.len(), 6);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable && ix.accounts[1].is_writable);
        assert_eq!((ix.accounts[1].pubkey, ix.accounts[2].pubkey, ix.accounts[3].pubkey), (a(5), a(3), a(2)));
    }

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
