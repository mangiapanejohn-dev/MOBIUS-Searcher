# MØBIUS-Searcher launch kit

This file is a distribution checklist, not product documentation. Keep every
public claim tied to measured results in the repository.

## Ready-to-submit outreach — 2026-09-23

### Publication status

- Repository baseline at preparation: **11 stars**. This is a snapshot, not a growth claim.
- HelloGitHub: submission prepared below; **not published**. The GitHub integration returned HTTP 403 (`Resource not accessible by integration`) when creating the submission.
- Awesome Ratatui: contribution guidelines and Finance and Markets category checked; **not submitted**. A fork and pull request are needed; the browser is not signed in.
- X, Hacker News and developer-group copy below: **drafts, not posted**.
- Published binary: v0.1.0. Development branch: 0.2.0-beta.1. Keep those audiences and feature sets explicit.

### X — short post

```text
I built a Rust + Ratatui terminal for Solana arbitrage research.

Live quotes, mainnet simulation, cost breakdowns and session replay. PAPER by default.

Try it, inspect a rejected route, and star it if useful:
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher
```

Attach the real [Markets screenshot](images/markets.png). Do not use a simulated return as a realized-profit claim.

### WeChat / Chinese developer groups

```text
把 Solana 行情、套利路线、主网模拟和成本分析装进一个终端

MØBIUS-Searcher
Rust + Ratatui 开源项目

实时行情 / 路线检查 / 成本拆解 / 运行回放
默认 PAPER 模式  不签名、不发送交易

适合玩 Rust、终端 UI、Solana 或量化研究的朋友
欢迎试用、提 issue
觉得有用点个 Star

https://github.com/mangiapanejohn-dev/MOBIUS-Searcher
```

### Show HN

**Title:** Show HN: A Rust terminal for inspecting Solana arbitrage routes

**URL:** https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

**Author comment:**

I built MØBIUS-Searcher to inspect what happens between a visible price spread and an executable transaction.

It combines live Jupiter quotes, v0 transaction construction, mainnet simulation, integer cost accounting, and SQLite record/replay in a Ratatui terminal UI. PAPER is the default and does not sign or send transactions.

The published v0.1 PAPER report covers 3,091 route evaluations and 2,333 simulations, with zero executable opportunities at the tested default configuration. That is a result for those runs, not proof that arbitrage never exists. The rejected routes are part of the output, so other people can inspect the assumptions.

Prebuilt v0.1.0 downloads support macOS, Linux and Windows. The main branch is 0.2.0-beta.1 development; its extra research commands require a source build.

I'd appreciate feedback on the route inspector, replay workflow, cost model and quote freshness. Can you reproduce a rejection and identify which cost or assumption decides it?

Results: https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/blob/main/docs/PAPER_RUN.md
Architecture: https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/blob/main/docs/ARCHITECTURE.md

### Awesome Ratatui submission

Destination: https://github.com/ratatui/awesome-ratatui

Guidelines: https://github.com/ratatui/awesome-ratatui/blob/main/contributing.md

Category: **Apps → Productivity and Planning → Finance and Markets**.

Insert alphabetically between `invoicepilot` and `Rex`:

```markdown
- [MØBIUS-Searcher](https://github.com/mangiapanejohn-dev/MOBIUS-Searcher) - A Solana arbitrage research terminal with live market data, transaction simulation, cost inspection, and session replay.
```

**PR title:** Add MØBIUS-Searcher to Finance and Markets

**PR body:**

Adds MØBIUS-Searcher, a Rust application built with Ratatui, to Finance and Markets.

Its terminal UI includes market charts, route inspection, system health, logs, and recorded-session replay. It defaults to PAPER mode. The repository includes source, screenshots, installation instructions, and MIT / Apache-2.0 licenses.

Project: https://github.com/mangiapanejohn-dev/MOBIUS-Searcher
Ratatui dependency: https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/blob/main/Cargo.toml
Screenshots: https://github.com/mangiapanejohn-dev/MOBIUS-Searcher#terminal-ui

Disclosure: submitted on behalf of the project maintainer.

### HelloGitHub submission

Destination: https://github.com/521xueweihan/HelloGitHub/issues

Use the Chinese project-submission template. Self-submissions are welcomed by the template; this is a submission for editorial review, not a promised listing.

**Title:** [开源推荐] MØBIUS-Searcher：Rust + Ratatui 构建的 Solana 套利研究终端

### 项目地址
https://github.com/mangiapanejohn-dev/MOBIUS-Searcher

### 类别
Rust

### 项目标题
用 Rust 构建的 Solana 套利研究终端

### 项目描述
MØBIUS-Searcher 将实时行情、路线报价、交易构建、主网模拟和成本归因放进一个终端界面。它默认使用 PAPER 模式，不签名或发送交易，并记录被拒绝的路线及原因。适合学习 Rust 异步系统、Ratatui 界面和链上交易执行，也方便研究者复现价差在计入实际成本后为何消失。

### 亮点
- 自荐项目，采用 MIT / Apache-2.0 双许可证，提供中英文文档和 macOS、Linux、Windows 的 v0.1.0 下载。
- Ratatui 多页界面：实时行情、路线检查、图表、系统健康和日志；本地 SQLite 录制与回放。
- Jupiter 路线报价、真实 v0 交易构建和主网模拟，逐项核算费用、Jito tip、ATA rent 等成本。
- 项目公开的 v0.1 PAPER 报告记录了 3,091 次评估、2,333 次主网模拟，默认配置下 0 个可执行机会。该结果限定于报告的环境和样本，不代表普遍不存在套利机会。
- main 分支正在开发 0.2.0-beta.1；研究功能与已发布的 v0.1.0 二进制有差异。没有已验证的实盘收益，适合作为工程与研究工具介绍。

报告：https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/blob/main/docs/PAPER_RUN.md
架构：https://github.com/mangiapanejohn-dev/MOBIUS-Searcher/blob/main/docs/ARCHITECTURE.md

### 示例代码
```bash
mobius-searcher --doctor
mobius-searcher
mobius-searcher --report latest
mobius-searcher --replay latest
```

### 截图或演示视频
![MØBIUS-Searcher 行情界面](https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/docs/images/markets.png)


### Publishing and follow-up

1. Submit the Ratatui directory entry and HelloGitHub recommendation once authenticated access is available. Check for an existing submission first.
2. Publish the short post with the actual screenshot; link directly to the repository.
3. Submit Show HN when available to answer technical questions. Keep the headline focused on the working tool.
4. Share the Chinese post in relevant groups that permit project sharing.
5. Record each published URL and date here. Do not mark drafts as sent.
6. After 24–48 hours, compare GitHub traffic, referrers, clones, stars, and useful feedback if those metrics are accessible. Record real observations; do not invent attribution.
7. Reply to questions with code and measurements. Do not request coordinated upvotes or repeatedly repost the same link.

---

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

1. Keep the README and release-version note aligned with the current release.
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
- Repository description and topics are already set; revisit only when positioning changes.
- Add a project homepage once a landing page exists.
- Add good-first-issue labels only for genuinely self-contained work.
- Turn strong external questions into reproducible benchmark issues.
- Keep PAPER_RUN.md and LATENCY.md current as new measurements land.
