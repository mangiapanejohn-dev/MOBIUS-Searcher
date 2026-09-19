# MØBIUS-Searcher launch kit

This file is a distribution checklist, not product documentation. Keep every
public claim tied to measured results in the repository.

## Core story

> I built a Solana arbitrage searcher, ran 3,091 real route evaluations and
> 2,333 mainnet simulations, and found 0 executable opportunities at the
> default configuration. I open-sourced the system and the losing data too.

The hook is not "this bot prints money." The hook is that it measures how much
of the apparent arbitrage disappears after executable quotes, transaction
construction, fees, slippage, Jito tips, rent, freshness and simulation.

## 10-second demo to record

Record the terminal at normal speed. No voice-over is required.

1. Start on the MØBIUS-Searcher banner / Markets page.
2. Let live prices and the DEX quote book visibly update.
3. Open an opportunity / route inspection.
4. Show transaction simulation and cost accounting.
5. End on a route being rejected because the net edge is not executable.
6. Hold the final frame for ~2 seconds.

Suggested final overlay:

```text
3,091 real evaluations
2,333 mainnet simulations
0 executable opportunities
Open source: MØBIUS-Searcher
```

Do not use fake PnL, sped-up "profit" animations, or imply that PAPER results
represent realized returns.

## Hacker News

### Title

Show HN: I built a Solana arbitrage searcher and 3,091 real evaluations found 0 executable trades

### Body

I built MØBIUS-Searcher, a Rust + Ratatui Solana arbitrage research tool.

It consumes live market data, prices routes with Jupiter, builds the actual v0
transaction, includes fees / slippage / Jito tip / ATA rent in the accounting,
and simulates the transaction on mainnet before a route can pass.

The result I found more interesting than a profit screenshot: across 3,091 real
evaluations and 2,333 mainnet simulations at the default configuration, it found
0 executable opportunities.

I open-sourced the system, the PAPER results, and the latency measurements
because I wanted the negative result to be reproducible too.

Repo:
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

I'd especially like feedback on the execution model, quote freshness, routing
assumptions, and what measurements would make the result more useful.

## X / Bluesky

### Short post

I built a Solana arbitrage searcher.

3,091 real route evaluations.
2,333 mainnet simulations.
0 executable opportunities at the default configuration.

So I open-sourced the bot — and the losing data.

Rust + Ratatui + Jupiter + Jito:
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

### Technical post

Most arbitrage screenshots show the spread before execution.

MØBIUS-Searcher measures what survives after executable Jupiter quotes,
transaction construction, fees, slippage, Jito tips, ATA rent, freshness checks
and mainnet simulation.

Current PAPER run:
- 3,091 evaluations
- 2,333 mainnet simulations
- 0 executable opportunities
- event-driven scheduling: 30% fewer quote requests
- quote age at decision time p50: 2.6s -> 0.64s

Open source:
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

## Reddit

### r/rust angle

**Title:** I built an event-driven Solana arbitrage searcher in Rust + Ratatui — and the negative result was the interesting part

Focus the post on:
- async/event-driven architecture
- dependency-aware quote scheduling
- Ratatui UI
- integer accounting
- SQLite record/replay
- measured request reduction and quote freshness

Do not lead with "make money."

### Solana / DeFi developer angle

**Title:** I tested 3,091 real Solana arbitrage routes; after executable costs and simulation, 0 passed

Focus the post on:
- Jupiter executable quotes
- v0 transaction construction
- Jito tip
- mainnet simulation
- RPC / pool freshness
- why gross spread is not executable edge

## WeChat / developer groups

MØBIUS-Searcher 开源了

不是“稳赚套利 Bot”

它直接接真实 Solana 市场数据
用 Jupiter 报价
构建真实 v0 transaction
上主网 simulation
把 fee / slippage / Jito tip / ATA rent 全部算进去

目前 PAPER 实测：

3,091 次真实 route evaluation
2,333 次 mainnet simulation
0 个默认配置下真正可执行的套利机会

反而这就是这个项目最有意思的地方
它把“看起来有价差”和“真的能成交赚钱”之间的东西全部测出来

Rust + Ratatui
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

## Distribution order

1. Merge the README positioning change.
2. Record and upload the 10-second demo.
3. Put the demo directly under the README intro if the file size is reasonable.
4. Publish the technical blog post.
5. Post to HN.
6. Post the Rust-specific angle to r/rust.
7. Post the execution-specific angle to Solana / DeFi developer communities.
8. Post the short demo on X / Bluesky.
9. Share the Chinese version to relevant developer groups.
10. Reply to technical comments with measurements and code links; do not just
    drop the repository link repeatedly.

## Next repository improvements

- Add the short demo to the README above the feature table.
- Set a concise GitHub repository description.
- Add a project homepage once a landing page exists.
- Add good-first-issue labels only for genuinely self-contained work.
- Turn strong external questions into reproducible benchmark issues.
- Keep PAPER_RUN.md and LATENCY.md current as new measurements land.
