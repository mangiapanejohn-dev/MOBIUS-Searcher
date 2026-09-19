//! Event fan-out. Producers never block: the UI path drops on overflow (the
//! hub only needs the latest state), the storage path drops *and counts* so
//! loss is visible in the System page and the session report.

use searcher_core::Event;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use tokio::sync::mpsc;

pub struct EventBus {
    ui: Option<mpsc::Sender<Event>>,
    store: Option<SyncSender<Event>>,
    dropped_ui: AtomicU64,
    dropped_store: Arc<AtomicU64>,
    emitted: AtomicU64,
}

impl EventBus {
    pub fn new(ui: Option<mpsc::Sender<Event>>, store: Option<SyncSender<Event>>) -> Self {
        Self {
            ui,
            store,
            dropped_ui: AtomicU64::new(0),
            dropped_store: Arc::new(AtomicU64::new(0)),
            emitted: AtomicU64::new(0),
        }
    }

    pub fn emit(&self, e: Event) {
        self.emitted.fetch_add(1, Ordering::Relaxed);
        match (&self.ui, &self.store) {
            (Some(ui), Some(store)) => {
                if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = store.try_send(e.clone()) {
                    self.dropped_store.fetch_add(1, Ordering::Relaxed);
                }
                if ui.try_send(e).is_err() {
                    self.dropped_ui.fetch_add(1, Ordering::Relaxed);
                }
            }
            (Some(ui), None) => {
                if ui.try_send(e).is_err() {
                    self.dropped_ui.fetch_add(1, Ordering::Relaxed);
                }
            }
            (None, Some(store)) => {
                if store.try_send(e).is_err() {
                    self.dropped_store.fetch_add(1, Ordering::Relaxed);
                }
            }
            (None, None) => {}
        }
    }

    pub fn dropped_ui(&self) -> u64 {
        self.dropped_ui.load(Ordering::Relaxed)
    }

    pub fn dropped_store(&self) -> u64 {
        self.dropped_store.load(Ordering::Relaxed)
    }

    /// Shared counter (lets the recorder read drops without holding a sender).
    pub fn dropped_store_counter(&self) -> Arc<AtomicU64> {
        self.dropped_store.clone()
    }

    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::Ts;
    use searcher_core::event::LogLevel;

    fn ev(i: i64) -> Event {
        Event::Log { ts: Ts(i), level: LogLevel::Info, message: String::new() }
    }

    #[test]
    fn slow_consumers_never_block_producers() {
        let (ui_tx, mut ui_rx) = mpsc::channel(2);
        let (st_tx, st_rx) = std::sync::mpsc::sync_channel(3);
        let bus = EventBus::new(Some(ui_tx), Some(st_tx));
        for i in 0..10 {
            bus.emit(ev(i)); // must return immediately even though nobody reads
        }
        assert_eq!(bus.emitted(), 10);
        assert_eq!(bus.dropped_ui(), 8);
        assert_eq!(bus.dropped_store(), 7);
        assert_eq!(ui_rx.try_recv().unwrap().ts(), Ts(0));
        assert_eq!(st_rx.try_iter().count(), 3);
    }
}
