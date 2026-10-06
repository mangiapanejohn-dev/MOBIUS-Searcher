//! What each page is and what its keys do, said in Chinese for an operator
//! who reads it (`?`). The English help is a list of keys; this one explains,
//! because the pages about the operator's own money are read by someone who
//! may have never seen a trading screen.

use crate::app::Page;

/// A line of a guide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Line {
    /// A heading.
    Head(&'static str),
    /// A paragraph.
    Text(&'static str),
    /// A key (or a name on the page) and what it does (or means).
    Item(&'static str, &'static str),
}

use Line::{Head, Item, Text};

/// Keys that work on every page.
const EVERYWHERE: [Line; 5] = [
    Head("每一页都能用的键"),
    Item("1–9、0", "切换页面，页脚写着每个数字对应哪一页"),
    Item("?", "打开这份说明（任意键关闭）"),
    Item("K", "急停：让套利引擎立刻不再发新交易；再按 K、然后按 y 才解除。不影响机器人"),
    Item("q", "退出界面。已经启动的机器人在后台继续运行，不会跟着退出"),
];

/// The guide of a page.
pub fn guide(page: Page) -> Vec<Line> {
    match page {
        Page::Bots => vec![
            Text(
                "这一页管理“低买高卖”的机器人。一个机器人就是一条规则加一笔预算：价格跌到买入线下方，它用预算买入 SOL；涨回卖出线上方，它卖出。左边是机器人列表（真钱的和纸面模拟的），右边是选中那个的全部情况。",
            ),
            Head("右边从上到下"),
            Item("第一句话", "它现在在做什么、在等什么价格"),
            Item("刻度条", "现价离它下一步动作还有多远。持仓时两个刻度是买入成本和卖出线，没持仓时是买入线和卖出线"),
            Item(
                "资金与盈亏",
                "预算和它现在值多少；买成 SOL 的和闲着的 USDC 各多少；已实现盈亏（卖掉了的）、浮动盈亏（还拿着的，按现价）、两者合计；累计买卖了多少钱；平仓笔数和胜率；清仓线（市值跌到这个数就全部卖出并永久结束）",
            ),
            Item(
                "实时运算",
                "规则的算式每秒按现价重算一遍：平均价、波动、现价偏离了几个波动，一条刻度上标着买入和卖出的位置，以及“这根线若现在收盘它会不会动手”。图上的线、刻度条和这里的数字是同一套实时数字；真正动手只在每根线收盘时",
            ),
            Item(
                "交易历史",
                "它每一轮买卖：什么时候什么价买的、什么时候什么价卖的、拿了多久、这一轮赚亏多少钱和百分之几。还拿着的那一轮排在最上面，按现价估算",
            ),
            Item("全部真钱机器人", "左下角：所有真钱机器人的预算、现值、已实现、浮动和合计盈亏"),
            Item("实时成交", "左下角：交易所里这个币刚刚成交的每一笔（时间、价格、数量、买还是卖），每两秒更新"),
            Item(
                "这笔持仓",
                "持有 SOL 时：什么时候、什么价买的，拿了多久，现在浮动盈亏多少，离卖出线和止损线各差百分之几。没持仓时这里是它在等的买入线",
            ),
            Item("行情", "这个币今天的涨跌和 24 小时高低"),
            Item(
                "实时判断",
                "机器人自己写下来的每一次判断，最新的在最上面：几点、看到的价格、拿它和哪条线比、差多少、结论是什么。实时方式下每 2 秒一行，买入、卖出成交也会出现在这里；第一行前面的圆点在跳，说明它还活着，超过半分钟没有新的一行会变成黄色提醒。收盘方式下每根线一行，最上面“此刻”那行是页面按现价替它算的“如果现在收盘会怎样”",
            ),
            Item(
                "它的成交",
                "它每一笔交易一行：什么时候什么价买的、什么时候什么价卖的、多少 SOL、拿了多久、赚了多少、百分之几；还没卖的那笔按现价算浮动盈亏。有兑换已经发出、还在等链上确认时，最上面会有一行提示",
            ),
            Item("市值走势", "它的市值一根线一格画出来：高于预算是绿色，低于是红色"),
            Item(
                "K 线",
                "交易所的实时行情。黄线是卖出线，白线是买入成本（没持仓时绿线是买入线），红色是止损线，▲ 是它买的位置，▼ 是它卖的位置。下面的柱子是成交量",
            ),
            Item(
                "算式",
                "“这些线是怎么算出来的”把规则的算法直接写了出来：取最近多少根线的平均价和波动，买入线和卖出线各是多少",
            ),
            Item("记录", "“它做过什么”是它每一步的流水，回车看全部"),
            Head("这一页的键"),
            Item("j / k", "选上一个 / 下一个机器人（也可以用鼠标点）"),
            Item("s", "启动选中的机器人。会先问一次，按 y 才启动；启动后它在后台自己运行，关掉界面也继续"),
            Item("x", "停止。只是让它不再买卖，持有的东西原样留着；停着的时候它的止损也不生效"),
            Item(
                "b",
                "调预算：给它加钱或减钱（1 到 25 美元）。加钱先用钱包里空闲的 USDC，不够的部分卖出 SOL 来换；减钱是把 USDC 还给钱包。它已有的盈亏不变",
            ),
            Item(
                "t",
                "切换判断方式（会先问一次）。实时：每 2 秒看一次价格，价格一低于买入线就买；持有时只要卖出能保证比成本多赚 0.1% 以上就立刻卖（用链上的最低到手量锁住，达不到就不发单），到卖出线、止损线也立刻卖；刚卖出的不会马上原价买回。收盘：只在每根线收盘时判断一次。运行中切换几秒内生效",
            ),
            Item(
                "n",
                "新建一个机器人：选类型、填预算和止损、选判断方式（默认实时），输入确认词后创建。创建后不会自动启动，要再按 s",
            ),
            Item("c", "卖出并结束：把它持有的 SOL 卖成 USDC 留在钱包里，这一轮永久结束。要先停止（x）"),
            Item("[ ]", "换 K 线周期（1 秒到 1 天）。只是换个周期看价格，规则仍按它自己的周期判断"),
            Item("v", "K 线和折线之间切换"),
            Item("⏎", "看它的完整记录"),
            Item("p", "显示 / 隐藏纸面实验。纸面实验是用模拟账户试规则，不是真钱，默认不显示"),
            Head("要知道的"),
            Text(
                "机器人可能亏钱，没有任何保证。每次买卖有约千分之一到千分之二的成本，交易越频繁成本越多。“有赚就卖”保证的是这一次卖出比成本多，不保证买入之后价格会涨回来：跌下去时它会一直拿着，直到卖出线或止损线。实时方式没有回测过。电脑休眠或关机时它什么都不会做，止损也不会。",
            ),
        ],
        Page::Wallet => vec![
            Text("这一页是你的 Solana 钱包：有多少钱、怎么收钱、怎么转出去、最近的进出。"),
            Head("页面上的内容"),
            Item("总值", "SOL 按现价折成美元，加上 USDC"),
            Item("机器人持有", "机器人占用的那部分也算在余额里，但不算进“可以转出”；把它转走，机器人就卖不出去了"),
            Item(
                "钱都在哪",
                "一张表：每个机器人占用的、留作手续费的、空闲可以转出的，各是多少 SOL、多少 USDC、约合多少美元、占多大比例",
            ),
            Item(
                "盈亏（累计）",
                "每个真钱机器人一行：预算、现值、已实现盈亏、浮动盈亏、合计、平仓笔数；下面是所有机器人的合计，以及套利在本次运行里的已实现盈亏。很早结束的机器人合并成一行",
            ),
            Item("可以转出", "扣掉机器人占用的、再留出付手续费的 SOL 之后，可以放心转走的数量"),
            Item(
                "收款",
                "把这个地址或二维码给对方就能收钱。只能收 Solana 网络上的 SOL 和 USDC；从交易所提币时网络必须选 Solana，选错收不到",
            ),
            Item("最近进出", "从链上读到的记录：兑换、收到、转出。回车看完整签名，可以拿到区块浏览器里查"),
            Head("这一页的键"),
            Item("s", "转出 SOL"),
            Item("u", "转出 USDC"),
            Item("c", "复制收款地址（终端不支持时，按住 Shift 或 Option 拖动选中再复制）"),
            Item("r", "立刻重新读取余额和记录（平时每 10 秒自动读一次）"),
            Item("j / k、⏎", "选一条记录、看它的详情"),
            Head("转出的步骤"),
            Item("第 1 步", "填对方的钱包地址（可以粘贴；转过的地址按 ↑ ↓ 直接选）和数量；按 m 填入全部可转的；回车"),
            Item(
                "第 2 步",
                "程序去链上核对并把结果给你看：地址四位一组、对方是已有的钱包还是新地址、网络费、转出后还剩多少。地址填的是程序或代币账户、余额不够，会在这里被拒绝并说明原因",
            ),
            Item("第 3 步", "输入收款地址的最后 4 位，回车，才真正发出。几秒后显示是否到账和签名"),
            Text("链上转账发出后无法撤回，也没有客服能追回：发出前请逐位核对地址，第一次可以先转一小笔试试。"),
        ],
        Page::Overview => vec![
            Text(
                "这一页是套利引擎的总览。引擎不停地找“在一个交易所买、在另一个卖”能赚差价的机会；绝大多数机会算上手续费后不赚钱，会被跳过。",
            ),
            Item(
                "左上",
                "评估过的机会。GROSS 是不算成本的差价，NET 是扣掉全部成本后的结果，STATUS 是结论（EDGE_TOO_SMALL = 差价不够付成本）",
            ),
            Item("右上", "价格图表"),
            Item("左下", "事件流：引擎每一步在做什么"),
            Item("右下", "选中机会的明细：路线、每一项成本"),
            Item("Tab", "在几个区域之间切换焦点；j / k 在当前区域里上下移动；⏎ 看详情"),
        ],
        Page::Markets => vec![
            Text("这一页是交易所风格的行情：K 线、链上各交易所的报价、最近成交、你的资产。"),
            Item("[ ]", "换 K 线周期（1 秒到 1 天）"),
            Item("p", "换交易对"),
            Item("c", "蜡烛图 / 折线图"),
            Item("← →", "把游标移到某一根 K 线上看它的开高低收"),
            Item("t / o", "切换右侧和底部的标签页"),
        ],
        Page::Opportunities => vec![
            Text("这一页列出引擎评估过的每一条套利路线，以及为什么做了或没做。"),
            Item("f", "筛选：全部 / 差价为正的 / 可执行的 / 被跳过的"),
            Item("j / k、⏎", "选一条、看它的成本明细"),
        ],
        Page::Graphs => vec![
            Text("这一页最多并排看 6 个指标的曲线（价格、差价、延迟等）。"),
            Item("+ / -", "添加 / 移除指标"),
            Item("← →", "移动游标；a / b 打两个标记比较差值，x 清除"),
            Item("[ ]", "放大 / 缩小时间范围"),
        ],
        Page::Trades => vec![
            Text("这一页是成交记录。"),
            Item(
                "机器人成交",
                "最上面是低买高卖机器人的每一次买入和卖出：时间、哪个机器人、数量、成交价，卖出时这一笔的盈亏。有真钱机器人时才显示",
            ),
            Item("TRADES", "套利引擎的成交（paper 是模拟的，不是真钱）和本次运行的盈亏曲线"),
        ],
        Page::Risk => vec![
            Text("这一页是风控：急停开关的状态、各项限额、机会没有被执行的原因统计。"),
            Item("T", "调整阈值（最小利润等）。改之前会让你复核一遍"),
        ],
        Page::System => vec![Text(
            "这一页是每个连接的健康状况：链上节点（RPC）、Jupiter（报价）、Jito（发送）、行情源。看延迟、错误和是否被限速。",
        )],
        Page::Logs => vec![Text("这一页把所有日志合在一起。j / k 滚动，⏎ 看一条的全文，End 回到最新。")],
    }
}

/// The guide of a page as a document (see `panels::doc_rows`): what the page
/// is in words, then each group of names or keys as a table.
pub fn doc(page: Page) -> String {
    let mut out = String::new();
    let mut in_table = false;
    // the table's heading says what its two columns are: keys and what they do, or names and what they mean
    let mut keys = false;
    for line in guide(page).iter().chain(&EVERYWHERE) {
        match line {
            Head(h) => {
                out += &format!("\n## {h}\n\n");
                (in_table, keys) = (false, h.contains('键'));
            }
            Text(t) => {
                if in_table {
                    out.push('\n');
                }
                out += &format!("{t}\n");
                in_table = false;
            }
            Item(k, what) => {
                if !in_table {
                    out += if keys { "| 按键 | 作用 |\n|---|---|\n" } else { "| 名称 | 说明 |\n|---|---|\n" };
                    in_table = true;
                }
                out += &format!("| {k} | {} |\n", what.replace('|', "/"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_has_a_guide_and_every_key_of_the_money_pages_is_in_theirs() {
        for p in Page::ALL {
            assert!(!guide(p).is_empty(), "{p:?}");
        }
        let keys =
            |p| guide(p).iter().filter_map(|l| if let Item(k, _) = l { Some(*k) } else { None }).collect::<Vec<_>>();
        for k in ["s", "x", "b", "c", "[ ]", "v", "⏎", "j / k"] {
            assert!(keys(Page::Bots).contains(&k), "Bots: {k}");
        }
        for k in ["s", "u", "c", "r"] {
            assert!(keys(Page::Wallet).contains(&k), "Wallet: {k}");
        }
        // as a document: words, then a table of names and one of keys
        let d = doc(Page::Bots);
        assert!(d.starts_with("这一页管理"), "{d}");
        assert!(d.contains("## 右边从上到下\n\n| 名称 | 说明 |\n|---|---|\n| 第一句话 | "), "{d}");
        assert!(d.contains("## 这一页的键\n\n| 按键 | 作用 |\n|---|---|\n| j / k | "), "{d}");
        assert!(d.contains("## 每一页都能用的键\n\n| 按键 | 作用 |"), "{d}");
    }
}
