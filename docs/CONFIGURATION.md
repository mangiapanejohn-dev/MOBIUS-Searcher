# Configuration

MØBIUS reads TOML in layers; a later layer wins.

| Layer | Where | Holds |
|---|---|---|
| built-in | `crates/core/src/config.rs` | every default (identical to `config/mobius.toml`, enforced by a test) |
| shared | `config/mobius.toml` (checked in) | generic defaults that work for anyone: PAPER, keyless public endpoints, no wallet |
| yours | `~/.config/mobius/config.toml` (or `--config PATH`, or `$MOBIUS_CONFIG`) | only what you change — `--setup` writes it |
| flags | `--mode`, `--scheduler`, `--glyphs`, … | this run only |

Tables merge key by key, so `[venues.okx] markets = ["BTC-USDT"]` in your file
changes only that key. Arrays (strategies, pools, oracles) replace as a whole.
`$MOBIUS_HOME` moves the whole per-user directory.

```bash
mobius-searcher --print-config    # every effective value and the layer it came from
```

```bash
mobius-searcher --doctor          # config, secrets (names only), proxy, every endpoint and venue
```

`--doctor --mode live` (or `confirm`) also checks that the keypair loads,
matches `wallet.pubkey`, and that the wallet balance covers
`risk.min_wallet_sol_for_fees_lamports`.

## Secrets

Secrets never go in a config file — only the **names** of environment
variables. Values are read from, in order (the first that sets a name wins):

1. the process environment
2. `~/.config/mobius/.env` (written by `--setup`, `chmod 600`)
3. `./.env` in the working directory

| Variable | Used for | Needed? |
|---|---|---|
| `JUPITER_API_KEY` | Jupiter Swap API (quotes and swap instructions) | optional — keyless works at a lower rate |
| `SOLANA_RPC_URL`, `SOLANA_WS_URL` | a keyed RPC provider (full URLs) | optional — the public endpoint is the default |
| `JITO_UUID` | higher Jito block-engine limits | optional |
| `PYTH_API_KEY` | Pyth Hermes (only with `feeds.oracle_source = "hermes"`) | optional |
| `OKX_API_KEY`, `OKX_API_SECRET`, `OKX_API_PASSPHRASE` | OKX trading (not implemented yet; market data needs no key) | no |

The private key of the bot wallet is a **file** (`wallet.keypair_path`,
`chmod 600`), never an environment variable. PAPER never reads it.

## Venues

Markets are tables under `[venues.<name>]`, named as you like, each with a
connector `kind`, endpoints, credential variable names and instruments.

```toml
[venues.okx]
kind = "okx"
enabled = true
rest_url = "https://www.okx.com"      # regional hosts (e.g. my.okx.com) work too
markets = ["SOL-USDT", "SOL-USDC", "SOL-USD"]   # the Markets page cycles these (p)
watchlist = ["BTC-USDT", "ETH-USDT", "SOL-USDT"] # the ticker strip
trading = false                       # market data only
```

A second host or account is another block:

```toml
[venues.okx_eu]
kind = "okx"
rest_url = "https://my.okx.com"
markets = ["ETH-EUR"]
api_key_env = "OKX_EU_API_KEY"
```

Only kinds with a connector in the build are accepted — today `okx`, for
market data; `trading = true` is refused until its order connector exists.
The Solana stack still uses the top-level sections below and moves under
`[venues.solana]` later.

## Sections

| Section | What it controls | Keys you are most likely to change |
|---|---|---|
| `[general]` | mode and where data lives | `mode` (`paper` \| `confirm` \| `live`), `data_dir` (empty = per-user default) |
| `[venues.solana.wallet]` | the bot wallet | `pubkey`, `keypair_path` (CONFIRM/LIVE only) |
| `[execution]` | the hard gate for sending | `live_enabled` (must be `true` for CONFIRM and LIVE) |
| `[risk]` | limits checked before anything is sent | `max_trade_lamports`, `max_trade_pct_of_equity_bps`, `max_daily_loss_usd`, `min_wallet_sol_for_fees_lamports`, `max_consecutive_failures` |
| `[profit]` | what counts as profitable after costs | `min_profit_lamports`, `min_profit_bps`, `min_profit_usd`, `protect_min_out`, `max_new_deposit_lamports` (rent a trade may lock in accounts it leaves created: capital, not a cost) |
| `[strategies]` | routes to evaluate: `round_trip`, `cross_dex`, `triangular` | `amount_lamports`, `dexes`, `cycle`, `enabled`, `weight` |
| `[venues.solana.jupiter]` | Jupiter Swap API V2 | `api_key_env`, `slippage`, `for_jito_bundle` |
| `[venues.solana.rpc]` | Solana JSON-RPC and WebSocket | `url`, `ws_url` (or the `*_env` variables) |
| `[venues.solana.jito]` | tips and bundles | `block_engine_url`, tip policy |
| `[venues.solana.feeds]` | real-time data that costs no Jupiter budget (pool and Pyth accounts, fees, tips) | `enabled`, `pools`, `oracles`, `oracle_source` |
| `[scheduler]` | how the Jupiter budget is spent ([LATENCY.md](LATENCY.md)) | `kind` (`event` \| `round_robin`), `window_capacity`, `window_ms` |
| `[paper]` | PAPER simulation | `equity_lamports`, `simulation_taker` |
| `[venues.*]` | other markets (above) | `markets`, `watchlist`, `rest_url` |
| `[network]` | proxy for every connection | `proxy` = `"auto"` (environment, else the macOS system proxy) \| `"none"` \| `"http://host:port"` |
| `[storage]` | the recording database ([STORAGE.md](STORAGE.md)) | `retention_days`, `keep_trading_days`, `max_db_mb`, `prune_interval_min` |
| `[research]` | `--research` measurements | `ladder_sizes_lamports`, `xchain_assets`, `lag_trigger_bps`, `lag_control_every_s`, `keep_awake` |
| `[canary]` | `--canary`, the first real trade | `max_loss_lamports` (also the on-chain bound), `amount_lamports` |
| `[ui]` | terminal UI | `glyphs`, `color`, `fps`, `mouse`, `max_graphs` |

Run `--print-config` for every key and its current value.

## Your file, for example

```toml
# ~/.config/mobius/config.toml — only what differs from the defaults
[wallet]
pubkey = "<bot hot wallet public key>"
keypair_path = "/path/to/bot-hot-wallet.json"   # chmod 600

[risk]
max_trade_lamports = 10000000                   # 0.01 SOL

[venues.okx]
markets = ["BTC-USDT", "ETH-USDT"]              # only this key changes
```

Before enabling CONFIRM or LIVE, read [LIVE_CHECKLIST.md](LIVE_CHECKLIST.md).

## The Solana sections moved under `[venues.solana]`

Since 0.2 the Solana stack's settings sit next to the other venues:
`[venues.solana.rpc]`, `[venues.solana.jupiter]`, `[venues.solana.jito]`,
`[venues.solana.feeds]` and `[venues.solana.wallet]`. Files that still use
the old top-level `[rpc]`, `[jupiter]`, … are read as before until 0.4, with
a note at startup. To move yours:

```bash
mobius-searcher --migrate-config
```

It prints the new file, checks that it loads to exactly the same effective
configuration, and writes it only after you type `yes`, keeping comments and
the previous file as `.bak`. A section set in both places is refused.

## Secrets and accounts

Secrets are read from the process environment, then `~/.config/mobius/.env`,
then `./.env`. The configuration holds the *names* of these variables, never
their values, and nothing here is printed to a log or a report.

| Variable | Needed for | Where it comes from |
|---|---|---|
| `JUPITER_API_KEY` | doubles the quote rate (keyless 0.5/s → 1/s) | free, self-serve at [portal.jup.ag](https://portal.jup.ag) |
| `SOLANA_RPC_URL`, `SOLANA_WS_URL` | your own RPC instead of the public one | any Solana RPC provider |
| `JITO_UUID` | optional, a Jito account's bundle quota | Jito |
| `PYTH_API_KEY` | only with `oracle_source = "hermes"` | Pyth Hermes |
| `ETHEREUM_RPC_URL`, `BASE_RPC_URL`, `ARBITRUM_RPC_URL` | cross-chain research and `--quote` on EVM venues | any EVM RPC provider (public nodes are the default) |
| `OKX_API_KEY`, `OKX_API_SECRET`, `OKX_API_PASSPHRASE` | only for a signed OKX read; market data needs none | OKX |
| `BINANCE_API_KEY`, `BINANCE_API_SECRET` | same, for Binance | Binance |

**None of these let anything be sent.** Signing needs
`[venues.solana.wallet] keypair_path` *and* `execution.live_enabled = true`
*and* a sending mode — see [LIVE_CHECKLIST.md](LIVE_CHECKLIST.md).

`mobius-searcher --doctor` prints which of these are set (names only) and
tests every endpoint they configure.
