//! Terminal UI. Reads a `ViewModel` built from engine events; sends only
//! `Command`s back. Never touches providers or keys; its only network access
//! is OKX's public market data for the Markets page (display only). The Bots
//! page shows, and acts through, what the application hands it (`bots`).

pub mod app;
pub mod bots;
pub mod brand;
pub mod cex;
pub mod chart;
pub mod guide;
pub mod hub;
pub mod kline;
pub mod markets;
pub mod panels;
pub mod run;
pub mod theme;
pub mod thresholds;
pub mod timeline;
pub mod ui;
pub mod wallet;
pub mod workspace;

pub use app::{App, TuiOptions, default_graphs};
pub use hub::ViewModel;
pub use run::{buffer_html, buffer_text, parse_keys, run, snapshot};
