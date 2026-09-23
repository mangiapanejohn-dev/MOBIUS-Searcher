# Getting started

From nothing to a recorded research session. Fifteen minutes, no keys, no
money. Trading comes at the end, and only if you want it.

Chinese version: [GETTING_STARTED.zh-CN.md](GETTING_STARTED.zh-CN.md).

## 1. What you actually need

| | Needed for | How to get it |
|---|---|---|
| The binary | everything | [INSTALL.md](INSTALL.md) — one file, no runtime |
| A terminal ≥ 80×24 | the UI | any modern terminal |
| **Nothing else** | **PAPER and `--research`** | Jupiter answers without a key and the public Solana RPC is the default |
| Jupiter API key | 2× the quote rate | free, self-serve: [portal.jup.ag](https://portal.jup.ag) → API Keys → Free |
| Your own Solana RPC | fewer stalls, fresher data | any provider; put the URL in `SOLANA_RPC_URL` |
| A funded wallet | CONFIRM / LIVE / `--canary` only | your own hot wallet, see step 7 |

Rate limits, from [Jupiter's documentation](https://developers.jup.ag/docs/portal/rate-limits):
**keyless 0.5 requests/second**, **free key 1/second**, counted per
organisation in a 60-second sliding window. MØBIUS learns the window at
runtime and paces itself; it is the reason a quote is the scarce resource
here and why `--research` refuses to run while a trading session is running.

Nothing is sent anywhere except the providers you configure. The recording
stays in a local SQLite file.

## 2. First run

```bash
mobius-searcher
```

The first run opens a short setup with three paths:

| Path | What it does |
|---|---|
| **Research mode** | PAPER only: no private key, nothing can be sent. The recommended start. |
| **Assisted trading** | prepares a dedicated bot wallet and CONFIRM mode behind a typed unlock phrase. |
| **Advanced setup** | every provider, strategy, limit and execution option by hand. |

Pick **Research mode**. It asks for a wallet (choose *Create* for a watch-only
key, or *None*), an RPC (*Public* is fine to start), which strategies to scan
and how strict the limits are. Nothing is written until the final review, and
it tells you the two files it writes:

- `~/.config/mobius/config.toml` — your settings (only what differs from the defaults)
- `~/.config/mobius/.env` — secrets, `chmod 600`, names only ever appear in logs

Re-open it any time with `mobius-searcher --setup`.

## 3. Check everything is reachable

```bash
mobius-searcher --doctor
```

Every line is a real request: config layers, which secrets are set (names
only, never values), the proxy in use, Solana RPC and WebSocket, the pool and
oracle accounts, Jupiter, the Jito tip stream, each enabled venue, and — in
sending modes — the wallet. Fix the `FAIL` lines before going further;
`warn` lines are safe to ignore while you are only researching.

## 4. Add a Jupiter key (optional, 2 minutes)

Twice the quote rate, free:

1. Open [portal.jup.ag](https://portal.jup.ag), sign in, create an API key on the **Free** plan.
2. Put it in your `.env` (the file the setup created):

```bash
printf "JUPITER_API_KEY='paste-your-key'\n" >> ~/.config/mobius/.env
```

3. `mobius-searcher --doctor` should now show `set  JUPITER_API_KEY`, and the
   Jupiter line reports the wider window.

The key is read from the environment, then `~/.config/mobius/.env`, then
`./.env`. It is never written into `config.toml` and never printed.

## 5. Watch it work (PAPER)

```bash
mobius-searcher
```

PAPER prices every route from real quotes, builds a real transaction and
simulates it **on mainnet**, then records the verdict. It never signs
anything.

| Key | |
|---|---|
| `1`–`8` | pages: Overview · Markets · Opportunities · Graphs · Trades · Risk · System · Logs |
| `3` then `⏎` | one opportunity in full: the quotes, the costs, and why it was skipped |
| `T` | thresholds panel (see [USAGE.md](USAGE.md#thresholds)) |
| `K` | kill switch |
| `?` | all keys · `q` | quit |

Let it run for a while, then:

```bash
mobius-searcher --report latest
```

The report counts **every** evaluation, not only the good ones: how many
routes were quoted, how many were positive before and after costs, which
profit guard stopped them, what the simulations did, and what the quoted
outputs looked like against the executed ones.

```bash
mobius-searcher --replay latest
```

replays the whole session through the same UI.

## 6. Run the research

This is what 0.2 is for: measuring where an edge could come from, instead of
guessing.

```bash
mobius-searcher --research
```

It measures three things and writes them to `<data dir>/research.sqlite`:
a **size ladder** (the configured routes at 0.01–2 SOL), **cross-chain
spreads** (ETH and cbBTC on Solana vs Base/Arbitrum), and **DEX lag**
(on-chain pool prices against the OKX/Binance best bid/ask, with an
executable quote at each gap, control samples at random times, and the
on-chain round trip at +0/5/15/30 s). Nothing is signed or sent.

It holds the Jupiter budget while it runs — a trading session started at the
same time is refused, and the other way round — and on macOS it keeps the
machine awake (on mains power; on battery macOS still sleeps).

```bash
mobius-searcher --research-report all
```

Whole distributions, sample counts next to positive counts, and the caveats
printed with the numbers. What our own runs found:
[RESEARCH_2026-09.md](RESEARCH_2026-09.md) — short version: no direction paid
for its costs.

## 7. Only if you want to trade: money, slowly

Read [LIVE_CHECKLIST.md](LIVE_CHECKLIST.md) first. The order that matters:

1. **A dedicated hot wallet.** Never your main wallet. Fund it with what you
   are willing to lose, plus some USDC (a leg's input is fixed at the
   previous leg's quote; your USDC covers the difference).
2. **`mobius-searcher --canary`** — one real, loss-bounded trade
   (`canary.max_loss_lamports`, default 0.0005 SOL, also written into the
   transaction's on-chain minimum output), approved by pressing `y`, then
   reconciled account by account. It refuses to start if the wallet cannot
   fund it and tells you what is missing.
3. **CONFIRM** (`--mode confirm`): every transaction waits for your `y`.
4. **LIVE** (`--mode live`) with small limits, watching the Risk and System
   pages. `K` stops new submissions at any time.

By default a landed trade cannot lose money: the final leg's on-chain minimum
output covers the input, every cost and your minimum profit, so the
transaction reverts instead of filling badly. Turning that off needs you to
type `ALLOW LOSS`.

**Expect no trades.** With the thresholds at their defaults, a session that
finds nothing profitable simply does not trade — that is the intended
behaviour, and it is what our own LIVE session did: 817 opportunities, zero
transactions.

## Where things live

| | |
|---|---|
| Settings | `~/.config/mobius/config.toml` (`$MOBIUS_CONFIG` overrides) |
| Secrets | `~/.config/mobius/.env`, then `./.env` |
| Recording | `<data dir>/mobius.sqlite` |
| Research | `<data dir>/research.sqlite` |
| Canary reports | `<data dir>/canary/` |
| Data dir | `general.data_dir`, else `$MOBIUS_HOME/data`, else `$XDG_DATA_HOME/mobius`, else `~/.local/share/mobius` |

## When something looks wrong

| Symptom | What it means |
|---|---|
| `429` / rate limited | the Jupiter window is full. A free key doubles it; two MØBIUS processes cannot share it (the budget lock says so). |
| Everything is `EDGE_TOO_SMALL` | normal. The routes are not profitable after costs; `--report` shows which guard stopped them. |
| `INVENTORY_LOW` | the first leg delivered less than the next leg's fixed input and the wallet had no USDC to cover it. |
| `DEPOSIT_TOO_HIGH` | the route would create an account whose rent exceeds `profit.max_new_deposit_lamports` (capital, not a cost). |
| `NO_ROUTE`, timeouts, `Oracle is stale` | the provider's answer, recorded as an error rather than a price. |
| Nothing happens overnight | your machine slept. `--research` keeps it awake on mains power only. |
| "terminal too small" | the UI needs 80×24; `--headless` needs no terminal at all. |

Sources: [Jupiter rate limits](https://developers.jup.ag/docs/portal/rate-limits) ·
[Jupiter developer portal](https://portal.jup.ag)
