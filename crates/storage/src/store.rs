//! SQLite store: event log (replay source) + normalised analysis tables.
//!
//! What is kept, and how:
//! - Event log: every event the UI consumed, except that metrics are thinned
//!   to one per metric per second (PnL and equity: all) and health snapshots
//!   to state changes or one per service per 5 s. Written uncompressed to
//!   `events`, then compacted into deflated blocks of up to 1024 events
//!   (`event_blocks`, ~40x smaller) once a block fills or is a minute old.
//! - Analysis tables: numeric rows for every opportunity, simulation, trade…
//!   Full opportunity snapshots and raw provider quotes only for notable
//!   opportunities (gross > 0 or past the skip stage); the log has the rest.
//! - Retention: see [`crate::retention`].

use crate::schema::{SCHEMA, SCHEMA_VERSION};
use flate2::Compression;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use searcher_core::event::SessionInfo;
use searcher_core::metrics::MetricId;
use searcher_core::model::{OppStatus, Opportunity, OpportunityId, ServiceId};
use searcher_core::{Event, Ts};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

/// Events per compressed block.
pub const BLOCK_EVENTS: usize = 1024;
/// A partial block is compacted once its oldest event is this old.
const BLOCK_MAX_AGE_US: i64 = 60_000_000;
/// Rows compacted per call (bounds the transaction on old uncompacted logs).
const COMPACT_MAX_ROWS: usize = 64 * BLOCK_EVENTS;
/// Log thinning: one sample per metric per second, health per service per 5 s.
const METRIC_EVERY_US: i64 = 1_000_000;
const HEALTH_EVERY_US: i64 = 5_000_000;
/// Raw quotes wait this long for their opportunity's verdict.
const PENDING_QUOTE_US: i64 = 120_000_000;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session `{0}` not found")]
    NoSession(String),
    #[error("compression: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub started_at: Ts,
    pub ended_at: Option<Ts>,
    pub mode: String,
    pub events: i64,
    pub dropped: i64,
    pub opportunities: i64,
}

pub struct Store {
    conn: Connection,
    /// Last persisted health sample per service (system_metrics are sampled).
    last_health: HashMap<ServiceId, Ts>,
    thin: Thinning,
}

/// Write-side state for thinning the log and holding raw quotes.
#[derive(Default)]
struct Thinning {
    metric: HashMap<MetricId, Ts>,
    health: HashMap<ServiceId, (Ts, String)>,
    /// Raw quotes of opportunities without a verdict yet.
    pending_quotes: HashMap<OpportunityId, Vec<(Ts, u8, String)>>,
    notable: HashSet<OpportunityId>,
}

/// Worth a full snapshot and its raw quotes: gross-positive, or past the
/// skip stage (executable, sent, filled, failed).
pub fn notable(o: &Opportunity) -> bool {
    o.eval.gross_pnl > 0 || !matches!(o.status, OppStatus::Quoted | OppStatus::Skipped(_))
}

impl Thinning {
    /// Whether `e` goes into the event log.
    fn keep(&mut self, e: &Event) -> bool {
        match e {
            Event::Metric { ts, metric, .. } if !matches!(metric, MetricId::Pnl | MetricId::Equity) => {
                let keep = self.metric.get(metric).is_none_or(|t| (ts.0 - t.0).abs() >= METRIC_EVERY_US);
                if keep {
                    self.metric.insert(*metric, *ts);
                }
                keep
            }
            Event::Health { ts, service, snapshot } => {
                let state = jstr(&snapshot.state);
                let keep =
                    self.health.get(service).is_none_or(|(t, s)| *s != state || (ts.0 - t.0).abs() >= HEALTH_EVERY_US);
                if keep {
                    self.health.insert(*service, (*ts, state));
                }
                keep
            }
            _ => true,
        }
    }

    /// Drop quotes whose opportunity never reported back.
    fn expire(&mut self, now: Ts) {
        self.pending_quotes.retain(|_, q| q.first().is_some_and(|(t, _, _)| now.0 - t.0 < PENDING_QUOTE_US));
        if self.notable.len() > 50_000 {
            self.notable.clear();
        }
    }
}

fn deflate(text: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut z = flate2::write::DeflateEncoder::new(Vec::with_capacity(text.len() / 8), Compression::default());
    z.write_all(text)?;
    z.finish()
}

fn inflate(data: &[u8]) -> std::io::Result<String> {
    let mut out = String::new();
    flate2::read::DeflateDecoder::new(data).read_to_string(&mut out)?;
    Ok(out)
}

fn status_str(s: &OppStatus) -> (&'static str, Option<&'static str>) {
    match s {
        OppStatus::Quoted => ("quoted", None),
        OppStatus::Skipped(r) => ("skipped", Some(r.code())),
        OppStatus::Executable => ("executable", None),
        OppStatus::AwaitingConfirm => ("awaiting_confirm", None),
        OppStatus::Submitted => ("submitted", None),
        OppStatus::PaperFilled => ("paper_filled", None),
        OppStatus::Landed => ("landed", None),
        OppStatus::Failed => ("failed", None),
    }
}

fn jstr<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(Store { conn, last_health: HashMap::new(), thin: Thinning::default() })
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn, last_health: HashMap::new(), thin: Thinning::default() })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn begin_session(&mut self, s: &SessionInfo) -> Result<(), StoreError> {
        self.thin = Thinning::default();
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions(id, started_at, mode, version, config_summary, taker) VALUES (?1,?2,?3,?4,?5,?6)",
            params![s.session_id, s.started_at.0, s.mode.label(), s.version, s.config_summary, s.taker],
        )?;
        Ok(())
    }

    pub fn end_session(&mut self, id: &str, ended: Ts, dropped: u64) -> Result<(), StoreError> {
        while self.compact(id, ended, true)? > 0 {}
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2, dropped = dropped + ?3, events = ?4 WHERE id = ?1",
            params![id, ended.0, dropped as i64, self.event_count(id)?],
        )?;
        Ok(())
    }

    /// Events recorded for a session (logs from before `event_counts` existed
    /// are counted row by row).
    pub fn event_count(&self, id: &str) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE((SELECT SUM(n) FROM event_counts WHERE session_id = ?1),
                             (SELECT COUNT(*) FROM events WHERE session_id = ?1))",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// Events of one kind in a session.
    pub fn kind_count(&self, id: &str, kind: &str) -> Result<i64, StoreError> {
        let counted: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM event_counts WHERE session_id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        let sql = if counted {
            "SELECT COALESCE(SUM(n), 0) FROM event_counts WHERE session_id = ?1 AND kind = ?2"
        } else {
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND kind = ?2"
        };
        Ok(self.conn.query_row(sql, params![id, kind], |r| r.get(0))?)
    }

    /// First and last event time of a session.
    pub fn session_span(&self, id: &str) -> Result<(Option<i64>, Option<i64>), StoreError> {
        Ok(self.conn.query_row(
            "SELECT MIN(t), MAX(t) FROM (
                SELECT t0 AS t FROM event_blocks WHERE session_id = ?1
                UNION ALL SELECT t1 FROM event_blocks WHERE session_id = ?1
                UNION ALL SELECT ts FROM events WHERE session_id = ?1)",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Persist a batch atomically. `seq` must be strictly increasing per
    /// session. Thinned events are not written (see the module docs).
    pub fn write_batch(&mut self, session: &str, batch: &[(u64, Event)]) -> Result<(), StoreError> {
        let Store { conn, last_health, thin } = self;
        // IMMEDIATE: take the write lock up front (waits on the janitor via
        // busy_timeout instead of failing on a read→write upgrade)
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut kinds: HashMap<&'static str, i64> = HashMap::new();
        let mut latest = Ts(0);
        for (seq, e) in batch {
            latest = latest.max(e.ts());
            if !thin.keep(e) {
                continue;
            }
            let json = serde_json::to_string(e)?;
            tx.execute(
                "INSERT OR REPLACE INTO events(session_id, seq, ts, kind, json) VALUES (?1,?2,?3,?4,?5)",
                params![session, *seq as i64, e.ts().0, e.kind(), json],
            )?;
            *kinds.entry(e.kind()).or_default() += 1;
            Self::project(&tx, session, e, last_health, thin)?;
        }
        for (kind, n) in kinds {
            tx.execute(
                "INSERT INTO event_counts VALUES (?1,?2,?3)
                 ON CONFLICT(session_id, kind) DO UPDATE SET n = n + excluded.n",
                params![session, kind, n],
            )?;
        }
        tx.commit()?;
        thin.expire(latest);
        Ok(())
    }

    /// Compact the session's uncompressed events into blocks: full blocks
    /// when enough have accumulated, everything when the oldest is a minute
    /// old or `force`. Returns the number of events compacted (bounded per
    /// call; repeat while > 0 to drain).
    pub fn compact(&mut self, session: &str, now: Ts, force: bool) -> Result<usize, StoreError> {
        let (n, oldest): (i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), MIN(ts) FROM events WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let n = n as usize;
        let stale = oldest.is_some_and(|t| now.0 - t >= BLOCK_MAX_AGE_US);
        let take = if force || stale { n } else { n / BLOCK_EVENTS * BLOCK_EVENTS }.min(COMPACT_MAX_ROWS);
        if take == 0 {
            return Ok(0);
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(i64, i64, String)> = {
            let mut st = tx.prepare("SELECT seq, ts, json FROM events WHERE session_id = ?1 ORDER BY seq LIMIT ?2")?;
            st.query_map(params![session, take as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        for chunk in rows.chunks(BLOCK_EVENTS) {
            let text = chunk.iter().map(|r| r.2.as_str()).collect::<Vec<_>>().join("\n");
            let t0 = chunk.iter().map(|r| r.1).min().unwrap_or(0);
            let t1 = chunk.iter().map(|r| r.1).max().unwrap_or(0);
            tx.execute(
                "INSERT OR REPLACE INTO event_blocks VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    session,
                    chunk[0].0,
                    chunk[chunk.len() - 1].0,
                    t0,
                    t1,
                    chunk.len() as i64,
                    text.len() as i64,
                    deflate(text.as_bytes())?
                ],
            )?;
        }
        tx.execute("DELETE FROM events WHERE session_id = ?1 AND seq <= ?2", params![session, rows[rows.len() - 1].0])?;
        tx.commit()?;
        Ok(rows.len())
    }

    fn project(
        tx: &Transaction<'_>,
        session: &str,
        e: &Event,
        last_health: &mut HashMap<ServiceId, Ts>,
        thin: &mut Thinning,
    ) -> Result<(), StoreError> {
        match e {
            Event::Sample(s) => {
                tx.execute(
                    "INSERT INTO samples VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        session,
                        s.ts.0,
                        s.pair,
                        jstr(&s.side),
                        s.price.micros_per_token as i64,
                        s.source,
                        s.size_atoms as i64
                    ],
                )?;
            }
            Event::Opportunity(o) => {
                let (status, skip) = status_str(&o.status);
                tx.execute(
                    "INSERT OR REPLACE INTO opportunities VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28)",
                    params![
                        session,
                        o.id.0 as i64,
                        o.key,
                        o.strategy.label(),
                        o.label,
                        o.route.dex_path(),
                        o.detected_at.0,
                        o.updated_at.0,
                        status,
                        skip,
                        o.input as i64,
                        o.gross_output as i64,
                        o.eval.gross_pnl,
                        o.eval.expected_net,
                        o.eval.simulated_net,
                        o.eval.gross_edge.0,
                        o.eval.net_edge.0,
                        o.eval.expected_net_usd.map(|u| u.0),
                        o.costs.base_fee as i64,
                        o.costs.priority_fee as i64,
                        o.costs.jito_tip as i64,
                        o.costs.ata_rent as i64,
                        o.costs.expected_slippage as i64,
                        o.costs.safety_buffer as i64,
                        o.costs.compute_units_used.map(|c| c as i64),
                        o.costs.compute_units_limit as i64,
                        o.route.total_latency_ms() as i64,
                        if notable(o) { serde_json::to_string(o)? } else { String::new() },
                    ],
                )?;
                // raw quotes: kept for notable opportunities, dropped once a
                // plain one reaches its verdict
                if notable(o) {
                    thin.notable.insert(o.id);
                    for (ts, leg, body) in thin.pending_quotes.remove(&o.id).unwrap_or_default() {
                        Self::quote(tx, session, o.id, leg, ts, &body)?;
                    }
                } else if o.status.is_terminal() {
                    thin.pending_quotes.remove(&o.id);
                }
            }
            Event::RawQuote { ts, opportunity, leg, body } => {
                if thin.notable.contains(opportunity) {
                    Self::quote(tx, session, *opportunity, *leg, *ts, body)?;
                } else {
                    thin.pending_quotes.entry(*opportunity).or_default().push((*ts, *leg, body.clone()));
                }
            }
            Event::Simulation(s) => {
                tx.execute(
                    "INSERT INTO simulations VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![
                        session,
                        s.opportunity.0 as i64,
                        s.simulated_at.0,
                        s.ok,
                        jstr(&s.plan),
                        jstr(&s.fidelity),
                        s.failure.as_ref().map(|f| jstr(&f.class)),
                        s.failure.as_ref().map(|f| f.message.clone()),
                        s.units_consumed() as i64,
                        s.cu_limit() as i64,
                        s.txs.iter().map(|t| t.size_bytes as i64).sum::<i64>(),
                        s.latency_ms as i64,
                        s.context_slot.map(|x| x as i64),
                        serde_json::to_string(s)?,
                    ],
                )?;
            }
            Event::Risk(r) => {
                tx.execute(
                    "INSERT INTO risk_decisions VALUES (?1,?2,?3,?4,?5)",
                    params![
                        session,
                        r.opportunity.0 as i64,
                        r.checked_at.0,
                        r.approved,
                        serde_json::to_string(&r.violations)?
                    ],
                )?;
            }
            Event::Execution(x) => {
                tx.execute(
                    "INSERT INTO executions VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        session,
                        x.opportunity.0 as i64,
                        x.updated_at.0,
                        x.mode.label(),
                        serde_json::to_value(&x.state)?.get("state").and_then(|v| v.as_str()).unwrap_or(""),
                        x.bundle_id,
                        x.tip_lamports as i64,
                        x.latency_ms.map(|l| l as i64),
                        serde_json::to_string(x)?,
                    ],
                )?;
            }
            Event::Trade(t) => {
                tx.execute(
                    "INSERT INTO trades VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        session,
                        t.opportunity.0 as i64,
                        t.exit_ts.0,
                        t.strategy.label(),
                        t.label,
                        t.paper,
                        t.input as i64,
                        t.output as i64,
                        t.fees_lamports as i64,
                        t.tip_lamports as i64,
                        t.expected_net,
                        t.net,
                        t.net_usd.map(|u| u.0)
                    ],
                )?;
            }
            Event::Metric { ts, metric, value } => {
                use searcher_core::metrics::{MetricId, Unit};
                if *metric == MetricId::Pnl {
                    tx.execute("INSERT INTO pnl VALUES (?1,?2,?3)", params![session, ts.0, value])?;
                } else if metric.unit() == Unit::Millis {
                    tx.execute(
                        "INSERT INTO latency VALUES (?1,?2,?3,?4)",
                        params![session, ts.0, jstr(metric), value],
                    )?;
                }
            }
            Event::Error { ts, service, message } => {
                tx.execute("INSERT INTO errors VALUES (?1,?2,?3,?4)", params![session, ts.0, service, message])?;
            }
            Event::Health { ts, service, snapshot } => {
                let due = last_health.get(service).is_none_or(|t| ts.0 - t.0 >= 5_000_000);
                if due {
                    last_health.insert(*service, *ts);
                    tx.execute(
                        "INSERT INTO system_metrics VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                        params![
                            session,
                            ts.0,
                            jstr(service),
                            jstr(&snapshot.state),
                            snapshot.p50_latency_ms.map(|v| v as i64),
                            snapshot.requests as i64,
                            snapshot.errors as i64,
                            snapshot.rate_limited as i64,
                            snapshot.quota_remaining
                        ],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn quote(
        tx: &Transaction<'_>,
        session: &str,
        id: OpportunityId,
        leg: u8,
        ts: Ts,
        body: &str,
    ) -> Result<(), StoreError> {
        tx.execute(
            "INSERT OR REPLACE INTO quotes VALUES (?1,?2,?3,?4,?5)",
            params![session, id.0 as i64, leg as i64, ts.0, body],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT s.id, s.started_at, s.ended_at, s.mode,
                    COALESCE((SELECT SUM(n) FROM event_counts c WHERE c.session_id = s.id),
                             (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id)),
                    s.dropped,
                    (SELECT COUNT(*) FROM opportunities o WHERE o.session_id = s.id)
             FROM sessions s ORDER BY s.started_at DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                started_at: Ts(r.get(1)?),
                ended_at: r.get::<_, Option<i64>>(2)?.map(Ts),
                mode: r.get(3)?,
                events: r.get(4)?,
                dropped: r.get(5)?,
                opportunities: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn session_exists(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row("SELECT 1 FROM sessions WHERE id = ?1", params![id], |_| Ok(())).optional()?.is_some())
    }

    /// All recorded events of a session in emission order (the replay
    /// stream): the compacted blocks, then the uncompressed tail.
    pub fn load_events(&self, id: &str) -> Result<Vec<Event>, StoreError> {
        if !self.session_exists(id)? {
            return Err(StoreError::NoSession(id.into()));
        }
        let mut out = Vec::new();
        // Unknown/legacy event shapes are skipped, not fatal.
        let mut st = self.conn.prepare("SELECT data FROM event_blocks WHERE session_id = ?1 ORDER BY first_seq")?;
        for data in st.query_map(params![id], |r| r.get::<_, Vec<u8>>(0))? {
            let text = inflate(&data?)?;
            out.extend(text.split('\n').filter_map(|j| serde_json::from_str::<Event>(j).ok()));
        }
        let mut st = self.conn.prepare("SELECT json FROM events WHERE session_id = ?1 ORDER BY seq")?;
        for j in st.query_map(params![id], |r| r.get::<_, String>(0))? {
            if let Ok(e) = serde_json::from_str::<Event>(&j?) {
                out.push(e);
            }
        }
        Ok(out)
    }
}
