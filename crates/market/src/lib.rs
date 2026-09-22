//! Solana JSON-RPC client, slot WebSocket feed and chain state.

pub mod accounts;
pub mod feed;
pub mod hermes;
pub mod hot;
pub mod rpc;

pub use feed::{ChainState, Emit};
pub use rpc::{EpochInfo, RpcClient, RpcError, SimulateOutcome, TokenBalance, TxMeta};
