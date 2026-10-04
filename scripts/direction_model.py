#!/usr/bin/env python3
"""Train a model of SOL's next move and judge it the way the lab judges a rule.

    python3 scripts/direction_model.py fetch  [--db data/model-candles.sqlite]
    python3 scripts/direction_model.py train  [--db ...] [--out data/direction-model.json]

`fetch` downloads 15-minute candles of SOL-USDT and BTC-USDT from OKX's public
history, from the listing on, into a SQLite file (it goes on where it stopped).
`train` builds features that use the past only, fits a logistic regression
and boosted trees for three horizons on rolling windows, and reports every
configuration: nothing is chosen on the months it is judged on.

numpy, scipy and pandas only. Nothing is sent anywhere but OKX's candle endpoint.
"""
import argparse
import calendar
import json
import sqlite3
import sys
import time
import urllib.request

BAR_MS = 900_000
INSTS = ("SOL-USDT", "BTC-USDT")


def open_db(path):
    db = sqlite3.connect(path, timeout=60)
    db.execute("PRAGMA journal_mode = WAL")      # two fetches may write side by side
    db.execute("""CREATE TABLE IF NOT EXISTS candles (
        inst TEXT NOT NULL, ts INTEGER NOT NULL, open REAL, high REAL, low REAL, close REAL, vol REAL,
        PRIMARY KEY (inst, ts))""")
    return db


def fetch(db, inst, since_ms):
    """Completed candles of `inst` back to `since_ms`, newest first; resumes below what is stored."""
    url = "https://www.okx.com/api/v5/market/history-candles?instId=%s&bar=15m&limit=100&after=%d"
    have = db.execute("SELECT min(ts), max(ts), count(*) FROM candles WHERE inst = ?", (inst,)).fetchone()
    spans = [(int(time.time() * 1000), (have[1] or since_ms))]          # the newest part
    if have[0] and have[0] > since_ms + BAR_MS:
        spans.append((have[0], since_ms))                               # and what is missing at the old end
    fails = 0
    for after, stop in spans:
        while after > stop:
            req = urllib.request.Request(url % (inst, after), headers={"User-Agent": "mobius-research"})
            try:
                with urllib.request.urlopen(req, timeout=15) as r:
                    d = json.load(r)
                if d.get("code") != "0":
                    raise RuntimeError(d.get("msg"))
                rows = d["data"]
            except Exception as e:
                fails += 1
                if fails > 60:
                    sys.exit(f"{inst}: giving up: {e}")
                time.sleep(3)
                continue
            if not rows:
                break
            with db:
                db.executemany("INSERT OR REPLACE INTO candles VALUES (?,?,?,?,?,?,?)",
                               [(inst, int(c[0]), float(c[1]), float(c[2]), float(c[3]), float(c[4]), float(c[5]))
                                for c in rows if c[8] == "1"])
            after = int(rows[-1][0])
            time.sleep(0.2)
    n, lo, hi = db.execute("SELECT count(*), min(ts), max(ts) FROM candles WHERE inst = ?", (inst,)).fetchone()
    gaps = db.execute("""SELECT count(*) FROM (SELECT ts - lag(ts) OVER (ORDER BY ts) d FROM candles WHERE inst = ?)
                         WHERE d IS NOT NULL AND d != ?""", (inst, BAR_MS)).fetchone()[0]
    day = lambda ms: time.strftime("%Y-%m-%d", time.gmtime(ms / 1000))
    print(f"{inst}: {n} candles, {day(lo)} to {day(hi)}, {gaps} gap(s), {fails} retried request(s)", flush=True)



# ───────────────────────────── features ─────────────────────────────

LOOKBACKS = (1, 4, 16, 96, 288, 672)
FEATURES = ([f"sol_ret_{k}" for k in LOOKBACKS] + ["z_96", "z_288", "z_672", "range_96", "range_672", "vol_ratio",
            "volume_z", "volume_burst", "bar_body", "bar_upper_wick", "btc_ret_1", "btc_ret_4", "btc_ret_96",
            "rel_4", "rel_96", "hour_sin", "hour_cos", "dow_sin", "dow_cos", "down_streak"])


def load(db):
    """SOL's candles with BTC's close beside them, on an unbroken 15-minute
    grid: a bar the exchange does not have (it was down) repeats the last
    close with no volume, so that a window of 672 bars is always seven days."""
    import numpy as np
    import pandas as pd
    q = "SELECT ts, open, high, low, close, vol FROM candles WHERE inst = ? ORDER BY ts"
    sol = pd.read_sql_query(q, db, params=("SOL-USDT",)).set_index("ts")
    btc = pd.read_sql_query(q, db, params=("BTC-USDT",)).set_index("ts")
    first, last = max(sol.index[0], btc.index[0]), min(sol.index[-1], btc.index[-1])
    grid = np.arange(first, last + BAR_MS, BAR_MS)
    df = sol.reindex(grid)
    df["btc"] = btc["close"].reindex(grid).ffill()
    missing = int(df["close"].isna().sum())
    df["close"] = df["close"].ffill()
    for k in ("open", "high", "low"):
        df[k] = df[k].fillna(df["close"])
    df["vol"] = df["vol"].fillna(0.0)
    df.attrs["filled"] = missing
    return df


def features(df):
    """One row per bar, from that bar and the ones before it only."""
    import numpy as np
    import pandas as pd
    c, o, h, l, v, b = (df[k] for k in ("close", "open", "high", "low", "vol", "btc"))
    r = np.log(c).diff()
    rb = np.log(b).diff()
    sd = r.rolling(672).std()
    sdb = rb.rolling(672).std()
    f = pd.DataFrame(index=df.index)
    for k in LOOKBACKS:
        f[f"sol_ret_{k}"] = np.log(c / c.shift(k)) / (sd * np.sqrt(k))
    for k in (96, 288, 672):
        f[f"z_{k}"] = (c - c.rolling(k).mean()) / c.rolling(k).std(ddof=0)
    for k in (96, 672):
        lo, hi = l.rolling(k).min(), h.rolling(k).max()
        f[f"range_{k}"] = (c - lo) / (hi - lo)
    f["vol_ratio"] = r.rolling(16).std() / sd
    f["volume_z"] = np.log(v.rolling(4).mean() / v.rolling(672).mean())
    f["volume_burst"] = np.log((v + 1e-9) / v.rolling(96).mean())
    f["bar_body"] = (c - o) / (h - l + 1e-12)
    f["bar_upper_wick"] = (h - np.maximum(c, o)) / (h - l + 1e-12)
    for k in (1, 4, 96):
        f[f"btc_ret_{k}"] = np.log(b / b.shift(k)) / (sdb * np.sqrt(k))
    for k in (4, 96):
        f[f"rel_{k}"] = f[f"sol_ret_{k}"] - f[f"btc_ret_{k}"]
    hour = (df.index.values // 3_600_000 % 24) + (df.index.values // BAR_MS % 4) / 4
    dow = (df.index.values // 86_400_000 + 4) % 7 + hour / 24            # 1970-01-01 was a Thursday
    f["hour_sin"], f["hour_cos"] = np.sin(2 * np.pi * hour / 24), np.cos(2 * np.pi * hour / 24)
    f["dow_sin"], f["dow_cos"] = np.sin(2 * np.pi * dow / 7), np.cos(2 * np.pi * dow / 7)
    down = (r < 0).astype(int)
    f["down_streak"] = down.groupby((down == 0).cumsum()).cumsum().clip(upper=8) / 8
    return f[FEATURES].replace([np.inf, -np.inf], np.nan)


# ───────────────────────────── models ─────────────────────────────

class Logit:
    """Logistic regression on standardised features, a little ridge."""
    name = "logistic regression"

    def fit(self, X, y):
        import numpy as np
        from scipy.optimize import minimize
        self.mu, self.sd = X.mean(0), X.std(0) + 1e-9
        Z = np.c_[np.ones(len(X)), np.clip((X - self.mu) / self.sd, -5, 5)]
        def loss(w):
            m = Z @ w
            return np.mean(np.logaddexp(0, m) - y * m) + 1e-3 * w[1:] @ w[1:], Z.T @ (1 / (1 + np.exp(-m)) - y) / len(y) + 2e-3 * np.r_[0, w[1:]]
        self.w = minimize(loss, np.zeros(Z.shape[1]), jac=True, method="L-BFGS-B").x
        return self

    def predict(self, X):
        import numpy as np
        Z = np.c_[np.ones(len(X)), np.clip((X - self.mu) / self.sd, -5, 5)]
        return 1 / (1 + np.exp(-(Z @ self.w)))

    def dump(self):
        return {"kind": "logit", "mean": self.mu.tolist(), "sd": self.sd.tolist(), "weights": self.w.tolist()}


class Trees:
    """Gradient-boosted trees for the logistic loss on binned features: small
    trees, big leaves, half the rows a tree. Slow learners for a noisy target."""
    name = "boosted trees"

    def __init__(self, rounds=150, depth=3, rate=0.05, bins=32, min_leaf=2000, l2=10.0, seed=7):
        self.rounds, self.depth, self.rate, self.bins, self.min_leaf, self.l2, self.seed = rounds, depth, rate, bins, min_leaf, l2, seed

    def _bin(self, X):
        import numpy as np
        return np.stack([np.searchsorted(e, X[:, j]) for j, e in enumerate(self.edges)], 1).astype(np.int32)

    def fit(self, X, y):
        import numpy as np
        rng = np.random.default_rng(self.seed)
        q = np.linspace(0, 1, self.bins + 1)[1:-1]
        self.edges = [np.unique(np.quantile(X[:, j], q)) for j in range(X.shape[1])]
        B = self._bin(X)
        n, d, nb = len(y), X.shape[1], self.bins
        self.base = float(np.log(y.mean() / (1 - y.mean())))
        F = np.full(n, self.base)
        self.trees = []
        for _ in range(self.rounds):
            p = 1 / (1 + np.exp(-F))
            g, hs = p - y, p * (1 - p)
            rows = np.flatnonzero(rng.random(n) < 0.5)
            node = np.zeros(len(rows), dtype=np.int64)          # position in the level, among the rows of this tree
            tree = []                                           # per level: (feature, bin) for each node, or None
            for level in range(self.depth):
                k = 1 << level
                splits = [None] * k
                best = np.zeros(k)
                G = np.bincount(node, g[rows], k)
                H = np.bincount(node, hs[rows], k)
                for j in range(d):
                    idx = node * nb + B[rows, j]
                    gj = np.bincount(idx, g[rows], k * nb).reshape(k, nb).cumsum(1)
                    hj = np.bincount(idx, hs[rows], k * nb).reshape(k, nb).cumsum(1)
                    cj = np.bincount(idx, minlength=k * nb).reshape(k, nb).cumsum(1)
                    gain = gj ** 2 / (hj + self.l2) + (G[:, None] - gj) ** 2 / (H[:, None] - hj + self.l2) - (G ** 2 / (H + self.l2))[:, None]
                    ok = (cj >= self.min_leaf) & (cj[:, -1:] - cj >= self.min_leaf)
                    gain = np.where(ok, gain, 0.0)
                    b = gain.argmax(1)
                    for m in range(k):
                        if gain[m, b[m]] > best[m] + 1e-12:
                            best[m], splits[m] = gain[m, b[m]], (j, int(b[m]))
                tree.append(splits)
                right = np.zeros(len(rows), dtype=bool)
                for m, sp in enumerate(splits):
                    if sp is not None:
                        sel = node == m
                        right[sel] = B[rows[sel], sp[0]] > sp[1]
                node = node * 2 + right
            leaves = 1 << self.depth
            value = -np.bincount(node, g[rows], leaves) / (np.bincount(node, hs[rows], leaves) + self.l2) * self.rate
            self.trees.append((tree, value))
            F += value[self._leaf(tree, B)]
        return self

    def _leaf(self, tree, B):
        import numpy as np
        node = np.zeros(len(B), dtype=np.int64)
        for splits in tree:
            right = np.zeros(len(B), dtype=bool)
            for m, sp in enumerate(splits):
                if sp is not None:
                    sel = node == m
                    right[sel] = B[sel, sp[0]] > sp[1]
            node = node * 2 + right
        return node

    def predict(self, X):
        import numpy as np
        B = self._bin(X)
        F = np.full(len(X), self.base)
        for tree, value in self.trees:
            F += value[self._leaf(tree, B)]
        return 1 / (1 + np.exp(-F))

    def dump(self):
        return {"kind": "trees", "base": self.base, "edges": [e.tolist() for e in self.edges],
                "trees": [{"splits": t, "leaves": v.tolist()} for t, v in self.trees]}


def auc(p, y):
    import numpy as np
    order = np.argsort(p, kind="stable")
    rank = np.empty(len(p)); rank[order] = np.arange(1, len(p) + 1)
    pos = y.sum()
    return float((rank[y == 1].sum() - pos * (pos + 1) / 2) / (pos * (len(y) - pos)))


# ───────────────────────────── the rule a model makes, and its account ─────────────────────────────

ROUTE_BPS, FIXED_LAMPORTS, CAPITAL = 1.15, 6366.0, 23.0        # the lab's defaults


def trade(signal, o, c, ts, horizon):
    """In at the next open after a signal, out at the open `horizon` bars after
    the last one. The lab's costs. Returns the lab's numbers for these bars."""
    import numpy as np
    n = len(c)
    cash, sol, paid, eq, held, trades, hold_until = CAPITAL, 0.0, 0.0, np.empty(n), np.empty(n), [], -1
    cost = lambda usd, px: usd * ROUTE_BPS / 1e4 + FIXED_LAMPORTS / 1e9 * px
    for i in range(n):
        eq[i], held[i] = cash + sol * c[i], sol * c[i]
        if i + 1 >= n:
            break
        if signal[i]:
            hold_until = i + horizon
        px = o[i + 1]
        if sol == 0 and signal[i] and cash > cost(cash, px):
            paid = cash
            sol, cash = (cash - cost(cash, px)) / px, 0.0
        elif sol > 0 and i >= hold_until:
            gross = sol * px
            cash, sol = gross - cost(gross, px), 0.0
            trades.append((cash - paid) / paid)
    end = eq[-1] - (cost(sol * c[-1], c[-1]) if sol > 0 else 0.0)
    exposure = float(np.mean(held / eq))
    same = float(np.prod(1 + exposure * (c[1:] / c[:-1] - 1)) - 1)
    day = ts // 86_400_000
    last = np.r_[day[1:] != day[:-1], True]
    de, dc = eq[last], c[last]
    daily = (de[1:] / de[:-1] - 1) - exposure * (dc[1:] / dc[:-1] - 1)
    rng = np.random.default_rng(11)
    boot = np.sort([daily[rng.integers(0, len(daily), len(daily))].mean() for _ in range(2000)]) if len(daily) >= 20 else None
    peak = np.maximum.accumulate(np.maximum(eq, CAPITAL))
    return {
        "return": end / CAPITAL - 1, "max_fall": float(((eq - peak) / peak).min()), "in_sol": exposure, "same_sol": same,
        "rule_adds": end / CAPITAL - 1 - same,
        "adds_a_day_bp": None if boot is None else [float(daily.mean() * 1e4), float(boot[49] * 1e4), float(boot[1949] * 1e4)],
        "trades": len(trades), "won": float(np.mean(np.array(trades) > 0)) if trades else None,
        "net_a_trade_bp": float(np.mean(trades) * 1e4) if trades else None, "hold": float(c[-1] / c[0] - 1),
    }


# ───────────────────────────── training ─────────────────────────────

HORIZONS = (4, 16, 96)                 # 1 hour, 4 hours, 1 day
TRAIN_BARS = 2 * 365 * 96              # two years
TEST_BARS = 91 * 96                    # about three months
HOLDOUT_BARS = 183 * 96                # the last six months: looked at once, at the end


def self_test():
    """The features use the past only; the trees can learn."""
    import numpy as np
    import pandas as pd
    rng = np.random.default_rng(3)
    n = 3000
    px = 100 * np.exp(np.cumsum(rng.normal(0, 0.003, n)))
    df = pd.DataFrame({"open": px, "high": px * 1.002, "low": px * 0.998, "close": px * (1 + rng.normal(0, 0.001, n)),
                       "vol": rng.uniform(1, 2, n), "btc": 3e4 * np.exp(np.cumsum(rng.normal(0, 0.002, n)))},
                      index=np.arange(n) * BAR_MS)
    a = features(df)
    changed = df.copy()
    changed.iloc[2000:] = changed.iloc[2000:].values * rng.uniform(0.5, 1.5, (n - 2000, 6))
    b = features(changed)
    assert a.iloc[:2000].equals(b.iloc[:2000]), "a feature of a bar depends on bars after it"
    assert not a.iloc[2000:].equals(b.iloc[2000:])
    X = rng.normal(size=(20000, 4))
    y = ((X[:, 0] > 0) ^ (X[:, 1] > 0.5)).astype(float)                     # not linear
    t = Trees(rounds=60, min_leaf=200).fit(X[:15000], y[:15000])
    assert auc(t.predict(X[15000:]), y[15000:]) > 0.95, "the trees did not learn an easy pattern"
    assert auc(Logit().fit(X[:15000], y[:15000]).predict(X[15000:]), y[15000:]) < 0.75, "and it was not a linear one"
    y2 = (X[:, 2] + rng.normal(size=20000) > 0).astype(float)
    assert auc(Logit().fit(X[:15000], y2[:15000]).predict(X[15000:]), y2[15000:]) > 0.7
    # a rule that is always in buys once, at the second bar, and holds: SOL's return from there, less two costs
    c = px[:500]
    r = trade(np.ones(500, bool), c, c, np.arange(500) * BAR_MS, 4)
    held = c[-1] / c[1] - 1
    assert abs(r["in_sol"] - 1) < 0.01 and r["trades"] == 0 and 0 < held - r["return"] < 0.001, (r, held)
    # a rule that is never in keeps its cash
    r = trade(np.zeros(500, bool), c, c, np.arange(500) * BAR_MS, 4)
    assert r["return"] == 0 and r["in_sol"] == 0 and r["rule_adds"] == 0, r
    print("self-test passed: features use the past only; trees and logistic regression learn; the account adds up")


def train(db, out):
    import numpy as np
    self_test()
    df = load(db)
    X = features(df)
    o, c, ts = df["open"].values, df["close"].values, df.index.values
    n = len(df)
    day = lambda i: time.strftime("%Y-%m-%d", time.gmtime(ts[i] / 1000))
    print(f"{n} bars of SOL and BTC together, {day(0)} to {day(n - 1)}; {df.attrs['filled']} of them missing at the exchange "
          f"and filled with the last close; {len(FEATURES)} features")
    ok = ~np.isnan(X.values).any(1)
    first = int(np.argmax(ok))
    hold0 = n - HOLDOUT_BARS
    folds = []
    s = first + TRAIN_BARS
    while s + TEST_BARS <= hold0:
        folds.append((s - TRAIN_BARS, s, s + TEST_BARS))
        s += TEST_BARS
    print(f"rolling: {len(folds)} tests of ~3 months after 2 years of training each, {day(folds[0][1])} to {day(folds[-1][2] - 1)}; "
          f"kept aside: {day(hold0)} to {day(n - 1)}")
    Xv = X.values
    results = []
    for H in HORIZONS:
        fwd = np.full(n, np.nan)
        fwd[: n - 1 - H] = o[1 + H:] / o[1:n - H] - 1                 # next open to the open H bars later
        y = (fwd > 0).astype(float)
        for make in (Logit, Trees):
            t0 = time.time()
            def fit_predict(a, b, e):
                tr = np.arange(a, b - H - 1)                          # the last labels of training end before the test begins
                tr = tr[ok[tr] & ~np.isnan(fwd[tr])]
                m = make().fit(Xv[tr], y[tr])
                threshold = max(0.5, float(np.quantile(m.predict(Xv[tr]), 0.8)))   # the top fifth of what it saw in training
                te = np.arange(b, e)
                p = np.where(ok[te], m.predict(np.nan_to_num(Xv[te])), 0.0)
                return m, threshold, te, p
            ps, sigs, tes, per_fold = [], [], [], []
            for a, b, e in folds:
                m, thr, te, p = fit_predict(a, b, e)
                sig = p >= thr
                r = trade(sig, o[te], c[te], ts[te], H)
                per_fold.append(r["rule_adds"])
                ps.append(p); sigs.append(sig); tes.append(te)
            te = np.concatenate(tes); p = np.concatenate(ps); sig = np.concatenate(sigs)
            lab = ~np.isnan(fwd[te])
            roll = trade(sig, o[te], c[te], ts[te], H)
            res = {
                "model": make.name, "horizon_bars": H, "auc": auc(p[lab], y[te][lab]),
                "long_share": float(sig.mean()),
                "fwd_when_long_bp": float(np.nanmean(fwd[te][sig]) * 1e4) if sig.any() else None,
                "fwd_always_bp": float(np.nanmean(fwd[te]) * 1e4),
                "rolling": roll, "folds_positive": int(sum(x > 0 for x in per_fold)), "folds": len(per_fold),
                "per_fold_rule_adds": [round(x, 4) for x in per_fold],
            }
            results.append(res)
            print(f"  {make.name:<20} {H:>3} bars  AUC {res['auc']:.3f}  in SOL {roll['in_sol']*100:4.0f}%  after a signal {res['fwd_when_long_bp']:+.1f} bp vs {res['fwd_always_bp']:+.1f} always"
                  f"  return {roll['return']*100:+7.1f}%  same SOL {roll['same_sol']*100:+7.1f}%  rule adds {roll['rule_adds']*100:+7.1f}%"
                  f"  a day {roll['adds_a_day_bp'][0]:+.1f} [{roll['adds_a_day_bp'][1]:+.1f}, {roll['adds_a_day_bp'][2]:+.1f}] bp  folds + {res['folds_positive']}/{res['folds']}"
                  f"  trades {roll['trades']}  ({time.time() - t0:.0f} s)", flush=True)
    # the one looked at on the months kept aside: the best of the rolling tests, chosen before looking
    best = max(results, key=lambda r: r["rolling"]["adds_a_day_bp"][0])
    H = best["horizon_bars"]
    make = Logit if best["model"] == Logit.name else Trees
    fwd = np.full(n, np.nan)
    fwd[: n - 1 - H] = o[1 + H:] / o[1:n - H] - 1
    y = (fwd > 0).astype(float)
    tr = np.arange(hold0 - TRAIN_BARS, hold0 - H - 1)
    tr = tr[ok[tr] & ~np.isnan(fwd[tr])]
    m = make().fit(Xv[tr], y[tr])
    thr = max(0.5, float(np.quantile(m.predict(Xv[tr]), 0.8)))
    te = np.arange(hold0, n)
    p = m.predict(np.nan_to_num(Xv[te]))
    held = trade(p >= thr, o[te], c[te], ts[te], H)
    lab = ~np.isnan(fwd[te])
    passed = held["adds_a_day_bp"][1] > 0 and best["folds_positive"] * 3 >= best["folds"] * 2
    print(f"\nchosen by the rolling tests: {best['model']}, {H} bars")
    print(f"on the six months kept aside ({day(hold0)} to {day(n - 1)}; SOL {held['hold']*100:+.1f} %): AUC {auc(p[lab], y[te][lab]):.3f} · "
          f"return {held['return']*100:+.1f} % · max fall {held['max_fall']*100:.1f} % · in SOL {held['in_sol']*100:.0f} % · same SOL {held['same_sol']*100:+.1f} % · "
          f"rule adds {held['rule_adds']*100:+.1f} % · a day {held['adds_a_day_bp'][0]:+.1f} [{held['adds_a_day_bp'][1]:+.1f}, {held['adds_a_day_bp'][2]:+.1f}] bp · "
          f"trades {held['trades']} · net a trade {held['net_a_trade_bp'] if held['net_a_trade_bp'] is None else round(held['net_a_trade_bp'], 1)} bp")
    print("the bar set beforehand (interval above zero on the months kept aside, and two thirds of the rolling tests positive): "
          + ("PASSED" if passed else "NOT PASSED"))
    json.dump({"trained_to": day(hold0), "unseen_from_ms": int(ts[hold0]), "features": FEATURES, "horizon_bars": H, "threshold": thr, "model": m.dump(),
               "passed": bool(passed), "kept_aside": held, "all": results,
               "costs": {"route_bps": ROUTE_BPS, "fixed_fee_lamports": FIXED_LAMPORTS, "capital_usd": CAPITAL}},
              open(out, "w"))
    print(f"model and every result written to {out}")


def fixture(db, out):
    """673 real bars, their features and what two small models say of them:
    what the Rust that runs a model in the lab is checked against."""
    import numpy as np
    df = load(db).iloc[-(673 + 40 * 96):]                    # forty days to fit on, the last 673 bars to check
    X = features(df)
    o = df["open"].values
    n, H = len(df), 4
    fwd = np.full(n, np.nan)
    fwd[: n - 1 - H] = o[1 + H:] / o[1:n - H] - 1
    rows = np.flatnonzero(~np.isnan(X.values).any(1) & ~np.isnan(fwd))
    y = (fwd[rows] > 0).astype(float)
    x_last = X.values[-1]
    assert not np.isnan(x_last).any()
    made = {}
    for name, m in (("logit", Logit()), ("trees", Trees(rounds=25, min_leaf=60))):
        m.fit(X.values[rows], y)
        made[name] = {"trained_to": "2000-01-01", "features": FEATURES, "horizon_bars": H, "threshold": 0.5, "model": m.dump()}
        made[name + "_probability"] = float(m.predict(x_last[None, :])[0])
    last = df.iloc[-673:]
    json.dump({"bars": [[int(t), *map(float, r)] for t, r in zip(last.index, last[["open", "high", "low", "close", "vol", "btc"]].values)],
               "features": x_last.tolist(), **made}, open(out, "w"))
    print(f"fixture of {len(last)} bars ending {time.strftime('%Y-%m-%d %H:%M', time.gmtime(last.index[-1] / 1000))} UTC written to {out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("what", choices=["fetch", "train", "self-test", "fixture"])
    ap.add_argument("--db", default="data/model-candles.sqlite")
    ap.add_argument("--out", default="data/direction-model.json")
    ap.add_argument("--since", default="2021-02-01", help="first day fetched (UTC)")
    ap.add_argument("--inst", help="fetch this instrument only")
    a = ap.parse_args()
    if a.what == "self-test":
        return self_test()
    db = open_db(a.db)
    if a.what == "fetch":
        since = calendar.timegm(time.strptime(a.since, "%Y-%m-%d")) * 1000
        for inst in ([a.inst] if a.inst else INSTS):
            fetch(db, inst, since)
        return
    if a.what == "fixture":
        return fixture(db, a.out)
    train(db, a.out)


if __name__ == "__main__":
    main()
