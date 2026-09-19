## What does this change?

Describe the problem, uncertainty, or failure mode first.

## Evidence

What proves the change is useful or correct?

- [ ] Tests added or updated where appropriate
- [ ] Benchmark numbers included for performance/latency claims
- [ ] Screenshots included for visible TUI changes
- [ ] Negative results kept rather than filtered out

## Validation

Commands / checks run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add any runtime or benchmark commands below.

## Execution and safety impact

- [ ] No change to execution behaviour
- [ ] PAPER behaviour changed
- [ ] CONFIRM behaviour changed
- [ ] LIVE behaviour changed
- [ ] Key / secret handling changed

If any execution or secret-handling box other than "No change" is checked, explain the safety impact and how it was validated.

## Reproduction notes

Include OS/architecture, provider/tier, scheduler mode, and commit SHA when relevant.
