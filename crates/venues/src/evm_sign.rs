//! EVM signing: Keccak-256, RLP, EIP-55 addresses and EIP-1559 (type 2)
//! transactions signed with secp256k1 (RFC 6979, low-s). Checked against the
//! EIP-155 worked example and a real mainnet type-2 transaction (re-encoded
//! byte for byte, sender recovered from its signature).
//!
//! The private key never leaves [`EvmSigner`] and is never printed.

use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use sha3::{Digest, Keccak256};
use std::fmt;
use std::path::Path;

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// `0x` + first 4 bytes of `keccak256(signature)`, e.g. `slot0()` → `0x3850c7bd`.
pub fn selector(signature: &str) -> String {
    format!("0x{}", hex(&keccak256(signature.as_bytes())[..4]))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, SignError> {
    let s = s.trim().trim_start_matches("0x");
    if !s.len().is_multiple_of(2) {
        return Err(SignError::Format("odd hex length".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| SignError::Format(e.to_string())))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("key file: {0}")]
    Io(String),
    #[error("key file {0} is readable by others; chmod 600 it")]
    Permissions(String),
    #[error("bad format: {0}")]
    Format(String),
    #[error("key is for {got}, config expects {want}")]
    Mismatch { got: String, want: String },
}

// ── RLP ────────────────────────────────────────────────────────────────────

fn be_minimal(v: u128) -> Vec<u8> {
    let b = v.to_be_bytes();
    let first = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    b[first..].to_vec()
}

fn rlp_len_prefix(len: usize, short: u8, long: u8) -> Vec<u8> {
    if len <= 55 {
        vec![short + len as u8]
    } else {
        let l = be_minimal(len as u128);
        let mut out = vec![long + l.len() as u8];
        out.extend(l);
        out
    }
}

pub fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return b.to_vec();
    }
    let mut out = rlp_len_prefix(b.len(), 0x80, 0xb7);
    out.extend_from_slice(b);
    out
}

pub fn rlp_uint(v: u128) -> Vec<u8> {
    rlp_bytes(&be_minimal(v))
}

/// A list of already-encoded items.
pub fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = rlp_len_prefix(payload.len(), 0xc0, 0xf7);
    out.extend(payload);
    out
}

// ── addresses ──────────────────────────────────────────────────────────────

/// EIP-55 mixed-case checksum form of a 20-byte address.
pub fn checksum_address(addr: &[u8; 20]) -> String {
    let lower = hex(addr);
    let h = keccak256(lower.as_bytes());
    let mixed: String = lower
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let nibble = (h[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0xf;
            if c.is_ascii_alphabetic() && nibble >= 8 { c.to_ascii_uppercase() } else { c }
        })
        .collect();
    format!("0x{mixed}")
}

pub fn parse_address(s: &str) -> Result<[u8; 20], SignError> {
    unhex(s)?.try_into().map_err(|_| SignError::Format(format!("`{s}` is not a 20-byte address")))
}

fn address_of(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_encoded_point(false);
    keccak256(&point.as_bytes()[1..])[12..].try_into().unwrap()
}

// ── transactions ───────────────────────────────────────────────────────────

/// EIP-1559 transaction (no access list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tx1559 {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub value: u128,
    pub data: Vec<u8>,
}

impl Tx1559 {
    fn fields(&self) -> Vec<Vec<u8>> {
        vec![
            rlp_uint(self.chain_id as u128),
            rlp_uint(self.nonce as u128),
            rlp_uint(self.max_priority_fee_per_gas),
            rlp_uint(self.max_fee_per_gas),
            rlp_uint(self.gas_limit as u128),
            rlp_bytes(&self.to),
            rlp_uint(self.value),
            rlp_bytes(&self.data),
            rlp_list(&[]), // access list
        ]
    }

    /// The hash that is signed: keccak256(0x02 ‖ rlp(fields)).
    pub fn sighash(&self) -> [u8; 32] {
        let mut b = vec![0x02];
        b.extend(rlp_list(&self.fields()));
        keccak256(&b)
    }

    /// 0x02 ‖ rlp(fields ‖ y_parity ‖ r ‖ s): what `eth_sendRawTransaction` takes.
    pub fn encode_signed(&self, y_parity: u8, r: &[u8; 32], s: &[u8; 32]) -> Vec<u8> {
        let strip = |x: &[u8; 32]| x[x.iter().position(|b| *b != 0).unwrap_or(32)..].to_vec();
        let mut f = self.fields();
        f.push(rlp_uint(y_parity as u128));
        f.push(rlp_bytes(&strip(r)));
        f.push(rlp_bytes(&strip(s)));
        let mut b = vec![0x02];
        b.extend(rlp_list(&f));
        b
    }
}

/// Address that signed `sighash` with (`y_parity`, `r`, `s`).
pub fn recover_signer(sighash: &[u8; 32], y_parity: u8, r: &[u8; 32], s: &[u8; 32]) -> Result<String, SignError> {
    let sig = Signature::from_scalars(*r, *s).map_err(|e| SignError::Format(e.to_string()))?;
    let id = RecoveryId::from_byte(y_parity).ok_or_else(|| SignError::Format("recovery id".into()))?;
    let key = VerifyingKey::recover_from_prehash(sighash, &sig, id).map_err(|e| SignError::Format(e.to_string()))?;
    Ok(checksum_address(&address_of(&key)))
}

/// A secp256k1 key for one EVM account. `Debug` shows only the address.
pub struct EvmSigner {
    key: SigningKey,
    address: String,
}

impl fmt::Debug for EvmSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmSigner({})", self.address)
    }
}

impl EvmSigner {
    /// From a 32-byte hex private key (`0x` optional).
    pub fn from_hex(secret: &str) -> Result<EvmSigner, SignError> {
        let bytes = unhex(secret)?;
        let key =
            SigningKey::from_slice(&bytes).map_err(|_| SignError::Format("not a secp256k1 private key".into()))?;
        let address = checksum_address(&address_of(key.verifying_key()));
        Ok(EvmSigner { key, address })
    }

    /// From a file holding the hex private key. Must not be readable by
    /// others (unix); `expected` guards against loading the wrong account.
    pub fn from_file(path: &Path, expected: Option<&str>) -> Result<EvmSigner, SignError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path).map_err(|e| SignError::Io(e.to_string()))?;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(SignError::Permissions(path.display().to_string()));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| SignError::Io(e.to_string()))?;
        let signer = EvmSigner::from_hex(&text)?;
        if let Some(want) = expected
            && !want.eq_ignore_ascii_case(&signer.address)
        {
            return Err(SignError::Mismatch { got: signer.address, want: want.to_string() });
        }
        Ok(signer)
    }

    /// EIP-55 checksummed address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Sign a 32-byte hash: (y_parity, r, s) with low s.
    pub fn sign_hash(&self, hash: &[u8; 32]) -> Result<(u8, [u8; 32], [u8; 32]), SignError> {
        let (sig, id) = self.key.sign_prehash_recoverable(hash).map_err(|e| SignError::Format(e.to_string()))?;
        let (r, s) = sig.split_bytes();
        Ok((id.to_byte(), r.into(), s.into()))
    }

    /// Signed raw transaction bytes and its hash.
    pub fn sign_1559(&self, tx: &Tx1559) -> Result<(Vec<u8>, [u8; 32]), SignError> {
        let (y, r, s) = self.sign_hash(&tx.sighash())?;
        let raw = tx.encode_signed(y, &r, &s);
        let hash = keccak256(&raw);
        Ok((raw, hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm;

    #[test]
    fn keccak_and_the_hard_coded_selectors_agree() {
        assert_eq!(hex(&keccak256(b"")), "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
        assert_eq!(selector("slot0()"), evm::SLOT0);
        assert_eq!(selector("liquidity()"), evm::LIQUIDITY);
        assert_eq!(selector("token0()"), evm::TOKEN0);
        assert_eq!(selector("token1()"), evm::TOKEN1);
        assert_eq!(selector("fee()"), evm::FEE);
        assert_eq!(selector("symbol()"), evm::SYMBOL);
        assert_eq!(selector("decimals()"), evm::DECIMALS);
        assert_eq!(
            selector("quoteExactInputSingle((address,address,uint256,uint24,uint160))"),
            evm::QUOTE_EXACT_INPUT_SINGLE
        );
    }

    /// EIP-155's worked example (legacy transaction, chain id 1): exercises
    /// RLP, Keccak and deterministic secp256k1 signing against published bytes.
    #[test]
    fn the_eip155_worked_example_signs_byte_for_byte() {
        let signer = EvmSigner::from_hex("0x4646464646464646464646464646464646464646464646464646464646464646").unwrap();
        let to = [0x35u8; 20];
        let fields = |v: Vec<u8>, r: Vec<u8>, s: Vec<u8>| {
            rlp_list(&[
                rlp_uint(9),
                rlp_uint(20_000_000_000),
                rlp_uint(21_000),
                rlp_bytes(&to),
                rlp_uint(10u128.pow(18)),
                rlp_bytes(&[]),
                v,
                r,
                s,
            ])
        };
        let signing = fields(rlp_uint(1), rlp_uint(0), rlp_uint(0));
        assert_eq!(
            hex(&signing),
            "ec098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a764000080018080"
        );
        let hash = keccak256(&signing);
        assert_eq!(hex(&hash), "daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53");
        let (y, r, s) = signer.sign_hash(&hash).unwrap();
        let signed = fields(rlp_uint(y as u128 + 35 + 2), rlp_bytes(&r), rlp_bytes(&s));
        assert_eq!(
            hex(&signed),
            concat!(
                "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a7640000",
                "8025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276",
                "a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
            )
        );
        assert_eq!(recover_signer(&hash, y, &r, &s).unwrap(), signer.address());
        assert!(!format!("{signer:?}").contains("4646"), "Debug never shows the key");
    }

    /// A real Base transaction (type 2): rebuilt from its fields it must
    /// equal the raw bytes the chain returned, hash to its hash, and recover
    /// to its sender.
    #[test]
    fn a_mainnet_type2_transaction_round_trips() {
        let t: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/evm/tx1559-base.json")).unwrap();
        let s = |k: &str| t[k].as_str().unwrap().to_string();
        let n = |k: &str| u128::from_str_radix(s(k).trim_start_matches("0x"), 16).unwrap();
        let word = |k: &str| -> [u8; 32] {
            let b = unhex(&s(k)).unwrap();
            let mut w = [0u8; 32];
            w[32 - b.len()..].copy_from_slice(&b);
            w
        };
        let tx = Tx1559 {
            chain_id: n("chainId") as u64,
            nonce: n("nonce") as u64,
            max_priority_fee_per_gas: n("maxPriorityFeePerGas"),
            max_fee_per_gas: n("maxFeePerGas"),
            gas_limit: n("gas") as u64,
            to: parse_address(&s("to")).unwrap(),
            value: n("value"),
            data: unhex(&s("input")).unwrap(),
        };
        let raw = tx.encode_signed(n("yParity") as u8, &word("r"), &word("s"));
        assert_eq!(format!("0x{}", hex(&raw)), s("raw"));
        assert_eq!(format!("0x{}", hex(&keccak256(&raw))), s("hash"));
        let from = recover_signer(&tx.sighash(), n("yParity") as u8, &word("r"), &word("s")).unwrap();
        assert!(from.eq_ignore_ascii_case(&s("from")), "{from} vs {}", s("from"));
    }

    // The private keys in these tests are published test vectors (EIP-155's
    // worked example; the web3.js documentation example). Anyone can spend
    // from them: never send funds to their addresses.
    #[test]
    fn our_signatures_recover_to_our_address_and_key_files_are_guarded() {
        let signer = EvmSigner::from_hex("0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
        let tx = Tx1559 {
            chain_id: 8453,
            nonce: 7,
            max_priority_fee_per_gas: 1_000_000,
            max_fee_per_gas: 50_000_000,
            gas_limit: 200_000,
            to: parse_address("0x2626664c2603336E57B271c5C0b26F421741e481").unwrap(),
            value: 0,
            data: unhex("0x3850c7bd").unwrap(),
        };
        let (raw, hash) = signer.sign_1559(&tx).unwrap();
        assert_eq!(raw[0], 0x02);
        assert_eq!(hash, keccak256(&raw));
        let (y, r, s) = signer.sign_hash(&tx.sighash()).unwrap();
        assert_eq!(recover_signer(&tx.sighash(), y, &r, &s).unwrap(), signer.address());
        // EIP-55: a known address keeps its canonical casing
        assert_eq!(
            checksum_address(&parse_address("0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed").unwrap()),
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        );

        let dir = std::env::temp_dir().join(format!("mobius-evm-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("k.hex");
        std::fs::write(&p, "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(EvmSigner::from_file(&p, None), Err(SignError::Permissions(_))));
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(EvmSigner::from_file(&p, Some(signer.address())).unwrap().address(), signer.address());
        assert!(matches!(
            EvmSigner::from_file(&p, Some("0x0000000000000000000000000000000000000001")),
            Err(SignError::Mismatch { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
