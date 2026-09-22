<p align="center">
  <img src="docs/images/banner.png" alt="MØBIUS-Searcher" width="100%">
</p>

<p align="center">
  <a href="https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/releases/tag/v0.1.0"><img src="https://img.shields.io/github/v/release/mangiapanejohn-dev/MOBIUS-Searcher?style=for-the-badge&color=d97757" alt="Latest release"></a>
  <a href="https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/mangiapanejohn-dev/MOBIUS-Searcher/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI status"></a>
  <a href="https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/stargazers"><img src="https://img.shields.io/github/stars/mangiapanejohn-dev/MOBIUS-Searcher?style=for-the-badge&color=d97757" alt="GitHub stars"></a>
  <a href="https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/issues"><img src="https://img.shields.io/github/issues/mangiapanejohn-dev/MOBIUS-Searcher?style=for-the-badge&color=3a3a3a" alt="Open issues"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-2024-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust"></a>
  <a href="https://ratatui.rs"><img src="https://img.shields.io/badge/Built_With-Ratatui-000000?style=for-the-badge&logo=ratatui&logoColor=white" alt="Built with Ratatui"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT_OR_Apache--2.0-3a3a3a?style=for-the-badge" alt="License: MIT OR Apache-2.0"></a>
  <a href="#safety-model"><img src="https://img.shields.io/badge/mode-PAPER_by_default-2ea043?style=for-the-badge" alt="PAPER by default"></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E6%96%87%E6%A1%A3-%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-c9302c?style=for-the-badge" alt="简体中文"></a>
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#terminal-ui">Terminal UI</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="docs/CONFIGURATION.md">Configuration</a> ·
  <a href="#documentation">Docs</a> ·
  <a href="ROADMAP.md">Roadmap</a> ·
  <a href="CONTRIBUTING.md">Contributing</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

## Live Solana arbitrage research in your terminal

**Real DEX quotes. Real transaction construction. Real mainnet simulation. PAPER by default.**

MØBIUS-Searcher watches Solana DEXes in real time, prices routes with Jupiter,
builds the actual v0 transaction, simulates it on mainnet, and explains — with
numbers — why a route would or would not trade.

| **3,091** | **2,333** | **0** |
|:---:|:---:|:---:|
| real evaluations | mainnet simulations | executable opportunities at the default configuration |

Most trading-bot demos start with a winning screenshot. MØBIUS-Searcher starts
with a harder question: **after fees, slippage, Jito tips, ATA rent, stale
quotes, transaction construction and mainnet simulation, is the edge still
real?**

So far, at the default configuration, the answer is **no**. That result is the
point — the searcher records the losing routes too, making the system useful as
a reproducible Solana execution and MEV research tool rather than a profit
claim.

> [!WARNING]
> **Not a money printer.** The current PAPER results contain 3,091 real
> evaluations and 2,333 mainnet simulations with **0 executable opportunities**
> ([full results](docs/PAPER_RUN.md)). PAPER uses real market data and real
> simulation but signs and sends nothing. CONFIRM and LIVE are explicit opt-in
> modes and have never sent a transaction. Nothing here is financial advice.

<p align="center">
  <img src="docs/images/markets.png" alt="The Markets page: price chart with VWMA and live price tag, DEX quote book, bots, ticker strip" width="100%">
  <br><sub>The Markets page on live data (PAPER). Price, 24 h stats and candles from OKX; the quote book is what the bot can actually trade on-chain.</sub>
</p>

<table>
<tr><td><b>Simulation-first</b></td><td>Every route becomes one real v0 transaction (all legs, compute budget, Jito tip), simulated on mainnet before anything could be sent. A simulation error or a thin edge means it is never sent.</td></tr>
<tr><td><b>Honest accounting</b></td><td>Integer money everywhere. Base fee, priority fee, Jito tip, ATA rent, slippage share and a safety buffer are costed per route; every evaluation is recorded — including the losing ones and why they were skipped.</td></tr>
<tr><td><b>Event-driven scheduling</b></td><td>Pool and oracle accounts stream in over the chain WebSocket; a dependency graph spends the Jupiter rate limit only where the market moved. 30 % fewer requests, zero 429s, quotes 4× younger at decision time.</td></tr>
<tr><td><b>A terminal UI worth using</b></td><td>Eight pages built with Ratatui: an exchange-style Markets view, opportunities with an inspector, stacked graphs with cursors and A/B deltas, risk, system health, logs. Mouse and keyboard.</td></tr>
<tr><td><b>Gated execution</b></td><td>PAPER → CONFIRM (approve each transaction with <code>y</code>) → LIVE, each behind explicit configuration, a risk engine, an on-chain minimum output and a kill switch.</td></tr>
<tr><td><b>Record and replay</b></td><td>Every session goes to a local SQLite database (compressed, pruned automatically) and replays through the exact same UI. Reports summarise edges, costs and simulation failures.</td></tr>
<tr><td><b>Multi-venue config</b></td><td>Layered TOML (built-in → shared → yours → flags), <code>[venues.*]</code> side by side, secrets only as environment-variable names, <code>--doctor</code> checks every endpoint.</td></tr>
</table>

---

## Where it stands

| | |
|---|---|
| **PAPER results** | 3 soak runs · 3,091 evaluations · 2,333 mainnet simulations · **0 executable** · median gross edge ≈ −3.5 bp, net ≈ −8.9 bp ([docs/PAPER_RUN.md](docs/PAPER_RUN.md)) |
| **Scheduling** | event-driven by default: vs. round-robin, 30 % fewer requests, 0 × 429, quotes at decision time p50 2.6 s → 0.64 s. *Market change → usable quote did not get faster:* the Jupiter rate limit is the bottleneck ([docs/LATENCY.md](docs/LATENCY.md)) |
| **Data freshness** | pool updates arrive 0–2 slots behind the chain head on the default RPC |
| **CONFIRM / LIVE** | implemented and unit-tested, locked by default, **no transaction ever sent** |
| **Venues** | Solana: execution. OKX: market data only (no order connector yet) |

## Get involved

MØBIUS-Searcher is most useful when its claims can be reproduced outside the
machine they were measured on. If you want to contribute, the highest-value
work right now is **benchmark reproduction, execution correctness, terminal
compatibility, tests, and failure attribution**.

- [Roadmap](ROADMAP.md) — current engineering and research questions
- [Contributing](CONTRIBUTING.md) — development setup and benchmark format
- [Issues](https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/issues) — bugs, proposals and benchmark reproductions
- [Security](SECURITY.md) — private reporting for anything that could put keys or funds at risk

Negative results are welcome. A failed reproduction is useful evidence when the
environment and method are documented.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.sh | sh
```

<details>
<summary><b>Windows · Cargo · from source</b></summary>

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.ps1 | iex
```

Cargo (Rust ≥ 1.91):

```bash
cargo install --git https://github.com/mangiapanejohn-dev/MOBIUS-Searcher mobius-searcher --locked
```

From source:

```bash
git clone https://github.com/mangiapanejohn-dev/MOBIUS-Searcher && cd MOBIUS-Searcher && cargo build --release
```

</details>

Prebuilt binaries for macOS (Apple silicon, Intel), Linux (x86_64, ARM64) and
Windows (x64) are on the [Releases](https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/releases)
page; every installer verifies `SHA256SUMS`. Details, paths and uninstalling:
[docs/INSTALL.md](docs/INSTALL.md).

> [!TIP]
> Use a GPU terminal with an image protocol:
> on macOS and Linux [Ghostty](https://ghostty.org/) , [Warp](https://www.warp.dev/) on Windows. The logo then
> renders as a real image; everything else works in any modern terminal.

## Quick start

```bash
mobius-searcher --doctor        # config, secrets (names only), proxy, every endpoint
mobius-searcher                 # first run opens a short setup; PAPER by default
mobius-searcher --report latest # what happened, with numbers
mobius-searcher --replay latest # the same UI over the recording
```

No keys are needed to start: Jupiter works keyless at a lower rate, the public
Solana RPC is the default. An optional `JUPITER_API_KEY` goes in
`~/.config/mobius/.env`. More in [docs/USAGE.md](docs/USAGE.md).

## Safety model

| Mode | Market data | Transactions | Sent | Unlocked by |
|---|---|---|---|---|
| **PAPER** (default) | real | assembled + simulated on mainnet | never | — |
| **CONFIRM** | real | assembled + simulated | after you press `y`, one by one | `execution.live_enabled = true` + a keypair file + `--mode confirm` |
| **LIVE** | real | assembled + simulated | automatically, when simulation and risk pass | the same + `--mode live` |

Before anything is sent: simulation must show a profit after all costs; the
final swap carries an on-chain minimum output so a price move reverts instead
of losing; the risk engine checks size, equity share, daily loss, fee reserve
and staleness; **`K` stops new submissions from any page.** The private key is
a `chmod 600` file outside the repository (MØBIUS refuses one others can read;
on Windows keep it in your user profile, where only your account can read it),
never an environment variable.
See [SECURITY.md](SECURITY.md) and [docs/LIVE_CHECKLIST.md](docs/LIVE_CHECKLIST.md).

## Terminal UI

<table>
<tr>
<td width="50%"><img src="docs/images/overview.png" alt="Overview"><br><sub><b>1 Overview</b> — opportunities, graph workspace, event stream, inspector</sub></td>
<td width="50%"><img src="docs/images/opportunities.png" alt="Opportunities"><br><sub><b>3 Opportunities</b> — every route and why it was skipped</sub></td>
</tr>
<tr>
<td><img src="docs/images/graphs.png" alt="Graphs"><br><sub><b>4 Graphs</b> — stacked metrics, cursor, A/B deltas, samples</sub></td>
<td><img src="docs/images/system.png" alt="System"><br><sub><b>7 System</b> — every connection: state, latency, errors, rate limits</sub></td>
</tr>
<tr>
<td><img src="docs/images/last-trades.png" alt="Last trades"><br><sub><b>2 Markets</b> — last trades and buy/sell share</sub></td>
<td><img src="docs/images/keys.png" alt="Keys"><br><sub><b>?</b> — keys, with the logo</sub></td>
</tr>
</table>

| Keys | |
|---|---|
| `1`–`8` pages · `Tab` focus · `?` help · `q` quit | `K` **kill switch** (any page) |
| `←/→` cursor · `[ ]` timeframe / candle bar · `c` line / candles | `a` `b` `x` A/B markers · `+` graphs · `f` filter |
| Markets: `p` pair · `t` quote book / trades · `o` bottom tabs | `y` / `n` approve / decline in CONFIRM |

Full reference: [docs/USAGE.md](docs/USAGE.md#terminal-ui).

## How it works

```mermaid
flowchart LR
    WS["Chain WebSocket<br/>pools · Pyth · slots"] --> R["Router<br/>pool → route graph"]
    R --> J["Jupiter /swap/v2/build<br/>per leg, rate-limited"]
    J --> C["Cost model<br/>fees · tip · rent · slippage"]
    C -->|edge too small| X["Recorded + skipped"]
    C --> A["Assemble one v0 tx<br/>legs + CU + Jito tip<br/>min-out protected"]
    A --> S["simulateTransaction<br/>real CU · re-price"]
    S --> K["Risk engine<br/>limits · kill switch"]
    K --> P["PAPER fill"]
    K --> F["CONFIRM: press y"]
    K --> L["LIVE: sign → sendBundle"]
```

Every step emits an event: to the UI through a bounded channel (the UI can
never stall the engine) and to the recorder, which writes a compressed,
replayable log. Market data that needs no executable quote — pool mids, Pyth
prices, priority fees, Jito tips — comes from sources that cost no Jupiter
budget. Design decisions: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Configuration

Settings layer from built-in defaults → `config/mobius.toml` → your
`~/.config/mobius/config.toml` → command-line flags. Secrets never go in a
config file, only their variable names; values come from the environment or
`~/.config/mobius/.env`.

```toml
# ~/.config/mobius/config.toml — only what you change
[risk]
max_trade_lamports = 10000000                 # 0.01 SOL

[venues.okx]
markets = ["BTC-USDT", "ETH-USDT"]            # the Markets page cycles these
```

```bash
mobius-searcher --print-config   # every effective value and where it came from
```

Every section, venues and secrets: [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Workspace

| Crate | Purpose |
|---|---|
| `crates/core` | domain model, integer money, cost & profit engine, layered config, events (no IO) |
| `crates/telemetry` | service health, learned rate-limit window, 429 backoff, latency book, proxy discovery |
| `crates/market` | Solana JSON-RPC, chain WebSocket (slots, pool and Pyth accounts), hot path, network stats |
| `crates/jupiter` | Swap API V2 client and adapters |
| `crates/jito` | block engine client, tip policies |
| `crates/strategy` | round-trip, cross-DEX and triangular strategies, pricing |
| `crates/risk` | risk engine, kill switch |
| `crates/execution` | router, v0 assembly, simulation, pipeline, executors, wallet |
| `crates/storage` | SQLite recorder (compressed log, retention), replay, reports |
| `crates/tui` | Ratatui UI: pages, Markets view, charts, inspector |
| `apps/mobius-searcher` | the `mobius-searcher` binary, setup, `--doctor` |

## Documentation

| Document | What's covered |
|---|---|
| [INSTALL](docs/INSTALL.md) | install methods, platforms, file locations, terminals |
| [USAGE](docs/USAGE.md) | first run, modes, command line, pages and keys, replay |
| [CONFIGURATION](docs/CONFIGURATION.md) | layers, every section, venues, secrets |
| [ARCHITECTURE](docs/ARCHITECTURE.md) | design decisions, crates, runtime topology |
| [LATENCY](docs/LATENCY.md) | scheduling, rate limits, measured A/B results |
| [STORAGE](docs/STORAGE.md) | what is recorded, compression, retention |
| [PAPER_RUN](docs/PAPER_RUN.md) | the PAPER soak results in full |
| [RESEARCH](docs/RESEARCH.md) | API research behind the design |
| [LIVE_CHECKLIST](docs/LIVE_CHECKLIST.md) | everything that must be true before LIVE |
| [SECURITY](SECURITY.md) | secrets, keys, execution gates, reporting |
| [ROADMAP](ROADMAP.md) | current engineering priorities and research questions |
| [CONTRIBUTING](CONTRIBUTING.md) | development setup, benchmark format, PR expectations |
| [CITATION](CITATION.cff) | citation metadata for research and benchmark use |

## Development

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

```bash
mobius-searcher --replay latest --snapshot 120x40 --out shots/   # render pages to .txt/.html
```

CI builds and tests on Linux, macOS and Windows. Live smoke tests (Jupiter,
OKX) are `#[ignore]`d and run with `-- --ignored`.

## Roadmap

0.2 measures where an edge could come from (`--research`) and proves the
execution path with one loss-bounded, reconciled trade (`--canary`). 0.3 acts on
what the measurements show: on-chain order entry on Solana and EVM chains and a
strategy chosen from the data — or none, if no direction survives. OKX and
Binance stay market data (reference prices). See [ROADMAP.md](ROADMAP.md) and
the [CHANGELOG](CHANGELOG.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

<p align="center"><sub>MØBIUS-Searcher is research software. Trading can lose money. Use it at your own risk.</sub></p>
