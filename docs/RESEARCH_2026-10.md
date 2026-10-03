# Could a slow liquidator have won? — measurements, 2026-10-03

0.2 found no edge in taking prices ([RESEARCH_2026-09](RESEARCH_2026-09.md)).
The next candidate was liquidation: lending protocols pay a bonus of several
per cent to whoever closes an unhealthy position, hundreds of times the 1–2 bp
the swap measurements were about. The open question was never the size of the
bonus. It was whether a liquidator that learns of the chain's state from a
public node, a block late, is ever first.

`--research-liquidations 30` answers it from the chain. Nothing was traded.

**Summary: no.** On Morpho Blue on Base, over the 30 days to 2026-10-03,
95.3 % of the incentive was taken inside the 2-second block in which the
position became liquidatable, and the other 4.7 % in the block after. Everything
that waited two blocks or more was worth $26 in total.

## 1. What was measured

Every `Liquidate` event of Morpho Blue (`0xBBBB…FFCb`) on Base in blocks
50,833,011–52,129,447 (2026-09-03 to 2026-10-03): 830 events in 523
transactions, across 40 markets. For each one:

- **incentive**: `repaid × (LIF − 1)`, what the protocol pays the liquidator at
  its oracle price, with `LIF = min(1.15, 1 / (1 − 0.3 × (1 − LLTV)))`. Before
  gas, and before the seized collateral is sold. In dollars where the loan
  token is a dollar token or WETH: 802 of the 830 (the other 28 repaid jEUR,
  EURC or USDA, which are not valued here).
- **gas**: what the winning transaction paid, L2 gas plus the L1 data fee,
  shared between the liquidations in it.
- **available since**: the first block at whose end the position could be
  liquidated. This is asked of the contract itself: a `liquidate` call for one
  borrow share, as an `eth_call` at that block from an address that holds
  nothing. It reverts with `position is healthy`, or passes the health check
  and stops at the repayment transfer. Interest accrual and the oracle are
  the contract's own. The search steps back from the winner's block, doubling,
  for at most 8,192 blocks (4.5 hours).

All 830 were read without an error; 10 requests were repeated after a rate
limit. Raw report: [runs/liquidations-base-2026-10-03.txt](runs/liquidations-base-2026-10-03.txt)
(and `.json`).

## 2. How long each one had been available

| waited for its winner | events | share | incentive | share |
|---|---|---|---|---|
| inside the winner's block | 642 | 77.3 % | $142,478 | 95.3 % |
| 1 block (the next one) | 109 | 13.1 % | $7,016 | 4.7 % |
| 2–3 blocks | 29 | 3.5 % | $25 | 0.0 % |
| 4–10 blocks | 9 | 1.1 % | under $1 | 0.0 % |
| 11–100 blocks | 3 | 0.4 % | under $1 | 0.0 % |
| over 100 blocks | 38 | 4.6 % | $1 | 0.0 % |

"Inside the winner's block" means the position was healthy at the end of the
block before: the price moved and the liquidation landed within the same two
seconds. Every liquidation worth $10 or more (292 of them) was taken in the
same or the next block; the largest one that waited longer paid $3.68.

What a liquidator several blocks late would have found, counting only events
that paid more than the winner's own gas:

| waited at least | events in 30 days | median after gas | sum |
|---|---|---|---|
| 2 blocks (4 s) | 28 | $0.50 | $24 |
| 3 blocks (6 s) | 11 | $0.07 | $3 |
| 5 blocks (10 s) | 7 | $0.04 | $1 |
| 30 blocks (60 s) | 5 | $0.07 | $1 |

An upper bound: someone did take each of these, and a second liquidator would
have had to outbid them.

## 3. How much, how often, and who

- Incentive per event: median $2.19, 75th percentile $31, 95th $634, largest
  $11,274. Sum $149,520.
- It is a tail-event business. Four days (15–18 September) hold 81 % of the
  events and 91 % of the incentive, almost all of it one market: cbXRP
  collateral against USDC at 62.5 % LLTV ($128,917). On the other 24 days with
  any liquidation the median was 3 to 4 a day.
- 72 liquidator contracts, signed for by 179 accounts. The largest took 54 % of
  the incentive, the top three 88 %, the top five 94 %. Small ones are spread
  wider: in the $1–10 range the top three took 37 % of the events.
- Winners' gas: median $0.06, 95th percentile $5.53. As a share of the
  incentive: median 1.5 %, 95th percentile 24 %. Priority fees: median 0.05
  gwei, 95th percentile 2.7, largest 356, where the base fee in the one block
  checked was 0.005: where it mattered, it was bid for.

## Reading

The bonus is real and large, and it is gone before the block that creates it
is over. A liquidator has to see the oracle update while the block is still
being built and land behind it within a few hundred milliseconds. Polling a
public node shows a block after it is finished; from there the best case is
the next block, where 4.7 % of the money was, against liquidators already
running that race with priority fees.

So this direction is closed for a liquidator of this kind, and neither a live
watcher nor an executor will be built on it. The measurement stays: the
command is incremental and can be run again when the conditions change.

## Limits of the liquidation measurement

- One protocol on one chain for 30 days, 91 % of the value in four of them.
  A crash in another collateral would look different in size, not obviously
  in timing.
- Blocks, not milliseconds. Base builds its 2-second blocks in 200 ms steps;
  "inside the winner's block" does not say how many steps the winner needed.
- The incentive is at the oracle price. What a winner kept after selling
  thinly traded collateral is not measured.
- By Morpho's own API, Arbitrum had 20 liquidations worth about $22 over the
  same period and was left out; Ethereum had 127, worth about $56,000, and
  was not replayed.
- Liquidations through Morpho's pre-liquidation contracts do not emit this
  event and are not counted.

---

# What waiting costs a quote — first reading, 2026-10-03

`--report` now compares what every simulated leg was quoted with what it
returned, against the age of its quote when the simulation was sent. One
15-minute PAPER session on mainnet (no Jupiter key, public RPC, 0.1 SOL
cycles): 166 evaluations, 164 simulations, 314 executed legs.
Raw report: [runs/aging-paper-2026-10-03.txt](runs/aging-paper-2026-10-03.txt).

| quote age | legs | median | mean | below the quote |
|---|---|---|---|---|
| under 0.5 s | 99 | 0.00 bp | −0.10 bp | 32 % |
| 0.5–1 s | 181 | 0.00 | −0.07 | 23 % |
| 1–2 s | 18 | 0.00 | −0.38 | 28 % |
| 2–4 s | 16 | −0.11 | −1.13 | 50 % |

A straight line through the legs: +0.10 bp at age zero, −0.34 bp per second
of age, with a standard error of 0.25 on that slope.

| DEX of the leg | legs | median age | median |
|---|---|---|---|
| Whirlpool | 112 | 0.6 s | 0.00 bp |
| Meteora DLMM | 95 | 0.6 s | 0.00 |
| Raydium CLMM | 69 | 0.5 s | 0.00 |
| HumidiFi | 36 | 0.3 s | −0.80 |

**Reading, with the sample size in mind.** On the three pools whose state is
on chain, a quote under a second old executes at the quote: nothing is lost
at once, and what is lost grows with age. The slope is about a third of a
basis point per second but only 1.4 standard errors from zero: 314 legs do
not settle it. HumidiFi is different in kind: its legs were the youngest and
still returned 0.8 bp less than quoted at the median. That is not aging; it
is a quote the pool does not honour for this taker, and no amount of speed
recovers it.

Fifteen minutes on one afternoon. The table is in every session report from
now on, so longer sessions will say whether the slope is real.

---

# The gap between pools, without a quote API — 2026-10-03

`--research-pools` reads the three SOL/USDC pools (Orca Whirlpool, Raydium
CLMM, Meteora DLMM) with the tick and bin arrays around their price at one
slot, once a second, and works out with our own pool math what selling SOL on
one and buying it back on another returns. The math matches what the pool
programs pay in swaps simulated on mainnet: Whirlpool 19 of 19 samples and
DLMM 9 of 9 to the last unit, Raydium to the last unit inside a tick range
and within 6 parts in a billion across ticks.

One run of 38 minutes, 2,263 slots, no failed request.
Raw report: [runs/pools-2026-10-03.txt](runs/pools-2026-10-03.txt).

| size | best pair, median | 95th percentile | best seen | above one transaction's cost |
|---|---|---|---|---|
| 0.1 SOL | −2.49 bp | −1.45 bp | +0.38 bp | 0 of 2,263 (the cost is 0.6 bp) |
| 1 SOL | −3.21 | −1.98 | −0.94 | 0 |
| 10 SOL | −4.91 | −2.77 | −1.97 | 0 |

**Reading.** With the state taken from the chain and no quote API in the way,
the pools still never stood further apart than their own fees plus one
transaction: the best pair was positive before costs in 2 of 2,263 slots and
never after them. The −2 to −3 bp the Jupiter-quoted round trips showed is
therefore the market, not our latency. Being faster does not find a gap that
is not there. (A second run of 30 minutes over the same half hour saw a best
of +0.48 bp at 0.1 SOL, also below the cost.)

Thirty-eight minutes of one afternoon. A burst of volatility would look
different; the command is cheap to leave running.

---

# Resting orders, imagined — 2026-10-03

The same run keeps each snapshot of the Meteora pool with the exchange mid
(OKX and Binance best bid/ask) and imagines orders resting in its bins: base
token to sell above the price, quote token to buy below, a new pair every 5
seconds. An order is filled when the price goes through its whole bin within
60 seconds. Its result is the bin's price with the fee the bin's liquidity
earns, against the exchange mid after the fill; positive means the fill was
better than the exchange price then.

Thirty minutes, 1,762 slots, 316 orders of each kind.
Raw report: [runs/resting-orders-2026-10-03.txt](runs/resting-orders-2026-10-03.txt).

| order | filled | 0 s | 5 s | 15 s | 30 s |
|---|---|---|---|---|---|
| sell, 1 bin above | 45.6 % | −0.49 bp | −0.38 | −0.09 | +0.51 |
| sell, 2 bins above | 30.1 % | −0.24 | −0.07 | +0.24 | +0.86 |
| buy, 1 bin below | 43.0 % | −0.79 | −1.20 | −1.09 | −1.27 |
| buy, 2 bins below | 32.6 % | −0.97 | −0.88 | −1.29 | −1.57 |
| buy, 5 bins below | 12.0 % | −2.35 | −2.20 | −1.34 | −2.91 |

**Reading.** Right after a fill both sides are behind the exchange price,
with the fee already counted: the fill is the news. The price fell over this
half hour, so the buys kept losing and the sells recovered; that part is the
trend, not the maker. Whether the pool stood below or above the exchange when
an order went in could not be read: 3 and 13 orders fell outside the 2 bp
band. Not counted: partial fills, the two transactions that place and remove
the liquidity, and turning the filled token back.

Half an hour in one direction settles nothing. As a first reading it does not
show a maker's edge either.

---

# A model as the filter — 2026-10-03

Could a model look at what is known when a gap opens and pick the round trips
that pay, so that the quote and the simulation are spent only on those?
Tested on the 285 lag round trips of the two large September runs (233 of
them at a gap over the trigger), with `scripts/filter_compare.py`. "Pays" =
the quoted round trip covers its two transactions (1.2 bp at 0.1 SOL).
Ranking skill: 0.5 is chance, 1.0 is every paying one above every losing one;
the 95 % interval at this size is about ±0.07.

| | before asking Jupiter | after the entry quote |
|---|---|---|
| the gap alone (the rule in use) | 0.52 | 0.52 |
| logistic regression, trained on the other run | 0.51 | 0.69 |
| Laya 421M, zero-shot, three phrasings | 0.44 – 0.53 | 0.42 – 0.49 |
| random order | 0.44 | 0.49 |

Figures for the 233 round trips at a gap over the trigger.

- Before the quote nothing ranks them: not the model, not the regression,
  not the size of the gap. That step cannot be skipped by guessing.
- After the entry quote a six-coefficient regression does (its best third:
  +1.09 bp after the two transactions, against +0.04 for all), because the
  entry price is half of the answer. That is arithmetic more than
  prediction, and it is still within the 1.16 bp by which simulated outputs
  fell short of quotes.
- Laya is a text model for routing and rating; it was never shown market
  data, and here it does no better than chance with the decisive number in
  front of it. To the plain question "will this return more than it costs"
  it said yes to all 295 before the quote (87–100 % sure) and to 294 of them
  after it; 38 % did. About 50–70 ms a question on an M4.

Not tried: fine-tuning it (233 examples are too few for 421 million
parameters). Nothing here goes into the decision path.

---

# Who takes the arbitrage on these pools, and what it costs — 2026-10-03

Before paying for faster data, the prize was read back from the chain:
`scripts/arb_replay.py` lists every transaction that touched the three
SOL/USDC pools in one hour (to 19:05 UTC), reads a random 500 of those that
touched two or more of them and 500 of those that touched each pool alone,
and classifies each from its balance changes. An arbitrage is a transaction
in which at least two program-owned accounts swapped, nobody paid for what
came out, and what left the pools is positive. Figures per hour and per day
scale the sample up. Raw report: [runs/arb-replay-2026-10-03.txt](runs/arb-replay-2026-10-03.txt).

**What is on these pools.** About 72,000 transactions in the hour; some
22,000 failed. Of the 49,557 that succeeded, 87 % moved no token at all: a
searcher's program looked, found nothing and returned, paying its fee. Three
signers sent 93 % of those.

| | an hour | |
|---|---|---|
| transactions that succeeded | 49,557 | |
| … that moved no token | about 43,100 | $48 in fees, $1,150 a day |
| transactions that failed | about 22,300 | at least $320 a day at the base fee alone |
| arbitrage | about 1,280 | 76 in the sample |

**What an arbitrage is worth.** Taken from the pools: median $0.003, 90th
percentile $0.09, 99th $1.00. 78 % took less than one cent. Fee and tips ate
57–81 % of the median one, and 12 of the 58 on Meteora cost more than they
brought. The median arbitrage kept a tenth of a cent.

One of the 76 was different: 0.32 SOL ($39.65) through a memecoin pool, with
a $5.47 tip. With it, the sample scales to $13,100 a day taken and $10,600
kept; without it, to $1,050 a day taken. The day's figure is that one
transaction: this is a business of rare large gaps against the long tail of
other pools, not of the steady flow.

**Who.** On Meteora the largest signer kept 94 % of what was kept (the large
one was theirs); on the Whirlpool the top three kept 93 %, on Raydium 69 %.

**Between our three pools.** 2,343 successful transactions touched two or
more of them in the hour. Of 500 read, 498 moved nothing, one was someone's
swap, one was an arbitrage ($0.57 taken, $0.51 kept): about five an hour,
give or take a great deal. Four signers send those probes, some 39 a minute,
and pay about $59 a day for a cycle worth about $64 a day. This is the cycle
`--research-pools` computes, and the two agree: the gap is almost never
there, and others are already standing on it.

**Reading.** Leaving the quote API does not reach a prize that was hidden
behind it. Between these pools there is next to nothing, and it is watched
every slot. What money there is comes from rare gaps on pools our math does
not cover, goes almost entirely to whoever is first, and is paid for with
tens of thousands of transactions an hour that find nothing. Entering that
needs coverage of thousands of pools and a place among the first few
signers; faster data alone buys neither.

**Limits.** One hour; 500 transactions per group, so everything rare rests
on a handful of cases and the daily sums on one. A tip paid in another
transaction of the same bundle is not seen, so costs are a lower bound.
Program-owned accounts that swap are taken to be pools; fee collection and
liquidity changes fall under "other". The fees of failed transactions were
not read.

