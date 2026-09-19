//! Transaction assembly: provider-neutral `LegInstructions` → v0 transactions
//! with address lookup tables. Our own ComputeBudget instructions replace the
//! provider's; the Jito tip is a transfer inside the (last) transaction.
//!
//! Instruction order per leg follows Jupiter's `/build` guide:
//! setup → swap → cleanup → other.

use searcher_core::Address;
use searcher_core::address::well_known;
use searcher_core::ix::{LegInstructions, RawInstruction, set_cu_limit_ix, set_cu_price_ix, system_transfer_ix};
use searcher_core::units::{MAX_TX_ACCOUNTS, MAX_TX_BYTES};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::{BTreeMap, HashSet};

pub fn pk(a: &Address) -> Pubkey {
    Pubkey::new_from_array(a.to_bytes())
}

fn to_ix(r: &RawInstruction) -> Instruction {
    Instruction {
        program_id: pk(&r.program_id),
        accounts: r
            .accounts
            .iter()
            .map(|m| AccountMeta { pubkey: pk(&m.pubkey), is_signer: m.is_signer, is_writable: m.is_writable })
            .collect(),
        data: r.data.clone(),
    }
}

/// Associated-token-account `CreateIdempotent` (ATA program, data `[1]`).
/// Returns the ATA address (account index 1).
pub fn ata_create_target(ix: &RawInstruction) -> Option<Address> {
    (ix.program_id.to_string() == well_known::ASSOCIATED_TOKEN_PROGRAM && ix.data == [1])
        .then(|| ix.accounts.get(1).map(|m| m.pubkey))
        .flatten()
}

#[derive(Clone, Debug)]
pub struct AssemblyParams<'a> {
    pub payer: Address,
    pub cu_limit: u32,
    pub cu_price_micro: u64,
    /// (tip account, lamports). Never referenced through an ALT.
    pub tip: Option<(Address, u64)>,
    /// Optional read-only `jitodontfront…` account appended to the tip ix.
    pub dont_front: Option<Address>,
    /// ATAs known to exist: their `CreateIdempotent` no-ops are dropped.
    pub existing_atas: &'a HashSet<Address>,
    pub blockhash: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct AssembledTx {
    pub tx: VersionedTransaction,
    pub wire: Vec<u8>,
    pub size: usize,
    /// Static keys + ALT-loaded keys (account locks).
    pub accounts: usize,
    pub cu_limit: u32,
    pub cu_price_micro: u64,
    pub tip_lamports: u64,
    /// ATAs this transaction leaves created (rent locked) at its end.
    pub creates_atas: Vec<Address>,
    /// `CreateIdempotent` instructions kept (including ones closed again in-tx).
    pub ata_create_ixs: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssemblyError {
    #[error("compile: {0}")]
    Compile(String),
    #[error("transaction too large: {size} > {MAX_TX_BYTES} bytes")]
    TooLarge { size: usize },
    #[error("too many account locks: {n} > {MAX_TX_ACCOUNTS}")]
    TooManyAccounts { n: usize },
    #[error("serialize: {0}")]
    Serialize(String),
}

impl AssemblyError {
    pub fn is_size_limit(&self) -> bool {
        matches!(self, AssemblyError::TooLarge { .. } | AssemblyError::TooManyAccounts { .. })
    }
}

/// SPL Token `CloseAccount` (instruction 9): returns the closed account.
pub fn token_close_target(ix: &RawInstruction) -> Option<Address> {
    let token = ix.program_id.to_string();
    let is_token = token == well_known::TOKEN_PROGRAM || token == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
    (is_token && ix.data.first() == Some(&9)).then(|| ix.accounts.first().map(|m| m.pubkey)).flatten()
}

/// Instructions of one leg. `existing` is the set of ATAs that exist at this
/// point *inside the transaction*: accounts closed by an earlier leg's
/// cleanup are removed from it, so their `CreateIdempotent` is kept.
/// wSOL accounts are volatile (Jupiter's unwrap closes them, including our
/// own previous trades), so their `CreateIdempotent` is never dropped.
fn is_wsol_create(ix: &RawInstruction) -> bool {
    ata_create_target(ix).is_some()
        && ix.accounts.get(3).map(|m| m.pubkey.to_string()).as_deref() == Some(well_known::WSOL_MINT)
}

fn leg_instructions(
    leg: &LegInstructions,
    existing: &mut HashSet<Address>,
    creates: &mut Vec<Address>,
) -> Vec<RawInstruction> {
    let mut out = Vec::new();
    for ix in &leg.setup {
        match ata_create_target(ix) {
            Some(ata) if existing.contains(&ata) && !is_wsol_create(ix) => continue,
            Some(ata) => {
                if !creates.contains(&ata) {
                    creates.push(ata);
                }
                existing.insert(ata);
                out.push(ix.clone());
            }
            None => out.push(ix.clone()),
        }
    }
    out.push(leg.swap.clone());
    for ix in leg.cleanup.iter().chain(&leg.other) {
        if let Some(closed) = token_close_target(ix) {
            existing.remove(&closed);
            // Created and closed inside the same transaction: rent comes back,
            // nothing stays locked, so it is not an ATA/rent cost.
            creates.retain(|a| *a != closed);
        }
        out.push(ix.clone());
    }
    out
}

fn tip_ix(p: &AssemblyParams<'_>) -> Option<RawInstruction> {
    let (acct, lamports) = p.tip?;
    let mut ix = system_transfer_ix(p.payer, acct, lamports);
    if let Some(df) = p.dont_front {
        ix.accounts.push(searcher_core::ix::RawAccountMeta { pubkey: df, is_signer: false, is_writable: false });
    }
    Some(ix)
}

fn lookup_tables(legs: &[&LegInstructions], exclude: &HashSet<Pubkey>) -> Vec<AddressLookupTableAccount> {
    // Union by table address; tip account must not be resolved through a table.
    let mut map: BTreeMap<Address, Vec<Address>> = BTreeMap::new();
    for l in legs {
        for (k, v) in &l.lookup_tables {
            map.entry(*k).or_insert_with(|| v.clone());
        }
    }
    map.into_iter()
        .map(|(k, v)| AddressLookupTableAccount {
            key: pk(&k),
            // Keep table order (indexes must match on-chain order): replace
            // excluded keys by a key that can never match an instruction.
            addresses: v
                .iter()
                .map(|a| if exclude.contains(&pk(a)) { Pubkey::new_from_array([0xff; 32]) } else { pk(a) })
                .collect(),
        })
        .collect()
}

fn compile(
    ixs: Vec<RawInstruction>,
    legs: &[&LegInstructions],
    p: &AssemblyParams<'_>,
    cu_limit: u32,
    tip_lamports: u64,
    creates_atas: Vec<Address>,
) -> Result<AssembledTx, AssemblyError> {
    let mut all = vec![set_cu_limit_ix(cu_limit)];
    if p.cu_price_micro > 0 {
        all.push(set_cu_price_ix(p.cu_price_micro));
    }
    all.extend(ixs);
    let exclude: HashSet<Pubkey> = p.tip.iter().map(|(a, _)| pk(a)).chain(p.dont_front.iter().map(pk)).collect();
    let instructions: Vec<Instruction> = all.iter().map(to_ix).collect();
    let alts = lookup_tables(legs, &exclude);
    let msg = v0::Message::try_compile(&pk(&p.payer), &instructions, &alts, Hash::new_from_array(p.blockhash))
        .map_err(|e| AssemblyError::Compile(e.to_string()))?;
    let loaded: usize =
        msg.address_table_lookups.iter().map(|l| l.writable_indexes.len() + l.readonly_indexes.len()).sum();
    let accounts = msg.account_keys.len() + loaded;
    let signers = msg.header.num_required_signatures as usize;
    let tx =
        VersionedTransaction { signatures: vec![Signature::default(); signers], message: VersionedMessage::V0(msg) };
    let wire = wincode::serialize(&tx).map_err(|e| AssemblyError::Serialize(e.to_string()))?;
    let size = wire.len();
    if accounts > MAX_TX_ACCOUNTS {
        return Err(AssemblyError::TooManyAccounts { n: accounts });
    }
    if size > MAX_TX_BYTES {
        return Err(AssemblyError::TooLarge { size });
    }
    let ata_create_ixs = all.iter().filter(|ix| ata_create_target(ix).is_some()).count();
    Ok(AssembledTx {
        tx,
        wire,
        size,
        accounts,
        cu_limit,
        cu_price_micro: p.cu_price_micro,
        tip_lamports,
        creates_atas,
        ata_create_ixs,
    })
}

/// All legs in one transaction (exact simulation, atomic on-chain).
pub fn compose_single(legs: &[&LegInstructions], p: &AssemblyParams<'_>) -> Result<AssembledTx, AssemblyError> {
    let mut creates = Vec::new();
    let mut ixs = Vec::new();
    let mut existing = p.existing_atas.clone();
    for l in legs {
        ixs.extend(leg_instructions(l, &mut existing, &mut creates));
    }
    let tip = p.tip.map(|t| t.1).unwrap_or(0);
    ixs.extend(tip_ix(p));
    compile(ixs, legs, p, p.cu_limit, tip, creates)
}

/// One transaction per leg; the tip rides in the last one. `cu_limits`
/// gives a per-transaction limit (use `p.cu_limit` for all when simulating).
pub fn compose_bundle(
    legs: &[&LegInstructions],
    p: &AssemblyParams<'_>,
    cu_limits: Option<&[u32]>,
) -> Result<Vec<AssembledTx>, AssemblyError> {
    let mut out = Vec::with_capacity(legs.len());
    // Bundle txs run sequentially: state carries over between them.
    let mut existing = p.existing_atas.clone();
    for (i, l) in legs.iter().enumerate() {
        let mut creates = Vec::new();
        let mut ixs = leg_instructions(l, &mut existing, &mut creates);
        let last = i + 1 == legs.len();
        if last {
            ixs.extend(tip_ix(p));
        }
        let limit = cu_limits.and_then(|c| c.get(i).copied()).unwrap_or(p.cu_limit);
        let tip = if last { p.tip.map(|t| t.1).unwrap_or(0) } else { 0 };
        out.push(compile(ixs, &[l], p, limit, tip, creates)?);
    }
    Ok(out)
}

/// Full ordered account list of a v0 message, matching the order of
/// `preBalances`/`postBalances`: static keys, then all ALT-loaded writable
/// addresses (table order), then all ALT-loaded readonly addresses.
/// `None` if any lookup index cannot be resolved from the legs' tables.
pub fn message_account_keys(tx: &VersionedTransaction, legs: &[&LegInstructions]) -> Option<Vec<Address>> {
    let VersionedMessage::V0(m) = &tx.message else {
        return Some(tx.message.static_account_keys().iter().map(|k| Address(k.to_bytes())).collect());
    };
    let tables: BTreeMap<Pubkey, Vec<Address>> =
        legs.iter().flat_map(|l| l.lookup_tables.iter()).map(|(k, v)| (pk(k), v.clone())).collect();
    let mut out: Vec<Address> = m.account_keys.iter().map(|k| Address(k.to_bytes())).collect();
    let resolve = |key: &Pubkey, idx: &u8| tables.get(key).and_then(|t| t.get(*idx as usize)).copied();
    for l in &m.address_table_lookups {
        for i in &l.writable_indexes {
            out.push(resolve(&l.account_key, i)?);
        }
    }
    for l in &m.address_table_lookups {
        for i in &l.readonly_indexes {
            out.push(resolve(&l.account_key, i)?);
        }
    }
    Some(out)
}

/// SOL-equivalent balance change of the taker: native lamports plus the
/// lamports of the taker's wrapped-SOL account(s) (wSOL lamports = rent +
/// wrapped amount, so closing/recreating it nets out). Using only the
/// native balance is wrong when the taker already holds wSOL.
pub fn sol_equivalent_delta(
    keys: &[Address],
    taker: &Address,
    wsol_accounts: &[Address],
    pre: &[u64],
    post: &[u64],
) -> Option<i64> {
    if pre.len() != keys.len() || post.len() != keys.len() {
        return None;
    }
    let mut d: i64 = 0;
    let mut found_taker = false;
    for (i, k) in keys.iter().enumerate() {
        if k == taker || wsol_accounts.contains(k) {
            found_taker |= k == taker;
            d += post[i] as i64 - pre[i] as i64;
        }
    }
    found_taker.then_some(d)
}

/// Wrapped-SOL accounts touched by the legs (targets of wSOL close/create).
pub fn wsol_accounts(legs: &[&LegInstructions]) -> Vec<Address> {
    let wsol = well_known::addr(well_known::WSOL_MINT);
    let mut v = Vec::new();
    for l in legs {
        for ix in l.setup.iter().chain(l.cleanup.iter()).chain(&l.other) {
            let is_wsol_create = ata_create_target(ix).is_some() && ix.accounts.get(3).map(|m| m.pubkey) == Some(wsol);
            let target = if is_wsol_create { ata_create_target(ix) } else { None };
            let closed = token_close_target(ix);
            for t in [target, closed].into_iter().flatten() {
                if !v.contains(&t) {
                    v.push(t);
                }
            }
        }
    }
    v
}

/// Replace the signature slots after signing elsewhere, re-serialize.
pub fn reserialize(tx: &VersionedTransaction) -> Result<Vec<u8>, AssemblyError> {
    wincode::serialize(tx).map_err(|e| AssemblyError::Serialize(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::ix::decode_cu_limit;

    const UNCONSTRAINED: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_rtse.json");
    const LEG1: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_raydiumclmm.json");
    const LEG2: &str = include_str!("../../../fixtures/jupiter_build_usdc_sol_whirlpool.json");

    fn leg(json: &str, input: &str, output: &str) -> LegInstructions {
        let r: searcher_jupiter::wire::BuildResponse = serde_json::from_str(json).unwrap();
        let ctx = searcher_jupiter::adapter::LegContext {
            index: 0,
            slippage_spec: searcher_core::SlippageSpec::Rtse,
            mode: searcher_core::RoutingMode::Normal,
            dex_filter: searcher_core::DexFilter::Any,
            quoted_at: searcher_core::Ts(0),
            latency_ms: 0,
            request_id: None,
            expect_input: well_known::addr(input),
            expect_output: well_known::addr(output),
        };
        searcher_jupiter::adapter::to_leg(&r, ctx).unwrap().1
    }

    fn legs() -> (LegInstructions, LegInstructions) {
        (
            leg(LEG1, well_known::WSOL_MINT, well_known::USDC_MINT),
            leg(LEG2, well_known::USDC_MINT, well_known::WSOL_MINT),
        )
    }

    fn taker() -> Address {
        "F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe".parse().unwrap()
    }

    fn params<'a>(none: &'a HashSet<Address>, bh: [u8; 32], tip: bool) -> AssemblyParams<'a> {
        AssemblyParams {
            payer: taker(),
            cu_limit: 1_400_000,
            cu_price_micro: 2_719,
            tip: tip.then(|| (crate::KNOWN_TIP_ACCOUNT.parse().unwrap(), 10_000)),
            dont_front: None,
            existing_atas: none,
            blockhash: bh,
        }
    }

    #[test]
    fn two_constrained_legs_compose_into_one_v0_tx_with_tip() {
        let (l1, l2) = legs();
        let none = HashSet::new();
        let p = params(&none, l1.blockhash, true);
        let a = compose_single(&[&l1, &l2], &p).expect("dex-constrained round trip fits one tx");
        assert!(a.size <= MAX_TX_BYTES, "{}", a.size);
        assert!(a.accounts <= MAX_TX_ACCOUNTS, "{}", a.accounts);
        let VersionedMessage::V0(m) = &a.tx.message else { panic!("v0") };
        assert!(!m.address_table_lookups.is_empty(), "uses ALTs");
        assert_eq!(m.header.num_required_signatures, 1);
        assert_eq!(m.account_keys[0], pk(&taker()), "payer first");
        let tip_acct: Address = crate::KNOWN_TIP_ACCOUNT.parse().unwrap();
        assert!(m.account_keys.contains(&pk(&tip_acct)), "tip account is static, never via ALT");
        let cb = pk(&well_known::addr(well_known::COMPUTE_BUDGET_PROGRAM));
        let limits: Vec<u32> = m
            .instructions
            .iter()
            .filter(|ci| m.account_keys[ci.program_id_index as usize] == cb)
            .filter_map(|ci| {
                decode_cu_limit(&RawInstruction {
                    program_id: well_known::addr(well_known::COMPUTE_BUDGET_PROGRAM),
                    accounts: vec![],
                    data: ci.data.clone(),
                })
            })
            .collect();
        assert_eq!(limits, vec![1_400_000], "exactly one CU limit (ours)");
        let back: VersionedTransaction = wincode::deserialize(&a.wire).unwrap();
        assert_eq!(back, a.tx);
    }

    #[test]
    fn existing_atas_drop_noop_creates() {
        let (l1, l2) = legs();
        let targets: HashSet<Address> = l1.setup.iter().chain(&l2.setup).filter_map(ata_create_target).collect();
        assert!(!targets.is_empty());
        let none = HashSet::new();
        let with = compose_single(&[&l1, &l2], &params(&none, l1.blockhash, true)).unwrap();
        let without = compose_single(&[&l1, &l2], &params(&targets, l1.blockhash, true)).unwrap();
        assert!(without.size < with.size);
        assert!(without.creates_atas.len() < with.creates_atas.len());
        assert!(without.ata_create_ixs >= 1, "wSOL creates kept even when the account existed at check time");
        assert!(!with.creates_atas.is_empty());
    }

    #[test]
    fn ata_closed_by_earlier_leg_is_recreated_even_if_it_existed() {
        let (l1, l2) = legs();
        let wsol_ata = l1.cleanup.as_ref().and_then(token_close_target).expect("leg1 unwraps wSOL");
        let all: HashSet<Address> = l1.setup.iter().chain(&l2.setup).filter_map(ata_create_target).collect();
        assert!(all.contains(&wsol_ata));
        let a = compose_single(&[&l1, &l2], &params(&all, l1.blockhash, true)).unwrap();
        assert_eq!(a.ata_create_ixs, 2, "wSOL create is never dropped; leg 2 recreates what leg 1 closed");
        assert!(a.creates_atas.is_empty(), "recreated and closed again in-tx: no rent stays locked");
    }

    #[test]
    fn account_keys_cover_all_balances_and_sol_equivalent_delta() {
        let (l1, l2) = legs();
        let none = HashSet::new();
        let a = compose_single(&[&l1, &l2], &params(&none, l1.blockhash, true)).unwrap();
        let keys = message_account_keys(&a.tx, &[&l1, &l2]).expect("every ALT index resolves");
        assert_eq!(keys.len(), a.accounts);
        assert_eq!(keys[0], taker());
        assert!(message_account_keys(&a.tx, &[&l1]).is_none() || a.accounts == keys.len());
        let wsol = wsol_accounts(&[&l1, &l2]);
        assert_eq!(wsol.len(), 1);
        let wi = keys.iter().position(|k| *k == wsol[0]).unwrap();
        // taker +2.8M native, wSOL account closed (-2.0M rent+wrapped) → +0.76M
        let mut pre = vec![0u64; keys.len()];
        let mut post = vec![0u64; keys.len()];
        pre[0] = 10_000_000;
        post[0] = 12_800_000;
        pre[wi] = 2_039_280;
        post[wi] = 0;
        assert_eq!(sol_equivalent_delta(&keys, &taker(), &wsol, &pre, &post), Some(760_720));
        assert_eq!(sol_equivalent_delta(&keys, &taker(), &wsol, &pre[1..], &post), None);
    }

    #[test]
    fn unconstrained_64_account_route_does_not_fit_and_fails_cleanly() {
        // Real finding: an unconstrained route (84 metas) + our tip is > 1232 bytes.
        let big = leg(UNCONSTRAINED, well_known::WSOL_MINT, well_known::USDC_MINT);
        let none = HashSet::new();
        let e = compose_single(&[&big], &params(&none, big.blockhash, true)).unwrap_err();
        assert!(e.is_size_limit(), "{e}");
    }

    #[test]
    fn bundle_plan_puts_tip_only_in_last_tx_and_creates_each_ata_once() {
        let (l1, l2) = legs();
        let none = HashSet::new();
        let b = compose_bundle(&[&l1, &l2], &params(&none, l1.blockhash, true), Some(&[180_000, 120_000])).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].tip_lamports, b[1].tip_lamports), (0, 10_000));
        assert_eq!((b[0].cu_limit, b[1].cu_limit), (180_000, 120_000));
        // tx1 closes the wSOL account (unwrap) → tx2 must create it again
        let _ = l1.cleanup.as_ref().and_then(token_close_target).unwrap();
        assert!(b[1].ata_create_ixs >= 1);
    }
}
