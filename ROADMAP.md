# Roadmap

This is a public research and engineering roadmap, not a promise of release dates.

MØBIUS-Searcher is currently at **0.4.0**: Solana execution research is real, PAPER is the default, and the shipped configuration has produced **0 executable opportunities** across the published run. Since 0.4 one buy-low-sell-high rule can be run with a small budget of real money; on past candles such rules lose. The immediate goal is not to manufacture more "opportunities"; it is to reduce uncertainty about **why** apparent edge disappears and when the system can know that reliably.

## 0.2 — measure where an edge could come from, and prove the execution path

- [x] `--research`: size ladder, cross-chain spreads (Solana vs Base/Arbitrum), DEX lag vs CEX with control samples.
- [x] Attribution for every opportunity: the guard that failed, executed vs quoted leg outputs, accounts a transaction creates.
- [x] Two-token inventory accounting (SOL + USDC) and a USD wallet ledger.
- [x] Thresholds adjustable while running, with a typed acknowledgement for settings that can lose money.
- [x] `--canary`: one loss-bounded trade through the LIVE path, reconciled account by account.
- [x] Solana settings under `[venues.solana]` (the old layout is read until 0.5).
- [ ] A canary trade landed and reconciled on mainnet (the release gate).

## 0.3 — measure what the 0.2 numbers left open

0.2 ([docs/RESEARCH_2026-09.md](docs/RESEARCH_2026-09.md)) found no direction
that pays for its costs as a taker through a quote API. That left four
questions, and 0.3 is a research release that answers them with data
([docs/RESEARCH_2026-10.md](docs/RESEARCH_2026-10.md)). It adds no strategy
that sends anything.

- [x] **Is a bigger prize reachable?** `--research-liquidations`: Morpho's
      liquidations on Base read back from chain. 95 % of the incentive is taken
      inside the block that creates it; a liquidator a block late finds $26 in
      30 days. Closed.
- [x] **How much of the loss is waiting?** Quote aging in `--report`: executed
      vs quoted output of every simulated leg by the age of its quote.
- [x] **Local pool math** (no quote API in the loop): Whirlpool, Raydium CLMM
      and Meteora DLMM, exact against swaps simulated on mainnet.
- [x] **Is there a gap between pools at all, without our latency?**
      `--research-pools`: round trips from the pools' own accounts at one slot.
- [x] **The maker's side of the lag signal**: orders resting in Meteora bins,
      imagined, with what the exchange price did after each fill.
- [ ] Longer runs of the three measurements above (hours, several days).
- [ ] A venue execution interface and on-chain order entry beyond the Solana
      round trip: only once a measurement says there is something to execute.
- [ ] USDC-based cycles and choosing the direction by inventory (same condition).

## 0.4 — a position, on paper and with a small budget

The arbitrage found nothing to trade, and the question that came next was
whether a simple directional rule would. 0.4 answers it on paper (no) and
lets one such rule run with real money anyway, small and behind typed words,
for whoever wants to watch it ([docs/LAB.md](docs/LAB.md),
[docs/MODEL_2026-10.md](docs/MODEL_2026-10.md), [docs/TRADE.md](docs/TRADE.md)).

- [x] `--lab`: rules on live prices with a paper account, a backtest on past
      candles with costs that depend on the size of a trade, a report with
      intervals. The four shipped rules lost on a year of candles.
- [x] A trained model as a lab rule: strong in rolling tests, a coin on the
      six months kept aside. In the lab only.
- [x] `--trade`: one lab rule with a budget of 1 to 25 USD, as ordinary
      transactions that are simulated as signed and sent until confirmed.
- [x] Bots and Wallet pages: start, stop, close, budget, each decision and
      trade as it comes; balances, receive, send.
- [x] A rule that acts on the price itself and takes a gain that is there.
- [ ] A round trip closed on mainnet, and what it cost against its backtest.
- [ ] A backtest of the price trigger (it needs prices inside the bar).
- [ ] Weeks of the paper lab on live prices, reported.

## Now — make the result easier to reproduce

- [ ] Expand RPC freshness measurements across multiple providers and regions.
- [ ] Repeat scheduler A/B measurements on both keyless and keyed Jupiter tiers.
- [ ] Publish longer PAPER runs with the same accounting and simulation path.
- [ ] Add more structured benchmark metadata so results can be compared across machines.
- [ ] Improve Windows and Linux terminal smoke-test coverage.
- [ ] Add a short real-time demo to the README.

## Next — strengthen execution evidence

- [ ] Measure quote-to-simulation aging per route, not only aggregate quote age.
- [ ] Separate market-movement loss from API/rate-limit delay in reports.
- [x] Improve route-level failure attribution for stale quotes, insufficient edge, and simulation failure.
- [ ] Add regression fixtures for transaction size, ALT/account limits, and cost-model edge cases.
- [ ] Make external benchmark submissions easy to validate and compare.

## Later — broader venue model

- [x] Move Solana-specific settings fully under `[venues.solana]`.
- [ ] Define a stable venue execution interface.
- [ ] Add another executable venue only after the accounting, simulation, and safety model can be preserved.
- [ ] Keep market-data-only venues explicitly separate from executable venues.

## Research questions

These are more important than feature count:

1. **Fresh enough for what?** Which evidence must be fresh at quote time, build time, simulation time, and send time?
2. **Where does the edge disappear?** Market movement, quote budget, transaction construction, fees, or simulation?
3. **What can be ruled out early?** Which routes can be rejected without spending scarce quote/simulation budget?
4. **What is reproducible across providers?** Which latency/freshness results survive changes in RPC, region, and Jupiter tier?
5. **What evidence is missing?** When the system says "cannot tell," what additional witness would turn that into a decision?

## Good first contributions

Small, self-contained work that would be genuinely useful:

- verify the TUI on a terminal/OS combination not already tested;
- reproduce one published latency number on another network/provider;
- improve one error message that currently hides the actual skip/failure reason;
- add a regression test for a documented edge case;
- improve a benchmark/report table without changing trading behaviour.

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR.
