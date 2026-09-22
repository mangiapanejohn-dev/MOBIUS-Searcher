//! `--research` recordings: `<data_dir>/research.sqlite`, separate from the
//! trading database (its retention rules do not apply here). Every sample is
//! kept, including the unremarkable ones: the report shows whole
//! distributions, never only the positive tail.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::StoreError;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    version TEXT NOT NULL,
    setup TEXT NOT NULL,            -- where it ran, proxy, Jupiter tier, config (JSON)
    awake_s INTEGER NOT NULL DEFAULT 0,
    jupiter_requests INTEGER NOT NULL DEFAULT 0,
    jupiter_errors INTEGER NOT NULL DEFAULT 0,
    jupiter_429 INTEGER NOT NULL DEFAULT 0
);

-- Size ladder: one route, every size, quoted back to back.
CREATE TABLE IF NOT EXISTS ladder (
    run_id TEXT NOT NULL, round INTEGER NOT NULL, ts INTEGER NOT NULL,
    route_key TEXT NOT NULL, route_label TEXT NOT NULL, size INTEGER NOT NULL,
    out1 INTEGER, out2 INTEGER,
    gross INTEGER,                  -- out2 − size (lamports), before any cost
    priority_est INTEGER,           -- lamports: provider CU price × 300k CU
    impact1_ppm INTEGER, impact2_ppm INTEGER,
    dexes1 TEXT, dexes2 TEXT,
    gap_ms INTEGER,                 -- between the two quotes
    err TEXT
);
CREATE INDEX IF NOT EXISTS ladder_run ON ladder(run_id, size);

-- Cross-chain: the same asset bought and sold on Solana and on an EVM chain.
CREATE TABLE IF NOT EXISTS xchain (
    run_id TEXT NOT NULL, round INTEGER NOT NULL, ts INTEGER NOT NULL,
    asset TEXT NOT NULL, notional_usd INTEGER NOT NULL, venue TEXT NOT NULL,
    qty REAL,                       -- asset units bought on Solana for the notional
    sol_buy_px REAL, sol_sell_px REAL, evm_buy_px REAL, evm_sell_px REAL,
    sol_fee_usd REAL, evm_gas_usd REAL,
    a_bps REAL,                     -- buy on Solana, sell on the EVM chain, after fees and gas
    b_bps REAL,                     -- buy on the EVM chain, sell on Solana
    skew_ms INTEGER, evm_block TEXT, err TEXT
);
CREATE INDEX IF NOT EXISTS xchain_run ON xchain(run_id, asset, venue);

-- DEX lag: pool mid vs CEX mid, sampled on a fixed clock (time-weighted).
CREATE TABLE IF NOT EXISTS lag_ticks (
    run_id TEXT NOT NULL, ts INTEGER NOT NULL, dex TEXT NOT NULL,
    pool_mid REAL NOT NULL, pool_age_ms INTEGER NOT NULL,
    cex_mid REAL NOT NULL, cex_src TEXT NOT NULL, cex_age_ms INTEGER NOT NULL,
    gap_bps REAL NOT NULL           -- (pool − cex) / cex
);
CREATE INDEX IF NOT EXISTS lag_ticks_run ON lag_ticks(run_id, dex);

-- A gap over the trigger (kind = trigger) or a random check (kind = control),
-- with one executable quote on that DEX and CEX markouts afterwards.
CREATE TABLE IF NOT EXISTS lag_episodes (
    run_id TEXT NOT NULL, id INTEGER NOT NULL, kind TEXT NOT NULL,
    dex TEXT NOT NULL, side TEXT NOT NULL,   -- buy_on_dex | sell_on_dex
    start_ts INTEGER NOT NULL, end_ts INTEGER,
    start_gap_bps REAL NOT NULL, peak_gap_bps REAL NOT NULL, trigger_bps REAL NOT NULL,
    pool_age_ms INTEGER NOT NULL,
    cex_mid REAL NOT NULL, cex_bid REAL, cex_ask REAL, cex_src TEXT NOT NULL,
    confirm_ts INTEGER, confirm_ms INTEGER, size INTEGER,
    exec_px REAL,
    exec_gap_bps REAL,              -- vs CEX mid, positive = better than fair
    exec_gap_touch_bps REAL,        -- vs the CEX side you would unwind on
    exec_dexes TEXT, confirm_err TEXT,
    markouts TEXT,                  -- JSON [{"s":1,"cex_mid":…,"pool_mid":…}]
    PRIMARY KEY (run_id, id)
);

-- Wide gaps: the same episode quoted at bigger sizes (does the edge scale?).
CREATE TABLE IF NOT EXISTS lag_scale (
    run_id TEXT NOT NULL, episode INTEGER NOT NULL, size INTEGER NOT NULL,
    ts INTEGER NOT NULL, confirm_ms INTEGER,
    exec_px REAL, exec_gap_bps REAL, exec_gap_touch_bps REAL, err TEXT,
    PRIMARY KEY (run_id, episode, size)
);

-- The way back: after an episode's entry quote, the reverse swap (any route)
-- quoted at fixed delays for exactly what the entry delivered. rt_bps is the
-- on-chain round trip in the entry's input token, before transaction costs.
CREATE TABLE IF NOT EXISTS lag_exit (
    run_id TEXT NOT NULL, episode INTEGER NOT NULL, after_s INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    late_ms INTEGER,                -- quote answered this long after entry + after_s
    entry_in INTEGER, entry_out INTEGER, exit_out INTEGER,
    rt_bps REAL, dexes TEXT, err TEXT,
    PRIMARY KEY (run_id, episode, after_s)
);

-- How far behind the chain head each pool notification arrived (slots).
CREATE TABLE IF NOT EXISTS lag_feed (
    run_id TEXT NOT NULL, ts INTEGER NOT NULL, dex TEXT NOT NULL,
    slot INTEGER NOT NULL, head_slot INTEGER NOT NULL
);
"#;

pub struct ResearchStore {
    conn: Connection,
}

#[derive(Clone, Debug, Default)]
pub struct LadderRow {
    pub round: i64,
    pub ts: i64,
    pub route_key: String,
    pub route_label: String,
    pub size: u64,
    pub out1: Option<u64>,
    pub out2: Option<u64>,
    pub gross: Option<i64>,
    pub priority_est: Option<u64>,
    pub impact1_ppm: Option<i64>,
    pub impact2_ppm: Option<i64>,
    pub dexes1: Option<String>,
    pub dexes2: Option<String>,
    pub gap_ms: Option<i64>,
    pub err: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct XchainRow {
    pub round: i64,
    pub ts: i64,
    pub asset: String,
    pub notional_usd: u32,
    pub venue: String,
    pub qty: Option<f64>,
    pub sol_buy_px: Option<f64>,
    pub sol_sell_px: Option<f64>,
    pub evm_buy_px: Option<f64>,
    pub evm_sell_px: Option<f64>,
    pub sol_fee_usd: Option<f64>,
    pub evm_gas_usd: Option<f64>,
    pub a_bps: Option<f64>,
    pub b_bps: Option<f64>,
    pub skew_ms: Option<i64>,
    pub evm_block: Option<String>,
    pub err: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LagTick {
    pub ts: i64,
    pub dex: String,
    pub pool_mid: f64,
    pub pool_age_ms: i64,
    pub cex_mid: f64,
    pub cex_src: String,
    pub cex_age_ms: i64,
    pub gap_bps: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Episode {
    pub id: i64,
    pub kind: String,
    pub dex: String,
    pub side: String,
    pub start_ts: i64,
    pub end_ts: Option<i64>,
    pub start_gap_bps: f64,
    pub peak_gap_bps: f64,
    pub trigger_bps: f64,
    pub pool_age_ms: i64,
    pub cex_mid: f64,
    pub cex_bid: Option<f64>,
    pub cex_ask: Option<f64>,
    pub cex_src: String,
    pub confirm_ts: Option<i64>,
    pub confirm_ms: Option<i64>,
    pub size: Option<u64>,
    pub exec_px: Option<f64>,
    pub exec_gap_bps: Option<f64>,
    pub exec_gap_touch_bps: Option<f64>,
    pub exec_dexes: Option<String>,
    pub confirm_err: Option<String>,
    pub markouts: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ScaleRow {
    pub run_id: String,
    pub episode: i64,
    pub size: u64,
    pub ts: i64,
    pub confirm_ms: Option<i64>,
    pub exec_px: Option<f64>,
    pub exec_gap_bps: Option<f64>,
    pub exec_gap_touch_bps: Option<f64>,
    pub err: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ExitRow {
    pub run_id: String,
    pub episode: i64,
    pub after_s: u32,
    pub ts: i64,
    pub late_ms: Option<i64>,
    pub entry_in: Option<u64>,
    pub entry_out: Option<u64>,
    pub exit_out: Option<u64>,
    pub rt_bps: Option<f64>,
    pub dexes: Option<String>,
    pub err: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FeedRow {
    pub ts: i64,
    pub dex: String,
    pub slot: u64,
    pub head_slot: u64,
}

#[derive(Clone, Debug)]
pub struct RunRow {
    pub id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub version: String,
    pub setup: String,
    pub awake_s: i64,
    pub jupiter_requests: i64,
    pub jupiter_errors: i64,
    pub jupiter_429: i64,
}

impl ResearchStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn begin_run(&self, id: &str, started_at: i64, version: &str, setup: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO runs(id, started_at, version, setup) VALUES (?1,?2,?3,?4)",
            params![id, started_at, version, setup],
        )?;
        Ok(())
    }

    /// Progress so far (called periodically, so a killed run keeps its counters).
    pub fn update_run(
        &self,
        id: &str,
        ended_at: Option<i64>,
        awake_s: i64,
        requests: i64,
        errors: i64,
        rate_limited: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE runs SET ended_at = ?2, awake_s = ?3, jupiter_requests = ?4, jupiter_errors = ?5, jupiter_429 = ?6 WHERE id = ?1",
            params![id, ended_at, awake_s, requests, errors, rate_limited],
        )?;
        Ok(())
    }

    pub fn insert_ladder(&self, run: &str, r: &LadderRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO ladder(run_id, round, ts, route_key, route_label, size, out1, out2, gross, priority_est,
                impact1_ppm, impact2_ppm, dexes1, dexes2, gap_ms, err)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                run,
                r.round,
                r.ts,
                r.route_key,
                r.route_label,
                r.size as i64,
                r.out1.map(|v| v as i64),
                r.out2.map(|v| v as i64),
                r.gross,
                r.priority_est.map(|v| v as i64),
                r.impact1_ppm,
                r.impact2_ppm,
                r.dexes1,
                r.dexes2,
                r.gap_ms,
                r.err
            ],
        )?;
        Ok(())
    }

    pub fn insert_xchain(&self, run: &str, r: &XchainRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO xchain(run_id, round, ts, asset, notional_usd, venue, qty, sol_buy_px, sol_sell_px,
                evm_buy_px, evm_sell_px, sol_fee_usd, evm_gas_usd, a_bps, b_bps, skew_ms, evm_block, err)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                run,
                r.round,
                r.ts,
                r.asset,
                r.notional_usd,
                r.venue,
                r.qty,
                r.sol_buy_px,
                r.sol_sell_px,
                r.evm_buy_px,
                r.evm_sell_px,
                r.sol_fee_usd,
                r.evm_gas_usd,
                r.a_bps,
                r.b_bps,
                r.skew_ms,
                r.evm_block,
                r.err
            ],
        )?;
        Ok(())
    }

    pub fn insert_lag_tick(&self, run: &str, t: &LagTick) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO lag_ticks(run_id, ts, dex, pool_mid, pool_age_ms, cex_mid, cex_src, cex_age_ms, gap_bps)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![run, t.ts, t.dex, t.pool_mid, t.pool_age_ms, t.cex_mid, t.cex_src, t.cex_age_ms, t.gap_bps],
        )?;
        Ok(())
    }

    /// Insert or replace (an episode is written when it opens and again as it
    /// gains its quote, end time and markouts).
    pub fn upsert_episode(&self, run: &str, e: &Episode) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO lag_episodes(run_id, id, kind, dex, side, start_ts, end_ts, start_gap_bps,
                peak_gap_bps, trigger_bps, pool_age_ms, cex_mid, cex_bid, cex_ask, cex_src, confirm_ts, confirm_ms,
                size, exec_px, exec_gap_bps, exec_gap_touch_bps, exec_dexes, confirm_err, markouts)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            params![
                run,
                e.id,
                e.kind,
                e.dex,
                e.side,
                e.start_ts,
                e.end_ts,
                e.start_gap_bps,
                e.peak_gap_bps,
                e.trigger_bps,
                e.pool_age_ms,
                e.cex_mid,
                e.cex_bid,
                e.cex_ask,
                e.cex_src,
                e.confirm_ts,
                e.confirm_ms,
                e.size.map(|v| v as i64),
                e.exec_px,
                e.exec_gap_bps,
                e.exec_gap_touch_bps,
                e.exec_dexes,
                e.confirm_err,
                e.markouts
            ],
        )?;
        Ok(())
    }

    pub fn insert_scale(&self, r: &ScaleRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO lag_scale(run_id, episode, size, ts, confirm_ms, exec_px, exec_gap_bps,
                exec_gap_touch_bps, err) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                r.run_id,
                r.episode,
                r.size as i64,
                r.ts,
                r.confirm_ms,
                r.exec_px,
                r.exec_gap_bps,
                r.exec_gap_touch_bps,
                r.err
            ],
        )?;
        Ok(())
    }

    pub fn scale(&self, runs: &[String]) -> Result<Vec<ScaleRow>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT run_id, episode, size, ts, confirm_ms, exec_px, exec_gap_bps, exec_gap_touch_bps, err
                 FROM lag_scale WHERE run_id = ?1 ORDER BY episode, size",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(ScaleRow {
                    run_id: r.get(0)?,
                    episode: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    ts: r.get(3)?,
                    confirm_ms: r.get(4)?,
                    exec_px: r.get(5)?,
                    exec_gap_bps: r.get(6)?,
                    exec_gap_touch_bps: r.get(7)?,
                    err: r.get(8)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn insert_exit(&self, r: &ExitRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO lag_exit(run_id, episode, after_s, ts, late_ms, entry_in, entry_out, exit_out,
                rt_bps, dexes, err) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                r.run_id,
                r.episode,
                r.after_s,
                r.ts,
                r.late_ms,
                r.entry_in.map(|v| v as i64),
                r.entry_out.map(|v| v as i64),
                r.exit_out.map(|v| v as i64),
                r.rt_bps,
                r.dexes,
                r.err
            ],
        )?;
        Ok(())
    }

    pub fn exits(&self, runs: &[String]) -> Result<Vec<ExitRow>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT run_id, episode, after_s, ts, late_ms, entry_in, entry_out, exit_out, rt_bps, dexes, err
                 FROM lag_exit WHERE run_id = ?1 ORDER BY episode, after_s",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(ExitRow {
                    run_id: r.get(0)?,
                    episode: r.get(1)?,
                    after_s: r.get(2)?,
                    ts: r.get(3)?,
                    late_ms: r.get(4)?,
                    entry_in: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    entry_out: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    exit_out: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                    rt_bps: r.get(8)?,
                    dexes: r.get(9)?,
                    err: r.get(10)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn insert_feed(&self, run: &str, f: &FeedRow) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO lag_feed(run_id, ts, dex, slot, head_slot) VALUES (?1,?2,?3,?4,?5)",
            params![run, f.ts, f.dex, f.slot as i64, f.head_slot as i64],
        )?;
        Ok(())
    }

    pub fn feed(&self, runs: &[String]) -> Result<Vec<FeedRow>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare("SELECT ts, dex, slot, head_slot FROM lag_feed WHERE run_id = ?1")?;
            let rows = st.query_map([run], |r| {
                Ok(FeedRow {
                    ts: r.get(0)?,
                    dex: r.get(1)?,
                    slot: r.get::<_, i64>(2)? as u64,
                    head_slot: r.get::<_, i64>(3)? as u64,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn runs(&self) -> Result<Vec<RunRow>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT id, started_at, ended_at, version, setup, awake_s, jupiter_requests, jupiter_errors, jupiter_429
             FROM runs ORDER BY started_at DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                version: r.get(3)?,
                setup: r.get(4)?,
                awake_s: r.get(5)?,
                jupiter_requests: r.get(6)?,
                jupiter_errors: r.get(7)?,
                jupiter_429: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn run(&self, id: &str) -> Result<Option<RunRow>, StoreError> {
        Ok(self.runs()?.into_iter().find(|r| r.id == id))
    }

    pub fn ladder(&self, runs: &[String]) -> Result<Vec<LadderRow>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT round, ts, route_key, route_label, size, out1, out2, gross, priority_est, impact1_ppm,
                        impact2_ppm, dexes1, dexes2, gap_ms, err
                 FROM ladder WHERE run_id = ?1 ORDER BY ts",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(LadderRow {
                    round: r.get(0)?,
                    ts: r.get(1)?,
                    route_key: r.get(2)?,
                    route_label: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    out1: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    out2: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    gross: r.get(7)?,
                    priority_est: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                    impact1_ppm: r.get(9)?,
                    impact2_ppm: r.get(10)?,
                    dexes1: r.get(11)?,
                    dexes2: r.get(12)?,
                    gap_ms: r.get(13)?,
                    err: r.get(14)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn xchain(&self, runs: &[String]) -> Result<Vec<XchainRow>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT round, ts, asset, notional_usd, venue, qty, sol_buy_px, sol_sell_px, evm_buy_px, evm_sell_px,
                        sol_fee_usd, evm_gas_usd, a_bps, b_bps, skew_ms, evm_block, err
                 FROM xchain WHERE run_id = ?1 ORDER BY ts",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(XchainRow {
                    round: r.get(0)?,
                    ts: r.get(1)?,
                    asset: r.get(2)?,
                    notional_usd: r.get(3)?,
                    venue: r.get(4)?,
                    qty: r.get(5)?,
                    sol_buy_px: r.get(6)?,
                    sol_sell_px: r.get(7)?,
                    evm_buy_px: r.get(8)?,
                    evm_sell_px: r.get(9)?,
                    sol_fee_usd: r.get(10)?,
                    evm_gas_usd: r.get(11)?,
                    a_bps: r.get(12)?,
                    b_bps: r.get(13)?,
                    skew_ms: r.get(14)?,
                    evm_block: r.get(15)?,
                    err: r.get(16)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn lag_ticks(&self, runs: &[String]) -> Result<Vec<LagTick>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT ts, dex, pool_mid, pool_age_ms, cex_mid, cex_src, cex_age_ms, gap_bps
                 FROM lag_ticks WHERE run_id = ?1 ORDER BY ts",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(LagTick {
                    ts: r.get(0)?,
                    dex: r.get(1)?,
                    pool_mid: r.get(2)?,
                    pool_age_ms: r.get(3)?,
                    cex_mid: r.get(4)?,
                    cex_src: r.get(5)?,
                    cex_age_ms: r.get(6)?,
                    gap_bps: r.get(7)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub fn episodes(&self, runs: &[String]) -> Result<Vec<Episode>, StoreError> {
        let mut out = Vec::new();
        for run in runs {
            let mut st = self.conn.prepare(
                "SELECT id, kind, dex, side, start_ts, end_ts, start_gap_bps, peak_gap_bps, trigger_bps, pool_age_ms,
                        cex_mid, cex_bid, cex_ask, cex_src, confirm_ts, confirm_ms, size, exec_px, exec_gap_bps,
                        exec_gap_touch_bps, exec_dexes, confirm_err, markouts
                 FROM lag_episodes WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = st.query_map([run], |r| {
                Ok(Episode {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    dex: r.get(2)?,
                    side: r.get(3)?,
                    start_ts: r.get(4)?,
                    end_ts: r.get(5)?,
                    start_gap_bps: r.get(6)?,
                    peak_gap_bps: r.get(7)?,
                    trigger_bps: r.get(8)?,
                    pool_age_ms: r.get(9)?,
                    cex_mid: r.get(10)?,
                    cex_bid: r.get(11)?,
                    cex_ask: r.get(12)?,
                    cex_src: r.get(13)?,
                    confirm_ts: r.get(14)?,
                    confirm_ms: r.get(15)?,
                    size: r.get::<_, Option<i64>>(16)?.map(|v| v as u64),
                    exec_px: r.get(17)?,
                    exec_gap_bps: r.get(18)?,
                    exec_gap_touch_bps: r.get(19)?,
                    exec_dexes: r.get(20)?,
                    confirm_err: r.get(21)?,
                    markouts: r.get(22)?,
                })
            })?;
            out.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    /// Row counts of one run (status line).
    pub fn counts(&self, run: &str) -> Result<[i64; 4], StoreError> {
        let n = |t: &str| -> Result<i64, StoreError> {
            Ok(self
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {t} WHERE run_id = ?1"), [run], |r| r.get(0))
                .optional()?
                .unwrap_or(0))
        };
        Ok([n("ladder")?, n("xchain")?, n("lag_ticks")?, n("lag_episodes")?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip() {
        let s = ResearchStore::open_in_memory().unwrap();
        s.begin_run("r1", 1, "0.2.0", "{}").unwrap();
        s.insert_ladder(
            "r1",
            &LadderRow { round: 1, ts: 2, route_key: "rt".into(), size: 10, gross: Some(-3), ..Default::default() },
        )
        .unwrap();
        s.insert_xchain(
            "r1",
            &XchainRow {
                round: 1,
                ts: 2,
                asset: "ETH".into(),
                notional_usd: 25,
                venue: "base".into(),
                a_bps: Some(-4.5),
                ..Default::default()
            },
        )
        .unwrap();
        s.insert_lag_tick(
            "r1",
            &LagTick {
                ts: 3,
                dex: "Whirlpool".into(),
                pool_mid: 100.0,
                pool_age_ms: 10,
                cex_mid: 100.1,
                cex_src: "okx".into(),
                cex_age_ms: 5,
                gap_bps: -9.99,
            },
        )
        .unwrap();
        let mut e = Episode { id: 1, kind: "trigger".into(), dex: "Whirlpool".into(), ..Default::default() };
        s.upsert_episode("r1", &e).unwrap();
        e.exec_gap_bps = Some(1.5);
        s.upsert_episode("r1", &e).unwrap();
        s.update_run("r1", Some(9), 8, 7, 1, 0).unwrap();

        let runs = vec!["r1".to_string()];
        assert_eq!(s.ladder(&runs).unwrap()[0].gross, Some(-3));
        assert_eq!(s.xchain(&runs).unwrap()[0].a_bps, Some(-4.5));
        assert_eq!(s.lag_ticks(&runs).unwrap().len(), 1);
        let eps = s.episodes(&runs).unwrap();
        assert_eq!(eps.len(), 1, "upsert replaces");
        assert_eq!(eps[0].exec_gap_bps, Some(1.5));
        assert_eq!(s.counts("r1").unwrap(), [1, 1, 1, 1]);
        let r = s.run("r1").unwrap().unwrap();
        assert_eq!((r.ended_at, r.awake_s, r.jupiter_requests), (Some(9), 8, 7));
    }
}
