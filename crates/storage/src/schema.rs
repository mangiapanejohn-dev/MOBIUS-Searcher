pub const SCHEMA_VERSION: i64 = 2;

pub const SCHEMA: &str = r#"
-- Freed pages can be handed back to the OS (only takes effect on a new file).
PRAGMA auto_vacuum = INCREMENTAL;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
-- Truncate the WAL back to this size after checkpoints.
PRAGMA journal_size_limit = 67108864;

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    mode TEXT NOT NULL,
    version TEXT NOT NULL,
    config_summary TEXT NOT NULL,
    taker TEXT,
    events INTEGER NOT NULL DEFAULT 0,
    dropped INTEGER NOT NULL DEFAULT 0
);

-- Event log, the replay source of truth (the events the live UI consumed):
-- recent events uncompressed here, compacted into `event_blocks`.
CREATE TABLE IF NOT EXISTS events (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    seq INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

-- Compacted event log: consecutive events as deflated JSON lines.
CREATE TABLE IF NOT EXISTS event_blocks (
    session_id TEXT NOT NULL,
    first_seq INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    t0 INTEGER NOT NULL,
    t1 INTEGER NOT NULL,
    n INTEGER NOT NULL,
    raw_bytes INTEGER NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY (session_id, first_seq)
);

-- Events recorded per kind (the log itself is compressed).
CREATE TABLE IF NOT EXISTS event_counts (
    session_id TEXT NOT NULL, kind TEXT NOT NULL, n INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind)
);

CREATE TABLE IF NOT EXISTS samples (
    session_id TEXT NOT NULL, ts INTEGER NOT NULL, pair TEXT NOT NULL, side TEXT NOT NULL,
    price_micros INTEGER NOT NULL, source TEXT NOT NULL, size_atoms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS samples_ts ON samples(session_id, ts);

-- Latest state of each opportunity (upserted as it moves through the pipeline).
CREATE TABLE IF NOT EXISTS opportunities (
    session_id TEXT NOT NULL,
    id INTEGER NOT NULL,
    key TEXT NOT NULL,
    strategy TEXT NOT NULL,
    label TEXT NOT NULL,
    route TEXT NOT NULL,
    detected_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    status TEXT NOT NULL,
    skip_reason TEXT,
    input INTEGER NOT NULL,
    gross_output INTEGER NOT NULL,
    gross_pnl INTEGER NOT NULL,
    expected_net INTEGER NOT NULL,
    simulated_net INTEGER,
    gross_edge_ppm INTEGER NOT NULL,
    net_edge_ppm INTEGER NOT NULL,
    expected_net_usd_micros INTEGER,
    base_fee INTEGER NOT NULL,
    priority_fee INTEGER NOT NULL,
    jito_tip INTEGER NOT NULL,
    ata_rent INTEGER NOT NULL,
    expected_slippage INTEGER NOT NULL,
    safety_buffer INTEGER NOT NULL,
    cu_used INTEGER,
    cu_limit INTEGER NOT NULL,
    quote_latency_ms INTEGER NOT NULL,
    -- full JSON only for notable opportunities (gross > 0 or past the skip
    -- stage); '' otherwise — the event log has every one
    snapshot TEXT NOT NULL,
    PRIMARY KEY (session_id, id)
);
CREATE INDEX IF NOT EXISTS opp_key ON opportunities(session_id, key, detected_at);

-- Provider responses of notable opportunities (all of them are in the log).
CREATE TABLE IF NOT EXISTS quotes (
    session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, leg INTEGER NOT NULL,
    ts INTEGER NOT NULL, raw TEXT NOT NULL,
    PRIMARY KEY (session_id, opportunity_id, leg)
);

CREATE TABLE IF NOT EXISTS simulations (
    session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL,
    ok INTEGER NOT NULL, plan TEXT NOT NULL, fidelity TEXT NOT NULL,
    failure_class TEXT, failure_message TEXT,
    units INTEGER NOT NULL, cu_limit INTEGER NOT NULL, size_bytes INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL, context_slot INTEGER, json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS risk_decisions (
    session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL,
    approved INTEGER NOT NULL, violations TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS executions (
    session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL,
    mode TEXT NOT NULL, state TEXT NOT NULL, bundle_id TEXT, tip INTEGER NOT NULL,
    latency_ms INTEGER, json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS trades (
    session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL,
    strategy TEXT NOT NULL, label TEXT NOT NULL, paper INTEGER NOT NULL,
    input INTEGER NOT NULL, output INTEGER NOT NULL, fees INTEGER NOT NULL, tip INTEGER NOT NULL,
    expected_net INTEGER NOT NULL, net INTEGER NOT NULL, net_usd_micros INTEGER
);

CREATE TABLE IF NOT EXISTS pnl (
    session_id TEXT NOT NULL, ts INTEGER NOT NULL, cumulative_usd REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS errors (
    session_id TEXT NOT NULL, ts INTEGER NOT NULL, service TEXT NOT NULL, message TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS latency (
    session_id TEXT NOT NULL, ts INTEGER NOT NULL, metric TEXT NOT NULL, value REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS system_metrics (
    session_id TEXT NOT NULL, ts INTEGER NOT NULL, service TEXT NOT NULL, state TEXT NOT NULL,
    p50_ms INTEGER, requests INTEGER NOT NULL, errors INTEGER NOT NULL, rate_limited INTEGER NOT NULL,
    quota_remaining INTEGER
);
"#;
