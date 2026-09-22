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
- **Binance connector** (`kind = "binance"`, off by default): market data
  from the public mirror `data-api.binance.vision` (instrument rules incl.
  minimum notional, order book, paper fills); signed spot orders, balances
  and cancel (signature checked against Binance's documented examples),
  against `rest_url` or, with `demo = true`, the spot testnet. HTTP 451
  ("not available from this location") is reported as such.
- **One price question for every venue** (`searcher_venues::market`):
  "buy/sell N base now: average and net price after fees (and gas on
  chains), source, data age". Order books are walked; Uniswap uses
  QuoterV2 exact input (sell) / exact output (buy).
- `--quote MARKET --size N` prints that for every enabled venue listing the
  market, plus the cheapest buy, the best sell and the gap between them.
- `--doctor` checks enabled EVM venues (chain id, pool price, block) and,
  when OKX or Binance credentials are set, a signed read-only balance request.
- **`--research`**: measurements that decide what to build next, recorded to
  `<data dir>/research.sqlite` (nothing is signed or sent). Size ladder (the
  configured routes quoted at 0.01–2 SOL), cross-chain spreads (ETH and cbBTC:
  Solana via Jupiter vs Base/Arbitrum via Uniswap v3, after swap fees, Solana
  fees and gas), and DEX lag (pool mids vs OKX/Binance best bid/ask, with an
  executable Jupiter quote per episode, control samples at random times and
  CEX/pool markouts; gaps ≥ `lag_scale_trigger_bps` are also quoted at 0.5
  and 1 SOL to see whether the edge survives size; after each entry quote the
  reverse swap is quoted at +0/5/15/30 s for exactly what the entry delivered,
  the on-chain round trip; each pool notification's slot lag behind the chain
  head is recorded; entries have the first claim on Jupiter: exits never
  take the last rate-limit slot and everything else never the last two).
  `--research-report [RUN|latest|all]` prints whole
  distributions (sample counts next to positive counts).
- Attribution for every opportunity (`attribution` table): which profit guard
  failed (EDGE_TOO_SMALL covers three), the SOL price used, whether the
  verdict used quoted or simulated costs, each leg's quoted and executed
  output (Jupiter `Program return`), and the accounts a simulated transaction
  leaves created with their deposit. `--report` shows guard counts,
  executed-vs-quoted first legs and created accounts. This explained the
  0.013 SOL gap on HumidiFi-final routes: the route creates a 2,440-byte
  account (13,045,440 lamports = its rent-exempt minimum) paid by the taker.
- Two-token inventory (SOL + USDC): a leg's input stays fixed at the previous
  leg's quote, and the wallet's inventory of the intermediate token absorbs
  the difference instead of the transaction failing. The SOL-equivalent
  result now includes that inventory drift (executed leg outputs, valued at
  the rest of the route's quoted rate); protection adds the intermediate
  legs' worst-case shortfall to the final leg's minimum output. When the
  inventory cannot cover a shortfall the skip reason is `INVENTORY_LOW`.
- USDC inventory reminder: `--doctor` shows how many worst-case first-leg
  shortfalls the wallet's USDC covers (trade size × slippage tolerance), and
  a running session warns in the log below 20. Only SOL-based cycles trade in
  this release; USDC-based cycles are planned with the cross-venue work.
- Deposits (rent of accounts a trade leaves created) are capital, not a trade
  cost: excluded from net PnL, shown separately, and capped by
  `profit.max_new_deposit_lamports` (default 0.003 SOL, two token accounts;
  above it: `DEPOSIT_TOO_HIGH`). Previously the rent of a missing token
  account was charged to every candidate, so a new wallet could never pass.
- Wallet ledger in USD (CONFIRM/LIVE): SOL and USDC balances with the SOL
  price every minute (`inventory` table); `--report` splits the change of the
  wallet's value into trade PnL, deposits, SOL price change on holdings, and
  whatever is left unexplained (printed, not absorbed).
- The Solana stack's settings live under `[venues.solana]` now
  (`[venues.solana.rpc]`, `.jupiter`, `.jito`, `.feeds`, `.wallet`), next to
  the other venues. The old top-level sections are still read (until v0.4;
  a note says so); `--migrate-config` moves them in your file, shows the
  result, asks first, keeps comments and a `.bak`, and checks that the
  effective configuration is exactly the same. A section set in both places
  is refused.
- **`--canary`**: one real trade through the LIVE path to prove it end to
  end. CONFIRM mode (approve each candidate with `y`), one SOL → USDC → SOL
  route, and the profit guards replaced by a loss bound
  (`canary.max_loss_lamports`, default 0.0005 SOL) that is also written into
  the transaction's on-chain minimum output. After the first landed trade the
  session ends and the transaction is fetched and reconciled account by
  account: every lamport of the taker's SOL and every USDC atom must be
  explained by the executed leg outputs, the fee, the tip and deposits. The
  report is printed and kept under `<data dir>/canary/`.
- A negative `profit.min_profit_usd` is accepted (it used to fail to parse).
- Threshold panel in the TUI (`T`, keyboard only): minimum profit (lamports,
  bp, USD), on-chain min-out, slippage reserve, safety buffer, max deposit,
  slippage tolerance, max trade size, max daily loss. Changes are staged,
  reviewed old → new and applied with Enter; the engine validates them,
  applies them live, logs every change and writes only the changed keys into
  your config file (comments kept, previous file as `.bak`). Settings under
  which a landed trade can lose money (min-out off, negative minimum profit)
  need the typed words `ALLOW LOSS`, and the header then shows
  `LOSS ALLOWED`.
- One process at a time uses the Jupiter budget: `--research` and trading
  sessions take `<data dir>/jupiter-budget.lock`; the second one is refused
  with the holder's name. On macOS `--research` keeps the machine awake
  (`caffeinate`; `research.keep_awake`).

### Fixed

- Token-account rent is read from the chain at startup
  (`getMinimumBalanceForRentExemption`): mainnet lowered it from 2,039,280 to
  1,488,440 lamports, and the old constant overcharged every route that
  creates an account.
- Jupiter's custom error 6024 is `InsufficientFunds` (from the program's
  on-chain IDL) and is now classified as such instead of a generic program
  error. In the recorded LIVE simulations it occurred exactly when the first
  leg delivered less than its quote (136 of 136; 0 of 19 otherwise): the next
  leg's input is fixed at the quoted amount.
- Localhost and loopback Jupiter/RPC endpoints now bypass automatic system
  proxies and respect `NO_PROXY`, so local nodes and HTTP test servers are not
  routed through a macOS proxy.

### Documentation

- Removed npm from the advertised install methods until the package is
  available in the public registry.

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

