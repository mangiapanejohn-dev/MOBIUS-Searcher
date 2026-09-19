//! Live execution (CONFIRM/LIVE only). Every step fails closed:
//! permit → blockhash validity → sign → final signed simulation (must pass)
//! → wallet pre-state check → sendBundle → status polling → post-state
//! reconciliation. There is no automatic resend.

use crate::assemble::{AssembledTx, reserialize};
use crate::wallet::Wallet;
use base64::Engine;
use searcher_core::Address;
use searcher_jito::{InflightStatus, SendPermit};
use searcher_market::SimulateOutcome;
use std::future::Future;
use std::time::Duration;

/// `(confirmation_status, transaction error)` from `getBundleStatuses`.
pub type BundleConfirmation = Option<(Option<String>, Option<String>)>;

pub trait LiveBackend: Send + Sync {
    fn simulate_signed(&self, tx_b64: String) -> impl Future<Output = Result<SimulateOutcome, String>> + Send;
    fn send_bundle(&self, permit: &SendPermit, txs: Vec<String>)
    -> impl Future<Output = Result<String, String>> + Send;
    fn inflight(&self, bundle_id: String) -> impl Future<Output = Result<Option<InflightStatus>, String>> + Send;
    fn balance(&self, a: Address) -> impl Future<Output = Result<u64, String>> + Send;
    /// `getBundleStatuses`: (confirmation_status, err) once the bundle is visible.
    fn bundle_status(&self, bundle_id: String) -> impl Future<Output = Result<BundleConfirmation, String>> + Send;
    fn block_height(&self) -> Option<u64>;
}

#[derive(Clone, Debug)]
pub struct LiveParams {
    pub min_wallet_lamports: u64,
    /// Blocks of headroom required before `last_valid_block_height`.
    pub blockhash_margin: u64,
    pub poll: Duration,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveOutcome {
    /// Nothing was sent. Reason is recorded.
    NotSent(String),
    Landed {
        bundle_id: String,
        slot: u64,
        signatures: Vec<String>,
        realized_lamports: Option<i64>,
        latency_ms: u32,
    },
    Failed {
        bundle_id: String,
        reason: String,
        latency_ms: u32,
    },
    TimedOut {
        bundle_id: String,
    },
}

pub async fn execute_live<B: LiveBackend>(
    backend: &B,
    permit: &SendPermit,
    wallet: &Wallet,
    mut txs: Vec<AssembledTx>,
    last_valid_block_height: u64,
    p: &LiveParams,
) -> LiveOutcome {
    // 1. blockhash headroom
    match backend.block_height() {
        Some(h) if h + p.blockhash_margin < last_valid_block_height => {}
        Some(h) => {
            return LiveOutcome::NotSent(format!(
                "blockhash expiring: height {h} vs last valid {last_valid_block_height}"
            ));
        }
        None => return LiveOutcome::NotSent("block height unknown".into()),
    }
    // 2. sign
    let mut wire = Vec::with_capacity(txs.len());
    let mut sigs = Vec::new();
    for t in txs.iter_mut() {
        if let Err(e) = wallet.sign(&mut t.tx) {
            return LiveOutcome::NotSent(format!("sign: {e}"));
        }
        sigs.push(t.tx.signatures.first().map(|s| s.to_string()).unwrap_or_default());
        match reserialize(&t.tx) {
            Ok(w) => wire.push(base64::engine::general_purpose::STANDARD.encode(w)),
            Err(e) => return LiveOutcome::NotSent(format!("serialize: {e}")),
        }
    }
    // 3. final simulation of the exact signed bytes — any error: never send
    if txs.len() != 1 {
        return LiveOutcome::NotSent("multi-tx bundles cannot be exactly pre-simulated on standard RPC".into());
    }
    match backend.simulate_signed(wire[0].clone()).await {
        Ok(out) if out.err.is_none() => {}
        Ok(out) => {
            return LiveOutcome::NotSent(format!(
                "final simulation failed: {}",
                out.err.map(|e| e.to_string()).unwrap_or_default()
            ));
        }
        Err(e) => return LiveOutcome::NotSent(format!("final simulation unavailable: {e}")),
    }
    // 4. wallet pre-state
    let pre = match backend.balance(wallet.pubkey()).await {
        Ok(b) if b >= p.min_wallet_lamports => b,
        Ok(b) => {
            return LiveOutcome::NotSent(format!("wallet {b} lamports < minimum {}", p.min_wallet_lamports));
        }
        Err(e) => return LiveOutcome::NotSent(format!("wallet balance unknown: {e}")),
    };
    // 5. send once
    let started = std::time::Instant::now();
    let bundle_id = match backend.send_bundle(permit, wire).await {
        Ok(id) => id,
        Err(e) => return LiveOutcome::NotSent(format!("sendBundle rejected: {e}")),
    };
    // 6. poll
    loop {
        if started.elapsed() > p.timeout {
            return LiveOutcome::TimedOut { bundle_id };
        }
        tokio::time::sleep(p.poll).await;
        match backend.inflight(bundle_id.clone()).await {
            Ok(Some(InflightStatus::Landed { slot })) => {
                // "Landed" must also be confirmed with no transaction error.
                loop {
                    match backend.bundle_status(bundle_id.clone()).await {
                        Ok(Some((_, Some(err)))) => {
                            return LiveOutcome::Failed {
                                bundle_id,
                                reason: format!("landed with error: {err}"),
                                latency_ms: started.elapsed().as_millis() as u32,
                            };
                        }
                        Ok(Some((Some(c), None))) if c == "confirmed" || c == "finalized" => break,
                        _ => {}
                    }
                    if started.elapsed() > p.timeout {
                        return LiveOutcome::TimedOut { bundle_id };
                    }
                    tokio::time::sleep(p.poll).await;
                }
                let post = backend.balance(wallet.pubkey()).await.ok();
                return LiveOutcome::Landed {
                    bundle_id,
                    slot,
                    signatures: sigs,
                    realized_lamports: post.map(|b| b as i64 - pre as i64),
                    latency_ms: started.elapsed().as_millis() as u32,
                };
            }
            Ok(Some(InflightStatus::Failed)) => {
                return LiveOutcome::Failed {
                    bundle_id,
                    reason: "block engine reported Failed".into(),
                    latency_ms: started.elapsed().as_millis() as u32,
                };
            }
            // Invalid right after submit can mean "not propagated yet"; keep polling until timeout.
            Ok(_) | Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::Mode;
    use solana_keypair::Keypair;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Mock {
        sim_err: bool,
        balance: u64,
        height: Option<u64>,
        sends: AtomicUsize,
        statuses: Mutex<Vec<Option<InflightStatus>>>,
        landed_err: Option<String>,
    }

    impl LiveBackend for Mock {
        async fn simulate_signed(&self, _tx: String) -> Result<SimulateOutcome, String> {
            Ok(SimulateOutcome {
                err: self.sim_err.then(|| serde_json::json!({"InstructionError":[3,{"Custom":6001}]})),
                ..Default::default()
            })
        }
        async fn send_bundle(&self, _p: &SendPermit, _txs: Vec<String>) -> Result<String, String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok("bundle-1".into())
        }
        async fn inflight(&self, _id: String) -> Result<Option<InflightStatus>, String> {
            let mut s = self.statuses.lock().unwrap();
            Ok(if s.is_empty() { None } else { s.remove(0) })
        }
        async fn balance(&self, _a: Address) -> Result<u64, String> {
            Ok(self.balance)
        }
        fn block_height(&self) -> Option<u64> {
            self.height
        }
        async fn bundle_status(&self, _id: String) -> Result<BundleConfirmation, String> {
            Ok(Some((Some("confirmed".into()), self.landed_err.clone())))
        }
    }

    fn wallet() -> Wallet {
        let dir = std::env::temp_dir().join(format!("searcher-live-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = dir.join(format!("k{}.json", N.fetch_add(1, Ordering::SeqCst)));
        std::fs::write(&p, serde_json::to_string(&Keypair::new().to_bytes().to_vec()).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Wallet::load(&p, None).unwrap()
    }

    fn tx_for(w: &Wallet) -> AssembledTx {
        let none = std::collections::HashSet::new();
        let leg = searcher_core::ix::LegInstructions {
            compute_budget: vec![],
            setup: vec![],
            swap: searcher_core::ix::system_transfer_ix(w.pubkey(), Address([5; 32]), 1),
            cleanup: None,
            other: vec![],
            lookup_tables: vec![],
            blockhash: [1; 32],
            last_valid_block_height: 1_000,
        };
        crate::assemble::compose_single(
            &[&leg],
            &crate::assemble::AssemblyParams {
                payer: w.pubkey(),
                cu_limit: 10_000,
                cu_price_micro: 0,
                tip: None,
                dont_front: None,
                existing_atas: &none,
                blockhash: [1; 32],
            },
        )
        .unwrap()
    }

    fn params() -> LiveParams {
        LiveParams {
            min_wallet_lamports: 10,
            blockhash_margin: 10,
            poll: Duration::from_millis(1),
            timeout: Duration::from_millis(200),
        }
    }

    #[tokio::test]
    async fn simulation_error_never_sends() {
        let w = wallet();
        let permit = SendPermit::check(Mode::Live, true).unwrap();
        let m = Mock { sim_err: true, balance: 1_000, height: Some(900), ..Default::default() };
        let out = execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await;
        assert!(matches!(out, LiveOutcome::NotSent(ref r) if r.contains("final simulation failed")), "{out:?}");
        assert_eq!(m.sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn expiring_blockhash_or_poor_wallet_never_sends() {
        let w = wallet();
        let permit = SendPermit::check(Mode::Live, true).unwrap();
        let m = Mock { balance: 1_000, height: Some(995), ..Default::default() };
        assert!(matches!(
            execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await,
            LiveOutcome::NotSent(_)
        ));
        let m = Mock { balance: 1, height: Some(900), ..Default::default() };
        assert!(matches!(
            execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await,
            LiveOutcome::NotSent(_)
        ));
        assert_eq!(m.sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn landed_bundle_is_reconciled_and_sent_exactly_once() {
        let w = wallet();
        let permit = SendPermit::check(Mode::Live, true).unwrap();
        let m = Mock {
            balance: 1_000,
            height: Some(900),
            statuses: Mutex::new(vec![Some(InflightStatus::Pending), Some(InflightStatus::Landed { slot: 42 })]),
            ..Default::default()
        };
        let out = execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await;
        assert!(matches!(out, LiveOutcome::Landed { slot: 42, .. }), "{out:?}");
        assert_eq!(m.sends.load(Ordering::SeqCst), 1);
        let m = Mock { balance: 1_000, height: Some(900), ..Default::default() };
        let out = execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await;
        assert!(matches!(out, LiveOutcome::TimedOut { .. }));
        assert_eq!(m.sends.load(Ordering::SeqCst), 1, "no resend on timeout");
    }

    #[tokio::test]
    async fn landed_with_transaction_error_is_a_failure() {
        let w = wallet();
        let permit = SendPermit::check(Mode::Live, true).unwrap();
        let m = Mock {
            balance: 1_000,
            height: Some(900),
            statuses: Mutex::new(vec![Some(InflightStatus::Landed { slot: 7 })]),
            landed_err: Some("{\"InstructionError\":[3,{\"Custom\":6001}]}".into()),
            ..Default::default()
        };
        let out = execute_live(&m, &permit, &w, vec![tx_for(&w)], 1_000, &params()).await;
        assert!(
            matches!(out, LiveOutcome::Failed { ref reason, .. } if reason.contains("landed with error")),
            "{out:?}"
        );
    }
}
