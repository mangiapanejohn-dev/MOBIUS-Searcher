# Real-time data path: latency and request scheduling

How market changes reach a strategy decision, what was measured, what was
changed, and where the remaining time goes. All numbers come from mainnet runs
of the real binary (PAPER); nothing here is simulated or replayed.

## 1. Critical path

```
chain / market change
  → source publishes        (pool: every slot it is written; Pyth: on deviation/heartbeat)
  → delivered to us         (RPC WebSocket accountNotification, slotSubscribe)
  → decoded (HOT tick)      (feed::AccountFeed, µs)
  → detector                (router: only routes that read the changed input)
  → queue + budget          (Jupiter sliding window, continuations first)
  → Jupiter /swap/v2/build  (network RTT + router compute) × legs (sequential: leg n+1 needs leg n's output)
  → pricing = decision      (Pipeline::finish)
  → protect / assemble / simulate / risk (not part of the decision latency)
```

Three kinds of delay are kept apart (series names in brackets):

| kind | meaning | measured by |
|---|---|---|
| data itself old | the source had not published a newer value | `oracle.publish_age_ms.*` (Pyth publish time → receipt), `feed.interval_us.*` (update cadence) |
| received late | the value existed but reached us later | `pool.slot_lag_slots.*` (chain head − notification slot), `quote.source_lag_blocks` (our block height − Jupiter's blockhash height) |
| queued | we had it but did not act | `router.trigger_to_dispatch_us`, `router.continuation_wait_us`, `jupiter.token_wait_us` |

## 2. What the audit found (before)

* **Round-robin scheduling, blind to the market.** `Scheduler` rotated
  strategies by weight (round-trip 1 · cross-dex 3 · triangular 1); cross-dex
  rotated its 12 ordered DEX pairs. A given route was re-quoted every ~44
  requests, whatever the market did.
* **Fully serial scanner.** One plan at a time; its legs one after another
  (inherent: leg 2's input is leg 1's output), then the ATA check and assembly
  before the next plan could start.
* **Duplicate requests.** "SOL→USDC on DEX A @ 1 SOL" was requested again for
  each of the 3 routes starting on A; nothing was shared.
* **Pool data never reached the strategy.** On-chain pool mids fed only the UI
  and the recorder.
* **Scarce budget on cold data.** The Price API v3 poll (reference price) spent
  Jupiter's budget (~3 %); background RPC polls shared one bucket with
  simulation.
* **Quote age overstated.** `quoted_at` was stamped before the rate-limit wait.
* **Gateway limit is not what the docs say.** Docs: Free = 60 requests per
  60 s sliding window. Every response's `x-ratelimit-current + remaining` = 10,
  and the window estimated from `reset` is ≈ 10 s (`jupiter.window_est_ms`
  p50 9.9 s): the enforced limit is **10 per ~10 s** (keyless: 5 per ~10 s).
  The old token bucket (0.9 rps, burst 2) could never use a burst.

## 3. Architecture after

```
pool / Pyth ticks (HOT, unthrottled, receipt timestamps)
   ├─► probe (scheduler-independent measurement)
   └─► router::Core
         detector   dependency graph pool → routes; cross-dex routes react to
                    the *gap* between their two pools only (a parallel move is
                    no news); round trips over the best route are level-invariant
         priority   continuations  ›  triggered routes by score  ›  calibration
         score      ln Φ((predicted edge − cost) / σ) − ln(requests needed)
                    predicted edge = on-chain gap + learned offset (EWMA of
                    Jupiter gross − gap per route); σ grows with volatility and
                    time for routes nothing on-chain observes
         admission  a route starts only if the window can also pay for its
                    remaining legs and every leg still owed to routes in progress
         budget     sliding window learned from x-ratelimit-* (capacity =
                    current + remaining; gateway report authoritative, waits
                    until its reset); safety slots + a reserve only urgent work
                    may use
         collapsing identical requests share one HTTP call
         cache      legs keep their send/receive time; invalidated when a pool
                    they observe moves ≥ change_bp; a triggered route reuses a
                    quote only if it is newer than the change or its pools did
                    not move at all; a job made only of the quotes a route was
                    last priced from is dropped (no re-pricing of stale data)
   └─► Pipeline::finish (pricing = decision) in its own task
```

Tiers — HOT never waits for WARM/COLD:

| tier | data | path |
|---|---|---|
| HOT | pool state, slots, Jupiter quotes | chain WebSocket → router; Jupiter window limiter; RPC hot bucket (simulation, ATA checks) |
| WARM | priority fees, Jito tips | RPC background bucket; Jito tip stream (REST only while the stream is quiet) |
| COLD | TPS, UI metrics, recording | RPC background bucket; bounded non-blocking event bus |

Oracle delivery is an adapter (`hot::OracleUpdate`): on-chain Pyth accounts
(default, keyless) or Pyth Hermes SSE (`oracle_source = "hermes"`, needs
`PYTH_API_KEY`; Hermes answers 401 without one since 2026-08-26). Consumers do
not know which is configured.

Transport: one keep-alive HTTP/2 connection per API with PING frames (an idle
event-driven scheduler must not pay a new TLS handshake: 0.3–0.7 s measured
through the local proxy). When no `HTTPS_PROXY` is exported (Ghostty, the
Claude app's terminal panel) every client now uses the macOS system proxy;
before, they connected directly and timed out.

## 4. Honesty rules

* Pool mids and oracle prices only decide *what to ask*; every decision is
  priced from Jupiter `/build` quotes, and simulation re-checks on-chain.
* A quote's age starts when it was **sent** (after any rate-limit wait) — the
  earliest moment it can reflect; cached legs keep their original times, so a
  reused leg is reported as old as it is.
* No gateway limit is exceeded on purpose: both schedulers were benchmarked with
  zero 429 as a requirement (see results).

## 5. Method

`mobius-searcher --headless --duration N` prints the latency report and writes
it to `data/bench/<session>.json`. The same probes run in both schedulers
(`--scheduler round_robin | event`):

* **market change → usable quote**: a watched pool's mid moves ≥ 1 bp from its
  reference (an episode starts at the first such move); the episode ends when a
  Jupiter quote that observes that pool and was **sent after** the move arrives.
* **market change → decision**: same, ending at the strategy decision.
* **duplicate request**: the same request key sent again while none of the
  pools it observes moved since.
* **runtime lag**: how late a 50 ms Tokio sleep wakes up (the async runtime's
  scheduling delay; this is a Rust/Tokio program, there is no GC).

Runs alternate round-robin / event so both see the same market.

## 6. Results (A/B, 2026-09-19)

Four 300 s mainnet PAPER runs, alternating round-robin (RR, before) and event
(EV, after) so both saw the same market: 14:47 RR · 14:52 EV · 14:57 RR ·
15:02 EV. Keyless Jupiter (5 requests per ~10 s, learned from the gateway),
`data/bench/bench.toml`, same binary, public RPC `solana-rpc.publicnode.com`.
Reports: `data/bench/20260919-{144717-68ab,145718-91fc,145218-7dd6,150218-d97f}.json`.
Values are RR1 / RR2 → EV1 / EV2; times in ms.

| metric | before (round-robin) | after (event) |
|---|---|---|
| Jupiter requests (300 s) | 135 / 135 | **95 / 95** (−30 %) |
| 429 responses | 0 / 1 | **0 / 0** |
| window used (p50 · p95 of 5) | 5 · 5 (saturated) | 2 · 4 |
| decisions (300 s) | 57 / 58 | 57 / 54 |
| decision latency, pick → decision p50 · p95 · p99 | 4457 · 6766 · 17798 / 4448 · 6750 · 9039 | **617 · 963 · 11299 / 636 · 766 · 11314** |
| quote age at decision p50 · p95 | 2579 · 4863 / 2548 · 4816 | **639 · 965 / 640 · 979** |
| scheduler wait before a request p50 · p95 | token wait 1885 · 2016 / 1913 · 4077 | 0 · 0 (waits happen before a route starts, below) |
| trigger → first request p50 · p95 | — | 6801 · 29354 / 7999 · 54870 |
| **market change → usable quote** p50 · p95 | 4022 · 9465 / 6337 · 13192 | 7038 · 10632 / 5233 · 16756 |
| market change → decision p50 · p95 | 6445 · 14774 / 8459 · 14547 | 7207 · 19120 / 6425 · 16756 |
| same route re-decided, p50 interval | 97.7 s / 29.1 s | 22.7 s / 22.6 s |
| duplicate requests (probe definition) | 11 / 20 | 25 / 28 |
| Jupiter HTTP p50 · p95 | 334 · 555 / 309 · 591 | 301 · 522 / 309 · 437 |
| pool update interval, Whirlpool p50 · p95 | 1.5 s · 7.8 s / 1.4 s · 9.1 s | 1.3 s · 10.9 s / 2.0 s · 10.5 s |
| pool data behind chain head p50 (slots) | 26 / 26 | 26 / 25 |
| Pyth on-chain price age at receipt p50 | 10.8 s | 10.6–11.1 s |
| Tokio runtime lag p50 · p99 | 2.0 · 5.8 / 2.0 · 5.5 | 2.0 · 6.2 / 2.0 · 4.2 |

What this says, without rounding it up:

* **Done better:** a third fewer requests, no 429, and decisions priced from
  quotes 4× younger (p50 2.6 s → 0.64 s) because a route's legs go out back to
  back instead of each waiting for a token. Each route is re-decided more
  evenly (p50 every ~23 s).
* **Not better:** market change → usable quote. With 5 requests per 10 s the
  provider limit binds: a triggered route waits p50 7–8 s (p95 29–55 s) for
  enough budget to finish all its legs. Round-robin spends the whole budget
  all the time and so sometimes quotes a changed pool sooner by accident.
* **"Duplicates" went up:** the probe counts a request as duplicate when the
  pools it observes did not move. Most are legs through HumidiFi (no on-chain
  price to observe) and legs past their 2.5 s TTL; Jupiter routes through many
  pools we do not watch, so re-quoting them is deliberate, not waste.
* **Found by this benchmark and fixed afterwards:** (1) while the gateway
  count bound, the scheduler woke 12–15×/s (`router.continuation_blocked.gateway`
  3680 / 4443): `x-ratelimit-reset` has 1 s resolution, a reset in the past
  meant "retry now". It now counts the freed slot and otherwise waits for the
  report to expire. (2) the pool data itself was ~26 slots late (next section).

### After the two fixes (15:16, 300 s, event, keyless, new defaults)

One run with the default config as someone else would get it (empty user
directory, no Jupiter key, no `HTTPS_PROXY`), RPC now api.mainnet-beta:

| metric | EV before the fixes | after |
|---|---|---|
| pool data behind chain head p50 · max (slots) | 26 · 33 | **1 · 1** |
| Pyth on-chain price age at receipt p50 | 10.6–11.1 s | **2.1 s** |
| wake-ups while gateway-bound (`continuation_blocked.gateway`) | 3680 / 4443 | **0** |
| `router.budget_blocked` (300 s) | 3440 / 3994 | 546 |
| Jupiter requests · 429 · decisions | 95 · 0 · 57 | 101 · 0 · 59 |
| calibration requests (background) | 10 | 14 |
| change (at receipt) → usable quote p50 · p95 | 7.0 s · 10.6 s / 5.2 s · 16.8 s | 7.8 s · 18.3 s |

End to end, on-chain change → usable quote is now about 0.4 s + 7.8 s ≈ 8 s
p50, down from about 10.4 s + 5–7 s ≈ 16 s: the delivery delay is gone. The
part after receipt did not shrink; fresher data produced more market changes
(66 vs 47–58) competing for the same 5 requests per 10 s, which is the
remaining bottleneck (next section). One run at another time of day, so the
market differs.

## 7. Where the time goes

Market change → usable quote, event scheduler, keyless (p50 ≈ 6 s after
receipt, plus the delivery delay before it):

| part | measured | kind |
|---|---|---|
| delivery: pool change → our receipt | publicnode WebSocket ~30 slots ≈ **12 s** behind its own HTTP `getSlot`; api.mainnet-beta 0–2 slots | RPC / WebSocket |
| waiting for Jupiter budget | trigger → first request p50 7–8 s | provider rate limit |
| Jupiter round trips | 0.30 s p50 per leg, legs sequential (2–3) | network + Jupiter compute |
| our software | detect < 0.1 ms, decode < 0.1 ms, parse < 1 ms, runtime lag 2 ms | software |

Delivery was the largest part and cost nothing to fix: the same pools on
`wss://api.mainnet-beta.solana.com` arrived 0–2 slots behind the head and
twice as often (76 vs 39 notifications in the same 40 s). The default RPC is
back to api.mainnet-beta (built-in and `config/mobius.toml`).

What each option would change (estimates from the numbers above, not runs):

| option | expected effect |
|---|---|
| more code changes | software is < 5 ms of the path; what is left is budget allocation (fewer calibration quotes, skip routes whose predicted edge is hopeless): tens of percent of the queue wait at best |
| Jupiter key / higher tier | queue wait scales about inversely with the window: a free key doubles it (5 → 10 per 10 s, measured on the gateway), a paid tier more; this is the main lever once delivery is fixed |
| private RPC | vs api.mainnet-beta: little latency (it is already 0–2 slots), more request headroom and reliability; vs publicnode: ~12 s |
| Yellowstone gRPC | processed-commitment account stream, ~1 slot (0.4 s) earlier than confirmed WebSocket; paid |
| Pyth Hermes stream | reference prices 0.4 s old instead of ~11 s (on-chain push feed updates every ~52 s); needs a key; affects only oracle-driven routes (JUP triangle) and display |
