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
  with swaps built by Jupiter against a copied Meteora SOL/USDC pool. Budget
  set aside (opening the wallet's USDC account), three buys and three sales,
  a stop of the program while holding and the same command going on, a stop
  at 0.01 % down that sold on its third try and ended the run, a close by
  hand while holding, a second copy refused. Every swap was confirmed and
  the account matched the wallet to the last unit of USDC; the wallet's
  other SOL never went down.
- **Not yet:** a swap sent the new way and confirmed on mainnet. What the
  local copy cannot show is how readily a public RPC node and Jito's
  `sendTransaction` get a transaction into a block.

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

## Stopping and going on

Ctrl-C stops the program and leaves what the run holds as it is; the same
command goes on from there. `--close` sells what is held and ends the run
(stop the running program first: one at a time).
A run that ended (by the stop or by `--close`) does not start again; a
changed file is a new run.

The journal (every line it printed about a swap) and the account are in
`research.sqlite` (`lab_journal`, `lab_state`, `lab_equity`);
`--lab-report` prints them.
