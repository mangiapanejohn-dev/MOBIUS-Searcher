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
