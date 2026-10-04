#!/usr/bin/env python3
"""Follow a running session without touching it, and keep what it records.

The engine's database prunes old sessions (7 days, 1 GB). This copies every
table of it, as rows appear, into a dataset file that nothing prunes, and adds
the one thing the engine does not record: the exchange's best bid and ask,
once a second. Once a minute it writes where the session stands.

It only reads: the live database is opened read-only, in short queries (WAL:
a reader does not block the writer), and nothing is sent to Solana, Jupiter
or Jito. The exchange is asked one public ticker a second.

    python3 scripts/live_tracker.py                       # follow until stopped (Ctrl-C / SIGTERM)
    python3 scripts/live_tracker.py --once                # one pass, then exit
    python3 scripts/live_tracker.py --no-okx --every 30
"""
import argparse
import json
import os
import queue
import signal
import sqlite3
import sys
import threading
import time
import urllib.request

LIVE = os.path.expanduser("~/.local/share/mobius/mobius.sqlite")
BATCH = 2000

# Copied row by row as they appear. Where the engine replaces a row under its
# key (an opportunity moving on), the replacement gets a new, higher rowid, so
# it is seen again and replaces the copy.
ROWS = ["event_blocks", "samples", "opportunities", "quotes", "attribution", "leg_aging", "inventory", "simulations",
        "risk_decisions", "executions", "trades", "pnl", "errors", "latency", "system_metrics"]
# Small, rewritten in place: copied whole on every pass.
WHOLE = ["sessions", "event_counts"]


def open_live(path):
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=2)
    db.execute("PRAGMA query_only = 1")
    return db


def open_out(path, live):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    out = sqlite3.connect(path, timeout=10)
    out.execute("PRAGMA journal_mode = WAL")
    out.execute("PRAGMA synchronous = NORMAL")
    # the same tables as the engine's, made from its own definitions
    for (sql,) in live.execute("SELECT sql FROM sqlite_master WHERE type IN ('table','index') AND sql IS NOT NULL"):
        if "sqlite_" not in sql.split("(")[0]:
            out.execute(sql.replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1)
                        .replace("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ", 1)
                        .replace("IF NOT EXISTS IF NOT EXISTS", "IF NOT EXISTS"))
    out.execute("CREATE TABLE IF NOT EXISTS mirror_cursor (name TEXT PRIMARY KEY, at INTEGER NOT NULL)")
    out.execute("""CREATE TABLE IF NOT EXISTS okx_bbo (
        ts INTEGER NOT NULL,        -- when the answer arrived here, microseconds
        exchange_ts INTEGER,        -- the exchange's own time of the ticker, microseconds
        inst TEXT NOT NULL, bid REAL NOT NULL, ask REAL NOT NULL, bid_size REAL, ask_size REAL, last REAL)""")
    out.execute("CREATE INDEX IF NOT EXISTS okx_bbo_ts ON okx_bbo(inst, ts)")
    out.commit()
    return out


def cursor(out, name):
    row = out.execute("SELECT at FROM mirror_cursor WHERE name = ?", (name,)).fetchone()
    return row[0] if row else None


def set_cursor(out, name, at):
    out.execute("INSERT OR REPLACE INTO mirror_cursor VALUES (?, ?)", (name, at))


def copy_rows(live, out, table, sid):
    """Rows of one session in `table` past that session's cursor, in batches;
    each batch and its cursor are one transaction of the dataset.

    The cursor is per session because rowids are only safe within one: when
    retention deletes an old session's rows from the top of a table, the next
    session's rows take those rowids again."""
    copied, name = 0, f"{table}:{sid}"
    while True:
        at = cursor(out, name)
        rows = live.execute(f"SELECT rowid, * FROM {table} WHERE rowid > ? AND session_id = ? ORDER BY rowid LIMIT {BATCH}",
                            (at if at is not None else -1, sid)).fetchall()
        if not rows:
            return copied
        marks = ",".join("?" * (len(rows[0]) - 1))
        with out:
            out.executemany(f"INSERT OR REPLACE INTO {table} VALUES ({marks})", [r[1:] for r in rows])
            set_cursor(out, name, rows[-1][0])
        copied += len(rows)
        if len(rows) < BATCH:
            return copied


def copy_events(live, out, sid):
    """The part of a session's event log that is not yet in a block. The
    engine packs events into blocks and deletes them, so this goes by
    sequence number and drops what a copied block now holds."""
    packed = out.execute("SELECT max(last_seq) FROM event_blocks WHERE session_id = ?", (sid,)).fetchone()[0]
    at = max(cursor(out, f"events:{sid}") or -1, packed if packed is not None else -1)
    copied = 0
    while True:
        rows = live.execute("SELECT session_id, seq, ts, kind, json FROM events WHERE session_id = ? AND seq > ? "
                            f"ORDER BY seq LIMIT {BATCH}", (sid, at)).fetchall()
        with out:
            if rows:
                out.executemany("INSERT OR REPLACE INTO events VALUES (?,?,?,?,?)", rows)
                at = rows[-1][1]
                set_cursor(out, f"events:{sid}", at)
            if packed is not None:
                out.execute("DELETE FROM events WHERE session_id = ? AND seq <= ?", (sid, packed))
        copied += len(rows)
        if len(rows) < BATCH:
            return copied


def mirror(live_path, out):
    """One pass. Returns rows copied per table."""
    live = open_live(live_path)
    try:
        done = {}
        for table in WHOLE:
            rows = live.execute(f"SELECT * FROM {table}").fetchall()
            if rows:
                with out:
                    out.executemany(f"INSERT OR REPLACE INTO {table} VALUES ({','.join('?' * len(rows[0]))})", rows)
        for sid, ended in live.execute("SELECT id, ended_at FROM sessions ORDER BY started_at").fetchall():
            # a session that ended and had nothing new on a later pass is complete
            if cursor(out, f"complete:{sid}"):
                continue
            n = 0
            for table in ROWS + ["events"]:
                k = copy_events(live, out, sid) if table == "events" else copy_rows(live, out, table, sid)
                done[table] = done.get(table, 0) + k
                n += k
            if ended is not None and n == 0:
                with out:
                    set_cursor(out, f"complete:{sid}", 1)
        return done
    finally:
        live.close()


def okx_poller(inst, rows, stop):
    """Best bid and ask of `inst`, once a second, into `rows`."""
    # OKX answers 403 to urllib's default User-Agent
    req = urllib.request.Request(f"https://www.okx.com/api/v5/market/ticker?instId={inst}",
                                 headers={"User-Agent": "mobius-live-tracker", "Accept": "application/json"})
    while not stop.is_set():
        started = time.time()
        try:
            with urllib.request.urlopen(req, timeout=4) as r:
                d = json.load(r)["data"][0]
            rows.put((int(time.time() * 1e6), int(d["ts"]) * 1000, inst, float(d["bidPx"]), float(d["askPx"]),
                      float(d["bidSz"]), float(d["askSz"]), float(d["last"])))
            wait = 1.0
        except Exception as e:  # network, proxy, a changed answer: say so and slow down
            rows.put(("error", f"{type(e).__name__}: {e}"[:160]))
            wait = 5.0
        stop.wait(max(0.0, wait - (time.time() - started)))


def status(out):
    """Where the newest session stands, from the dataset (not the live file)."""
    s = out.execute("SELECT id, mode, version, started_at, ended_at FROM sessions ORDER BY started_at DESC LIMIT 1").fetchone()
    if not s:
        return "no session yet"
    sid = s[0]
    one = lambda sql: out.execute(sql, (sid,)).fetchone()
    opps, = one("SELECT count(*) FROM opportunities WHERE session_id = ?")
    # an edge exists only for a route that was quoted to the end
    best_gross, best_net = one("SELECT max(gross_edge_ppm), max(net_edge_ppm) FROM opportunities WHERE session_id = ? "
                               "AND coalesce(skip_reason, '') NOT IN ('NO_ROUTE', 'BUILD_FAILED', 'RATE_LIMITED')")
    sims, passed = one("SELECT count(*), sum(ok) FROM simulations WHERE session_id = ?")
    execs, = one("SELECT count(*) FROM executions WHERE session_id = ?")
    trades, net = one("SELECT count(*), sum(net) FROM trades WHERE session_id = ?")
    errors, = one("SELECT count(*) FROM errors WHERE session_id = ?")
    wallet = one("SELECT sol_lamports, usdc_atoms FROM inventory WHERE session_id = ? ORDER BY ts DESC LIMIT 1")
    bp = lambda ppm: "—" if ppm is None else f"{ppm / 100:+.2f}bp"
    parts = [f"{sid} {s[1]} {'ended' if s[4] else 'running'}",
             f"opps {opps} (best gross {bp(best_gross)}, net {bp(best_net)})",
             f"sims {sims} ({passed or 0} pass)", f"executions {execs}", f"trades {trades}",
             f"net {net or 0:+d} lamports", f"errors {errors}"]
    if wallet:
        parts.append(f"wallet {wallet[0] / 1e9:.6f} SOL + {(wallet[1] or 0) / 1e6:.2f} USDC")
    return " · ".join(parts)


def say(line):
    print(f"{time.strftime('%Y-%m-%d %H:%M:%S')}  {line}", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--db", default=LIVE, help="the engine's database (opened read-only)")
    ap.add_argument("--out", default="data/live-dataset.sqlite", help="the dataset file")
    ap.add_argument("--okx", default="SOL-USDC", help="OKX instrument whose best bid and ask are recorded")
    ap.add_argument("--no-okx", action="store_true")
    ap.add_argument("--every", type=float, default=15, help="seconds between passes over the live database")
    ap.add_argument("--once", action="store_true", help="one pass, then exit")
    a = ap.parse_args()

    live = open_live(a.db)
    out = open_out(a.out, live)
    live.close()
    stop = threading.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: stop.set())
    bbo = queue.Queue()
    if not a.no_okx and not a.once:
        threading.Thread(target=okx_poller, args=(a.okx, bbo, stop), daemon=True).start()

    say(f"following {a.db} (read-only) into {a.out}" + ("" if a.no_okx or a.once else f" · OKX {a.okx} every second"))
    last_status, last_pass, okx_rows, okx_errors, first = 0.0, 0.0, 0, 0, True
    while not stop.is_set():
        now = time.time()
        if now - last_pass >= a.every:
            try:
                done = mirror(a.db, out)
                if first:
                    say("copied " + (", ".join(f"{n} {t}" for t, n in done.items() if n) or "nothing new"))
                first = False
            except sqlite3.OperationalError as e:  # the writer was resetting its log: next pass
                say(f"pass skipped: {e}")
            last_pass = now
        batch = []
        while not bbo.empty():
            row = bbo.get()
            if row[0] == "error":
                okx_errors += 1
                if okx_errors in (1, 10, 100) or okx_errors % 1000 == 0:
                    say(f"OKX: {row[1]} ({okx_errors} so far)")
            else:
                batch.append(row)
        if batch:
            with out:
                out.executemany("INSERT INTO okx_bbo VALUES (?,?,?,?,?,?,?,?)", batch)
            okx_rows += len(batch)
        if a.once:
            break
        if now - last_status >= 60:
            say(status(out) + (f" · OKX rows {okx_rows}" if not a.no_okx else ""))
            last_status = now
        stop.wait(0.5)
    say(status(out))
    say("stopped")
    out.close()


if __name__ == "__main__":
    sys.exit(main())
