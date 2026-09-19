# Phase A — Research: census, official-doc verification, reference projects

Date of verification: 2026-09-18. Anything marked **UNVERIFIED** was not confirmed
from official docs or live calls and must not be relied on for real money.

## 0. Repo census

- Working directory `SOL-USD_BOT/` was **empty** (no git, no code, no config, no tests).
  There was no fork of `fape-labs/arbitrage-bot` to migrate — no migration cost.
- Toolchain on host: Rust 1.96, Go 1.26, SQLite 3 (system), Ghostty + Terminal.app.
- Decision: greenfield **Rust + Tokio + Ratatui** workspace (see ARCHITECTURE.md).

## 1. Official docs verification

### Jupiter (developers.jup.ag — `dev.jup.ag` now 301-redirects there)

Verified from the OpenAPI spec `docs/openapi-spec/swap/v2/swap.yaml` and live calls
(fixtures in `fixtures/`).

| Topic | Verified fact |
|---|---|
| Endpoint | `GET https://api.jup.ag/swap/v2/build` — Metis router only, you sign/send yourself; `/execute` does **not** accept `/build` txs |
| Auth | `x-api-key` header. Keyless works (lower limit). Invalid key → 401 |
| Required params | `inputMint`, `outputMint`, `amount`, `taker` |
| `slippageBps` | 0–10000 or literal `rtse` (opt-in on `/build`; default fixed 50). RTSE result lands in `otherAmountThreshold` and the numeric `slippageBps` in the response |
| `mode=fast` | BETA. Bellman-Ford, **no route splitting**, priority-fee estimate from global fees (not route hot accounts), default CU price percentile → 90th. Docs: "negligible" price difference for majors; default mode better for large/illiquid swaps. No allow-list |
| `dexes` / `excludeDexes` | comma-separated, **case-sensitive** labels; mutually exclusive (400). Unknown label → 400 `No routes found` (indistinguishable from no liquidity) |
| Labels | `GET /swap/v2/program-id-to-label` (live: 107 labels; `fixtures/jupiter_program_id_to_label.json`) |
| `maxAccounts` | 1–64, default 64; < ~50 may degrade/no-route |
| `blockhashSlotsToExpiry` | 1–300, default 150 |
| `forJitoBundle` | "Excludes DEXes that are incompatible with Jito bundles". Which DEXes: **UNVERIFIED**. It does **not** add a Jito tip |
| `computeUnitPricePercentile` | `medium`(p25) / `high`(p50, default) / `veryHigh`(p75) / integer |
| `tipAmount` | Adds `tipInstruction` = transfer to **Jupiter** tip accounts for `tx.jup.ag` — **not a Jito tip**. We never set it |
| ExactOut | Not supported on `/build` |
| Response | `inAmount, outAmount, otherAmountThreshold, swapMode, slippageBps, priceImpactPct (decimal ratio string), routePlan[{swapInfo{ammKey,label,inputMint,outputMint,inAmount,outAmount}, percent, bps}], computeBudgetInstructions (CU **price only**, no limit), setupInstructions (always includes ATA createIdempotent), swapInstruction, cleanupInstruction?, otherInstructions, tipInstruction?, addressesByLookupTableAddress (no RPC fetch needed), blockhashWithMetadata{blockhash:[u8;32], lastValidBlockHeight, fetchedAt}` |
| Swap fees | V2 swap instruction emits no fee events; `routePlan` has no LP fee fields → LP fees are only visible as "embedded in outAmount" |
| Submission | `POST https://tx.jup.ag` JSON-RPC `sendTransaction`, send-only, requires ≥ 1,000,000 lamport tip to Jupiter tip accounts. Not used: we submit via Jito |
| Plans / limits | 60 s sliding window **per organisation**. Keyless 0.5 rps, Free 1 rps, Developer 10, Launch 50, Pro 150. Swap+Price+Tokens share one bucket; `/swap/v2/execute` separate bucket. Headers `x-ratelimit-remaining/current/reset` on 200/429. 429 body `[API Gateway] Too many requests`; no lockout, retrying immediately keeps failing |
| Price | `GET /price/v3?ids=…` (≤ 50) → `usdPrice`, `blockId`, `decimals`, `priceChange24h`. Unreliable mints silently omitted |
| Deprecated | `lite-api.jup.ag` (being phased out), Ultra API (→ `/swap/v2/order`), `/tx/v1/submit` (→ `tx.jup.ag`). `/swap/v1/*` still billable with migration guides. v6 `quote-api.jup.ag/v6` and `public.jupiterapi.com`: not mentioned anywhere in current docs — treat as legacy |

Live observation: with the provided key, `x-ratelimit-remaining` starts at 9–10 with
`current` counting up → consistent with the Free tier (1 rps). The scheduler is
configured accordingly (`jupiter.general_rps = 0.9`).

### Jito Block Engine (docs.jito.wtf/lowlatencytxnsend, updated 2026-09-09)

- Hosts: `https://mainnet.block-engine.jito.wtf` + regional `amsterdam|dublin|frankfurt|london|ny|slc|singapore|tokyo`.
- JSON-RPC paths: `/api/v1/bundles` (`sendBundle`, `getBundleStatuses`, `getInflightBundleStatuses`), `/api/v1/getTipAccounts`, `/api/v1/transactions`.
- `sendBundle(params: [[tx…≤5], {"encoding":"base64"}])` → bundle id. **Default encoding is base58 (deprecated)** — always pass base64.
- `getInflightBundleStatuses` (≤5 ids, 5-minute lookback): `Invalid | Pending | Failed | Landed` + `landed_slot`.
- `getBundleStatuses` (≤5 ids): `confirmation_status`, `slot`, `err`, `transactions`.
- `simulateBundle` is **not** served by the block engine (live `-32601`); only Jito-Solana RPC nodes.
- Auth optional (`x-jito-auth` UUID). Default limit **1 rps per IP per region**, 429 over.
- Minimum tip 1000 lamports. Tip = any SOL transfer to one of the 8 tip accounts; **put it in the same tx as the strategy**; never reference tip accounts through ALTs.
- Tip floor REST: `https://bundles.jito.wtf/api/v1/bundles/tip_floor` → `landed_tips_{25,50,75,95,99}th_percentile`, `ema_landed_tips_50th_percentile`; **values are SOL, not lamports** (inferred from live values; docs don't state unit).
- Auction: 50 ms ticks, ranked by **tip / CU requested** → keep the CU limit tight.
- Atomicity: sequential, all-or-nothing, same slot. **Caveat:** on uncled blocks txs can be rebroadcast and hit normal banking stage without bundle protections → add pre/post assertions; a tip in a standalone tx increases uncle-bandit exposure.
- `jitodontfront…` read-only account: bundle rejected unless that tx is at index 0.
- BAM (Block Assembly Marketplace): exists; no official doc changes to searcher `sendBundle` API. **UNVERIFIED** impact.

### Solana

- `simulateTransaction`: `sigVerify` (false default), `replaceRecentBlockhash`, `commitment`, `innerInstructions`, `accounts{addresses,encoding}`; returns `err, logs, unitsConsumed, returnData, loadedAccountsDataSize, replacementBlockhash`, and on current nodes also **`fee`, `preBalances`, `postBalances`, `preTokenBalances`, `postTokenBalances`**.
- Fees: base 5000 lamports/signature; priority = `ceil(cu_limit × cu_price_micro / 1e6)` on the **requested** limit. ComputeBudget: disc 2 = SetComputeUnitLimit(u32 LE), 3 = SetComputeUnitPrice(u64 LE); duplicates fail.
- Limits: 1,232 bytes (v0), **64 account locks** (128 inactive), 1.4M CU/tx, blockhash valid 150 slots.
- **v1 transactions went live on mainnet 2026-09-15** (4,096 bytes, no ALTs, limits in `config_mask`). Jito support **UNVERIFIED** → we stay on v0 + ALTs.
- Token account rent-exempt minimum: 2,039,280 lamports.

## 2. Reference projects (clones analysed in scratchpad; nothing copied)

### fape-labs/arbitrage-bot (Go, 83-line main.go, 1 commit 2025-01-03, 0 tests)
- Borrow: only the idea of a closed round trip executed atomically.
- Outdated: v6-style `/quote` + `/swap` via `public.jupiterapi.com`; stitches two routePlans into one `/swap`.
- README-only: "calculates profitability", "customizable parameters", "automatic execution".
- Never for real money: profit = `finalOut > in` with no fees; no simulation; `SkipPreflight: true`; tight `continue` loop hammering the API (no backoff, panics on 429 body); private key in source; **cannot actually sign** (`AddSigner` never called); 100 bps slippage on a joined route means any edge < 1% can land at a loss.

### orakle-7th-sda/solana-mev-searcher (Go, ~5.2k LOC, 1 squashed commit 2026-03-06)
- Borrow: detection-only mode without key; package split rpc/searcher/mev; fee percentile stats from `getRecentPrioritizationFees`; bundle size validation; fixture-driven math tests.
- Outdated: Jupiter `/swap/v1`; testnet tip accounts labelled "official mainnet".
- README-only: "both legs submitted atomically" — false: detection models a round trip, execution submits a single one-way swap. The "backrun" is a directional bet.
- Never for real money: float64 profit math; hard-coded gross threshold (configured `MinProfitLamports` never read); `simulateBundle` called on the block engine (always `-32601`, so it never sends); `sendBundle` without `encoding: base64`; tip as a separate 1000-lamport tx; secrets compiled in.

### jito-labs/mev-bot (TypeScript, ~4.2k LOC, last code 2023-06, 0 tests, README: unmaintained)
- Borrow (best ideas of the set): **on-chain profit guard** — route `minOut = size + tip + fees + buffer` so the tx reverts unless profitable; tip in the **same** tx; tip = 50% of expected profit; stage timing telemetry; age limits + bounded pending queue with drop of stale items; ALT selection that avoids tables modified in the same bundle.
- Outdated/dead: Jito mempool (shut down 2024), Jupiter v4 core, Solend turbo flashloans, Node 16.
- README-only: "landed" = `getTransaction` returned anything (never checks `meta.err`).
- Not for real money as-is: no pre-simulation of its own bundle; no ComputeBudget; relies on dead infrastructure.

### jito-labs/searcher-examples (Rust, ~900 LOC, solana `=2.1`, 0 tests)
- Borrow: typed bundle rejection reasons; wait for Jito leader proximity before sending; tip inside the payload tx.
- Outdated: gRPC keypair auth now optional (JSON-RPC + optional UUID is the default path); solana 2.1 monolith crates.
- README-only: "adds retry semantics" (no send retry exists).
- Bugs: confirmation stream results not matched to bundle id; timeout never fires on busy streams; "landed" at `processed`; auth refresher retries in a hot loop and can die silently.

### TUI references
- **TX230/winproc-tui** (Rust, ratatui 0.30, v1.3.0): Graph Workspace = ordered registry with stable ids, cap, one active graph; one shared time state (span, offset, follow flag, cursor, A, B); values only at exact sample timestamps (`--` otherwise); common Y-label width so guides align; guides drawn before series; A/B letters on the baseline; Samples inspector with `A/B | Time | Value | Δ` and A→B min/max/avg/count; zoom ladder 60…7200 s, pan span/8, End = back to live, history freezes the window; recording = tagged JSONL, replay reuses the live UI. Avoid: 8k-line monolithic state.
- **tarkah/tickrs**: candle bucketing by plot width; volume in NINE_LEVELS under candles; timeframe strip; summary mode. Avoid: axes as block borders, faint braille-outlined candle bodies.
- **achannarasappa/ticker**: compact two-line rows, cells that appear progressively with width, P&L summary line, magnitude-scaled up/down color, changed-digit flash.
