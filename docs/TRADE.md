# One lab rule with real money (`--trade`)

The [lab](LAB.md) tries rules that buy, hold and sell later, on paper.
`--trade FILE` runs **one** of those rules with a small budget from your
wallet: real SOL/USDC swaps on Solana.

**It can lose money, and nothing on chain prevents it.** An arbitrage
transaction ends where it began or fails; a position bought here is simply
held, and the price can fall. On a year of SOL every rule the lab ships lost
money ([LAB.md](LAB.md#what-the-shipped-rules-did-on-one-year)); the trained
model did not pass its test ([MODEL_2026-10.md](MODEL_2026-10.md)). This
exists to try a rule at a few dollars and see what it really does, not
because a rule was found that pays.

## What has to be true before anything is sent

All four, and each is yours to set:

1. the rules file has a `[live]` section with `acknowledge = "ALLOW LOSS"`;
2. your config has `execution.live_enabled = true`;
3. your config names a wallet keypair (`[wallet] keypair_path`);
4. you start it with `--trade`, and let ten seconds pass (Ctrl-C in them and
   nothing happens).

## Commands

```bash
mobius-searcher --lab-backtest config/trade.toml      # the rule on past candles, on paper, at this budget
mobius-searcher --trade config/trade.toml --dry-run   # build and simulate the first swap; signs and sends nothing
mobius-searcher --trade config/trade.toml             # real swaps, until Ctrl-C or the stop
mobius-searcher --trade config/trade.toml --close     # sell what the run holds and end it
mobius-searcher --lab-report                          # what it did, swap by swap
```

A backtest, a paper run (`--lab`) and a dry run of the file work without the
acknowledgement, and a dry run needs no keypair (only `[wallet] pubkey`).
`--duration N` stops a run after N seconds.

## The file

The lab's [rules file](LAB.md#the-rules-file) with exactly one `[[experiment]]`
and a `[live]` section ([config/trade.toml](../config/trade.toml)):

| Key | Meaning |
|---|---|
| `budget_usd` | USD the rule may use, 1 to 25. Set aside as USDC once, never topped up. |
| `stop_total_loss` | Share of the budget. When the budget is worth this much less, everything held is sold and the run ends for good. |
| `slippage_bps` | A swap filling more than this under its quote fails on chain instead (default 30, 1 to 300). |
| `acknowledge` | Must be `ALLOW LOSS`. |
| `dexes` | Optional: the only DEXes a swap may route through, by Jupiter's names, e.g. `["Whirlpool", "Meteora DLMM", "Raydium CLMM"]`. Left out: any. |
| `trigger` | Optional: `"close"` (the default: it decides when a bar closes) or `"price"` (it looks at the price every 2 seconds and acts at once). See [Acting on the price itself](#acting-on-the-price-itself). |
| `take_profit` | Optional: a share of what a buy cost, above 0 and at most 0.2. What it holds is sold as soon as the sale is certain to bring that much more than was paid. Left out: it sells at the rule's own price and its stop only. |

`[stops] daily_loss` works as on paper (nothing is bought for the rest of the
day). `[stops] total_loss` is refused: the total stop is the one in `[live]`.
`[costs]` matters only on paper; with real money the cost is what the wallet
shows.

A run is named after the file's content. Change anything and it is a new run
with a new budget; close the old one first.

## What it does

1. **Once:** if the wallet holds less USDC than the budget, it sells SOL for
   the difference. The fee reserve of your config and 0.003 SOL more must
   remain, or it sends nothing. USDC already there counts; when 95 % of the
   budget or more is there (what an earlier run left), that is the budget
   and nothing is swapped.
2. **At each bar's close** (OKX candles of `instrument`), in this order: a
   swap still open from before is settled from the wallet; if the budget's
   value is at or under the stop, everything is sold and the run ends;
   otherwise the rule decides as it does on paper, and its sells, then its
   buys, are sent.
3. **A swap** is quoted and built by Jupiter (without your API key, so it
   does not use a running session's budget), assembled with a priority fee
   and a small Jito tip, simulated, signed, and simulated again as signed. A
   quote that fails in simulation (some routes do not hold what they quote)
   is asked for again without that route's DEXes, three quotes at most; the
   journal names the routes that failed. The same goes for a route that in
   the simulation would cost the wallet anything beyond what it swaps and
   its fees. Nothing is sent if all three fail, or if in the simulation a buy
   does not arrive in the wallet as SOL in the amount quoted.
   It is then given a blockhash of your own RPC node and sent as an ordinary
   transaction, to that node and through Jito straight to the leader, again
   every two seconds until it is confirmed or its blockhash has expired
   (under a minute). The signature is the same every time, so it lands at
   most once. The journal has the signature and what became of it:
   confirmed, landed and failed, or expired.
4. **The account is what the wallet shows.** After a send the wallet's SOL
   and USDC are read; the difference is the fill, every fee inside. A swap
   that did not land changes nothing, and is given up only two minutes after
   it was sent (it cannot land after that); until then nothing else is sent.
   The state is written down before a swap is sent, so a program stopped in
   the middle reads the wallet on its next start.

It sells only SOL it bought and spends only its own USDC. A bar seen more
than a third of a bar late (the machine slept) is not acted on. One `--trade`
runs at a time; a second is refused.

## Acting on the price itself

A rule decides at the close of each bar, as it does on paper and in a
backtest. With `trigger = "price"` (or `t` on the Bots page, which changes a
bot that runs within seconds and writes nothing into its file) it does not
wait for the close:

* **Every 2 seconds** it reads the exchange's best bid and ask and decides on
  the bar that is forming as if it closed at the price between them: the
  same average and deviation, with that price as the newest close. Under its
  buy price it buys at once; over its sale price, or under its stop, it
  sells at once. The total stop is measured at the bid, every look.
* **A gain that is there is taken** when `take_profit` is set (`t` sets it
  to 0.001, a thousandth): while it holds SOL, as soon as selling at the
  bid would bring `take_profit` more than the buy cost even at the swap's
  on-chain minimum (`slippage_bps` under its quote), it asks for the sale
  and sends it only if the quote's minimum output is that much. A sale sent
  this way lands for more than was paid or fails on chain; it is never a
  sale at a loss. With the default 30 bp tolerance that is a price about
  0.4 % over what the buy cost.
* **What it has just sold it does not buy straight back.** For one bar's
  length after a sale at a loss it buys nothing. After a sale at a gain it
  buys again only under a price from which the price it sold at would be a
  gain to take again; at the same price it would only pay for two swaps.
* **A try that sends nothing** (no quote, a quote under the floor, a route
  that fails in simulation) is tried again ten seconds later, and said in
  its record once, not every time.

Every look is written down (`<data dir>/<run>.looks`, the last 120) and shown
on the Bots page as it comes: the time, the price it saw, the price it
measured that against, how far it was, and what came of it.

What this is not: it has **not been backtested** (a backtest acts at
closes), it trades more often and each trade has its cost, and a price that
only spikes for a second moves it. `take_profit` holds a *sale* to a gain;
it does not make the price come back after a buy. Under what it paid, the
rule holds until its own sale price or its stop.

## What a swap costs

Signature 5,000 lamports, priority fee 2,000 to 7,000 (what Jupiter
suggests, within those), tip 1,000 to 4,000 (the going rate, capped): at most
16,000 lamports, about 0.002 USD, whatever the size. On a 2 USD trade that is
up to 10 bp a side, plus about 1 bp lost to the route. A sale keeps 16,000
lamports of its lot back to pay for itself.

A swap that lands and fails (the price moved past `slippage_bps` before it
was included) swaps nothing and still pays its signature and priority fee,
from the wallet.

On paper, with these costs, at 2 USD, over the year to 2026-10-04, `dip-3d`
returned −45.3 % (holding SOL: −46.8 %) in 107 trades; the costs alone were
20.4 % of the budget, and its deepest fall was 61.2 %: a 50 % stop would
have ended it.

## What it cannot do

- **Act while the machine sleeps or is off.** It keeps the machine from idle
  sleep while it runs; a closed lid or a lost network stops it, the stop
  included. What it holds then stays held.
- **Act between bars.** The stop is looked at when a bar closes. A fall
  inside a bar is seen at its end; the loss can be larger than the stop says.
- **Make a swap land.** It is sent for about a minute; in a crowded minute
  it may still expire. The rule's order is then sent again only if the rule
  still wants it at the next bar; the stop's sale is sent again every bar
  until it lands.
- **Hide a swap.** An ordinary transaction can be seen before it lands. What
  anyone can take from it is bounded by `slippage_bps`.
- **Tell its swaps from another program's in the same seconds.** It reads
  the wallet before and after. A session trading the same wallet at the same
  moment would blur both readings (and the session would count this run's
  swap as its own result). Arbitrage leaves USDC untouched and its SOL
  changes are small, so the effect here is small; the funding swap is the
  large one.
- **Trade the price it watches.** The rule reads OKX's SOL-USDT candles; the
  swap is SOL/USDC on Solana at whatever Jupiter routes.

When a run ends its USDC stays in the wallet as USDC.

## What has been tried, and what has not

As of 2026-10-04:

- **On mainnet, read-only:** both directions of the swap built and simulated
  (the buy arrives as SOL in the wallet, the quote less its fees); a fresh
  transaction's status read as confirmed.
- **On mainnet, for real:** the first run sent its funding swap as a single
  Jito bundle at bar after bar (five by the time this was written, tips of
  1,721 to 10,000 lamports). None landed: a
  bundle sent once lives only until the next Jito leader, and two minutes
  later the block engine no longer knew them. The wallet was unchanged and
  nothing was lost; this is why swaps are now ordinary transactions.
- **On a local copy of mainnet, with test SOL** (below): the whole runner,
  with swaps built by Jupiter against a copied Meteora SOL/USDC pool, debug
  and release builds. Budget set aside (opening the wallet's USDC account),
  five buys and five sales,
  a stop of the program while holding and the same command going on, a stop
  at 0.01 % down that sold on its third try and ended the run, a close by
  hand while holding, a second copy refused. Every swap was confirmed and
  the account matched the wallet to the last unit of USDC; the wallet's
  other SOL never went down.
- **On mainnet, for real, the new way** (2026-10-05): the funding swap,
  0.016696 SOL for 2.005052 USDC, was in a block within ten seconds of the
  countdown's end; it cost 7,000 lamports of fees and a tip of 1,968. The
  run's account and the wallet agree.
- **On mainnet, for real, since** (to 2026-10-06, the day of 0.4.0): four
  swaps in all (2 to 15 USD), each confirmed: two budgets set aside, one
  buy by a rule, and one budget raised by hand from the Bots page.
- **Not yet on mainnet:** a sale by a rule (no trade has closed, so there is
  no result to report); a rule acting on the price itself (run with the
  real program on a local chain, where it had no venue to swap on) and a
  sale that takes a gain (tested against a mock venue only); and a busy
  hour: whether a public RPC node still gets a transaction into a block
  then.

## Trying it without money

```bash
python3 scripts/trade_fork.py /tmp/mobius-fork
```

makes a throwaway key, a config that points only at a local
`solana-test-validator`, and a rules file that trades every few minutes, and
prints three commands: the validator (with Jupiter's program and one DEX's
SOL/USDC pools copied from mainnet as they are now), test SOL for the
wallet, and the runner. Everything the runner does with real money it then
does there. The copy is frozen while Jupiter quotes the live pools: now and
then a quote names an account that was not copied, the runner says so and
tries again at the next bar; after an hour or two make a new copy.

The sending alone, against a plain local validator:

```bash
solana-test-validator --rpc-port 18899 --faucet-port 19900 --ledger /tmp/mobius-ledger
MOBIUS_TEST_VALIDATOR=http://127.0.0.1:18899 cargo test -p mobius-searcher --lib on_a_local_validator -- --ignored --nocapture
```

## From the terminal UI

Page `9` (Bots) of the terminal UI lists every real run and the rules files
of your config directory that have not run yet; `p` adds the newest paper
run, which is a simulated account and is not shown until asked for. `n`
makes a new bot there without writing a file by hand: the kind of rule (the
average of one day or of three), `k`, the stop of a trade, the budget and
the total stop, and the words `ALLOW LOSS` typed by you; it writes
`trade-<name>.toml` into the config directory and starts nothing. For
the one selected it says in words what it holds and what it waits for
("Waiting to buy: when a 15m bar closes under 120.93, 0.15 % below the price
now"), and shows it three ways: a ruler with the price now between the two
prices that matter (its buy and its sell price while it waits; what its SOL
cost and its sell price while it holds); the exchange's live candles of its
market with those prices drawn across and its buys (`▲`) and sells (`▼`)
marked; and the sums the prices come from ("buy price = average − 1 ×
deviation = 120.93"). Then what it did. The header of every page has a word
on the real bot and counts the wallet's USDC into what the wallet is worth.
The page is in Chinese when the system language is (or with `--lang zh`).

`s` starts the selected bot, `x` stops it, `c` sells what it holds and ends
it; each asks first and only `y` does it. Starting is the same `--trade
FILE`, as a program of its own in the background: it goes on when the window
is closed, and what it prints goes to `<run>.log` in the data directory. The
file still has to carry `acknowledge = "ALLOW LOSS"`, and the config
`execution.live_enabled = true`; the page starts nothing without them. A bot
started by a version before the page existed shows as running and has to be
stopped once in its own window.

### How it acts: at a close, or on the price

`t` on the Bots page switches the selected bot between deciding at each
bar's close and [acting on the price itself](#acting-on-the-price-itself)
(with a gain of a thousandth taken when it is there), after saying what
that means and a `y`. A bot that runs changes within seconds, a stopped one
when it is started; its record says when it did. A bot started by a version
before this one has to be stopped (`x`) and started (`s`) once first. A bot
made with `n` acts on the price unless that is changed in the form.

### Changing its budget

`b` on the Bots page changes the USD the selected bot may use, between 1 and
25, while it runs or while it is stopped. The form shows what it has, what
the wallet holds free, and what the number you write would do; `⏎` asks once
more and `y` leaves the change for the bot's program, which acts on it
within seconds (a stopped one when it is started).

* **Raised**: the difference is taken from USDC the wallet holds beyond the
  bot's own. What is missing is bought by selling SOL, a real swap with its
  fee; never the SOL the bot itself holds, nor the SOL kept for fees and
  rent. If the wallet has not enough, the budget is not changed and the
  bot's record says why.
* **Lowered**: the difference stops being the bot's and stays in the wallet
  as USDC; nothing is swapped. It can only give what it holds as USDC: while
  it is in SOL the change waits until it has sold.

What the run has made or lost so far stays as it is (a budget of 2 worth
1.99 raised to 5 is worth 4.99), and the value at which everything is sold
follows the budget (`stop_total_loss` of the new one). While a change has
not been made yet the page says so; writing the budget it has now takes the
change back. A swap that does not go through is tried again after each bar.
The rules file is not rewritten: a budget changed by hand belongs to the
run, and a changed file is still a new run.

## Stopping and going on

Ctrl-C (or `x` on the Bots page) stops the program and leaves what the run
holds as it is; the same command (or `s`) goes on from there. `--close` sells what is held and ends the run
(stop the running program first: one at a time).
A run that ended (by the stop or by `--close`) does not start again; a
changed file is a new run.

The journal (every line it printed about a swap) and the account are in
`research.sqlite` (`lab_journal`, `lab_state`, `lab_equity`);
`--lab-report` prints them.
