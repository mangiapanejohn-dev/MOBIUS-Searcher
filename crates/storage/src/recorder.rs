//! Recorder thread: drains the storage channel, commits in batches and
//! compacts the log into compressed blocks. Runs on its own OS thread so
//! SQLite I/O never touches the async runtime. A janitor thread applies the
//! retention policy at start and periodically, on its own connection.

use crate::retention::Retention;
use crate::store::Store;
use searcher_core::event::SessionInfo;
use searcher_core::model::ServiceId;
use searcher_core::{Event, Ts};
use searcher_telemetry::Telemetry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct RecorderStats {
    pub written: u64,
    pub batches: u64,
    pub errors: u64,
}

/// [`spawn_recorder_with`] and the default [`Retention`].
pub fn spawn_recorder(
    path: PathBuf,
    session: SessionInfo,
    rx: Receiver<Event>,
    telemetry: Arc<Telemetry>,
    dropped: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> std::io::Result<std::thread::JoinHandle<RecorderStats>> {
    spawn_recorder_with(path, session, rx, telemetry, dropped, Retention::default())
}

pub fn spawn_recorder_with(
    path: PathBuf,
    session: SessionInfo,
    rx: Receiver<Event>,
    telemetry: Arc<Telemetry>,
    dropped: Arc<dyn Fn() -> u64 + Send + Sync>,
    retention: Retention,
) -> std::io::Result<std::thread::JoinHandle<RecorderStats>> {
    std::thread::Builder::new().name("recorder".into()).spawn(move || {
        let mut stats = RecorderStats::default();
        let mut store = match Store::open(&path) {
            Ok(s) => s,
            Err(e) => {
                telemetry.record_err(ServiceId::Db, None, format!("open: {e}"));
                // Drain so producers never see a full channel forever.
                for _ in rx.iter() {}
                return stats;
            }
        };
        if let Err(e) = store.begin_session(&session) {
            telemetry.record_err(ServiceId::Db, None, format!("begin: {e}"));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let janitor =
            spawn_janitor(path.clone(), session.session_id.clone(), retention, telemetry.clone(), stop.clone());
        let mut seq: u64 = 0;
        let mut buf: Vec<(u64, Event)> = Vec::with_capacity(512);
        let mut last_flush = Instant::now();
        let flush = |store: &mut Store, buf: &mut Vec<(u64, Event)>, stats: &mut RecorderStats| {
            if buf.is_empty() {
                return;
            }
            let t0 = Instant::now();
            let written = store.write_batch(&session.session_id, buf);
            if let Err(e) = store.compact(&session.session_id, Ts::now(), false) {
                telemetry.record_err(ServiceId::Db, None, format!("compact: {e}"));
            }
            match written {
                Ok(()) => {
                    stats.written += buf.len() as u64;
                    stats.batches += 1;
                    telemetry.record_ok(ServiceId::Db, t0.elapsed().as_millis() as u32);
                }
                Err(e) => {
                    stats.errors += 1;
                    telemetry.record_err(ServiceId::Db, Some(t0.elapsed().as_millis() as u32), e.to_string());
                }
            }
            buf.clear();
        };
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(e) => {
                    seq += 1;
                    buf.push((seq, e));
                    if buf.len() >= 512 {
                        flush(&mut store, &mut buf, &mut stats);
                        last_flush = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_flush.elapsed() >= Duration::from_millis(250) {
                flush(&mut store, &mut buf, &mut stats);
                last_flush = Instant::now();
            }
        }
        flush(&mut store, &mut buf, &mut stats);
        stop.store(true, Ordering::Relaxed);
        if let Some(j) = janitor {
            let _ = j.join();
        }
        if let Err(e) = store.end_session(&session.session_id, Ts::now(), dropped()) {
            telemetry.record_err(ServiceId::Db, None, format!("end: {e}"));
        }
        stats
    })
}

/// Apply `retention` now and every `prune_every_min` until `stop`.
fn spawn_janitor(
    path: PathBuf,
    current: String,
    retention: Retention,
    telemetry: Arc<Telemetry>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("db-janitor".into())
        .spawn(move || {
            let Ok(mut store) = Store::open(&path) else { return };
            let every = Duration::from_secs(retention.prune_every_min.max(1) as u64 * 60);
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    if let Err(e) = store.prune(&retention, Ts::now(), Some(&current)) {
                        telemetry.record_err(ServiceId::Db, None, format!("prune: {e}"));
                    }
                    next = Instant::now() + every;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .ok()
}
