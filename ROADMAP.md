# Roadmap

This is a public research and engineering roadmap, not a promise of release dates.

MØBIUS-Searcher is currently at **0.1.x**: Solana execution research is real, PAPER is the default, and the shipped configuration has produced **0 executable opportunities** across the published run. The immediate goal is not to manufacture more "opportunities"; it is to reduce uncertainty about **why** apparent edge disappears and when the system can know that reliably.

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
- [ ] Improve route-level failure attribution for stale quotes, insufficient edge, and simulation failure.
- [ ] Add regression fixtures for transaction size, ALT/account limits, and cost-model edge cases.
- [ ] Make external benchmark submissions easy to validate and compare.

## Later — broader venue model

- [ ] Move Solana-specific settings fully under `[venues.solana]`.
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
