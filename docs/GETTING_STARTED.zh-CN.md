# 上手指南

从零到跑出第一份研究报告。大约十五分钟，不需要密钥，也不需要花钱。交易放在最后，而且只在你想做的时候才做。

English: [GETTING_STARTED.md](GETTING_STARTED.md)。

## 1. 你真正需要准备什么

| | 什么时候需要 | 怎么获得 |
|---|---|---|
| 程序本体 | 全部 | 见 [INSTALL.md](INSTALL.md)，单个可执行文件，不依赖运行时 |
| 终端窗口 ≥ 80×24 | 界面 | 任意现代终端 |
| **其他什么都不要** | **PAPER 模式和 `--research`** | Jupiter 不带密钥也能用，Solana 公共 RPC 是默认值 |
| Jupiter API 密钥 | 把报价速率翻倍 | 免费自助申请：[portal.jup.ag](https://portal.jup.ag) → API Keys → Free |
| 自己的 Solana RPC | 更少卡顿、数据更新鲜 | 任意服务商，把地址填进 `SOLANA_RPC_URL` |
| 有钱的钱包 | 只有 CONFIRM / LIVE / `--canary` 需要 | 你自己的热钱包，见第 7 步 |

速率限制来自 [Jupiter 官方文档](https://developers.jup.ag/docs/portal/rate-limits)：**不带密钥 0.5 次/秒**，**免费密钥 1 次/秒**，按组织计算，60 秒滑动窗口。MØBIUS 会在运行时学习这个窗口并自我限速。这也是为什么"报价"是这里最稀缺的资源，以及为什么交易会话运行时 `--research` 会拒绝启动。

除了你自己配置的服务商，数据不会发往任何地方。记录保存在本地 SQLite 文件里。

## 2. 第一次运行

```bash
mobius-searcher
```

首次运行会打开一个简短的设置向导，有三条路径：

| 路径 | 作用 |
|---|---|
| **Research mode（研究模式）** | 只有 PAPER：不涉及私钥，任何交易都发不出去。推荐从这里开始。 |
| **Assisted trading（辅助交易）** | 准备一个专用机器人钱包和 CONFIRM 模式，需要手动输入解锁短语。 |
| **Advanced setup（高级设置）** | 所有服务商、策略、限额、执行参数都自己填。 |

选 **Research mode**。它会问你钱包（选 *Create* 生成一个只读用的地址，或者 *None*）、RPC（先用 *Public* 就行）、扫描哪些策略、限额多严格。在最终确认之前不会写入任何文件，并且会告诉你它要写的两个文件：

- `~/.config/mobius/config.toml` —— 你的设置，只保存与默认值不同的部分
- `~/.config/mobius/.env` —— 密钥，权限 600，日志里只会出现变量名，不会出现值

之后随时可以用 `mobius-searcher --setup` 重新打开。

## 3. 检查各项是否连得上

```bash
mobius-searcher --doctor
```

每一行都是一次真实请求：配置层级、哪些密钥已设置（只显示变量名，绝不显示值）、正在使用的代理、Solana RPC 和 WebSocket、池子与预言机账户、Jupiter、Jito 小费流、每个启用的交易场所，以及在发送模式下的钱包状态。继续之前先解决 `FAIL` 的行；只做研究的话，`warn` 的行可以先不管。

## 4. 加一个 Jupiter 密钥（可选，两分钟）

报价速率翻倍，免费：

1. 打开 [portal.jup.ag](https://portal.jup.ag)，登录后在 **Free** 套餐下创建一个 API key。
2. 写进向导创建的 `.env` 文件：

```bash
printf "JUPITER_API_KEY='把密钥粘在这里'\n" >> ~/.config/mobius/.env
```

3. 再跑一次 `mobius-searcher --doctor`，应该能看到 `set  JUPITER_API_KEY`，Jupiter 那一行也会显示更大的窗口。

密钥的读取顺序是：系统环境变量 → `~/.config/mobius/.env` → 当前目录的 `./.env`。它不会被写进 `config.toml`，也不会被打印出来。

## 5. 先看它怎么工作（PAPER 模式）

```bash
mobius-searcher
```

PAPER 模式会用真实报价给每条路线定价，构建真实交易，并**在主网上模拟执行**，然后记录结论。它不会签名任何东西。

| 按键 | |
|---|---|
| `1`–`8` | 页面：总览 · 行情 · 机会 · 图表 · 成交 · 风险 · 系统 · 日志 |
| `3` 再按 `⏎` | 查看某条机会的完整细节：报价、成本，以及被跳过的原因 |
| `T` | 门槛面板（见 [USAGE.md](USAGE.md#thresholds)） |
| `K` | 急停 |
| `?` | 全部按键 · `q` | 退出 |

让它跑一会儿，然后：

```bash
mobius-searcher --report latest
```

报告统计的是**每一次**评估，而不只是好的那些：报价了多少条路线、扣成本前后各有多少是正的、是哪道利润门槛拦下的、模拟结果如何，以及报价与实际成交的对比。

```bash
mobius-searcher --replay latest
```

可以用同一套界面回放整场会话。

## 6. 跑研究

这正是 0.2 版的重点：用测量代替猜测。

```bash
mobius-searcher --research
```

它测量三件事，结果写进 `<数据目录>/research.sqlite`：**仓位曲线**（配置里的路线，在 0.01–2 SOL 各档报价）、**跨链价差**（ETH 和 cbBTC 在 Solana 与 Base/Arbitrum 之间）、**DEX 滞后**（链上池子价格对比 OKX/Binance 的买一卖一，每次价差都取一次可成交报价，随机时刻取对照样本，并在 +0/5/15/30 秒做链上往返）。全程不签名、不发送。

运行期间它会独占 Jupiter 额度：同时启动的交易会话会被拒绝，反过来也一样。在 macOS 上它会阻止电脑睡眠（仅限插电；用电池时系统仍会睡）。

```bash
mobius-searcher --research-report all
```

输出完整分布，样本数和为正的数量并列，注意事项直接写在数字旁边。我们自己跑出来的结果见 [RESEARCH_2026-09.md](RESEARCH_2026-09.md)，简短版：**没有一个方向能覆盖自己的成本。**

## 7. 只有在你想真的交易时：慢慢来

先读 [LIVE_CHECKLIST.md](LIVE_CHECKLIST.md)。顺序很重要：

1. **专用热钱包**，绝对不要用你的主钱包。只放你能承受损失的金额，另外放一点 USDC（下一条腿的输入金额固定为上一条腿的报价，差额由你的 USDC 垫上）。
2. **`mobius-searcher --canary`** —— 一笔真实的、亏损封顶的交易（`canary.max_loss_lamports`，默认 0.0005 SOL，这个上限同时写进交易的链上最低成交保护），按 `y` 确认后发送，然后逐账户对账。钱包不够时它会直接拒绝启动，并告诉你还差多少。
3. **CONFIRM 模式**（`--mode confirm`）：每一笔都要你按 `y`。
4. **LIVE 模式**（`--mode live`），用很小的限额，盯着风险页和系统页。任何时候按 `K` 都能停止新的发送。

默认情况下，成交的交易不会亏钱：最后一条腿的链上最低成交保护覆盖了本金、全部成本和你设定的最低利润，达不到就整笔回滚，而不是以差价成交。要关掉这个保护，必须手动输入 `ALLOW LOSS`。

**请预期它不会交易。** 在默认门槛下，找不到有利可图的机会时它就是不交易——这是设计如此。我们自己的实盘会话就是这样：817 个机会，0 笔交易。

## 文件都在哪

| | |
|---|---|
| 设置 | `~/.config/mobius/config.toml`（`$MOBIUS_CONFIG` 可覆盖） |
| 密钥 | `~/.config/mobius/.env`，然后是 `./.env` |
| 会话记录 | `<数据目录>/mobius.sqlite` |
| 研究数据 | `<数据目录>/research.sqlite` |
| canary 对账报告 | `<数据目录>/canary/` |
| 数据目录 | `general.data_dir`，否则 `$MOBIUS_HOME/data`，否则 `$XDG_DATA_HOME/mobius`，否则 `~/.local/share/mobius` |

## 遇到问题时

| 现象 | 含义 |
|---|---|
| `429` / 被限流 | Jupiter 窗口满了。免费密钥能翻倍；两个 MØBIUS 进程不能共用额度（预算锁会提示你）。 |
| 全是 `EDGE_TOO_SMALL` | 正常。扣完成本后这些路线不赚钱，`--report` 会告诉你是哪道门槛拦下的。 |
| `INVENTORY_LOW` | 第一条腿交付的数量少于第二条腿的固定输入，而钱包里没有 USDC 可以垫。 |
| `DEPOSIT_TOO_HIGH` | 这条路线会新建账户，其租金超过了 `profit.max_new_deposit_lamports`（租金算占用资金，不算成本）。 |
| `NO_ROUTE`、超时、`Oracle is stale` | 服务商返回的结果，被记录成错误而不是价格。 |
| 放一夜什么都没跑 | 电脑睡着了。`--research` 只有在插电时才能阻止睡眠。 |
| 提示终端太小 | 界面需要 80×24；`--headless` 则完全不需要终端。 |

资料来源：[Jupiter 速率限制](https://developers.jup.ag/docs/portal/rate-limits) · [Jupiter 开发者门户](https://portal.jup.ag)
