# Contributing to MØBIUS-Searcher

Thanks for taking the project seriously enough to test, break, measure, or improve it.

MØBIUS-Searcher is a research-first execution system. Contributions are most useful when they make a claim **more reproducible, more measurable, or safer**.

## Good contribution areas

- **Reproduction and benchmarks** — RPC freshness, Jupiter quote latency, scheduler A/B runs, simulation outcomes.
- **Execution correctness** — transaction construction, cost accounting, staleness checks, failure handling.
- **Terminal UI** — Ratatui layout, accessibility, terminal compatibility, snapshot quality.
- **Platform support** — macOS, Linux, Windows, x86_64/ARM64.
- **Documentation** — setup, diagnostics, benchmark methodology, failure explanations.
- **Tests** — especially edge cases that can reject a false positive or prevent unsafe execution.

Large exchange/venue integrations are welcome as proposals first; open an issue before spending significant time on them.

## Before opening a PR

1. Search existing issues and pull requests.
2. Keep a change focused. One measurable idea per PR is ideal.
3. Do not include secrets, wallet files, API keys, personal config, databases, or recordings containing sensitive data.
4. For performance or latency claims, include the exact command/config and the before/after numbers.
5. For trading/execution changes, explain how PAPER behaviour was validated and why the change does not weaken safety gates.

## Development setup

Requirements:

- Rust 1.91+
- Git
- a modern terminal

```bash
git clone https://github.com/mangiapanejohn-dev/MOBIUS-Searcher
cd MOBIUS-Searcher
cargo build --workspace
cargo test --workspace
```

Useful local checks before opening a PR:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For runtime diagnostics:

```bash
cargo run -p mobius-searcher -- --doctor
```

For a bounded PAPER run:

```bash
cargo run -p mobius-searcher -- --headless --duration 300
```

Never use a real funded wallet for contribution testing unless you independently choose to do so. PAPER is the default and preferred mode for development.

## Benchmark contributions

A useful benchmark report contains:

- date/time and rough region
- OS / architecture
- RPC provider or endpoint class
- Jupiter tier (keyless/keyed)
- scheduler mode
- duration
- sample count
- p50 / p95 / p99 where relevant
- rate-limit events
- exact commit SHA
- enough config detail to reproduce the result, with secrets removed

If you are comparing two schedulers or providers, change one major variable at a time.

## Pull requests

A PR should answer:

- What problem or uncertainty does this address?
- What changed?
- How was it tested?
- What evidence shows the change helped?
- Could it change PAPER / CONFIRM / LIVE behaviour?

Screenshots are useful for TUI changes. Numbers are more useful than adjectives for performance changes.

## Security

Do **not** open a public issue for vulnerabilities that could expose keys, bypass execution gates, or put funds at risk. Use the private vulnerability reporting path described in [SECURITY.md](SECURITY.md).

## Style

Prefer small, auditable changes over cleverness. If a result is negative, keep it. Negative results are data.
