#!/usr/bin/env python3
"""Arbitrage that was actually done on the watched pools, read back from chain.

Before paying for faster data: how big is the prize, what do the winners pay
away, and who takes it? For the last HOURS hours this lists every transaction
that touched the three SOL/USDC pools (Orca Whirlpool, Raydium CLMM, Meteora
DLMM), reads

  * a random sample of the successful ones that touched two or more of them
    (a cycle between the pools our own math covers), and
  * a random sample of the rest, pool by pool (arbitrage against any venue),

and classifies each from its balance changes. Nothing is signed or sent.

    python3 scripts/arb_replay.py [HOURS] [SAMPLE_PER_POOL] [OUT.json]

A transaction is counted as arbitrage when all of this holds:
  * it succeeded, and at least two program-owned accounts swapped (each gained
    one token and gave another): the pools;
  * no wallet gave up a token and the fee payer put in no SOL beyond the fee
    and tips, so nobody paid for the tokens that came out;
  * what left the pools, valued in SOL/USDC/USDT, is positive, with nothing
    left over in a token that cannot be priced here.
gross = what left the pools. Part of it can go to a venue's fee accounts, so
"reached the signer" counts only the tokens (and unwrapped SOL) that arrived
in the fee payer's own accounts; when the signer's program keeps them in an
account of its own, gross is used instead. costs = the fee plus every lamport
sent to accounts that hold no tokens (tips). kept = reached − costs. A tip
paid in a separate transaction of the same bundle is not seen.

Most transactions that touch these pools move no token at all: a searcher's
program looked, found nothing and returned. They still pay their fee, so the
report also shows what those cost the signers who sent them.

Standard library only. Uses the public RPC unless SOLANA_RPC_URL is set.
"""
import collections, json, os, random, sys, threading, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

RPC = os.environ.get("SOLANA_RPC_URL", "https://api.mainnet-beta.solana.com")
JITO = "https://mainnet.block-engine.jito.wtf/api/v1/getTipAccounts"
POOLS = {
    "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE": "Whirlpool",
    "CYbD9RaToYMtWKA7QZyoLahnHdWq553Vm62Lh6qWtuxq": "Raydium CLMM",
    "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR": "Meteora DLMM",
}
SOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
USDT = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"
DECIMALS = {SOL: 9, USDC: 6, USDT: 6}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
P = 2**255 - 19
D = (-121665 * pow(121666, P - 2, P)) % P


def on_curve(address):
    """A wallet (a point on the ed25519 curve), as opposed to a program-derived account."""
    n = 0
    for c in address:
        n = n * 58 + B58.index(c)
    y = int.from_bytes(n.to_bytes(32, "big"), "little") & ((1 << 255) - 1)
    if y >= P:
        return False
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P:
        x = x * pow(2, (P - 1) // 4, P) % P
    return (x * x - x2) % P == 0


PAUSE = {"until": 0.0}  # shared: a rate limit holds every reader back, not just the one that met it


def rpc(method, params, url=RPC, attempts=12):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for attempt in range(attempts):
        wait = PAUSE["until"] - time.time()
        if wait > 0:
            time.sleep(wait)
        try:
            req = urllib.request.Request(url, body, {"content-type": "application/json"})
            r = json.loads(urllib.request.urlopen(req, timeout=40).read())
            if "error" in r and r["error"].get("code") in (429, -32005):
                raise OSError("rate limited")
            return r
        except Exception:
            PAUSE["until"] = max(PAUSE["until"], time.time() + min(4 + 4 * attempt, 30))
    return None


def signatures(address, since):
    """Everything that touched `address` since unix time `since`, newest first."""
    out, before = [], None
    while True:
        cfg = {"limit": 1000, "commitment": "confirmed"}
        if before:
            cfg["before"] = before
        page = (rpc("getSignaturesForAddress", [address, cfg]) or {}).get("result")
        if page is None:
            raise SystemExit(f"no answer from {RPC} while listing {address}")
        out += page
        if len(page) < 1000 or (page[-1].get("blockTime") or 0) < since:
            break
        before = page[-1]["signature"]
        time.sleep(0.3)
    return [s for s in out if (s.get("blockTime") or 0) >= since]


def classify(tx, sol_usd, tip_accounts):
    meta, msg = tx["meta"], tx["transaction"]["message"]
    loaded = meta.get("loadedAddresses") or {}
    keys = list(msg["accountKeys"]) + loaded.get("writable", []) + loaded.get("readonly", [])
    payer = keys[0]
    delta, token_accounts = collections.defaultdict(int), set()
    for side, sign in (("preTokenBalances", -1), ("postTokenBalances", 1)):
        for b in meta.get(side) or []:
            token_accounts.add(b["accountIndex"])
            delta[(b.get("owner"), b["mint"])] += sign * int(b["uiTokenAmount"]["amount"])
    owners = collections.defaultdict(dict)
    for (owner, mint), v in delta.items():
        if v and owner:
            owners[owner][mint] = v
    swapped = [o for o, m in owners.items() if min(m.values()) < 0 < max(m.values())]
    pools = [o for o in swapped if not on_curve(o)]
    wallet_paid = any(v < 0 for o, m in owners.items() if on_curve(o) for v in m.values())
    lamports = [post - pre for pre, post in zip(meta["preBalances"], meta["postBalances"])]
    tips = sum(v for i, v in enumerate(lamports) if v > 0 and i != 0 and i not in token_accounts)
    jito = sum(v for i, v in enumerate(lamports) if v > 0 and keys[i] in tip_accounts)
    # lamports the fee payer put into something other than the fee and tips
    paid_in = -lamports[0] - meta["fee"] - tips
    left_pools = collections.defaultdict(int)
    for o in pools:
        for mint, v in owners[o].items():
            left_pools[mint] -= v
    gross = sum(v / 10 ** DECIMALS[m] * (sol_usd if m == SOL else 1.0) for m, v in left_pools.items() if m in DECIMALS)
    unpriced = any(v for m, v in left_pools.items() if m not in DECIMALS)
    arb = len(pools) >= 2 and not wallet_paid and paid_in < 1_000_000 and gross > 0 and not unpriced
    value = lambda mint, v: v / 10 ** DECIMALS[mint] * (sol_usd if mint == SOL else 1.0)
    own = sum(value(m, v) for m, v in owners.get(payer, {}).items() if m in DECIMALS) + max(-paid_in, 0) / 1e9 * sol_usd
    return {
        # what arrived in the fee payer's own accounts; None when its program holds the tokens
        "reached_usd": (own if own > 0 else None) if arb else None,
        "payer": payer, "arb": arb, "pools": len(pools), "watched": sorted({POOLS[k] for k in keys if k in POOLS}),
        "moved": bool(owners), "user_swap": wallet_paid or paid_in >= 1_000_000,
        "gross_usd": gross if arb else 0.0, "fee": meta["fee"], "tips": tips, "jito": jito,
    }


def pct(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, int(q * len(values)))] if values else float("nan")


def report(title, rows, sol_usd, hours, scale=1.0):
    """`scale`: how many transactions each sampled one stands for."""
    arbs = [r for r in rows if r["arb"]]
    usd = lambda lamports: lamports / 1e9 * sol_usd
    idle = [r for r in rows if not r["moved"]]
    per_day = 24 / hours * scale
    print(f"\n{title}")
    print(f"  read {len(rows)}: {len(arbs)} arbitrage, {sum(r['user_swap'] for r in rows)} someone's swap, "
          f"{len(idle)} moved no token at all, {sum(r['moved'] and not r['arb'] and not r['user_swap'] for r in rows)} other")
    if idle:
        fees = [usd(r["fee"]) for r in idle]
        senders = collections.Counter(r["payer"] for r in idle)
        top = sum(n for _, n in senders.most_common(3)) / len(idle) * 100
        print(f"  those that moved nothing: fee p50 ${pct(fees, .5):.4f}  p90 ${pct(fees, .9):.4f}  sum ${sum(fees):.2f}  → ${sum(fees) * per_day:,.0f} a day"
              f" · {len(senders)} signers, the three busiest sent {top:.0f} %")
    if not arbs:
        return
    gross = [r["gross_usd"] for r in arbs]
    reached = [r["reached_usd"] if r["reached_usd"] is not None else r["gross_usd"] for r in arbs]
    seen_own = sum(r["reached_usd"] is not None for r in arbs)
    cost = [usd(r["fee"] + r["tips"]) for r in arbs]
    kept = [g - c for g, c in zip(reached, cost)]
    share = [c / g * 100 for g, c in zip(reached, cost) if g > 0]
    print(f"  arbitrage per hour: {len(arbs) * scale / hours:.0f}" + ("  (estimated from the sample)" if scale != 1 else ""))
    print(f"  taken from the pools   p50 ${pct(gross, .5):.4f}  p90 ${pct(gross, .9):.3f}  max ${max(gross):.2f}  sum ${sum(gross):.2f}  → ${sum(gross) * per_day:,.0f} a day at this rate")
    print(f"  reached the signer     p50 ${pct(reached, .5):.4f}  p90 ${pct(reached, .9):.3f}  sum ${sum(reached):.2f}"
          f"  (seen in the signer's own accounts in {seen_own} of {len(arbs)}; gross used for the rest)")
    print(f"  fee and tips           p50 ${pct(cost, .5):.4f}  p90 ${pct(cost, .9):.3f}  sum ${sum(cost):.2f}  ({sum(cost) / sum(reached) * 100:.0f} % of what reached the signers;"
          f" {sum(usd(r['jito']) for r in arbs) / max(sum(cost), 1e-12) * 100:.0f} % of it to Jito tip accounts)")
    print(f"  fee and tips ÷ reached p50 {pct(share, .5):.0f} %  p90 {pct(share, .9):.0f} %   ·   cost more than it brought: {sum(k < 0 for k in kept)} of {len(arbs)}")
    print(f"  kept                   p50 ${pct(kept, .5):.4f}  p90 ${pct(kept, .9):.3f}  sum ${sum(kept):.2f}  → ${sum(kept) * per_day:,.0f} a day at this rate")
    by = collections.defaultdict(lambda: [0, 0.0])
    for r, k in zip(arbs, kept):
        by[r["payer"]][0] += 1
        by[r["payer"]][1] += k
    ranked = sorted(by.values(), key=lambda v: -v[1])
    total = sum(k for k in kept if k > 0) or 1e-12
    tops = ", ".join(f"top {n}: {sum(max(v[1], 0) for v in ranked[:n]) / total * 100:.0f} %" for n in (1, 3, 5))
    print(f"  signers: {len(by)} · share of what was kept: {tops}")
    # the same signers' other transactions here: the ones that found nothing or did not pay
    rest = [r for r in rows if not r["arb"] and not r["user_swap"] and r["payer"] in by]
    spent = sum(usd(r["fee"] + r["tips"]) for r in rest)
    print(f"  the signers with an arbitrage also sent {len(rest)} that brought nothing, costing ${spent:.2f}:"
          f" kept ${sum(kept):.2f} − ${spent:.2f} = ${sum(kept) - spent:.2f}  → ${(sum(kept) - spent) * per_day:,.0f} a day")
    print(f"  pools in one transaction: " + ", ".join(f"{n} pools × {c}" for n, c in sorted(collections.Counter(r['pools'] for r in arbs).items())))


def main():
    hours = float(sys.argv[1]) if len(sys.argv) > 1 else 2.0
    sample = int(sys.argv[2]) if len(sys.argv) > 2 else 800
    out = sys.argv[3] if len(sys.argv) > 3 else None
    since = time.time() - hours * 3600
    tip_accounts = set((rpc("getTipAccounts", [], JITO, attempts=2) or {}).get("result") or [])
    # the price used to value SOL: the Whirlpool's own, now
    import base64
    data = base64.b64decode(rpc("getAccountInfo", [next(iter(POOLS)), {"encoding": "base64"}])["result"]["value"]["data"][0])
    sol_usd = (int.from_bytes(data[65:81], "little") / 2**64) ** 2 * 1e3
    print(f"last {hours:g} h · {RPC.split('?')[0]} · SOL at ${sol_usd:.2f} · {len(tip_accounts)} Jito tip accounts known")

    lists, seen = {}, collections.Counter()
    for address, name in POOLS.items():
        lists[name] = signatures(address, since)
        ok = [s for s in lists[name] if s["err"] is None]
        for s in ok:
            seen[s["signature"]] += 1
        print(f"  {name:<13} {len(lists[name]):>7} transactions touched it: {len(ok)} succeeded, {len(lists[name]) - len(ok)} failed")
    cycles = [s for s, n in seen.items() if n >= 2]
    rng = random.Random(20261003)
    plan = {"between": rng.sample(cycles, min(sample, len(cycles)))}
    for name, entries in lists.items():
        single = [s["signature"] for s in entries if s["err"] is None and seen[s["signature"]] == 1]
        plan[name] = rng.sample(single, min(sample, len(single)))
        plan[name + " n"] = len(single)

    rows, started, lock, state = {}, time.time(), threading.Lock(), {"n": 0, "next": 0.0, "unread": 0}
    todo = [(k, s) for k in ["between", *lists] for s in plan[k]]

    def read(job):
        group, sig = job
        with lock:  # about 1.6 requests a second in total: the public node refuses much more
            wait = state["next"] - time.time()
            state["next"] = max(state["next"], time.time()) + 0.62
        if wait > 0:
            time.sleep(wait)
        r = rpc("getTransaction", [sig, {"encoding": "json", "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}])
        with lock:
            state["n"] += 1
            state["unread"] += r is None
            if r and r.get("result"):
                rows.setdefault(group, []).append({"sig": sig, **classify(r["result"], sol_usd, tip_accounts)})
            if state["n"] % 500 == 0:
                print(f"  read {state['n']} of {len(todo)} transactions ({time.time() - started:.0f} s)", flush=True)

    with ThreadPoolExecutor(2) as pool:
        list(pool.map(read, todo))
    if state["unread"]:
        print(f"  {state['unread']} transactions could not be read (no answer from the node) and are left out")

    got = rows.get("between", [])
    report(f"A. Transactions that touched two or more of the three pools: a random {len(got)} of the {len(cycles)} that succeeded",
           got, sol_usd, hours, scale=len(cycles) / max(len(got), 1))
    for name in lists:
        n, got = plan[name + " n"], rows.get(name, [])
        if got:
            report(f"B. {name}: a random {len(got)} of the {n} successful transactions that touched only this pool",
                   got, sol_usd, hours, scale=n / len(got))
    print("\n  A tip paid in another transaction of the same bundle is not seen, so costs are a lower bound.")
    print("  Sampled figures scale one transaction to many: a few large ones decide the sums.")
    if out:
        json.dump({"hours": hours, "sol_usd": sol_usd, "counts": {k: v for k, v in plan.items() if k.endswith(" n")},
                   "listed": {k: len(v) for k, v in lists.items()}, "rows": rows}, open(out, "w"))


if __name__ == "__main__":
    main()
