# Roadmap

This is a public research and engineering roadmap, not a promise of release dates.

MØBIUS-Searcher is currently at **0.1.x**: Solana execution research is real, PAPER is the default, and the shipped configuration has produced **0 executable opportunities** across the published run. The immediate goal is not to manufacture more "opportunities"; it is to reduce uncertainty about **why** apparent edge disappears and when the system can know that reliably.

## 0.2 — measure where an edge could come from, and prove the execution path

- [x] `--research`: size ladder, cross-chain spreads (Solana vs Base/Arbitrum), DEX lag vs CEX with control samples.
- [x] Attribution for every opportunity: the guard that failed, executed vs quoted leg outputs, accounts a transaction creates.
- [x] Two-token inventory accounting (SOL + USDC) and a USD wallet ledger.
- [x] Thresholds adjustable while running, with a typed acknowledgement for settings that can lose money.
- [x] `--canary`: one loss-bounded trade through the LIVE path, reconciled account by account.
- [x] Solana settings under `[venues.solana]` (the old layout is read until 0.4).
- [ ] A canary trade landed and reconciled on mainnet (the release gate).

## 0.3 — act on what the research shows

- [ ] On-chain order entry beyond the Solana round trip (Solana + EVM chains).
- [ ] A strategy chosen from the 0.2 research data — or none, if no direction survives.
- [ ] USDC-based cycles and choosing the direction by inventory.
- [ ] Local pool math (no quote API in the loop), if the data says latency decides.

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
