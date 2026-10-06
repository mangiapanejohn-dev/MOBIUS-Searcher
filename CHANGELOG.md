# Changelog

All notable changes to MØBIUS. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0 a minor version may change configuration or behaviour.

## Unreleased

### Added

- **Wallet page** (`0`) in the terminal UI: what the wallet holds in SOL and
  USDC and what that is worth, how much of it a bot holds and how much is
  free to send; the address to receive at, whole and as a QR code (`c` copies
  it); what came and went, read from the chain (`⏎` shows a transaction's
  signature). `s` / `u` send SOL / USDC to another Solana address: the
  request is checked against the chain (the address is a wallet's and not a
  program's or a token account's, the amount is there, what is left is
  enough to keep the account), shown back in full with its fee, what opening
  the recipient's USDC account costs when they have none, and what is left;
  it is sent only after the last four characters of the address are typed,
  then signed, simulated as signed and sent until it is confirmed or has
  expired. Sending needs `execution.live_enabled` and a key file; the key is
  read only at that moment. `scripts/wallet_local.py` prepares a local chain
  to try it on without money. See [docs/USAGE.md](docs/USAGE.md#the-wallet-page).

- **Bots page** (`9`) in the terminal UI: the rules that hold a position,
  real and paper. For each: what it holds and is worth and what it waits for
  in words; a ruler with the price now between the two prices that matter
  (its buy and its sell price while it waits; what its SOL cost and its sell
  price while it holds) and how far it has to go; the exchange's live
  candles of its market, the newest following the ticker each second, with
  those prices drawn across and its buys and sells marked (`v`: a line; `[`
  `]` or a click: any bar from 1s to 1D, the rule's own marked, with volume
  under the price); the sums the prices come from (the average of how many bars, their deviation,
  how many of them under it buys); and what it did. A real one is started
  (`s`), stopped (`x`) and closed (`c`) from the page, each after a `y`; it
  then runs as a program of its own in the background. A rules file of the
  config directory that has not run yet is listed, to be started. The header
  names the real bot on every page, and the wallet's USDC is counted into
  Equity and listed under Assets. See
  [docs/TRADE.md](docs/TRADE.md#from-the-terminal-ui).

- **A bot can act on the price itself**, not only when a bar closes:
  `trigger = "price"` in `[live]`, or `t` on the Bots page for a bot that
  runs (it changes within seconds; its file is not rewritten). It then looks
  at the exchange's bid and ask every 2 seconds and decides on the bar that
  is forming as if it closed there: under its buy price it buys at once,
  over its sale price or under its stop it sells at once. With
  `take_profit` (`t` sets a thousandth) a gain that is there is taken: the
  sale is asked for as soon as it would bring that much more than the buy
  cost even at the swap's on-chain minimum, and sent only if the quote's
  minimum output is that much, so a sale sent this way is never one at a
  loss. What it has just sold it does not buy back at the same price. Not
  backtested (a backtest acts at closes), and it trades more often. See
  [docs/TRADE.md](docs/TRADE.md#acting-on-the-price-itself).

- **The Bots page shows each decision and each trade as it comes.** Under
  the chart, the bot's own looks at the price, newest first, written down by
  its program: the time, the price it saw, the price it measured that
  against, how far it was and what came of it (nothing, bought, sold and for
  how much more than paid, sent and not confirmed yet, tried and not sent,
  not buying back what it just sold); over them how often it looks and when
  it last did, in a warning's colour when that is too long ago. For a bot
  that decides at a close the first row is what it would do if the bar
  closed this second. Beside them its trades as round trips, with a line
  over them while a swap is sent and not accounted for yet. Where its gain
  is taken is a line on the chart and a row of its position.

- **What a bot was worth is a curve**, not a row of blocks: under the list
  of bots, with its values on an axis, the hours under it and its high and
  low marked, drawn like the other charts. It starts at the last change of
  the bot's budget: money given to it or taken back is not made or lost by
  it, and drawn together that one step flattened everything else.

- **A bot is made on the Bots page** (`n`): its kind (the average of one day
  or of three), the deviations under it at which it buys, the stop of a
  trade, its budget and its total stop, each with what it means, what such
  rules cost on a year of past prices, and the words `ALLOW LOSS`, which
  the operator types. It acts on the price itself unless the form says at
  each bar's close. It writes a rules file like one written by hand into
  the config directory and starts nothing: `s` does.

- **A bot's buys and sales are listed as trades**: first on the Trades page
  (when, which bot, what for what, at what price, what a sale made), in the
  Markets page's order history among the arbitrage's own, and as a table on
  the Bots page. On a wide screen the Bots page puts the account beside the
  chart and says more of it under words of their own: the position (when it
  was bought, for how long, what it is up or down, how far to its sale and
  to its stop) or what it waits for, what it has done so far, the day's
  market, when it decides next, and its worth bar by bar. A price the rule
  acts at that is far from the candles (a stop five per cent under) is named
  at the chart's edge instead of pressing the candles into a corner.

- **The Bots page works its sums out again every second** at the price
  now, with the bar that is forming counted at it: the average, the
  deviation, how many deviations the price is from the average, where that
  stands on a scale between the rule's two thresholds, and whether it would
  act if the bar closed this second. The chart's lines, the ruler, the
  sentence and the header's word use the same live numbers (a stopped bot's
  are those of its last closed bar). Beside the chart: the money (budget and
  what it is worth, what is in SOL and what is idle, made on closed trades,
  up or down on what it holds, both together, what it bought and sold in
  all, how many trades and how many won, the best and the worst). Under it:
  its trades as round trips (bought and sold when and at what, held how
  long, made in USD and per cent; one that is held stands first, at the
  price now). Under the list: all the real bots in one sum, and the
  exchange's own trades as they come.

- **The Wallet page says where the money is and what it made**: a table of
  what each real bot holds, what is kept for fees and what is free to send
  (SOL, USDC, about how many USD, its share); a table of what the money
  made, a row a bot (budget, worth, made on closed trades, up or down now,
  in all, trades closed) with their sum, the runs that ended long ago in one
  row, and the arbitrage of the session; and what the listed transactions
  come to (received, sent, fees).

- **A detail is drawn as a document with tables**, in a larger overlay
  that scrolls. A bot's record (`⏎` on the Bots page) is a framed table a
  day: time, what it did, and the parts of that as named rows (a swap that
  was sent: what for what, the least it accepts, the route, the fees, the
  outcome, the signature), wrapped between words inside its cell. A
  transaction's detail on the Wallet page is a table too, and so is the
  Chinese guide of a page (`?`): its names and its keys, each with what it
  means. The text behind them is plain Markdown tables (`| a | b |`).

- **Paper experiments are not listed among the bots** until `p` asks for
  them: a simulated account is not the operator's money. A run that has
  ended is shown as ended with what it ended at, not as holding it, and a
  live session's header no longer carries a simulated figure it does not
  have.

- **Addresses the wallet sent to** are remembered (the last six, in the data
  directory) and offered when the next transfer is written: `↑` `↓` on the
  address puts one in. It is checked and confirmed like any other.

- **A bot's budget can be changed** while it runs or is stopped (`b` on the
  Bots page, 1 to 25 USD): the form says what the number written would do
  before anything is asked. Raised, the difference comes from USDC the wallet
  holds free and, for what is missing, from SOL sold (never the bot's own,
  nor the SOL kept for fees); lowered, it stays in the wallet as USDC, no
  longer the bot's, which waits until it has sold when it holds SOL. What
  the run has made or lost stays as it is, and the price it is sold out at
  follows the budget. The change is left for the bot's own program, which
  takes it within seconds, tries again after each bar while it cannot, and
  says in its record what it did.

- **Chinese** when the system language lists it or `--lang zh` is given: the
  Bots and Wallet pages (labels, what a bot waits for, what it printed, the
  questions asked before an action and the answers), the header's and the
  footer's names, and `?`, which there explains the page one is on (what it
  is, what each part of it means, what each key does) for every page.

- **The lab** (`--lab FILE`, `--lab-backtest FILE [--days N]`, `--lab-report`):
  rules that hold a position, tried on paper. Four kinds of rule (reversal
  after a down bar, buying a dip under an average, a grid, a breakout) are
  written in a rules file (`config/lab.toml`), run over past candles or on
  live prices bar by bar with a paper account, and reported in full: return,
  deepest fall, the share held in SOL and what holding that share alone
  returned, what the rule added beyond it with its uncertainty, every trade's
  average, costs that grow as the trade shrinks, every month. Nothing is
  signed or sent and no Jupiter request is made; it can run beside a session.
  On one year of SOL none of the shipped rules made money. See
  [docs/LAB.md](docs/LAB.md).
- **A trained model as a lab rule** (`rule = "model"`), and the script that
  trains one (`scripts/direction_model.py`): a logistic regression and
  boosted trees on 26 features of SOL's and BTC's 15-minute candles since
  2021, three horizons, rolling tests, six months kept aside and looked at
  once. The model trained on 2026-10-04 predicts a little in the rolling
  tests (rebounds after short falls, and the hour) and nothing on the months
  kept aside: it did not pass, and is in the lab only. The lab builds the same features in
  Rust, checked against the Python on a fixture. See
  [docs/MODEL_2026-10.md](docs/MODEL_2026-10.md).
- **One lab rule with real money** (`--trade FILE`, `--dry-run`, `--close`):
  the rule decides at each bar's close as it does on paper; the fill is a
  real SOL/USDC swap through Jupiter, sent as an ordinary transaction (to
  the RPC and through Jito to the leader, again until it is confirmed or its
  blockhash has expired) and then read back from the wallet's balances. A
  budget of 1 to 25 USD is set aside as USDC once; at `stop_total_loss` down
  everything is sold and the run ends for good; a swap pays at most 16,000
  lamports in fees, and is not sent if in the simulation it would cost the
  wallet anything else. It can lose money and nothing on
  chain prevents it: the file must carry `acknowledge = "ALLOW LOSS"` and the
  config `execution.live_enabled = true`. On paper, at 2 USD and with these
  costs, the shipped example lost 45 % over the year to 2026-10-04.
  `scripts/trade_fork.py` sets up a local copy of mainnet on which the whole
  runner can be tried with test SOL. See [docs/TRADE.md](docs/TRADE.md).
- **`scripts/live_tracker.py`**: follows the database of a running session,
  read-only, and copies every table into a dataset file that retention never
  prunes, with OKX's best bid and ask once a second beside it (the engine
  does not record them) and a status line a minute. For keeping real
  sessions to learn from; see [docs/STORAGE.md](docs/STORAGE.md).

### Fixed

- **`--doctor` no longer warns about OKX on the shipped configuration.** OKX
  stopped listing `SOL-USD` (it answers "Instrument ID doesn't exist"; it
  still did on 2026-09-17), so the default `markets` of `[venues.okx]` are
  now `SOL-USDT` and `SOL-USDC`. A `markets` list of your own that still
  names it keeps the warning, as it should.
- **Keys no longer act on lists that are not on the page.** On Risk and
  System, `j`/`k` scrolled the event stream of the Overview and `⏎` opened
  one of its lines; on Trades, `j`/`k` and `f` moved and filtered the
  opportunity list. The footer named those keys there too. Now each page's
  footer lists the keys that do something on it, and the others do nothing.
- One line of the key help was two columns too long for an 80-column
  terminal and wrapped.

### Changed

- **Text that does not fit ends in `…`** instead of stopping mid-word: routes
  in the opportunity list and the event stream, venue names in the quote
  book, status codes, error lines on System, descriptions in the graph
  picker. At 120 columns the Opportunities page now shows the longest route
  (`Raydium CLMM → Meteora DLMM`) and its status whole.
- **`? help` is always in the footer**, at every terminal size; the kill
  switch and the page's first keys come next, the rest as room allows. Below
  120 columns the footer names the page you are on and keeps the others'
  digits (it used to cut every name to three letters).
- **Overlays say how to close them**, on their bottom border. A detail
  (`⏎` on a stream or log line) is as tall as its text instead of filling
  the screen, and a long one scrolls (`j`/`k`, PgUp/PgDn, the wheel). The key
  help is grouped (Navigate, Graphs, Markets, Trading, Mouse) with the keys
  in the accent colour; the graph picker is as wide as its descriptions.
- Risk: the reasons opportunities were not executed show their share; counts
  have thousands separators. System: pipeline and network values start in
  one column next to their labels.

## 0.3.0 — 2026-10-03

A research release: new measurements, and no strategy that sends anything.
None of them found a direction that pays for its costs at this scale
([docs/RESEARCH_2026-10.md](docs/RESEARCH_2026-10.md)). As in 0.2, nothing has
been sent to the chain: the canary has not been run against mainnet, so
CONFIRM and LIVE remain verified only by tests and simulation.

This is the first release after 0.1.0 that the installers download by
default. It also contains everything from 0.2.0-beta.1 (`--research`,
`--canary`, the venue connectors, inventory accounting, the thresholds
panel): see that section of the changelog.

### Added

- **`--research-liquidations [DAYS]`**: every Morpho Blue liquidation of the
  last DAYS days (default 30) on Base, read back from chain into
  `research.sqlite`. For each: the incentive the protocol paid at its oracle
  price (`repaid × (LIF − 1)`), the gas the winner paid (L2 and L1 data fee),
  the sender, and the first block at whose end the position was liquidatable.
  That block is found by asking the Morpho contract itself (`liquidate` as an
  `eth_call` at past blocks: it reverts with "position is healthy" or goes on
  to the repayment), so interest and the oracle are the contract's, not a
  formula of ours. The report shows the whole distribution: incentive sizes,
  how concentrated they are in days and in senders, how long each one waited
  for its winner, gas as a share of the incentive, and what was left for a
  liquidator several blocks late. Signs and sends nothing; needs no key and
  no Jupiter budget.
- **Pool math of our own** (`searcher_market::amm`): what a swap returns on
  Orca Whirlpool, Raydium CLMM and Meteora DLMM, from the pool account and
  the tick or bin arrays around the price. Written from the published
  mathematics, and checked against what each pool program itself paid in
  swaps simulated on mainnet at the same state (`scripts/amm_parity.py`,
  `fixtures/amm`): Whirlpool and DLMM to the last unit (up to 150 SOL,
  through 11 bins), Raydium to the last unit inside a tick range and within
  6 parts in a billion across ticks.
- **`--research-pools`**: once a second, the round trip between each two
  watched pools at 0.1, 1 and 10 SOL, from the pools' accounts at one slot.
  No quote API and no Jupiter budget: the gap between pools without our
  latency in it, and how long a gap that would pay for a transaction lasts.
  `--research-pools-report` prints every recorded run.
- **Resting orders, imagined** (in the same run): every snapshot of the
  Meteora pool is kept with the exchange price, and the report works out what
  orders resting one, two and five bins from the price would have met: how
  often the price went through them within a minute, and where the exchange
  price was 0–30 s after the fill, with the fee the bin earns counted in.
  Split by whether the pool was below or above the exchange when the order
  went in: the maker's side of the DEX-lag signal.
- `scripts/arb_replay.py`: the arbitrage actually done on the watched pools,
  read back from chain: how many, what each took, what the winners paid in
  fees and tips, who they were, and what the transactions that found nothing
  cost (docs/RESEARCH_2026-10.md).
- `scripts/filter_compare.py`: a text decision model (Laya, zero-shot) and
  a logistic regression as filters on the recorded lag round trips. Neither
  ranks them before the quote; the model does not after it either
  (docs/RESEARCH_2026-10.md).
- EVM JSON-RPC: logs, receipts, block headers and calls that keep a revert
  reason apart from a failed request.
- **Quote aging** in `--report`: every simulated leg is recorded with its
  quoted output, its executed output and the age of its quote when the
  simulation was sent (`leg_aging`). The report shows executed − quoted by
  age (under 0.5 s … over 4 s), by DEX, and a straight line through the
  legs: what a perfectly fresh quote would still lose, and what each second
  of waiting adds, with the standard error of that slope. This separates the
  loss that is there at once from the loss that comes from the quote API and
  the queue in front of it.

### Fixed

- On Windows the second process refused the Jupiter budget could not say who
  holds it (a locked file cannot be read there); the holder is now also kept
  in `jupiter-budget.holder`. This was the failing Windows CI test.

## 0.2.0-beta.1 — 2026-09-23

A research release: it measures where an edge could come from, and reports
that it did not find one ([docs/RESEARCH_2026-09.md](docs/RESEARCH_2026-09.md)).
Nothing in 0.2 has been sent to the chain: the canary that proves the sending
path end to end has not been run against mainnet, so CONFIRM and LIVE remain
verified only by tests and simulation.


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
- A Jupiter `/build` answer whose route returns nothing is an error
  (`ZeroOutput`), not a price of zero. Research also refuses any quote more
  than 500 bp from its reference (the CEX mid for a lag entry, the entry
  amount for its exit, the Solana buy price for a cross-chain sell) and
  records it as an error: mainnet returns the occasional 200 that delivers a
  third of the amount, and one such number swamped a whole distribution.
- Research survives the machine sleeping: a wall-clock jump ends open
  episodes as interrupted and drops every held price, and a pool mid older
  than 30 s is never compared with a live CEX price.
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

