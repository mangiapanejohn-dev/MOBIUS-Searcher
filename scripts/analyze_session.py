#!/usr/bin/env python3
"""Extra survivorship analysis of a recorded session (stdlib only).

usage: analyze_session.py [DB] SESSION_ID
"""
import json
import sqlite3
import statistics as st
import sys

PROP_AMMS = {"HumidiFi", "SolFi", "SolFi V2", "TesseraV", "Quantum", "Scorch", "ZeroFi", "AlphaQ", "Obric V2", "GoonFi V2", "BisonFi"}


def med(xs):
    return st.median(xs) if xs else None


def bp(ppm):
    return None if ppm is None else ppm / 100.0


def main():
    args = sys.argv[1:]
    db, sid = (args[0], args[1]) if len(args) == 2 else ("data/mobius.sqlite", args[0])
    c = sqlite3.connect(db)
    rows = c.execute(
        "SELECT id, strategy, label, route, gross_pnl, expected_net, simulated_net, gross_edge_ppm, net_edge_ppm, snapshot "
        "FROM opportunities WHERE session_id=? AND skip_reason IS NOT 'NO_ROUTE'",
        (sid,),
    ).fetchall()
    sims = {r[0]: r for r in c.execute(
        "SELECT opportunity_id, ok, plan, fidelity, failure_class, failure_message, json FROM simulations WHERE session_id=?", (sid,))}

    print(f"session {sid}: {len(rows)} priced cycles, {len(sims)} simulations\n")

    # 1. cross-dex by ordered pair
    pairs = {}
    for r in rows:
        if r[1] == "cross-dex":
            pairs.setdefault(r[2], []).append(r)
    print("CROSS-DEX BY ORDERED PAIR (median gross / median net / best gross, bp; n)")
    for k, v in sorted(pairs.items(), key=lambda kv: -max(x[7] for x in kv[1])):
        print(f"  {k:<30} {bp(med([x[7] for x in v])):+7.2f} {bp(med([x[8] for x in v])):+8.2f} {bp(max(x[7] for x in v)):+8.2f}   n={len(v)}")

    # 2. gross-positive survivorship
    gp = [r for r in rows if r[4] > 0]
    print(f"\nGROSS-POSITIVE QUOTES: {len(gp)} of {len(rows)} ({100*len(gp)/max(1,len(rows)):.1f}%)")
    with_prop = [r for r in gp if any(p in r[3] for p in PROP_AMMS)]
    print(f"  involving a prop AMM (HumidiFi/SolFi/TesseraV/Quantum/Scorch/…): {len(with_prop)}")
    simd = [r for r in gp if r[0] in sims]
    ok = [r for r in simd if sims[r[0]][1] == 1]
    print(f"  simulated: {len(simd)} · simulation passed: {len(ok)}")
    still_pos = [r for r in ok if (r[6] if r[6] is not None else r[5]) > 0]
    print(f"  net-positive after costs AND simulation: {len(still_pos)}")
    for r in sorted(gp, key=lambda r: -r[4])[:10]:
        s = sims.get(r[0])
        sv = "no sim" if s is None else ("sim ok" if s[1] else f"sim FAIL {s[4]}")
        print(f"    #{r[0]:<5} {r[1]:<11} {r[3][:42]:<42} gross {r[4]:>+9} net {r[5]:>+9} simnet {str(r[6]):>9}  {sv}")

    # 3. simulation failures, with our own defects separated
    print("\nSIMULATION FAILURES BY CAUSE")
    causes = {}
    for oid, okf, plan, fid, cls, msg, js in sims.values():
        if okf:
            continue
        logs = " ".join(l for t in json.loads(js).get("txs", []) for l in t.get("logs", []))
        tx_idx = (json.loads(js).get("failure") or {}).get("tx_index", 0)
        if "IncorrectProgramId" in logs or "IncorrectProgramId" in (msg or ""):
            key = "OUR BUG: stale wSOL-ATA cache → SyncNative on closed account (fixed)"
        elif plan == "bundle" and tx_idx and tx_idx > 0:
            key = "per-tx sim artifact: bundle tx needs prior tx output"
        elif "6001" in (msg or "") or cls == "slippage_exceeded":
            key = "SlippageToleranceExceeded (quote moved before sim)"
        elif "6024" in (msg or ""):
            key = "Jupiter 6024 at route start (intermediate leg under-delivered → input short)"
        else:
            key = f"other: {cls}: {(msg or '')[:70]}"
        causes.setdefault(key, 0)
        causes[key] += 1
    for k, n in sorted(causes.items(), key=lambda kv: -kv[1]):
        print(f"  {n:>5}  {k}")

    # 4. model vs simulation
    diffs = [r[6] - r[5] for r in rows if r[6] is not None]
    if diffs:
        exact = sum(1 for d in diffs if d == 0)
        print(f"\nSIM − MODEL NET (exact single-tx sims with balance deltas): n={len(diffs)}  median {med(diffs):+.0f}  "
              f"identical {exact} ({100*exact/len(diffs):.0f}%)  worse {sum(1 for d in diffs if d < 0)}  better {sum(1 for d in diffs if d > 0)}")
        worst = sorted(diffs)[:3]
        print(f"  worst three: {worst}")


if __name__ == "__main__":
    main()
