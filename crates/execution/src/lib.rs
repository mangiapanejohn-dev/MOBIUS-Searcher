//! Transaction assembly, simulation, and the opportunity pipeline
//! (scanner → route builder → simulator → risk → executor).

pub mod assemble;
pub mod live;
pub mod pipeline;
pub mod probe;
pub mod router;
pub mod simulate;
pub mod view;
pub mod wallet;

pub use pipeline::{Confirmations, Pipeline, PipelineConfig, RealBackend, SimJob};
pub use probe::{Observes, Probe};
pub use view::RuntimeView;
pub use wallet::{GeneratedWallet, Wallet};

/// A mainnet Jito tip account (verified via `getTipAccounts` 2026-09-18);
/// the live list from the block engine is preferred at runtime.
pub const KNOWN_TIP_ACCOUNT: &str = searcher_jito::KNOWN_TIP_ACCOUNTS[0];
