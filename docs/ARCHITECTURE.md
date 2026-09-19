# MØBIUS-Searcher — architecture

## Decision summary

| # | Decision | Why |
|---|---|---|
| 1 | Greenfield Rust 2024 workspace (Tokio + Ratatui), one crate per concern | Repo was empty; Rust fits the hot path and the Solana/Jito/Ratatui ecosystem |
| 2 | Jupiter **Swap API V2 `/build` only**; no v6/`/quote`/`/swap`/`/swap-instructions`, no `public.jupiterapi.com`, no routePlan surgery | Current official API returns raw instructions + ALTs + blockhash — exactly what an assembler needs |
| 3 | Execution plan **SingleTx first**: all legs' instructions composed into one v0 transaction with our own CU limit/price and the **Jito tip inside the same tx**, sent as a 1-tx bundle. **Bundle fallback** (one tx per leg, tip in the last) only when the composed tx exceeds 1,232 bytes or 64 account locks | One tx = atomic even if an uncled block is rebroadcast outside bundle protection; it can be simulated **exactly** (intermediate balances flow inside the tx); Jito's docs recommend the tip in the strategy tx. Multi-tx bundles cannot be simulated exactly on standard RPC (`simulateBundle` is Jito-RPC only) — they are marked `fidelity = per_tx` and the risk engine refuses them for sending unless explicitly allowed |
| 4 | On-chain profit floor: final leg rebuilt with explicit `slippageBps` so `otherAmountThreshold ≥ input + fees + tip + min_profit` | The transaction reverts instead of realizing a loss (mev-bot's best idea); intermediate legs have fixed downstream inputs, so a shortfall there fails the next swap → atomic revert |
| 5 | Simulation-first, fail-closed: any simulation error ⇒ never send | Spec |
| 6 | Integer money everywhere (u64 lamports/atoms, i64 signed PnL, ppm ratios, micro-USD); floats only in chart/display code | Spec; avoids rounding drift |
| 7 | Event-sourced recording: every engine event is persisted (JSON, thinned and compacted into compressed blocks) + normalised tables; **replay re-feeds the same events through the same state hub** | Replay is byte-for-byte the same UI code path; A/B works identically. See [STORAGE.md](STORAGE.md) |
| 8 | Event-driven Jupiter scheduler: a dependency graph from pools to routes decides *what* to quote when the market moves; a sliding-window limiter learned from `x-ratelimit-*` headers decides *when*; exponential backoff with full jitter on 429; no automatic retry of the same request — candidates are re-planned or dropped when stale | Jupiter's budget (free tier ≈ 10 requests / 10 s per org) is the bottleneck; spend it where the market changed. See [LATENCY.md](LATENCY.md) |
| 9 | Modes PAPER (default) / CONFIRM / LIVE; CONFIRM and LIVE require `execution.live_enabled = true` **and** `wallet.keypair_path`; PAPER needs only a public key (or none) | Spec: a private key in the env never enables sending |
| 10 | UI decoupled: engine → bounded channel (try_send, drop+count on full) → state hub → `RwLock<ViewModel>` read by a TUI thread with `catch_unwind`; storage has its own bounded channel and a dedicated OS thread | UI can never stall the scanner; a TUI panic leaves the searcher running headless |
| 11 | Markets are venues: `[venues.<name>] kind = "…"` with endpoints, credential variable *names* and instruments; configuration is layered (built-in → `config/mobius.toml` → your file → flags) | One bot, several markets, no personal data in the repository. Today: Solana executes, OKX provides market data only |

## Crates

```
crates/
  core/        domain model, integer units, cost & profit engine, layered config + venues, Event   (no IO)
  telemetry/   ServiceHealth registry, WindowLimiter (learned from x-ratelimit-*), 429 backoff,
               LatencyBook (benchmark histograms), proxy discovery (env → macOS system proxy)
  market/      Solana JSON-RPC client, chain WebSocket (slots + pool / Pyth accounts), hot path
               (HotTick / OracleUpdate), block-height and network pollers, Jito tip stream
  jupiter/     Swap API V2 client (/build, /program-id-to-label, /price/v3) + adapters → core
  jito/        JitoClient (getTipAccounts, tip_floor, sendBundle, getInflight/BundleStatuses), tip policies
  strategy/    Strategy trait, RoundTrip / CrossDex / Triangular, pricing
  risk/        RiskEngine (limits), KillSwitch
  execution/   router (event-driven scheduling), tx assembly (v0 + ALTs), simulator + failure
               classification, pipeline, executors, wallet, Probe (benchmark reports)
  storage/     SQLite recorder (compressed event log, retention janitor), replay loader, reports
  tui/         state hub (ViewModel), 8 pages, Markets page (OKX market data, candles/line chart,
               DEX quote book), Graph Workspace, charts, inspector, event stream, brand (logo)
apps/
  mobius-searcher/  CLI + first-run setup wizard, --doctor, run | --replay | --report | --prune …
```

Dependency direction: `core ← telemetry ← {market, jupiter, jito} ← execution → {strategy, risk}`;
`storage` depends on `core` and `telemetry`; `tui` on `core` (and `market` for proxy discovery
of its OKX display feed). The UI never sees Solana provider JSON; the OKX data on the Markets
page is display-only and never reaches the engine or the recording.

## Runtime topology

```
                ┌──────────────── market feeds (no Jupiter budget) ────────────────┐
 chain WebSocket ─┤ slotSubscribe → Slot                                              │
                  │ accountSubscribe pools (Whirlpool/CLMM/DLMM) → Sample mid,        │
                  │   PoolMid, PoolSpread · Pyth price accounts → Sample oracle        │
 RPC poll        ─┤ getEpochInfo → BlockHeight · fees + TPS → Network                │
 Jito WebSocket  ─┤ tip stream → TipFloor (REST poll only while the stream is quiet) │
                  └───────────────────────────────────────────────────────────────────┘
 hot path (pool / oracle changes) ──► router: pool → route dependency graph
   (cross-dex routes fire on a change in the gap between their two pools)
   priority: continuations → triggered routes by ln Φ((pred − cost)/σ) − ln(requests)
             → calibration every 90 s · admission control · leg cache (event invalidation)
             · request coalescing · repeat suppression · WindowLimiter
        ──► route builder ──► Jupiter /build ×N legs
                                                        │ adapters → Leg/Route (core)
                                                        ▼
                                             pricing (CostBreakdown, ProfitEval)
                                                        │  EDGE_TOO_SMALL / NO_ROUTE → skip (recorded)
                                                        ▼
                                  min-out protection (rebuild final leg, explicit slippageBps)
                                                        ▼
                               assemble v0 tx(s): CU limit 1.4M sim, CU price, legs, Jito tip
                                                        ▼
                                  simulateTransaction (sigVerify=false, replaceRecentBlockhash)
                                  → units consumed → CU limit = used × 1.2 → recompute fees/tip
                                  → recompute NET PnL (+ taker balance delta when reported)
                                                        ▼
                                       RiskEngine (limits, kill switch, staleness)
                                                        ▼
                         executor: PAPER fill | CONFIRM (TUI y/n) | LIVE (sign → sendBundle → poll)
 every step ──Event──► engine bus (bounded) ──► hub (ViewModel) ──► TUI thread
                                          └──► recorder thread (SQLite, batched, WAL,
                                               compressed blocks) + janitor (retention)
```

`--scheduler round_robin` keeps the previous weighted scanner for A/B comparisons.

All providers share one `Telemetry` registry that reports state, latency, error rate,
request counts, rate-limit/backoff state and last success for the System page.

## Honesty rules baked into the design

- Every evaluated cycle is recorded and shown, including negative ones and the skip reason
  (`EDGE_TOO_SMALL`, `STALE_QUOTE`, `SIM_FAILED`, `SLIPPAGE`, `TIP_TOO_HIGH`, `RISK_LIMIT`, …).
- PAPER PnL is labelled *simulated*; it assumes the bundle lands at the simulated state and
  applies no landing probability. Realized PnL is only on-chain PnL.
- Price graph source is named (Jupiter `/build` executable SOL→USDC price at the configured size,
  or the on-chain pool mid once the chain feed delivers). Pool mids and Pyth prices are labelled
  `mid` / `oracle` and never used as executable prices. Account decoders check the owner program
  and the pool mints / Pyth feed id; a mismatch disables that feed with an error instead of
  showing a wrong price.
  Candles for the recorded graphs are built only from our own samples, only when a bucket has
  enough of them, and carry no volume. The Markets page's candles, 24 h statistics and trades
  come from OKX, are labelled as such, and are display-only.
- LP fees are embedded in Jupiter's `outAmount`; the cost model shows them as "embedded" and
  never subtracts them twice.
- A simulation taker other than the bot wallet (`paper.simulation_taker`) is allowed only in
  PAPER and is shown in the header; ATA/rent effects then reflect that account's state.

## What is deliberately *not* done

- No v1 transactions (Jito support unverified).
- No `simulateBundle` dependency (standard RPC does not serve it).
- No Jupiter `tipAmount` (that tips Jupiter, not Jito).
- No automatic retries of sends; bundle status polling only.
