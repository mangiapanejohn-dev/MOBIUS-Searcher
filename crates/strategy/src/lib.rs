//! Strategies (RoundTrip, CrossDex, Triangular), weighted scheduling and
//! opportunity pricing. Depends only on the domain core.

pub mod plan;
pub mod pricing;
pub mod scheduler;
pub mod strategies;

pub use plan::{CandidatePlan, LegSpec};
pub use scheduler::Scheduler;
pub use strategies::{CrossDex, FastPairs, RoundTrip, Strategy, Triangular};
