# Changelog

All notable changes to MØBIUS. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0 a minor version may change configuration or behaviour.

## Unreleased

### Added
- **OKX connector** (`searcher-venues`): signed v5 REST (HMAC-SHA256, clock
  offset from `/public/time`), instrument rules (lot, tick, minimum size),
  place / query / cancel orders, balances, and paper fills walking the live
  order book with the configured taker fee. `demo = true` (default) sends
  orders to OKX demo trading. An order needs a `TradePermit`: a real-account
  permit also requires CONFIRM/LIVE and `execution.live_enabled`. Not yet
  used by any strategy; `trading = true` is still refused at startup.
- **EVM connector**: JSON-RPC, Uniswap v3 pool state (`slot0`, `liquidity`,
  tokens and decimals read on chain) and exact quotes from QuoterV2.
  Built-in venues `ethereum`, `base`, `arbitrum` (WETH/USDC 0.05 %, official
  QuoterV2 / SwapRouter02 addresses checked on chain), off by default.
- **EVM signing and swaps**: Keccak-256, RLP, EIP-55, EIP-1559 transactions
  signed with secp256k1 (reproduces the EIP-155 worked example byte for byte
  and re-encodes a real Base type-2 transaction exactly). `EvmTrader`
  approves and swaps through SwapRouter02; every swap is simulated from our
  address first and refused below `min_out`. Our swap calldata, simulated on
  the real routers, returns exactly QuoterV2's output on all three chains.
  Needs a real-account permit (no demo on chains). No strategy sends yet.
- `--doctor` checks enabled EVM venues (chain id, pool price, block) and,
  when OKX credentials are set, a signed read-only balance request.

## 0.1.0 — 2026-09-19

First public release. MØBIUS is built as a multi-venue quant trading bot; in
this release **Solana is the only venue that can trade** (arbitrage through
Jupiter) and **OKX provides market data only**.

### Status — read this first

- **PAPER by default.** Three mainnet PAPER soaks: 3,091 evaluations, 2,333
  mainnet simulations, **0 executable opportunities** at the shipped settings
  ([docs/PAPER_RUN.md](docs/PAPER_RUN.md)).
- **CONFIRM and LIVE are locked** behind `execution.live_enabled` and a keypair
  file. The code paths are unit-tested; LIVE runs recorded **0 executions**.
  No transaction has been confirmed on chain by this release.
- **OKX: market data only** (Markets page). No order connector; `trading = true`
  on a venue is refused at startup.

### Added

**Solana venue**
- Quotes and swap instructions from Jupiter Swap API V2 (`/swap/v2/build`),
  keyed or keyless.
- Strategies: SOL round trip, cross-DEX (ordered DEX pairs), triangular.
- Cost model (fees, priority fee, Jito tip, ATA rent, slippage share, safety
  buffer) and a risk engine (size, daily loss, failures, quote/simulation age,
  slot lag, fee reserve).
- Simulation-first execution: every candidate transaction is simulated on
  mainnet before any decision to send; Jito bundle submission behind the LIVE gate.

**Real-time data (no Jupiter budget spent)**
- Pool accounts (Whirlpool, Raydium CLMM, Meteora DLMM) and Pyth price accounts
  over the RPC WebSocket; slots, priority fees, TPS; Jito tip stream.
- Pyth Hermes stream adapter (`feeds.oracle_source = "hermes"`). Hermes needs
  a key; without one it falls back to on-chain Pyth. Not verified against live
  Hermes in this release (no key available).

**Request scheduling and latency** ([docs/LATENCY.md](docs/LATENCY.md))
- Event-driven Jupiter scheduler (default): pool events → only the routes that
  read them, priority by expected value per request, admission control,
  shared leg cache with event invalidation, request coalescing.
- Sliding-window rate limiter learned from Jupiter's `x-ratelimit-*` headers;
  safety and reserve scale with the learned window (keyless 5 vs keyed 10 per
  ~10 s).
- Monotonic-time instrumentation of every stage; `--headless --duration N`
  writes a latency report (p50/p95/p99/max) to `<data_dir>/bench/`.
- Measured A/B vs the old round-robin scanner (`--scheduler round_robin`, kept
  for comparison), keyless: 30 % fewer requests, 0 × 429, quotes at decision
  time 2.6 s → 0.64 s (p50). Market change → usable quote did **not** get
  faster: the Jupiter rate limit is the bottleneck.

**Configuration**
- Layers: built-in defaults → `config/mobius.toml` (shared, no personal data)
  → `~/.config/mobius/config.toml` (your changes only) → flags.
- Secrets only as variable names; values from the environment,
  `~/.config/mobius/.env`, then `./.env`.
- `[venues.<name>]` registry with connector `kind` (`okx`), endpoints,
  credential variable names and instruments.
- `[network] proxy` (`auto` / `none` / `http://host:port`), `[storage]` retention.
- `--print-config` (every value and its layer) and `--doctor` (secrets by
  name, proxy, every endpoint and venue; with `--mode live` also keypair and
  fee-reserve balance).

**Terminal UI and onboarding**
- Ratatui TUI with mouse support, OKX-style Markets page (line/candles,
  VWMA, live price tag), graph workspace, replay of recorded sessions and
  snapshot rendering.
- First-use wizard (`--setup`): Research / Assisted / Advanced paths, bot
  wallet creation outside the repository (`0600`), secrets written only to the
  user's `.env`.

**Storage**
- SQLite recording of every session; event log compacted into compressed
  blocks; retention policy (7 days, 90 days for sessions with trades, 1 GB
  cap) applied at startup and every 30 min; `--prune`, `--db-info`.

### Known limitations

- Only Solana can trade. OKX trading, Binance and EVM-chain DEXes are planned
  (next release) and not present.
- The Solana settings are still top-level sections (`[rpc]`, `[jupiter]`,
  `[jito]`, `[feeds]`, `[wallet]`); they move under `[venues.solana]` later.
- On the keyless Jupiter tier (5 requests per ~10 s) a triggered route waits
  p50 7–10 s for budget.
- Public RPC endpoints differ widely in freshness; the default
  `api.mainnet-beta.solana.com` measured 0–2 slots behind the chain head,
  `solana-rpc.publicnode.com` ~30 slots. Check yours with `--doctor`.

