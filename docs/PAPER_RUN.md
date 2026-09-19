# PAPER soak results

All numbers come from real mainnet data: Jupiter `/swap/v2/build` quotes (Free
plan, 1 rps), transactions assembled locally and simulated with mainnet
`simulateTransaction`, Jito tip-floor data. Nothing was signed or sent.
Raw artefacts: `docs/runs/` (console log, text + JSON report, survivorship analysis).

Common setup: 1 SOL per cycle; round-trip SOL→USDC→SOL (weight 1), cross-DEX over
Raydium CLMM / Whirlpool / Meteora DLMM / HumidiFi (12 ordered pairs, weight 3),
triangular SOL→USDC→JUP→SOL (weight 1); RTSE slippage; tip policy = landed-tip p50;
guards 10,000 lamports · 5 bp · $0.01; simulation taker = public funded shadow
account (`F7p3…gmNe`, simulation-only).

## Soak 1 — `20260918-034520-a157` (64 min)

| metric | value |
|---|---|
| scanned opportunities (every evaluation) | **1,546** |
| priced (full route quoted) | 1,485 (61 build failures: 47 Jupiter upstream 503s, 13 timeouts, 1 403) |
| gross-positive | 112 (7.2%) — **110 of them involve a prop AMM** (HumidiFi/TesseraV/Quantum/Scorch/…) |
| net-positive after all costs | 1 (0.1%) |
| **executable (simulation + risk passed)** | **0** |
| gross expected PnL, all priced | −0.6225 SOL (sum over evaluations, not a trading result) |
| net expected PnL, all priced | −1.6429 SOL |
| gross / net expected PnL, executable | 0 / 0 |
| median gross edge | −3.62 bp (p90 −0.22 bp, max +11.50 bp) |
| median net edge | −8.96 bp |
| median opportunity lifetime | not measurable at this resolution: gross-positive episodes last 0 µs–13.6 s (median lower/upper bound) while the same route key is only re-sampled every **49.6 s** |
| simulations | 1,463 (every assembled cycle, profitable or not) |
| simulated failure rate (raw) | 34.9% — **inflated by our own bug**, see below |
| simulated failure rate excluding our bug and per-tx artefacts | 221 / 1,173 = **18.8%** |
| median CU consumed | 178,252 (limit set to × 1.2) |
| expected bundle landing cost | median **8,221 lamports** (base 5,000 + priority ~1.5k + tip ~3k) |
| Jito landed-tip floor at end | p50 4,066 · p75 22,284 · p95 100,000 lamports |
| model vs simulation | median sim − model = 0; identical in 46%, simulation worse in 370 / 883 (quotes decay in 1–2 s), better in 106 |
| Jupiter `/build` latency | median ~729 ms per leg from this host; 1.5 s median per full cycle |
| 429s / dropped storage events / DB write errors | 2 / 0 / 0 |

### Strategy by strategy

| strategy | scanned | gross+ | net+ | exec | median gross | median net | best net | sims ok |
|---|---|---|---|---|---|---|---|---|
| cross-dex | 928 | 15 | 0 | 0 | −3.74 bp | −8.73 bp | −0.63 bp | 715 / 877 |
| round-trip | 310 | 93 | 1 | 0 | −0.31 bp | −5.51 bp | +4.09 bp | 168 / 294 |
| triangular | 308 | 4 | 0 | 0 | −9.10 bp | −22.74 bp | −3.19 bp | 69 / 292 |

Cross-DEX by ordered pair (median gross bp, best gross bp, n≈77 each): best pairs
`Whirlpool → Raydium CLMM` (−5.99 / +4.26), `HumidiFi → Raydium CLMM` (−2.54 / +3.94);
every pair's **median** is negative before costs.

### Where the gross-positive quotes went (survivorship)

112 gross-positive → 110 simulated → **60 passed simulation** → **2 still net-positive**
after costs and simulation, and neither passed the guards:

- `#187` round-trip TesseraV → AlphaQ+Scorch: gross +11.5 bp, net +4.10 bp (409,691 lamports),
  simulation identical to the model — rejected by the 5 bp guard.
- `#426` cross-dex Whirlpool → Raydium CLMM: gross +4.27 bp, model net −62,490 lamports,
  simulated net +41,360 — rejected (the smaller of model/simulated net is used).

The other gross-positive quotes failed simulation (slippage exceeded, under-delivering
intermediate legs) or turned negative once simulated — typical of prop-AMM quotes that
move within the ~1–2 s between quote and simulation.

### Simulation failures by cause

| n | cause |
|---|---|
| 249 | **our bug**: a cached "wSOL account exists" let the assembler drop its `CreateIdempotent`; the shadow taker (and Jupiter's own unwrap) closes that account, so `SyncNative` hit a closed account (`IncorrectProgramId`). Fixed: wSOL creates are never dropped, ATA cache has a 60 s TTL, error now classified `account_not_found`. |
| 109 | `SlippageToleranceExceeded` — quote moved before simulation |
| 102 | Jupiter 6024 at route start in single-tx triangular cycles — leg 3's input is fixed to leg 2's *quoted* output, so any under-delivery of the illiquid USDC→JUP leg reverts the whole transaction (intended atomic behaviour; it means most triangular cycles would not land) |
| 41 | per-tx simulation artefact — a bundle's later tx needs the earlier tx's output (now classified `depends_on_prior_tx`) |
| 10 | other program errors / 1 RPC timeout |

Also found during this soak and fixed before soak 2: the slot WebSocket could not
connect through the local HTTP proxy (slots came from the RPC poller fallback; the
System page showed it as down) — WebSocket now tunnels through `HTTPS_PROXY`.

### Reading

At these sizes and this sampling rate there is **no executable edge**: the median
cycle loses ~3.6 bp before costs and ~9 bp after; the few positive quotes are almost
all prop-AMM quotes that either fail or decay in simulation. The cost model is
deliberately conservative (25% of the final leg's slippage tolerance ≈ 3.7 bp and a
1 bp + 5,000 lamport safety buffer dominate the non-gross costs; fees + tip are
~0.08 bp). Relaxing it would not change the sign of the median. One hour at 1 rps is
also far too coarse to measure opportunity lifetimes.

## Soak 2 — `20260918-045128-2dfd` (fixed binary; 22 min, ~16 min usable)

Fixes in this binary: wSOL creates never dropped + 60 s ATA-cache TTL, dependent
bundle-tx failures classified separately, WebSocket through `HTTPS_PROXY`, Jupiter
upstream-503s classified as transient.

The host lost connectivity at ~05:07 (Jupiter *and* RPC stalled; slot froze), and
the process ended with the agent session at ~05:13 without closing the session
row (`ended_at` open). Everything up to that point was flushed. During the outage
the pipeline kept running without crashing, recorded 241 `BUILD_FAILED`
evaluations (166 transport errors, 74 timeouts) and the health model showed
Jupiter `down` / `degraded` — the intended behaviour.

| metric | value |
|---|---|
| priced cycles (usable window) | 322 |
| gross-positive / net-positive / executable | 21 (6.5%, **all 21 via prop AMMs**) / 1 / **0** |
| median gross / net edge | −3.64 bp / −8.83 bp (max gross +4.90 bp) |
| simulations / failure rate | 317 / 23.0% (single-tx exact 21.3%, per-tx bundle 42.3%) |
| failure causes | 32 slippage exceeded · 30 program errors (29 = Jupiter 6024 intermediate-leg shortfall) · 10 `depends_on_prior_tx` · 1 RPC timeout · **0 stale-wSOL** (bug fixed) |
| gross-positive → passed sim → net-positive after costs+sim | 21 → 13 → **0** |
| model vs simulation | median 0; identical 44%, worse 95 / 229, better 34 |
| median CU / landing cost / tip | 195,048 / 7,994 lamports / 2,529 lamports |

Same conclusion as soak 1, with our own defect removed.

## Soak 3 — `20260918-052001-34c5` (fixed binary; 60 awake minutes, 3 h 25 m wall clock)

Closed cleanly. The host slept repeatedly (status-line gaps of 10–40 min; the
monotonic `--duration` clock does not advance during sleep) and had one ~9-minute
network outage (05:29–05:38: continuous Jupiter timeouts, a Jito transport error).
Every evaluation is timestamped, nothing is interpolated across gaps; the 367
`BUILD_FAILED` rows are those outages / wake-ups.

| metric | value |
|---|---|
| scanned / priced | 982 / 615 |
| gross-positive / net-positive / **executable** | 45 (7.3% of priced, **all via prop AMMs**) / 0 / **0** |
| median gross / net edge | −3.38 bp / −8.77 bp (p90 gross −0.16 bp, max +4.86 bp) |
| simulations / failure rate | 553 / 22.6% (single-tx exact 22.5%, per-tx bundle 24.0%) |
| failure causes | 58 slippage exceeded · 41 Jupiter 6024 intermediate-leg shortfall · 11 `depends_on_prior_tx` · 13 RPC errors (outage) · 3 other · **0 stale-wSOL** |
| gross-positive → passed sim → net-positive after costs+sim | 45 → 29 → **0** |
| model vs simulation | median 0; identical 41%, worse 183 / 390, better 49 |
| median CU / landing cost / tip | 194,678 / 8,646 lamports / 3,029 lamports |
| Jito landed-tip floor at end | p25 1,000 · p50 1,411 · p75 11,994 · p95 23,092 lamports |
| 429s / dropped storage events / DB write errors | 4 / 0 / 0 |

| strategy | scanned | priced | gross+ | net+ | exec | median gross | median net | best net | sims ok |
|---|---|---|---|---|---|---|---|---|---|
| cross-dex | 589 | 376 | 5 | 0 | 0 | −3.58 bp | −8.51 bp | −0.53 bp | 333 / 347 |
| round-trip | 197 | 121 | 39 | 0 | 0 | −0.19 bp | −5.46 bp | −0.07 bp | 77 / 116 |
| triangular | 196 | 118 | 1 | 0 | 0 | −5.30 bp | −19.02 bp | −13.22 bp | 18 / 90 |

Also found from this run's replay and fixed: cycles whose second leg failed to
build were priced with a USDC amount as "final output" (−89% in the TUI table;
reports were unaffected because they exclude unpriced rows). Incomplete routes
now carry no edge and render as `—`.

## Combined (three sessions, ~2 h 20 m of awake scanning)

| | soak 1 | soak 2 | soak 3 | total |
|---|---|---|---|---|
| scanned | 1,546 | 563 | 982 | **3,091** |
| priced | 1,485 | 322 | 615 | **2,422** |
| gross-positive | 112 | 21 | 45 | 178 (7.3% of priced; ≥ 98% via prop AMMs) |
| net-positive after costs | 1 | 1 | 0 | 2 |
| **executable (sim + risk)** | 0 | 0 | 0 | **0** |
| simulations | 1,463 | 317 | 553 | 2,333 |
| median gross edge | −3.62 bp | −3.64 bp | −3.38 bp | ≈ −3.5 bp |
| median net edge | −8.96 bp | −8.83 bp | −8.77 bp | ≈ −8.9 bp |

**Conclusion.** With a Free-plan Jupiter key (1 rps), public RPC, 1 SOL cycles
and these three strategies, the system found **no executable arbitrage** in
3,091 real evaluations. The 7% of quotes that look profitable before costs are
almost exclusively prop-AMM quotes that fail or decay in simulation within
1–2 s. This is a statement about this configuration and sampling rate, not about
Solana arbitrage in general: a competitive setup needs sub-second quotes (paid
plan, co-located RPC, ideally local pool-state pricing), which this Phase 1 does
not have. Do not enable LIVE on this evidence — see `LIVE_CHECKLIST.md`.
