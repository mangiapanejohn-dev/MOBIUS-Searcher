#!/usr/bin/env python3
"""The tracker against a database that behaves like the engine's: rows
replaced under their key, events packed into blocks and deleted, a table
emptied. Nothing may be lost or doubled, and the live file is never written.

    python3 scripts/test_live_tracker.py
"""
import hashlib
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import live_tracker as lt  # noqa: E402

SCHEMA = """
CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, mode TEXT NOT NULL,
    version TEXT NOT NULL, config_summary TEXT NOT NULL, taker TEXT, events INTEGER NOT NULL DEFAULT 0,
    dropped INTEGER NOT NULL DEFAULT 0);
CREATE TABLE events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL,
    json TEXT NOT NULL, PRIMARY KEY (session_id, seq));
CREATE TABLE event_blocks (session_id TEXT NOT NULL, first_seq INTEGER NOT NULL, last_seq INTEGER NOT NULL,
    t0 INTEGER NOT NULL, t1 INTEGER NOT NULL, n INTEGER NOT NULL, raw_bytes INTEGER NOT NULL, data BLOB NOT NULL,
    PRIMARY KEY (session_id, first_seq));
CREATE TABLE event_counts (session_id TEXT NOT NULL, kind TEXT NOT NULL, n INTEGER NOT NULL, PRIMARY KEY (session_id, kind));
CREATE TABLE samples (session_id TEXT NOT NULL, ts INTEGER NOT NULL, pair TEXT NOT NULL, side TEXT NOT NULL,
    price_micros INTEGER NOT NULL, source TEXT NOT NULL, size_atoms INTEGER NOT NULL);
CREATE INDEX samples_ts ON samples(session_id, ts);
CREATE TABLE opportunities (session_id TEXT NOT NULL, id INTEGER NOT NULL, status TEXT NOT NULL, skip_reason TEXT,
    gross_edge_ppm INTEGER NOT NULL, net_edge_ppm INTEGER NOT NULL, simulated_net INTEGER, PRIMARY KEY (session_id, id));
CREATE TABLE quotes (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, leg INTEGER NOT NULL, ts INTEGER NOT NULL,
    raw TEXT NOT NULL, PRIMARY KEY (session_id, opportunity_id, leg));
CREATE TABLE attribution (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL, guard TEXT,
    PRIMARY KEY (session_id, opportunity_id));
CREATE TABLE leg_aging (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, leg INTEGER NOT NULL, age_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, opportunity_id, leg));
CREATE TABLE inventory (session_id TEXT NOT NULL, ts INTEGER NOT NULL, sol_lamports INTEGER NOT NULL, usdc_atoms INTEGER,
    sol_usd_micros INTEGER);
CREATE TABLE simulations (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL, ok INTEGER NOT NULL);
CREATE TABLE risk_decisions (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL, approved INTEGER NOT NULL);
CREATE TABLE executions (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL, state TEXT NOT NULL);
CREATE TABLE trades (session_id TEXT NOT NULL, opportunity_id INTEGER NOT NULL, ts INTEGER NOT NULL, net INTEGER NOT NULL);
CREATE TABLE pnl (session_id TEXT NOT NULL, ts INTEGER NOT NULL, cumulative_usd REAL NOT NULL);
CREATE TABLE errors (session_id TEXT NOT NULL, ts INTEGER NOT NULL, service TEXT NOT NULL, message TEXT NOT NULL);
CREATE TABLE latency (session_id TEXT NOT NULL, ts INTEGER NOT NULL, metric TEXT NOT NULL, value REAL NOT NULL);
CREATE TABLE system_metrics (session_id TEXT NOT NULL, ts INTEGER NOT NULL, service TEXT NOT NULL, state TEXT NOT NULL);
"""


def digest(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


class Tracker(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.live_path = os.path.join(self.dir.name, "live.sqlite")
        self.eng = sqlite3.connect(self.live_path)  # the engine's own connection
        self.eng.executescript(SCHEMA)
        self.eng.execute("INSERT INTO sessions VALUES ('s1', 1, NULL, 'LIVE', '0.3.0', '', NULL, 0, 0)")
        self.eng.commit()
        ro = lt.open_live(self.live_path)
        self.out = lt.open_out(os.path.join(self.dir.name, "data", "set.sqlite"), ro)
        ro.close()

    def tearDown(self):
        self.out.close()
        self.eng.close()
        self.dir.cleanup()

    def opp(self, i, status, sim=None):
        self.eng.execute("INSERT OR REPLACE INTO opportunities VALUES ('s1', ?, ?, NULL, -300, -800, ?)", (i, status, sim))
        self.eng.commit()

    def count(self, table):
        return self.out.execute(f"SELECT count(*) FROM {table}").fetchone()[0]

    def test_a_replaced_row_replaces_its_copy(self):
        for i in range(1, 4):
            self.opp(i, "quoted")
        lt.mirror(self.live_path, self.out)
        self.assertEqual(self.count("opportunities"), 3)
        self.opp(3, "skipped", -5000)  # the newest row
        self.opp(1, "skipped", -7000)  # an older one
        self.assertEqual(lt.mirror(self.live_path, self.out)["opportunities"], 2)
        rows = dict(self.out.execute("SELECT id, simulated_net FROM opportunities"))
        self.assertEqual(rows, {1: -7000, 2: None, 3: -5000})
        self.assertEqual(lt.mirror(self.live_path, self.out)["opportunities"], 0, "nothing new: nothing copied")

    def test_rows_that_take_the_rowids_of_a_pruned_session_are_not_missed(self):
        # an old session's trades are the newest rows of the table
        for i in range(3):
            self.eng.execute("INSERT INTO trades VALUES ('s1', ?, ?, 100)", (i, i))
        self.eng.execute("UPDATE sessions SET ended_at = 9 WHERE id = 's1'")
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        lt.mirror(self.live_path, self.out)  # nothing new for an ended session: complete
        # retention deletes them; the table is empty, so the next session's first trade is rowid 1 again
        self.eng.execute("DELETE FROM trades WHERE session_id = 's1'")
        self.eng.execute("DELETE FROM sessions WHERE id = 's1'")
        self.eng.execute("INSERT INTO sessions VALUES ('s2', 10, NULL, 'LIVE', '0.3.0', '', NULL, 0, 0)")
        self.eng.execute("INSERT INTO trades VALUES ('s2', 1, 11, 2500)")
        self.eng.commit()
        self.assertEqual(self.eng.execute("SELECT rowid FROM trades").fetchone()[0], 1, "the rowid was taken again")
        self.assertEqual(lt.mirror(self.live_path, self.out)["trades"], 1)
        got = self.out.execute("SELECT session_id, count(*), sum(net) FROM trades GROUP BY 1 ORDER BY 1").fetchall()
        self.assertEqual(got, [("s1", 3, 300), ("s2", 1, 2500)], "the pruned session stays in the dataset; the new trade arrived")
        self.assertEqual(self.count("sessions"), 2)

    def test_appended_rows_are_copied_once(self):
        for i in range(5):
            self.eng.execute("INSERT INTO simulations VALUES ('s1', ?, ?, 1)", (i, i))
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        lt.mirror(self.live_path, self.out)
        self.eng.execute("INSERT INTO simulations VALUES ('s1', 9, 9, 0)")
        self.eng.commit()
        self.assertEqual(lt.mirror(self.live_path, self.out)["simulations"], 1)
        self.assertEqual(self.count("simulations"), 6)

    def test_more_rows_than_one_batch(self):
        lt.BATCH, old = 7, lt.BATCH
        try:
            for i in range(30):
                self.opp(i, "quoted")
                self.eng.execute("INSERT INTO samples VALUES ('s1', ?, 'SOL/USDC', 'mid', 1, 'pool', 0)", (i,))
            self.eng.commit()
            done = lt.mirror(self.live_path, self.out)
            self.assertEqual((done["opportunities"], done["samples"]), (30, 30))
            self.assertEqual((self.count("opportunities"), self.count("samples")), (30, 30))
        finally:
            lt.BATCH = old

    def test_events_survive_being_packed_and_an_emptied_table(self):
        ev = lambda a, b: [("s1", i, i, "metric", "{}") for i in range(a, b)]
        self.eng.executemany("INSERT INTO events VALUES (?,?,?,?,?)", ev(0, 10))
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        self.assertEqual(self.count("events"), 10)
        # the engine packs 0..9 into a block and deletes them: the table is empty, rowids start again
        self.eng.execute("INSERT INTO event_blocks VALUES ('s1', 0, 9, 0, 9, 10, 20, x'00')")
        self.eng.execute("DELETE FROM events")
        self.eng.executemany("INSERT INTO events VALUES (?,?,?,?,?)", ev(10, 14))
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        self.assertEqual(self.count("event_blocks"), 1)
        seqs = [r[0] for r in self.out.execute("SELECT seq FROM events ORDER BY seq")]
        self.assertEqual(seqs, [10, 11, 12, 13], "packed events left the loose ones; the new ones arrived")
        # events packed and deleted between two passes are in the block, not lost
        self.eng.execute("INSERT INTO event_blocks VALUES ('s1', 10, 19, 10, 19, 10, 20, x'00')")
        self.eng.execute("DELETE FROM events")
        self.eng.executemany("INSERT INTO events VALUES (?,?,?,?,?)", ev(20, 22))
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        covered = sum(r[0] for r in self.out.execute("SELECT last_seq - first_seq + 1 FROM event_blocks")) + self.count("events")
        self.assertEqual(covered, 22)

    def test_the_live_file_is_not_written_and_refuses_writes(self):
        self.opp(1, "quoted")
        self.eng.close()  # everything on disk, no log left behind
        before = digest(self.live_path)
        lt.mirror(self.live_path, self.out)
        self.assertEqual(digest(self.live_path), before)
        ro = lt.open_live(self.live_path)
        with self.assertRaises(sqlite3.OperationalError):
            ro.execute("DELETE FROM opportunities")
        ro.close()
        self.eng = sqlite3.connect(self.live_path)

    def test_status_names_the_session_and_ignores_unpriced_routes(self):
        self.opp(1, "skipped", -5000)
        self.eng.execute("INSERT OR REPLACE INTO opportunities VALUES ('s1', 2, 'skipped', 'BUILD_FAILED', 900000, 900000, NULL)")
        self.eng.execute("INSERT INTO simulations VALUES ('s1', 1, 1, 1)")
        self.eng.execute("INSERT INTO inventory VALUES ('s1', 1, 188116340, 0, 121000000)")
        self.eng.commit()
        lt.mirror(self.live_path, self.out)
        line = lt.status(self.out)
        self.assertIn("s1 LIVE running", line)
        self.assertIn("opps 2 (best gross -3.00bp, net -8.00bp)", line)
        self.assertIn("sims 1 (1 pass)", line)
        self.assertIn("executions 0", line)
        self.assertIn("wallet 0.188116 SOL + 0.00 USDC", line)


if __name__ == "__main__":
    unittest.main()
