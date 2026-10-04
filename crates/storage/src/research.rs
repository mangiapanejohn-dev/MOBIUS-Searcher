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

-- Liquidations read back from chain (facts of the chain, not of a run).
-- Amounts are raw token units as decimal text: they exceed 64 bits.
CREATE TABLE IF NOT EXISTS liq_events (
    venue TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL,
    tx TEXT NOT NULL, market TEXT NOT NULL, borrower TEXT NOT NULL,
    caller TEXT NOT NULL,           -- msg.sender of liquidate (usually the winner's contract)
    repaid TEXT NOT NULL, seized TEXT NOT NULL, bad_debt TEXT NOT NULL,
    block_time INTEGER,             -- unix seconds
    tx_index INTEGER, sender TEXT,  -- the transaction's position in its block and its signer
    loan TEXT, collateral TEXT, lltv REAL,
    incentive REAL,                 -- loan-token units at the oracle price: repaid × (LIF − 1)
    loan_usd REAL,                  -- NULL: loan token not valued
    gas_eth REAL,                   -- this event's share of what the transaction paid (L2 gas + L1 data)
    priority_gwei REAL,             -- effective gas price − base fee
    eth_usd REAL,
    since_block INTEGER,            -- first block at whose end the position was liquidatable
    since_time INTEGER,
    since_kind TEXT,                -- same_block | earlier | after_liquidation | horizon
    probes INTEGER, err TEXT,
    PRIMARY KEY (venue, block, log_index)
);
-- Round trips between two pools, worked out from their accounts at one slot
-- (no quote API): sell `size` of the base token on one pool, buy it back on
-- the other. A row per new slot seen, pair and size.
CREATE TABLE IF NOT EXISTS pool_edge (
    run_id TEXT NOT NULL, ts INTEGER NOT NULL, slot INTEGER NOT NULL,
    size INTEGER NOT NULL, sell_on TEXT NOT NULL, buy_on TEXT NOT NULL,
    gross_bps REAL NOT NULL         -- (base back − base in) / base in, after both pools' fees
);
CREATE INDEX IF NOT EXISTS pool_edge_run ON pool_edge(run_id, size);

-- A bin pool at each snapshot of --research-pools with the exchange price at
-- that moment: what an order resting in one of its bins would have met.
CREATE TABLE IF NOT EXISTS pool_snap (
    run_id TEXT NOT NULL, ts INTEGER NOT NULL, slot INTEGER NOT NULL, pool TEXT NOT NULL,
    active_id INTEGER NOT NULL, bin_step INTEGER NOT NULL,
    price REAL NOT NULL,            -- quote per base at the active bin
    lp_fee_bps REAL NOT NULL,       -- of a swap's input there, the part the bin's liquidity earns
    cex_mid REAL, cex_src TEXT      -- NULL: no fresh exchange price
);
CREATE INDEX IF NOT EXISTS pool_snap_run ON pool_snap(run_id, pool, ts);

-- Blocks first..next−1 have been searched for events.
CREATE TABLE IF NOT EXISTS liq_scan (venue TEXT PRIMARY KEY, first INTEGER NOT NULL, next INTEGER NOT NULL);

-- The lab (`--lab`): candles its rules read, and what each paper experiment did.
CREATE TABLE IF NOT EXISTS lab_bars (
    inst TEXT NOT NULL, bar TEXT NOT NULL, ts INTEGER NOT NULL,     -- ts: the bar's start, ms
    open REAL NOT NULL, high REAL NOT NULL, low REAL NOT NULL, close REAL NOT NULL, vol REAL NOT NULL,
    PRIMARY KEY (inst, bar, ts)
);
-- One run per rules file as it was (id = hash of the file): a changed file is a new run.
CREATE TABLE IF NOT EXISTS lab_runs (
    id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, version TEXT NOT NULL, manifest TEXT NOT NULL
);
-- Where an experiment stands (its account, as JSON) after the bar `bar_ts`.
CREATE TABLE IF NOT EXISTS lab_state (
    run_id TEXT NOT NULL, experiment TEXT NOT NULL, bar_ts INTEGER NOT NULL, json TEXT NOT NULL,
    PRIMARY KEY (run_id, experiment)
);
CREATE TABLE IF NOT EXISTS lab_fills (
    run_id TEXT NOT NULL, experiment TEXT NOT NULL, ts INTEGER NOT NULL, bar_ts INTEGER NOT NULL,
    side TEXT NOT NULL, price REAL NOT NULL,        -- the side of the book it was filled at
    bid REAL NOT NULL, ask REAL NOT NULL,           -- the book when the signal was acted on
    usd REAL NOT NULL, sol REAL NOT NULL, cost_usd REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS lab_equity (
    run_id TEXT NOT NULL, experiment TEXT NOT NULL, bar_ts INTEGER NOT NULL,
    close REAL NOT NULL, equity REAL NOT NULL, sol_value REAL NOT NULL,
    PRIMARY KEY (run_id, experiment, bar_ts)
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

/// One round trip between two pools at one slot (`pool_edge`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PoolEdge {
    pub run: String,
    pub ts: i64,
    pub slot: u64,
    pub size: u64,
    pub sell_on: String,
    pub buy_on: String,
    pub gross_bps: f64,
}

/// A bin pool at one snapshot (`pool_snap`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PoolSnap {
    pub run: String,
    pub ts: i64,
    pub slot: u64,
    pub pool: String,
    pub active_id: i32,
    pub bin_step: u16,
    pub price: f64,
    pub lp_fee_bps: f64,
    pub cex_mid: Option<f64>,
    pub cex_src: Option<String>,
}

/// One liquidation (`liq_events`). The fields after `bad_debt` are filled in
/// after the event is found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiqEvent {
    pub venue: String,
    pub block: u64,
    pub log_index: u32,
    pub tx: String,
    pub market: String,
    pub borrower: String,
    pub caller: String,
    pub repaid: String,
    pub seized: String,
    pub bad_debt: String,
    pub block_time: Option<i64>,
    pub tx_index: Option<u32>,
    pub sender: Option<String>,
    pub loan: Option<String>,
    pub collateral: Option<String>,
    pub lltv: Option<f64>,
    pub incentive: Option<f64>,
    pub loan_usd: Option<f64>,
    pub gas_eth: Option<f64>,
    pub priority_gwei: Option<f64>,
    pub eth_usd: Option<f64>,
    pub since_block: Option<u64>,
    pub since_time: Option<i64>,
    pub since_kind: Option<String>,
    pub probes: Option<u32>,
    pub err: Option<String>,
}

/// A candle: start (ms), open, high, low, close, volume.
pub type LabBar = (i64, f64, f64, f64, f64, f64);

#[derive(Clone, Debug, PartialEq)]
pub struct LabFill {
    pub ts: i64,
    pub bar_ts: i64,
    pub buy: bool,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub usd: f64,
    pub sol: f64,
    pub cost_usd: f64,
}

impl ResearchStore {
    pub fn insert_lab_bars(&self, inst: &str, bar: &str, bars: &[LabBar]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for b in bars {
            tx.execute(
                "INSERT OR REPLACE INTO lab_bars VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![inst, bar, b.0, b.1, b.2, b.3, b.4, b.5],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Candles from `from` (ms) on, oldest first.
    pub fn lab_bars(&self, inst: &str, bar: &str, from: i64) -> Result<Vec<LabBar>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT ts, open, high, low, close, vol FROM lab_bars WHERE inst = ?1 AND bar = ?2 AND ts >= ?3 ORDER BY ts",
        )?;
        let rows = st.query_map(params![inst, bar, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Registers the run unless it exists (a rules file keeps its run).
    pub fn begin_lab_run(&self, id: &str, started_at: i64, version: &str, manifest: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO lab_runs VALUES (?1,?2,?3,?4)",
            params![id, started_at, version, manifest],
        )?;
        Ok(())
    }

    /// Every lab run: id, start (ms), the rules file as it was.
    pub fn lab_runs(&self) -> Result<Vec<(String, i64, String)>, StoreError> {
        let mut st = self.conn.prepare("SELECT id, started_at, manifest FROM lab_runs ORDER BY started_at")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The last bar an experiment has seen and its account then.
    pub fn lab_state(&self, run: &str, experiment: &str) -> Result<Option<(i64, String)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT bar_ts, json FROM lab_state WHERE run_id = ?1 AND experiment = ?2",
                params![run, experiment],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// One bar of one experiment (`key`: run and experiment), whole or not at all: its fills, its equity, its account.
    pub fn record_lab_bar(
        &self,
        (run, experiment): (&str, &str),
        bar: &LabBar,
        fills: &[LabFill],
        equity: f64,
        sol_value: f64,
        state: &str,
    ) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for f in fills {
            tx.execute(
                "INSERT INTO lab_fills VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    run,
                    experiment,
                    f.ts,
                    f.bar_ts,
                    if f.buy { "buy" } else { "sell" },
                    f.price,
                    f.bid,
                    f.ask,
                    f.usd,
                    f.sol,
                    f.cost_usd
                ],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO lab_equity VALUES (?1,?2,?3,?4,?5,?6)",
            params![run, experiment, bar.0, bar.4, equity, sol_value],
        )?;
        tx.execute("INSERT OR REPLACE INTO lab_state VALUES (?1,?2,?3,?4)", params![run, experiment, bar.0, state])?;
        tx.commit()?;
        Ok(())
    }

    /// An experiment's equity at each bar it saw: bar start (ms), close, equity, value held in SOL.
    pub fn lab_equity(&self, run: &str, experiment: &str) -> Result<Vec<(i64, f64, f64, f64)>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT bar_ts, close, equity, sol_value FROM lab_equity WHERE run_id = ?1 AND experiment = ?2 ORDER BY bar_ts",
        )?;
        let rows = st.query_map(params![run, experiment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // candles cached before they carried their volume: only a cache, made again
        let old: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('lab_bars') WHERE name = 'close'
               AND NOT EXISTS (SELECT 1 FROM pragma_table_info('lab_bars') WHERE name = 'vol')",
            [],
            |r| r.get(0),
        )?;
        if old > 0 {
            conn.execute_batch("DROP TABLE lab_bars")?;
        }
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

    /// The round trips of one snapshot, written together.
    pub fn insert_pool_edges(&self, rows: &[PoolEdge]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for r in rows {
            tx.execute(
                "INSERT INTO pool_edge(run_id, ts, slot, size, sell_on, buy_on, gross_bps) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![r.run, r.ts, r.slot as i64, r.size as i64, r.sell_on, r.buy_on, r.gross_bps],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Round trips of the given runs (all runs when empty), in time order.
    pub fn pool_edges(&self, runs: &[String]) -> Result<Vec<PoolEdge>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT run_id, ts, slot, size, sell_on, buy_on, gross_bps FROM pool_edge ORDER BY run_id, ts, rowid",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PoolEdge {
                run: r.get(0)?,
                ts: r.get(1)?,
                slot: r.get::<_, i64>(2)? as u64,
                size: r.get::<_, i64>(3)? as u64,
                sell_on: r.get(4)?,
                buy_on: r.get(5)?,
                gross_bps: r.get(6)?,
            })
        })?;
        let all: Vec<PoolEdge> = rows.collect::<Result<_, _>>()?;
        Ok(all.into_iter().filter(|e| runs.is_empty() || runs.contains(&e.run)).collect())
    }

    pub fn insert_pool_snap(&self, s: &PoolSnap) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO pool_snap(run_id, ts, slot, pool, active_id, bin_step, price, lp_fee_bps, cex_mid, cex_src)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                s.run,
                s.ts,
                s.slot as i64,
                s.pool,
                s.active_id,
                s.bin_step,
                s.price,
                s.lp_fee_bps,
                s.cex_mid,
                s.cex_src
            ],
        )?;
        Ok(())
    }

    /// Bin-pool snapshots of the given runs (all runs when empty), by run, pool and time.
    pub fn pool_snaps(&self, runs: &[String]) -> Result<Vec<PoolSnap>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT run_id, ts, slot, pool, active_id, bin_step, price, lp_fee_bps, cex_mid, cex_src
             FROM pool_snap ORDER BY run_id, pool, ts",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PoolSnap {
                run: r.get(0)?,
                ts: r.get(1)?,
                slot: r.get::<_, i64>(2)? as u64,
                pool: r.get(3)?,
                active_id: r.get(4)?,
                bin_step: r.get(5)?,
                price: r.get(6)?,
                lp_fee_bps: r.get(7)?,
                cex_mid: r.get(8)?,
                cex_src: r.get(9)?,
            })
        })?;
        let all: Vec<PoolSnap> = rows.collect::<Result<_, _>>()?;
        Ok(all.into_iter().filter(|e| runs.is_empty() || runs.contains(&e.run)).collect())
    }

    /// Record newly found liquidations; ones already known are left as they are.
    pub fn insert_liq_events(&self, events: &[LiqEvent]) -> Result<(), StoreError> {
        for e in events {
            self.conn.execute(
                "INSERT OR IGNORE INTO liq_events(venue, block, log_index, tx, market, borrower, caller, repaid, seized, bad_debt)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    e.venue,
                    e.block as i64,
                    e.log_index,
                    e.tx,
                    e.market,
                    e.borrower,
                    e.caller,
                    e.repaid,
                    e.seized,
                    e.bad_debt
                ],
            )?;
        }
        Ok(())
    }

    /// Store what was worked out about an event (everything after `bad_debt`).
    pub fn update_liq_event(&self, e: &LiqEvent) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE liq_events SET block_time = ?4, tx_index = ?5, sender = ?6, loan = ?7, collateral = ?8, lltv = ?9,
                incentive = ?10, loan_usd = ?11, gas_eth = ?12, priority_gwei = ?13, eth_usd = ?14,
                since_block = ?15, since_time = ?16, since_kind = ?17, probes = ?18, err = ?19
             WHERE venue = ?1 AND block = ?2 AND log_index = ?3",
            params![
                e.venue,
                e.block as i64,
                e.log_index,
                e.block_time,
                e.tx_index,
                e.sender,
                e.loan,
                e.collateral,
                e.lltv,
                e.incentive,
                e.loan_usd,
                e.gas_eth,
                e.priority_gwei,
                e.eth_usd,
                e.since_block.map(|b| b as i64),
                e.since_time,
                e.since_kind,
                e.probes,
                e.err
            ],
        )?;
        Ok(())
    }

    /// A venue's liquidations from block `from` on, in chain order.
    pub fn liq_events(&self, venue: &str, from: u64) -> Result<Vec<LiqEvent>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT venue, block, log_index, tx, market, borrower, caller, repaid, seized, bad_debt, block_time,
                    tx_index, sender, loan, collateral, lltv, incentive, loan_usd, gas_eth, priority_gwei, eth_usd,
                    since_block, since_time, since_kind, probes, err
             FROM liq_events WHERE venue = ?1 AND block >= ?2 ORDER BY block, log_index",
        )?;
        let rows = st.query_map(params![venue, from as i64], |r| {
            Ok(LiqEvent {
                venue: r.get(0)?,
                block: r.get::<_, i64>(1)? as u64,
                log_index: r.get(2)?,
                tx: r.get(3)?,
                market: r.get(4)?,
                borrower: r.get(5)?,
                caller: r.get(6)?,
                repaid: r.get(7)?,
                seized: r.get(8)?,
                bad_debt: r.get(9)?,
                block_time: r.get(10)?,
                tx_index: r.get(11)?,
                sender: r.get(12)?,
                loan: r.get(13)?,
                collateral: r.get(14)?,
                lltv: r.get(15)?,
                incentive: r.get(16)?,
                loan_usd: r.get(17)?,
                gas_eth: r.get(18)?,
                priority_gwei: r.get(19)?,
                eth_usd: r.get(20)?,
                since_block: r.get::<_, Option<i64>>(21)?.map(|b| b as u64),
                since_time: r.get(22)?,
                since_kind: r.get(23)?,
                probes: r.get(24)?,
                err: r.get(25)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The block range already searched for a venue's liquidations: `first..next`.
    pub fn liq_scan(&self, venue: &str) -> Result<Option<(u64, u64)>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT first, next FROM liq_scan WHERE venue = ?1", params![venue], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
            })
            .optional()?)
    }

    pub fn set_liq_scan(&self, venue: &str, first: u64, next: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO liq_scan(venue, first, next) VALUES (?1,?2,?3)
             ON CONFLICT(venue) DO UPDATE SET first = ?2, next = ?3",
            params![venue, first as i64, next as i64],
        )?;
        Ok(())
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

    #[test]
    fn liquidations_are_found_once_then_filled_in() {
        let s = ResearchStore::open_in_memory().unwrap();
        let found = LiqEvent {
            venue: "base".into(),
            block: 52_121_149,
            log_index: 419,
            tx: "0xfd".into(),
            market: "0x45".into(),
            borrower: "0xb8".into(),
            caller: "0xb3".into(),
            repaid: "141980096".into(),
            seized: "121595503775334797723".into(), // more than 64 bits
            bad_debt: "0".into(),
            ..Default::default()
        };
        s.insert_liq_events(std::slice::from_ref(&found)).unwrap();
        let done = LiqEvent {
            block_time: Some(1_791_031_645),
            sender: Some("0xc1".into()),
            lltv: Some(0.86),
            incentive: Some(6.22),
            loan_usd: Some(1.0),
            since_block: Some(52_121_148),
            since_kind: Some("earlier".into()),
            probes: Some(2),
            ..found.clone()
        };
        s.update_liq_event(&done).unwrap();
        // a later run finds the same log again: what was worked out stays
        s.insert_liq_events(std::slice::from_ref(&found)).unwrap();
        assert_eq!(s.liq_events("base", 0).unwrap(), vec![done]);
        assert!(s.liq_events("base", 52_121_150).unwrap().is_empty());
        assert!(s.liq_events("arbitrum", 0).unwrap().is_empty());

        let edge = PoolEdge {
            run: "p1".into(),
            ts: 5,
            slot: 453_000_000,
            size: 100_000_000,
            sell_on: "Whirlpool".into(),
            buy_on: "Raydium CLMM".into(),
            gross_bps: -6.5,
        };
        s.insert_pool_edges(&[edge.clone(), PoolEdge { run: "p2".into(), ..edge.clone() }]).unwrap();
        assert_eq!(s.pool_edges(&["p1".into()]).unwrap(), vec![edge]);
        assert_eq!(s.pool_edges(&[]).unwrap().len(), 2);
        let snap = PoolSnap {
            run: "p1".into(),
            ts: 5,
            slot: 453_000_000,
            pool: "Meteora DLMM".into(),
            active_id: -21_212,
            bin_step: 1,
            price: 119.9,
            lp_fee_bps: 0.97,
            cex_mid: Some(119.93),
            cex_src: Some("okx".into()),
        };
        s.insert_pool_snap(&snap).unwrap();
        s.insert_pool_snap(&PoolSnap { ts: 6, cex_mid: None, cex_src: None, ..snap.clone() }).unwrap();
        let back = s.pool_snaps(&["p1".into()]).unwrap();
        assert_eq!((back.len(), &back[0], back[1].cex_mid), (2, &snap, None));

        assert_eq!(s.liq_scan("base").unwrap(), None);
        s.set_liq_scan("base", 100, 200).unwrap();
        s.set_liq_scan("base", 50, 300).unwrap();
        assert_eq!(s.liq_scan("base").unwrap(), Some((50, 300)));
    }

    #[test]
    fn a_lab_run_keeps_its_first_rules_and_each_bar_whole() {
        let s = ResearchStore::open_in_memory().unwrap();
        s.insert_lab_bars("SOL-USDT", "15m", &[(900_000, 1.0, 2.0, 0.5, 1.5, 7.0), (0, 1.0, 1.0, 1.0, 1.0, 3.0)])
            .unwrap();
        s.insert_lab_bars("SOL-USDT", "15m", &[(900_000, 1.0, 2.0, 0.5, 1.6, 8.0)]).unwrap();
        assert_eq!(
            s.lab_bars("SOL-USDT", "15m", 0).unwrap(),
            vec![(0, 1.0, 1.0, 1.0, 1.0, 3.0), (900_000, 1.0, 2.0, 0.5, 1.6, 8.0)]
        );
        assert_eq!(s.lab_bars("SOL-USDT", "15m", 1).unwrap().len(), 1);
        assert!(s.lab_bars("SOL-USDT", "1m", 0).unwrap().is_empty());

        s.begin_lab_run("abc", 10, "0.3.0", "first").unwrap();
        s.begin_lab_run("abc", 99, "0.3.0", "changed").unwrap();
        assert_eq!(s.lab_runs().unwrap(), vec![("abc".to_string(), 10, "first".to_string())]);

        assert_eq!(s.lab_state("abc", "grid").unwrap(), None);
        let fill = LabFill {
            ts: 5,
            bar_ts: 0,
            buy: true,
            price: 1.0,
            bid: 0.99,
            ask: 1.0,
            usd: 23.0,
            sol: 22.9,
            cost_usd: 0.003,
        };
        s.record_lab_bar(
            ("abc", "grid"),
            &(0, 1.0, 1.0, 1.0, 1.0, 3.0),
            std::slice::from_ref(&fill),
            23.0,
            0.0,
            "{\"a\":1}",
        )
        .unwrap();
        s.record_lab_bar(("abc", "grid"), &(900_000, 1.0, 2.0, 0.5, 1.6, 8.0), &[], 36.6, 36.6, "{\"a\":2}").unwrap();
        // the same bar again (a restart): its row is replaced, not doubled
        s.record_lab_bar(("abc", "grid"), &(900_000, 1.0, 2.0, 0.5, 1.6, 8.0), &[], 36.7, 36.7, "{\"a\":3}").unwrap();
        assert_eq!(s.lab_state("abc", "grid").unwrap(), Some((900_000, "{\"a\":3}".to_string())));
        assert_eq!(s.lab_equity("abc", "grid").unwrap(), vec![(0, 1.0, 23.0, 0.0), (900_000, 1.6, 36.7, 36.7)]);
        assert!(s.lab_equity("abc", "other").unwrap().is_empty());
        let fills: i64 =
            s.conn().query_row("SELECT count(*) FROM lab_fills WHERE side = 'buy'", [], |r| r.get(0)).unwrap();
        assert_eq!(fills, 1);
    }

    #[test]
    fn a_candle_cache_from_before_volumes_is_made_again() {
        let path = std::env::temp_dir().join(format!("mobius-lab-cache-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE lab_bars (inst TEXT, bar TEXT, ts INTEGER, open REAL, high REAL, low REAL, close REAL,
                 PRIMARY KEY (inst, bar, ts)); INSERT INTO lab_bars VALUES ('SOL-USDT','15m',0,1,1,1,1);",
            )
            .unwrap();
        }
        let s = ResearchStore::open(&path).unwrap();
        assert!(s.lab_bars("SOL-USDT", "15m", 0).unwrap().is_empty(), "the old rows had no volume: fetched again");
        s.insert_lab_bars("SOL-USDT", "15m", &[(0, 1.0, 1.0, 1.0, 1.0, 5.0)]).unwrap();
        drop(s);
        // and a current file is left as it is
        assert_eq!(ResearchStore::open(&path).unwrap().lab_bars("SOL-USDT", "15m", 0).unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
