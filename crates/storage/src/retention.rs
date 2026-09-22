//! Retention: what is deleted, and when.
//!
//! - Sessions whose last activity is older than `keep_days` are deleted;
//!   sessions with a trade or an execution attempt are kept for
//!   `keep_trading_days` instead.
//! - A session that runs longer than `keep_days` loses its detail rows (log
//!   blocks, samples, opportunities, …) older than that; its summary stays.
//! - While the database is larger than `max_db_mb`, the oldest sessions are
//!   deleted (never the running one, never one with trades).
//! - Freed pages are handed back to the OS (incremental vacuum) and the WAL is
//!   truncated.
//!
//! Runs when a recording starts and every `prune_every_min` while it runs
//! (see [`crate::recorder`]), and on demand (`--prune`). Deletes go in small
//! transactions so the recorder is never blocked for long.

use crate::store::{Store, StoreError};
use rusqlite::params;
use searcher_core::Ts;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retention {
    pub keep_days: u32,
    pub keep_trading_days: u32,
    pub max_db_mb: u64,
    pub prune_every_min: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self { keep_days: 7, keep_trading_days: 90, max_db_mb: 1024, prune_every_min: 30 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PruneReport {
    pub sessions_deleted: Vec<String>,
    pub detail_rows_deleted: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl fmt::Display for PruneReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "sessions deleted   {}", self.sessions_deleted.len())?;
        for s in &self.sessions_deleted {
            writeln!(f, "  {s}")?;
        }
        writeln!(f, "old detail rows    {}", self.detail_rows_deleted)?;
        write!(f, "database           {} → {}", mb(self.bytes_before), mb(self.bytes_after))
    }
}

/// Size of the database and of each session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DbInfo {
    pub file_bytes: u64,
    pub free_bytes: u64,
    pub wal_bytes: u64,
    pub incremental_vacuum: bool,
    /// (session id, mode, started, events, compressed log bytes, raw log bytes)
    pub sessions: Vec<(String, String, Ts, i64, i64, i64)>,
}

impl fmt::Display for DbInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "file {}  (free pages {}, WAL {})  incremental vacuum: {}",
            mb(self.file_bytes),
            mb(self.free_bytes),
            mb(self.wal_bytes),
            if self.incremental_vacuum { "on" } else { "off — `--prune` compacts old files" }
        )?;
        writeln!(f, "{:<24} {:<8} {:>9} {:>10} {:>10}", "SESSION", "MODE", "EVENTS", "LOG", "RAW")?;
        for (id, mode, _, n, z, raw) in &self.sessions {
            writeln!(f, "{id:<24} {mode:<8} {n:>9} {:>10} {:>10}", mb(*z as u64), mb(*raw as u64))?;
        }
        Ok(())
    }
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

/// Tables holding per-session rows, and their time column (None: no detail
/// pruning by time).
const TABLES: [(&str, Option<&str>); 17] = [
    ("events", Some("ts")),
    ("event_blocks", Some("t1")),
    ("event_counts", None),
    ("samples", Some("ts")),
    ("opportunities", Some("detected_at")),
    ("quotes", Some("ts")),
    ("attribution", Some("ts")),
    ("inventory", None),
    ("simulations", Some("ts")),
    ("risk_decisions", Some("ts")),
    ("executions", None),
    ("trades", None),
    ("pnl", None),
    ("errors", Some("ts")),
    ("latency", Some("ts")),
    ("system_metrics", Some("ts")),
    ("sessions", None),
];
const DELETE_CHUNK: i64 = 5_000;

impl Store {
    fn pragma(&self, name: &str) -> Result<i64, StoreError> {
        Ok(self.conn().query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
    }

    /// Bytes in use (file size minus free pages).
    pub fn used_bytes(&self) -> Result<u64, StoreError> {
        let (pages, free, size) =
            (self.pragma("page_count")?, self.pragma("freelist_count")?, self.pragma("page_size")?);
        Ok(((pages - free).max(0) * size) as u64)
    }

    /// Delete `sql` matches in chunks (short write locks); returns rows deleted.
    fn delete_chunked(&self, table: &str, cond: &str, args: &[&dyn rusqlite::ToSql]) -> Result<u64, StoreError> {
        let sql =
            format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {cond} LIMIT {DELETE_CHUNK})");
        let mut total = 0u64;
        loop {
            let n = self.conn().execute(&sql, args)?;
            total += n as u64;
            if (n as i64) < DELETE_CHUNK {
                return Ok(total);
            }
        }
    }

    /// Delete a session and all its rows.
    pub fn delete_session(&self, id: &str) -> Result<u64, StoreError> {
        let mut n = 0;
        for (t, _) in TABLES {
            let cond = if t == "sessions" { "id = ?1" } else { "session_id = ?1" };
            n += self.delete_chunked(t, cond, &[&id as &dyn rusqlite::ToSql])?;
        }
        Ok(n)
    }

    fn protected(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM trades WHERE session_id = ?1) OR EXISTS(SELECT 1 FROM executions WHERE session_id = ?1)",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// Sessions oldest first: (id, last activity, still open).
    fn sessions_by_age(&self) -> Result<Vec<(String, Ts, bool)>, StoreError> {
        let mut st = self
            .conn()
            .prepare("SELECT id, COALESCE(ended_at, started_at), ended_at IS NULL FROM sessions ORDER BY started_at")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, Ts(r.get(1)?), r.get(2)?)))?;
        let mut out: Vec<(String, Ts, bool)> = rows.collect::<Result<_, _>>()?;
        // an open session's activity is its newest event
        for (id, last, _) in out.iter_mut() {
            if let (_, Some(t)) = self.session_span(id)? {
                *last = (*last).max(Ts(t));
            }
        }
        Ok(out)
    }

    /// Apply `r` now. `current` (the session being recorded) is never
    /// deleted, nor is any open session with activity in the last hour
    /// (another instance may be recording it).
    pub fn prune(&mut self, r: &Retention, now: Ts, current: Option<&str>) -> Result<PruneReport, StoreError> {
        let mut rep = PruneReport { bytes_before: self.used_bytes()?, ..Default::default() };
        let day = 86_400_000_000i64;
        let cutoff = Ts(now.0 - r.keep_days as i64 * day);
        let trading_cutoff = Ts(now.0 - r.keep_trading_days as i64 * day);
        let sessions = self.sessions_by_age()?;
        let mut kept = Vec::new();
        for (id, last, open) in sessions {
            let protected = self.protected(&id)?;
            let limit = if protected { trading_cutoff } else { cutoff };
            let running = Some(id.as_str()) == current || (open && now.0 - last.0 < 3_600_000_000);
            if !running && last < limit {
                self.delete_session(&id)?;
                rep.sessions_deleted.push(id);
            } else {
                kept.push((id, protected, running));
            }
        }
        // long-running sessions: drop detail older than the cutoff
        for (id, protected, _) in &kept {
            if *protected {
                continue;
            }
            for (t, col) in TABLES {
                if let Some(col) = col {
                    rep.detail_rows_deleted += self.delete_chunked(
                        t,
                        &format!("session_id = ?1 AND {col} < ?2"),
                        &[id as &dyn rusqlite::ToSql, &cutoff.0],
                    )?;
                }
            }
        }
        // size cap: oldest unprotected sessions first
        let cap = r.max_db_mb * 1_000_000;
        for (id, protected, running) in &kept {
            if self.used_bytes()? <= cap {
                break;
            }
            if !*protected && !*running {
                self.delete_session(id)?;
                rep.sessions_deleted.push(id.clone());
            }
        }
        self.release()?;
        rep.bytes_after = self.used_bytes()?;
        Ok(rep)
    }

    /// Hand free pages back to the OS (when the file supports it) and
    /// truncate the WAL.
    pub fn release(&self) -> Result<(), StoreError> {
        if self.pragma("auto_vacuum")? == 2 {
            while self.pragma("freelist_count")? > 0 {
                self.conn().execute_batch("PRAGMA incremental_vacuum(2000)")?;
            }
        }
        self.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Rewrite the whole file (compacts files created before incremental
    /// vacuum existed; needs free disk space about the size of the data).
    pub fn vacuum(&self) -> Result<(), StoreError> {
        self.conn().execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
        Ok(())
    }

    pub fn db_info(&self, path: &std::path::Path) -> Result<DbInfo, StoreError> {
        let size = self.pragma("page_size")?;
        let mut info = DbInfo {
            file_bytes: (self.pragma("page_count")? * size) as u64,
            free_bytes: (self.pragma("freelist_count")? * size) as u64,
            wal_bytes: std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0),
            incremental_vacuum: self.pragma("auto_vacuum")? == 2,
            sessions: Vec::new(),
        };
        let mut st = self.conn().prepare(
            "SELECT s.id, s.mode, s.started_at,
                    COALESCE((SELECT SUM(n) FROM event_counts c WHERE c.session_id = s.id),
                             (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id)),
                    COALESCE((SELECT SUM(length(data)) FROM event_blocks b WHERE b.session_id = s.id), 0)
                      + COALESCE((SELECT SUM(length(json)) FROM events e WHERE e.session_id = s.id), 0),
                    COALESCE((SELECT SUM(raw_bytes) FROM event_blocks b WHERE b.session_id = s.id), 0)
                      + COALESCE((SELECT SUM(length(json)) FROM events e WHERE e.session_id = s.id), 0)
             FROM sessions s ORDER BY s.started_at DESC",
        )?;
        info.sessions = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, Ts(r.get(2)?), r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<_, _>>()?;
        Ok(info)
    }
}
