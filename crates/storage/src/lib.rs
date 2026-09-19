//! SQLite session recording, replay loading and paper-run reports.

pub mod recorder;
pub mod report;
pub mod retention;
pub mod schema;
pub mod store;

pub use recorder::{RecorderStats, spawn_recorder, spawn_recorder_with};
pub use report::{Report, build as build_report, render as render_report};
pub use retention::{DbInfo, PruneReport, Retention};
pub use store::{SessionRow, Store, StoreError};

/// Human-friendly, sortable session id: `YYYYMMDD-HHMMSS-xxxx`.
pub fn new_session_id() -> String {
    let now = searcher_core::Ts::now();
    format!("{}-{:04x}", now.format("%Y%m%d-%H%M%S"), (now.micros() as u64 ^ std::process::id() as u64) & 0xffff)
}
