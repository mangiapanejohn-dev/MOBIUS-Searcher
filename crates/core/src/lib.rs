//! Searcher domain core: no IO, no async, no provider JSON.

pub mod address;
pub mod config;
pub mod costs;
pub mod event;
pub mod ix;
pub mod metrics;
pub mod model;
pub mod profit;
pub mod series;
pub mod time;
pub mod token;
pub mod units;

pub use address::Address;
pub use costs::{CostBreakdown, CostParams};
pub use event::Event;
pub use model::*;
pub use profit::{ProfitEval, ProfitGuards};
pub use time::Ts;
pub use units::{Ppm, UsdMicros, UsdPrice};
