# Where an edge could come from — measurements, 2026-09-22/23

`--research` ran for 5.4 awake hours on a Mac on a home connection, through a
Jupiter API key and public Solana/EVM RPCs: 10,889 Jupiter requests, **0
rate-limited**. Six runs, all recorded in `<data dir>/research.sqlite`; the
report below is `--research-report all` with the exclusions named under each
table. Nothing was traded: these are quotes and on-chain prices.

**Summary: none of the three directions pays for its costs at these sizes on
this infrastructure.** The DEX-lag signal is real — pool prices do revert
toward the exchange price — but what is left after fees is around zero
before the quote-to-fill gap, and negative after it.

## 1. Size ladder: does a bigger trade change the edge?

SOL → USDC → SOL round trips, quoted at seven sizes in turn, after the base
fee, the minimum Jito tip and the provider's priority-fee estimate.

| size | samples | positive | median | mean |
|---|---|---|---|---|
| 0.01 SOL | 269 | 0 % | −8.54 bp | −9.70 bp |
| 0.05 SOL | 265 | 4 % | −3.61 | −4.61 |
| 0.1 SOL | 265 | 5 % | −3.16 | −3.88 |
| 0.25 SOL | 267 | 7 % | −3.16 | −4.25 |
| 0.5 SOL | 266 | 7 % | −3.13 | −3.91 |
| 1 SOL | 264 | 7 % | −3.41 | −4.27 |
| 2 SOL | 268 | 10 % | −4.01 | −4.50 |

Below 0.05 SOL the fixed costs dominate. Above it the curve is flat at about
−3 to −4 bp and turns worse again at 2 SOL: **size does not buy an edge.**

## 2. Cross-chain: the same asset on Solana and on an EVM chain

Buy an asset for N USDC on Solana (Jupiter), price selling and buying that
quantity on Base and Arbitrum (Uniswap v3 QuoterV2). A = buy on Solana, sell
on the chain; B = the other way. After swap fees, Solana fees and gas, before
any cost of moving inventory between chains. Quotes more than 500 bp from the
Solana buy price are excluded as bad quotes (see *Data quality*).

| market | A median | A positive | B median | B positive |
|---|---|---|---|---|
| ETH · Arbitrum · $25 | −9.97 bp | 0 % | −7.60 bp | 2 % |
| ETH · Arbitrum · $250 | −6.65 | 1 % | −4.92 | 5 % |
| ETH · Base · $25 | −7.91 | 1 % | −4.97 | 6 % |
| ETH · Base · $250 | −6.68 | 1 % | −4.58 | 9 % |
| cbBTC · Base · $25 | −6.61 | 0 % | −6.19 | 2 % |
| cbBTC · Base · $250 | −5.57 | 4 % | −5.40 | 6 % |

About 450 samples per cell. The best cell is 9 % positive and its median is
−4.6 bp, and bridging is not yet counted. Uniswap v3 is the only EVM venue
asked; a better EVM price would move these numbers, not their sign.

## 3. DEX lag against the exchange price

Pool mids (Whirlpool, Raydium CLMM, Meteora DLMM) against the OKX/Binance
best bid/ask. A gap over 4 bp opens an episode and buys or sells 0.1 SOL on
that DEX through Jupiter; **control** episodes take the same quote at random
times. Then the same amount is swapped back at +0/5/15/30 s: the on-chain
round trip. All figures after each transaction's fixed cost (two of them for
a round trip). Excluded: the run whose entry quotes were starved (see *Data
quality*), pool mids older than 30 s, quotes over 500 bp from their reference.

**The entry alone** (532 triggers, 106 controls):

| | positive | median | mean |
|---|---|---|---|
| trigger, vs the exchange mid | 36 % | −0.48 bp | −0.24 bp |
| trigger, vs the side you would unwind on | 16 % | −1.39 | −1.19 |
| control, vs the exchange mid | 8 % | −1.28 | −1.27 |

**The on-chain round trip**, and what the trigger is worth over random timing:

| held | trigger median | trigger mean | control median | trigger − control (mean) |
|---|---|---|---|---|
| 0 s | −0.30 bp | +0.02 bp | −1.36 bp | **+1.42 bp** |
| 5 s | −0.28 | +0.33 | −0.83 | +0.89 |
| 15 s | +0.06 | +0.12 | −0.68 | +0.05 |
| 30 s | +0.05 | +0.12 | +0.21 | −0.60 |

**Reading.** The trigger is informative: buying the moment a pool is 4 bp away
from the exchange is worth about 1.4 bp over buying at a random moment, and
the markouts agree (after a trigger the pool moves 3–5 bp toward the exchange
while the exchange barely moves). But the trade itself is not profitable:

- The executable price already gives most of the gap back. The mid may be
  4 bp away; the quote to actually trade it is 0.48 bp *worse* than the
  exchange mid at the median.
- The round trip is about break-even at best (median −0.30 bp, mean +0.02 bp,
  43 % positive), and the audit measured executed outputs **1.16 bp below
  quotes**, which is larger than the whole advantage.
- Holding longer does not help. The trigger's advantage decays to zero by
  30 s: what is left then is the SOL price moving, not the gap closing.

## Data quality

Three problems were found and fixed while measuring; they are why some data
is excluded above.

1. **Exit quotes starved the entries they measure.** Adding the +0/5/15/30 s
   exit quotes put them in the same queue as the entry quote, so entries were
   answered a median of 8.4 s (p90 35 s) after the trigger, while episodes
   last 2 s. Run `20260922-101613-da39` is excluded from every lag figure.
   Fixed: entries have their own priority *and* reserved rate-limit slots
   (entry quote latency is now p50 0.7 s).
2. **Mainnet returns the occasional 200 whose route delivers a third of the
   amount, or nothing.** Stored as a price, one such answer read as −10,000 bp
   and swamped a distribution. Fixed: a zero output is an error in the
   adapter, and research refuses any quote more than 500 bp from its
   reference.
3. **The machine slept.** On waking, pool mids from before the sleep were
   compared with fresh exchange prices, which produced −30 bp "gaps" and one
   episode lasting 63,161 s. Fixed: a wall-clock jump ends open episodes and
   drops held prices; a pool mid older than 30 s is never compared.

## Limits of this measurement

- 5.4 awake hours in one place on one day. Market conditions vary.
- Quotes, not fills. Nothing here was sent to the chain.
- One CEX pair (SOL/USDC on OKX and Binance) as the reference price.
- Jupiter's routing is the only Solana execution path measured, and Uniswap
  v3 the only EVM one.
- Competition is not measured: another searcher taking the same opportunity
  first would show up as a worse fill, not as a missing sample.
