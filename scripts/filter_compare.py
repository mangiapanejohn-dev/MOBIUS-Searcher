#!/usr/bin/env python3
"""Can a model pick, from what is known when a gap opens, the round trips that pay?

Laya (a text decision model, zero-shot, three phrasings) against a logistic
regression and the rule in use, on the lag round trips `--research` recorded.

    pip install "laya-mlx==0.3.0" scikit-learn      # Apple silicon; downloads about 800 MB of weights on first use
    python3 scripts/filter_compare.py ~/.local/share/mobius/research.sqlite
"""
import json, sqlite3, sys, time, random, statistics as st
import numpy as np

FIXED_BPS = 1.2   # two transactions at 0.1 SOL
db = sqlite3.connect(sys.argv[1]); db.row_factory = sqlite3.Row
rows = db.execute("""
 SELECT e.run_id run, e.kind, e.dex, e.side, e.start_gap_bps gap, e.pool_age_ms age, e.cex_mid mid, e.cex_bid bid, e.cex_ask ask,
        e.cex_src src, e.exec_gap_bps exec_gap, e.exec_gap_touch_bps exec_touch, e.confirm_ms confirm_ms, x.rt_bps rt
 FROM lag_exit x JOIN lag_episodes e ON e.run_id=x.run_id AND e.id=x.episode
 WHERE x.after_s=0 AND x.rt_bps IS NOT NULL AND x.err IS NULL AND e.run_id<>'20260922-101613-da39'
   AND e.pool_age_ms<=30000 AND ABS(x.rt_bps)<=500 ORDER BY e.run_id, e.start_ts""").fetchall()
data = []
for r in rows:
    spread = (r["ask"] - r["bid"]) / r["mid"] * 1e4 if r["bid"] and r["ask"] else None
    data.append(dict(run=r["run"], kind=r["kind"], dex=r["dex"], side=r["side"], gap=abs(r["gap"]), age=r["age"], spread=spread,
                     exec_gap=r["exec_gap"], confirm_ms=r["confirm_ms"], net=r["rt"] - FIXED_BPS, pays=int(r["rt"] - FIXED_BPS > 0)))
y = np.array([d["pays"] for d in data]); net = np.array([d["net"] for d in data])
trig = np.array([d["kind"] == "trigger" for d in data])
print(f"{len(data)} round trips ({trig.sum()} at a gap over the trigger, {(~trig).sum()} at random times); "
      f"{y.mean()*100:.1f} % pay for their two transactions; mean {net.mean():+.2f} bp after them")

def auc(score, label):
    pos, neg = score[label == 1], score[label == 0]
    if len(pos) == 0 or len(neg) == 0: return float("nan")
    return (sum((p > neg).sum() + 0.5 * (p == neg).sum() for p in pos)) / (len(pos) * len(neg))

def auc_ci(score, label, n=2000, seed=7):
    rng = np.random.default_rng(seed); out = []
    for _ in range(n):
        i = rng.integers(0, len(label), len(label)); a = auc(score[i], label[i])
        if a == a: out.append(a)
    return np.percentile(out, [2.5, 97.5])

def describe(name, score, mask):
    s, l, v = score[mask], y[mask], net[mask]
    a = auc(s, l); lo, hi = auc_ci(s, l)
    k = max(1, len(s) // 3); top = np.argsort(-s, kind="stable")[:k]
    print(f"  {name:<44} ranking {a:.3f} (95 % {lo:.2f}–{hi:.2f})   best third by its score: {v[top].mean():+.2f} bp, {l[top].mean()*100:.0f} % pay")

# ── logistic regression, tested on a run it did not see ──
from sklearn.linear_model import LogisticRegression
from sklearn.preprocessing import StandardScaler
from sklearn.pipeline import make_pipeline
def features(d, after_quote):
    f = [d["gap"], np.log1p(d["age"]), d["spread"] if d["spread"] is not None else 0.0,
         d["dex"] == "Whirlpool", d["dex"] == "Raydium CLMM", d["side"] == "buy_on_dex"]
    if after_quote: f += [d["exec_gap"], np.log1p(d["confirm_ms"])]
    return [float(x) for x in f]
runs = sorted({d["run"] for d in data}, key=lambda r: -sum(e["run"] == r for e in data))[:2]   # the two large runs
def cross_run(after_quote):
    X = np.array([features(d, after_quote) for d in data]); out = np.full(len(data), np.nan)
    for test in runs:
        tr = np.array([d["run"] != test for d in data]); te = ~tr
        m = make_pipeline(StandardScaler(), LogisticRegression(C=1.0, max_iter=1000)).fit(X[tr], y[tr])
        out[te] = m.predict_proba(X[te])[:, 1]
    return out
lr_before, lr_after = cross_run(False), cross_run(True)
tested = ~np.isnan(lr_before)

# ── Laya, zero-shot ──
import laya_mlx as laya
agent = laya.load("aac6fef/laya-mlx")
def state(d, after_quote):
    s = {"exchange": "Solana DEX pool " + d["dex"],
         "plan": ("buy" if d["side"] == "buy_on_dex" else "sell") + " 0.1 SOL on this pool and reverse it at once on the best route",
         "pool_price_better_than_exchange_by_bps": round(d["gap"], 2),
         "pool_price_age_ms": int(d["age"]),
         "exchange_bid_ask_spread_bps": None if d["spread"] is None else round(d["spread"], 2),
         "cost_of_the_two_transactions_bps": FIXED_BPS}
    if after_quote:
        s["executable_quote_better_than_exchange_mid_by_bps"] = round(d["exec_gap"], 2)
        s["quote_took_ms"] = int(d["confirm_ms"])
    return s
QUESTIONS = {
    "plain question": {"type": "noul", "instructions": "Will this round trip return more than it costs?"},
    "question with the reasoning spelled out": {"type": "noul", "instructions":
        "A round trip pays when the pool's price is far better than the exchange price, the pool price is fresh, and the gain exceeds the transaction cost. Does this round trip make a profit after costs?"},
    "five-step rating": {"type": "score", "instructions": "How likely is this round trip to make a profit after costs?",
        "criteria": ["very unlikely", "unlikely", "even odds", "likely", "very likely"]},
}
def laya_scores(after_quote):
    out = {k: [] for k in QUESTIONS}; times = []
    for d in data:
        t = time.perf_counter()
        r = agent.predict(state(d, after_quote), {f"q{i}": q for i, q in enumerate(QUESTIONS.values())})
        times.append((time.perf_counter() - t) * 1e3)
        for i, k in enumerate(QUESTIONS):
            a = r["answers"][f"q{i}"]
            out[k].append(a["noul"] if "noul" in a else a["score"])
    return {k: np.array(v, dtype=float) for k, v in out.items()}, times
show = json.dumps(state(data[0], False))
print("what Laya is shown, e.g.:", show)
print("one raw answer:", json.dumps(agent.predict(state(data[0], False), {"q": QUESTIONS["plain question"]})["answers"]["q"])[:300])
rng = np.random.default_rng(1)
for after, title in ((False, "BEFORE asking Jupiter (what is known when the gap opens: the step that would be saved)"),
                     (True, "AFTER the entry quote (the quote itself is known too)")):
    ls, times = laya_scores(after)
    print(f"\n{title}")
    for name, mask in (("all round trips of the two large runs", tested), ("only those at a gap over the trigger", tested & trig)):
        v, l = net[mask], y[mask]
        print(f" {name}: {mask.sum()} · taking every one: {v.mean():+.2f} bp, {l.mean()*100:.0f} % pay")
        describe("the gap alone (the rule in use)", np.array([d["gap"] for d in data]), mask)
        describe("logistic regression (other run's data)", lr_after if after else lr_before, mask)
        for k, s in ls.items():
            describe("Laya, " + k, s, mask)
        describe("random order", rng.random(len(data)), mask)
    print(f" Laya answered three questions in {st.median(times):.0f} ms per round trip (median), spread of its answers to the plain question: "
          f"{np.percentile(ls['plain question'], 5):.3f}–{np.percentile(ls['plain question'], 95):.3f}")
print("\nranking: 0.5 = no better than chance, 1.0 = every paying round trip ranked above every losing one")
