//! What each page is and what its keys do, said in Chinese for an operator
//! who reads it (`?`). The English help is a list of keys; this one explains,
//! because the pages about the operator's own money are read by someone who
//! may have never seen a trading screen.

use crate::app::{App, Hit, Page};
use crate::chart::{text, width};
use crate::panels::{overlay, overlay_hint, wrap_words};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

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
                "账户",
                "市值是它这笔预算现在值多少，后面的百分比是相对预算的盈亏；清仓线是市值跌到这个数就全部卖出并永久结束（总止损）",
            ),
            Item(
                "这笔持仓",
                "持有 SOL 时：什么时候、什么价买的，拿了多久，现在浮动盈亏多少，离卖出线和止损线各差百分之几。没持仓时这里是它在等的买入线",
            ),
            Item("战绩", "已经平仓（买了又卖掉）的笔数、其中赚钱的笔数、已实现盈亏"),
            Item("行情、接下来", "这个币今天的涨跌和 24 小时高低；它下一次判断是几点、还有多久"),
            Item("市值走势", "它的市值一根线一格画出来：高于预算是绿色，低于是红色"),
            Item(
                "K 线",
                "交易所的实时行情。黄线是卖出线，白线是买入成本（没持仓时绿线是买入线），红色是止损线，▲ 是它买的位置，▼ 是它卖的位置。下面的柱子是成交量",
            ),
            Item(
                "算式",
                "“这些线是怎么算出来的”把规则的算法直接写了出来：取最近多少根线的平均价和波动，买入线和卖出线各是多少",
            ),
            Item("成交记录", "它每一次买入、卖出：时间、数量、成交价，卖出时这一笔赚了还是亏了。成交页（5）也有"),
            Item("记录", "“它做过什么”是它每一步的流水，回车看全部"),
            Head("这一页的键"),
            Item("j / k", "选上一个 / 下一个机器人（也可以用鼠标点）"),
            Item("s", "启动选中的机器人。会先问一次，按 y 才启动；启动后它在后台自己运行，关掉界面也继续"),
            Item("x", "停止。只是让它不再买卖，持有的东西原样留着；停着的时候它的止损也不生效"),
            Item(
                "b",
                "调预算：给它加钱或减钱（1 到 25 美元）。加钱先用钱包里空闲的 USDC，不够的部分卖出 SOL 来换；减钱是把 USDC 还给钱包。它已有的盈亏不变",
            ),
            Item("n", "新建一个机器人：选类型、填预算和止损，输入确认词后创建。创建后不会自动启动，要再按 s"),
            Item("c", "卖出并结束：把它持有的 SOL 卖成 USDC 留在钱包里，这一轮永久结束。要先停止（x）"),
            Item("[ ]", "换 K 线周期（1 秒到 1 天）。只是换个周期看价格，规则仍按它自己的周期判断"),
            Item("v", "K 线和折线之间切换"),
            Item("⏎", "看它的完整记录"),
            Item("p", "显示 / 隐藏纸面实验。纸面实验是用模拟账户试规则，不是真钱，默认不显示"),
            Head("要知道的"),
            Text(
                "机器人可能亏钱，没有任何保证。每次买卖有约千分之一到千分之二的成本，交易越频繁成本越多。它只在每根线收盘时判断一次；电脑休眠或关机时它什么都不会做，止损也不会。",
            ),
        ],
        Page::Wallet => vec![
            Text("这一页是你的 Solana 钱包：有多少钱、怎么收钱、怎么转出去、最近的进出。"),
            Head("页面上的内容"),
            Item("总值", "SOL 按现价折成美元，加上 USDC"),
            Item("机器人持有", "机器人占用的那部分也算在余额里，但不算进“可以转出”；把它转走，机器人就卖不出去了"),
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

/// The guide of the page the operator is on, over it.
pub fn overlay_zh(buf: &mut Buffer, area: Rect, app: &App) {
    let th = &app.theme;
    let bg = th.select_bg;
    let w = 104.min(area.width.saturating_sub(2));
    let (inner_w, key_w) = (w.saturating_sub(4), 12);
    // every line as it will be drawn: its text, its key column, how it is styled
    let mut rows: Vec<(String, String, u8)> = Vec::new();
    for line in guide(app.page).iter().chain(&EVERYWHERE) {
        match line {
            Head(h) => {
                rows.push((String::new(), String::new(), 0));
                rows.push((String::new(), h.to_string(), 1));
            }
            Text(t) => rows.extend(wrap_words(t, inner_w).into_iter().map(|l| (String::new(), l, 2))),
            Item(k, what) => {
                for (i, l) in wrap_words(what, inner_w.saturating_sub(key_w)).into_iter().enumerate() {
                    rows.push((if i == 0 { k.to_string() } else { String::new() }, l, 3));
                }
            }
        }
    }
    let room = area.height.saturating_sub(4) as usize;
    let cut = rows.len() > room;
    rows.truncate(room.saturating_sub(usize::from(cut)));
    let title = format!("说明 · {}", app.page.label_zh());
    let inner = overlay(buf, area, w, rows.len() as u16 + 2 + u16::from(cut), &title, th, &app.glyphs);
    app.hit(Rect { x: inner.x - 2, y: inner.y - 1, width: inner.width + 4, height: inner.height + 2 }, Hit::Overlay);
    let mut y = inner.y;
    for (key, body, kind) in &rows {
        match kind {
            1 => {
                text(buf, inner.x, y, body, inner.width, th.header(false).bg(bg));
            }
            3 => {
                text(buf, inner.x, y, key, key_w, th.accent().bg(bg).add_modifier(Modifier::BOLD));
                let x = inner.x + key_w.max(width(key) + 1);
                text(buf, x, y, body, inner.right().saturating_sub(x), th.text().bg(bg));
            }
            _ => {
                text(buf, inner.x, y, body, inner.width, th.text().bg(bg));
            }
        }
        y += 1;
    }
    if cut {
        text(buf, inner.x, y, "（窗口再高一些可以看到全部）", inner.width, th.faint().bg(bg));
    }
    overlay_hint(buf, inner, "任意键关闭", th);
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
    }
}
