//! Hot-wallet signer. Isolated from strategies: only the live executor holds
//! it, and it is only constructed in modes that send transactions. The key is
//! read from a 0600 keypair file and never printed, logged or serialized. Both
//! Solana CLI JSON keypairs and 64-byte base58 private keys are accepted.

use searcher_core::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::fmt;
use std::io::Write;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("keypair file: {0}")]
    Io(String),
    #[error("keypair file must not be readable by group/others (chmod 600 {0})")]
    Permissions(String),
    #[error("keypair file must contain a 64-byte JSON array or base58 private key")]
    Format,
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("signer {signer} does not match configured wallet {configured}")]
    Mismatch { signer: String, configured: String },
}

pub struct Wallet {
    keypair: Keypair,
}

/// A keypair prepared during onboarding but not persisted until the user
/// confirms the final setup summary.
pub struct GeneratedWallet {
    keypair: Keypair,
}

impl fmt::Debug for Wallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Wallet({})", self.pubkey())
    }
}

impl fmt::Debug for GeneratedWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GeneratedWallet({})", self.pubkey())
    }
}

impl GeneratedWallet {
    pub fn new() -> Self {
        Self { keypair: Keypair::new() }
    }

    pub fn pubkey(&self) -> Address {
        Address(self.keypair.pubkey().to_bytes())
    }

    /// Create a new Solana CLI-compatible keypair file without overwriting an
    /// existing wallet. Unix files are born private instead of being chmodded
    /// after a world-readable creation window.
    pub fn write_new(&self, path: &Path) -> Result<(), WalletError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| WalletError::Io(error.to_string()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| WalletError::Io(error.to_string()))?;
        let bytes = serde_json::to_vec(&self.keypair.to_bytes().to_vec())
            .map_err(|error| WalletError::Io(error.to_string()))?;
        file.write_all(&bytes).map_err(|error| WalletError::Io(error.to_string()))?;
        file.write_all(b"\n").map_err(|error| WalletError::Io(error.to_string()))?;
        file.sync_all().map_err(|error| WalletError::Io(error.to_string()))?;
        Ok(())
    }
}

impl Default for GeneratedWallet {
    fn default() -> Self {
        Self::new()
    }
}

impl Wallet {
    /// Load a keypair from a private file.
    ///
    /// Accepted formats are a Solana CLI-style `[u8; 64]` JSON array and a
    /// base58-encoded 64-byte private key (a common Solana wallet export
    /// format).
    pub fn load(path: &Path, expected: Option<&Address>) -> Result<Wallet, WalletError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path).map_err(|e| WalletError::Io(e.to_string()))?;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(WalletError::Permissions(path.display().to_string()));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| WalletError::Io(e.to_string()))?;
        let secret = text.trim();
        let keypair = if secret.starts_with('[') {
            let bytes: Vec<u8> = serde_json::from_str(secret).map_err(|_| WalletError::Format)?;
            let arr: [u8; 64] = bytes.as_slice().try_into().map_err(|_| WalletError::Format)?;
            Keypair::try_from(&arr[..]).map_err(|_| WalletError::Format)?
        } else {
            Keypair::try_from_base58_string(secret).map_err(|_| WalletError::Format)?
        };
        let w = Wallet { keypair };
        if let Some(exp) = expected
            && w.pubkey() != *exp
        {
            return Err(WalletError::Mismatch { signer: w.pubkey().to_string(), configured: exp.to_string() });
        }
        Ok(w)
    }

    pub fn pubkey(&self) -> Address {
        Address(self.keypair.pubkey().to_bytes())
    }

    /// Sign an assembled (single-signer) transaction in place.
    pub fn sign(&self, tx: &mut VersionedTransaction) -> Result<(), WalletError> {
        let signed = VersionedTransaction::try_new(tx.message.clone(), &[&self.keypair])
            .map_err(|e| WalletError::Sign(e.to_string()))?;
        *tx = signed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_key(dir: &std::path::Path, mode: u32) -> std::path::PathBuf {
        let kp = Keypair::new();
        let p = dir.join(format!("k{mode:o}.json"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(serde_json::to_string(&kp.to_bytes().to_vec()).unwrap().as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        p
    }

    #[test]
    fn loads_only_private_files_and_never_prints_secret() {
        let dir = std::env::temp_dir().join(format!("searcher-wallet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // mode bits exist only on unix; Windows has ACLs and no such check
        #[cfg(unix)]
        {
            let open = write_key(&dir, 0o644);
            assert!(matches!(Wallet::load(&open, None), Err(WalletError::Permissions(_))));
        }
        let ok = write_key(&dir, 0o600);
        let w = Wallet::load(&ok, None).unwrap();
        let dbg = format!("{w:?}");
        assert!(dbg.starts_with("Wallet(") && dbg.len() < 60, "debug shows only the pubkey: {dbg}");
        let wrong = Address([9; 32]);
        assert!(matches!(Wallet::load(&ok, Some(&wrong)), Err(WalletError::Mismatch { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_base58_wallet_export() {
        let dir = std::env::temp_dir().join(format!("searcher-wallet-base58-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keypair = Keypair::new();
        let expected = Address(keypair.pubkey().to_bytes());
        let path = dir.join("jupiter-wallet.key");
        std::fs::write(&path, keypair.to_base58_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let wallet = Wallet::load(&path, Some(&expected)).unwrap();
        assert_eq!(wallet.pubkey(), expected);
        assert!(!format!("{wallet:?}").contains(&keypair.to_base58_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generated_wallet_is_deferred_private_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("searcher-generated-wallet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bot-wallet.json");
        let generated = GeneratedWallet::new();
        let expected = generated.pubkey();
        generated.write_new(&path).unwrap();
        let loaded = Wallet::load(&path, Some(&expected)).unwrap();
        assert_eq!(loaded.pubkey(), expected);
        assert!(generated.write_new(&path).is_err(), "an existing wallet must never be overwritten");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
