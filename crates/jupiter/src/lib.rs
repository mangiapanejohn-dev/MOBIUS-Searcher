//! Jupiter Swap API V2 (`api.jup.ag/swap/v2/build`). Legacy v6 endpoints
//! (`/quote`, `/swap`, `/swap-instructions`, `public.jupiterapi.com`) are not
//! implemented anywhere in this workspace.

pub mod adapter;
pub mod client;
pub mod wire;

pub use client::{ApiKey, BuildRequest, BuiltLeg, JupiterClient, JupiterError, QuoteTiming, ServerWindow};
