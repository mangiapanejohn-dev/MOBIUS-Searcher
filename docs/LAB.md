# The lab: rules that hold a position, on paper

Everything else in MØBIUS trades inside one transaction and ends where it
began. The lab is for the other kind of rule: buy, hold, sell later ("buy low,
sell high"). Such a rule can lose money, so it lives here, on paper, where it
can be watched for as long as it takes to know what it does.

**Nothing in the lab is signed or sent.** It makes no Jupiter request and does
not touch the trading database, so it can run beside a session. Candles and the
best bid and ask come from OKX's public API; results go to `research.sqlite`.

It is a measuring tool, not a strategy. On one year of SOL none of the rules it
ships with made money ([below](#what-the-shipped-rules-did-on-one-year)), and
the research behind it found no rule of this kind with out-of-sample evidence
that survives costs at this size.

## Three commands

```bash
mobius-searcher --lab-backtest config/lab.toml            # the last 365 days of candles
mobius-searcher --lab-backtest config/lab.toml --days 90
mobius-searcher --lab config/lab.toml                     # live prices, bar by bar, until Ctrl-C
mobius-searcher --lab-report                              # what the live runs did so far
```

`--json` gives the same numbers as JSON; `--duration N` stops a live run after
N seconds.

## The rules file

```toml
instrument = "SOL-USDT"   # OKX spot instrument: candles for the signals, bid/ask for live fills
bar = "15m"               # 1m 5m 15m 30m 1h 4h
capital_usd = 23.0        # the paper account, starting in USDC

[costs]
route_bps = 1.15          # lost to the route, each side, basis points of the trade
fixed_fee_lamports = 6366 # network fee, priority fee and tip of one transaction

[stops]                   # shares of the capital; leave out for none
daily_loss = 0.01         # down this much since the UTC day began: nothing is bought until the next day
total_loss = 0.02         # down this much since the start: nothing is bought again

[[experiment]]
name = "dip-3d"
rule = "dip"
window = 288
k = 2.0
```

Four kinds of rule, each decided at a bar's close:

| `rule` | What it does | Settings |
|---|---|---|
| `sign-reversal` | In for the bar after a bar that closed down, out after one that did not. | none |
| `dip` | In when the close is `k` deviations under its average of `window` bars; out when it is back at `exit_z` deviations (default 0), or `stop` under the buy price. | `window`, `k`, `exit_z`, `stop` |
| `grid` | One of `lots` equal parts bought each time the close is `step` under the last trade; each part sold `step` above its own buy. | `step`, `lots` |
| `breakout` | In on a close above the high of the `entry` bars before; out on a close under the low of the `exit` bars before. | `entry`, `exit` |

| `model` | A trained model ([below](#a-trained-model-as-a-rule)): in when it gives a rise at least its threshold, out its horizon after the last time it did. | `file` |

A misspelt setting or an unknown rule is refused, with the experiment named.

**A live run is named after the file's content.** Start `--lab` again with the
same file and it goes on where it stopped; change anything and it is a new
run, the old one stays as it was. That is on purpose: a rule tuned after
seeing its results is a different rule, and its old results are not its own.

## How a fill is made

- **Backtest:** a signal at a bar's close is filled at the next bar's open.
  That is an assumption: nobody is guaranteed that price.
- **Live:** when a bar closes, the lab asks OKX for the best bid and ask and
  buys at the ask, sells at the bid. If it sees the close more than a third of
  a bar late (the machine slept), the bar is recorded and **not** acted on:
  no fill is ever written at a price that had passed.
- **Costs** are taken from every fill, both sides: `route_bps` of the trade
  and the fixed fee of one transaction. The fixed part does not shrink with
  the trade: at the defaults a 12 USD trade pays 1.8 bp a side, a 2.30 USD
  trade 4.5 bp. The defaults were measured on mainnet on 2026-10-04 at 0.1 SOL
  (a SOL → USDC → SOL round trip quoted 2.29 bp under its input at the median
  of 3,707 simulated round trips; a transaction cost 6,366 lamports at the
  median).
- The pair is OKX `SOL-USDT` by default because OKX's `SOL-USDC` trades too
  rarely for short bars to move. A real trade would be SOL/USDC on chain: the
  USDT/USDC difference and the gap between OKX and the pools are not in the
  numbers.

## How to read the report

| Column | Meaning |
|---|---|
| `return` | Final value over the capital. Lots still held count at the last close, less the cost of selling them. |
| `max fall` | Deepest fall of the equity from an earlier high, marked at every bar. |
| `in SOL` | Average share of the account held in SOL. |
| `same SOL` | What a portfolio always holding that share, never trading, returned over the same bars. |
| `rule adds` | `return` less `same SOL`: what the rule did beyond carrying that much SOL. |
| `… a day [95 %]` | That difference per day, with an interval from resampling days. **An interval that includes zero means these bars do not tell the rule from simply holding that much SOL.** |
| `net a closed trade [95 %]` | After every cost, over what the trade paid in. Closed trades only: a grid's closed trades all win, its losses sit in the lots it has not sold (the line under the table says what those are worth). |
| `costs` | Everything paid in costs, over the capital. |
| `worst month` | The worst UTC month. Every month is listed under the table. |

Why `same SOL` matters: a rule that holds SOL a third of the time loses a third
as much as SOL in a falling year and gains a third as much in a rising one.
That is not the rule working; it is the rule holding less. `rule adds` takes
that part out.

Two things no column can show. One path of one coin proves little: a year
that falls and comes back flatters every rule that buys on the way down, and a
year that only falls punishes it. And several experiments on the same bars
make the best one look better than it is; none of the intervals knows the
others were tried.

## What the shipped rules did on one year

`config/lab.toml` over 2025-10-04 to 2026-10-04 (35,039 bars of 15 minutes;
SOL fell 46.3 %), 23 USD, default costs
([the whole report](runs/lab-backtest-2026-10-04.txt)):

| Experiment | Return | Max fall | In SOL | Same SOL | Rule adds | … a day [95 %] | Trades |
|---|---|---|---|---|---|---|---|
| `reversal` | −100.0 % | −100.0 % | 42 % | −18.3 % | −81.7 % | −195.8 bp [−276.2, −138.4] | 7,840 |
| `dip-3d` | −26.0 % | −54.8 % | 31 % | −13.3 % | −12.8 % | −1.8 bp [−21.6, +16.5] | 105 |
| `grid-1pct` | −29.3 % | −62.0 % | 80 % | −36.8 % | +7.5 % | +3.2 bp [−1.9, +9.1] | 73 |
| `breakout-1d` | −15.8 % | −53.2 % | 36 % | −15.3 % | −0.5 % | +1.2 bp [−14.9, +18.3] | 128 |

All four lost money. The reversal rule trades 21 times a day for a gross gain
of about half a basis point a trade and pays three for it. For the other
three the daily interval includes zero: over this year they cannot be told
from holding their average share of SOL. The grid's 73 closed trades all won;
its ten unsold lots were worth 39.7 % less than they cost.

The numbers were checked against a second implementation written separately
(Python, the same cached candles): return, deepest fall and both halves of the
year agree to the last printed digit for the dip, grid and breakout rules.

## A trained model as a rule

`scripts/direction_model.py` trains a model of SOL's next move and writes it
to a file the lab can run:

```bash
python3 scripts/direction_model.py fetch     # SOL and BTC 15-minute candles since 2021, from OKX
python3 scripts/direction_model.py train     # writes data/direction-model.json
```

```toml
[[experiment]]
name = "model"
rule = "model"
file = "docs/runs/direction-model-2026-10-04.json"   # the one trained on 2026-10-04; as written, or beside the rules file
```

- The model sees 26 numbers at each bar's close, all from that bar and the
  ones before it: SOL's return over six lookbacks, how far the close is from
  three averages and inside two ranges, volatility and volume against their
  own past, the bar's shape, BTC's returns and SOL's against them, the hour
  and the weekday, the run of down bars. A check in the script changes the
  future and requires the past's features to stay as they were.
- Two kinds are fitted, a logistic regression and boosted trees, for three
  horizons (1 hour, 4 hours, 1 day): six configurations, all reported. Each
  is trained on two years and tested on the three months after, rolling
  forward; the last six months are kept aside and looked at once, for the one
  configuration the rolling tests chose.
- The rule a model makes: in when its probability of a rise is in the top
  fifth of what it gave in training (and above a half), out `horizon` bars
  after the last such bar. The lab's costs.
- The bar it has to clear is fixed before the look: on the months kept aside,
  the interval of `… a day` above zero, and two thirds of the rolling tests
  positive. The file says whether it passed.
- In the lab a model **stays out on every day it was trained on** (it has
  seen those answers), so a backtest only counts the bars after. The lab
  builds the same 26 numbers in Rust; they are checked against the Python on
  a fixture of real bars, as are both kinds of model.

A model rule needs `instrument = "SOL-USDT"` and `bar = "15m"` (what it was
trained on); the lab then also keeps BTC-USDT candles.

What the trained model did is in [MODEL_2026-10.md](MODEL_2026-10.md).

## What it does not do

- It does not trade, and there is no switch that makes it trade.
- It does not model a queue: a resting order is not simulated, only taking
  the other side of the book.
- A backtest on candles cannot know the order of events inside a bar.
- Live fills use the exchange's book, not a route quoted on chain.
- It reads price only. Funding, open interest, flows are not inputs.

## Where things are kept

`research.sqlite` in the data directory: `lab_bars` (the candles, so a second
backtest does not download them again), `lab_runs` (each rules file as it
was), `lab_state` (each experiment's account after its last bar), `lab_fills`
(every paper fill with the bid and ask it met), `lab_equity` (the account at
every bar).
