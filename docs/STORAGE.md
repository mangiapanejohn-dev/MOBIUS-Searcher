# Storage

One SQLite file, `mobius.sqlite` in the per-user data directory (WAL mode; see
[INSTALL.md](INSTALL.md#where-files-live), or `--db PATH`). Written by the recorder
thread; a janitor thread applies the retention policy. Nothing leaves the
machine; no API key and no private key is ever written (the wallet's public
key is).

## What is kept

| Data | Kept | Why |
|---|---|---|
| Sessions, trades, executions, risk decisions, errors, PnL | everything | small, and the record of what the bot did |
| Event log (replay: `--replay`) | every event the UI saw, thinned: metrics ≤ 1 per metric per second (PnL, equity: all), health on state change or every 5 s per service | high-rate samples add nothing at replay resolution |
| Opportunities table | a numeric row per opportunity (edges, costs, status, latency) | `--report` statistics |
| Opportunity snapshot (full JSON), raw provider quotes | only for notable opportunities: gross > 0, or executable / sent / filled / failed | 99.9% are plain `EDGE_TOO_SMALL` skips; the event log still has them in full |
| Simulations | a row each (with logs) | calibration of the cost model |
| OKX data on the Markets page | never | display only |

## How

- Events are written uncompressed to `events` in batches (every 250 ms or 512
  events), then compacted into `event_blocks`: up to 1024 consecutive events
  as deflated JSON lines, once a block is full or its oldest event is a
  minute old. On a real session this is **42× smaller** (25 MB → 0.6 MB);
  compressing rows one by one would give 1.8×.
- Replay reads the blocks, then the uncompressed tail. Files written before
  compaction existed still replay.
- `event_counts` keeps per-kind counts (the log itself is compressed).
- Writes and compaction use `BEGIN IMMEDIATE`, so the janitor never makes a
  batch fail; the janitor deletes in chunks of 5 000 rows.

## When it is cleaned (retention)

At the start of every recording and every 30 minutes while it runs:

1. Sessions with no activity for **7 days** are deleted; sessions with a trade
   or an execution attempt are kept **90 days**.
2. A session running longer than 7 days drops its detail rows older than
   that (the session and its summary stay).
3. While the file holds more than **1 GB** of data, the oldest sessions are
   deleted — never the running one, never one with trades, never an open
   session written to in the last hour (another instance may be recording).
4. Freed pages go back to the OS (`auto_vacuum = INCREMENTAL` +
   `incremental_vacuum`); the WAL is truncated (`journal_size_limit` 64 MB).

The defaults live in `searcher_storage::Retention` (`keep_days`,
`keep_trading_days`, `max_db_mb`, `prune_every_min`); the `[storage]` config
section and the `--prune` / `--db-info` flags map onto them.

Files created before incremental vacuum existed stop growing (freed pages are
reused) but only shrink after a one-off `VACUUM` (`--prune` does it).

## Sizes to expect

Measured before this design: ~1.4 MB/min on a steady PAPER/LIVE run, and a
single 2-hour session of 460 k events at ~1 GB including indexes. With
thinning and compression the log is a few MB per hour; the 1 GB cap is
months of normal running.
