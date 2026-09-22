//! Setup's second language. The English text in the source is the key: the
//! UI translates at the moment it renders, so call sites stay plain English
//! and tests read English. Sentences with values use `{}` templates (the
//! captured values are translated too, when they are known phrases), and a
//! line made of `a · b · c` parts is translated part by part.

use std::borrow::Cow;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Lang {
    En,
    /// 简体中文
    Zh,
}

thread_local! {
    static LANG: Cell<Lang> = const { Cell::new(Lang::En) };
}

pub fn set(lang: Lang) {
    LANG.with(|l| l.set(lang));
}

pub fn current() -> Lang {
    LANG.with(Cell::get)
}

/// `--lang`, else `MOBIUS_LANG`, else the locale variables, else (macOS GUI
/// terminals often export no locale) the system's preferred language.
pub fn detect(flag: Option<Lang>) -> Lang {
    if let Some(lang) = flag {
        return lang;
    }
    let chinese = |v: &str| v.trim().to_ascii_lowercase().starts_with("zh");
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(v) = var("MOBIUS_LANG") {
        return if chinese(&v) { Lang::Zh } else { Lang::En };
    }
    for k in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = var(k) {
            return if chinese(&v) { Lang::Zh } else { Lang::En };
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let first = text.split(['(', ')', '"', ',', '\n', ' ']).find(|s| !s.is_empty());
        if first.is_some_and(chinese) {
            return Lang::Zh;
        }
    }
    Lang::En
}

/// `text` in the current language; unknown text is returned unchanged.
pub fn tr(text: &str) -> Cow<'_, str> {
    if current() == Lang::En || text.trim().is_empty() {
        return Cow::Borrowed(text);
    }
    translate(text, 0).map_or(Cow::Borrowed(text), Cow::Owned)
}

fn translate(text: &str, depth: u8) -> Option<String> {
    if let Some((_, zh)) = ZH.iter().find(|(en, _)| *en == text) {
        return Some((*zh).to_string());
    }
    for (en, zh) in ZH.iter().filter(|(en, _)| en.contains("{}")) {
        if let Some(caps) = capture(en, text) {
            let mut out = String::new();
            let mut caps = caps.into_iter();
            let mut pieces = zh.split("{}").peekable();
            while let Some(piece) = pieces.next() {
                out.push_str(piece);
                if pieces.peek().is_some() {
                    let cap = caps.next().unwrap_or_default();
                    let cap =
                        if depth < 2 { translate(cap, depth + 1).unwrap_or_else(|| cap.into()) } else { cap.into() };
                    out.push_str(&cap);
                }
            }
            return Some(out);
        }
    }
    if depth < 2 && text.contains(" · ") {
        let mut changed = false;
        let parts: Vec<String> = text
            .split(" · ")
            .map(|part| match translate(part, depth + 1) {
                Some(zh) => {
                    changed = true;
                    zh
                }
                None => part.to_string(),
            })
            .collect();
        return changed.then(|| parts.join(" · "));
    }
    None
}

/// The values standing in for each `{}` of `pattern` in `text`. A value
/// never spans a ` · ` the pattern does not have: such lines are translated
/// part by part instead.
fn capture<'a>(pattern: &str, text: &'a str) -> Option<Vec<&'a str>> {
    if text.contains(" · ") && !pattern.contains(" · ") {
        return None;
    }
    let parts: Vec<&str> = pattern.split("{}").collect();
    let mut rest = text.strip_prefix(parts[0])?;
    let mut caps = Vec::new();
    for (i, part) in parts.iter().enumerate().skip(1) {
        if i == parts.len() - 1 {
            let cap = rest.strip_suffix(part)?;
            if cap.is_empty() {
                return None;
            }
            caps.push(cap);
        } else {
            let at = rest.find(part)?;
            if at == 0 {
                return None;
            }
            caps.push(&rest[..at]);
            rest = &rest[at + part.len()..];
        }
    }
    Some(caps)
}

/// English → 简体中文. Product names, modes (PAPER / CONFIRM / LIVE), units
/// and command lines stay as they are.
static ZH: &[(&str, &str)] = &[
    // ── header, rail, keys ────────────────────────────────────────────────
    ("Solana · Jupiter arbitrage searcher", "Solana · Jupiter 套利搜索器"),
    ("v{} · research first", "v{} · 研究优先"),
    (
        "Nothing can sign or send a transaction until you explicitly unlock it.",
        "在你明确解锁之前，不会签名或发送任何交易。",
    ),
    ("中文界面：mobius-searcher --setup --lang zh", "English: mobius-searcher --setup --lang en"),
    ("First-run setup", "首次设置"),
    ("Setup", "设置"),
    ("about 2 minutes", "大约 2 分钟"),
    ("nothing is written until you save", "保存之前不会写入任何东西"),
    ("your current settings stay unless you change them", "你不改动的设置都保持原样"),
    ("Setup cancelled", "设置已取消"),
    ("nothing was written", "没有写入任何东西"),
    ("Setup saved", "设置已保存"),
    ("not started", "未启动"),
    ("cancelled", "已取消"),
    ("↑↓ move", "↑↓ 移动"),
    ("{} jump", "{} 直选"),
    ("enter select", "回车选择"),
    ("esc cancel", "Esc 取消"),
    ("←→ switch", "←→ 切换"),
    ("enter confirm", "回车确认"),
    ("ctrl-u clear", "Ctrl-U 清空"),
    ("hidden input", "输入不显示"),
    ("Yes", "是"),
    ("No", "否"),
    ("recommended", "推荐"),
    ("current", "当前"),
    ("saved (hidden)", "已保存（不显示）"),
    ("kept", "保持不变"),
    ("skipped", "已跳过"),
    ("Testing connections from this machine…", "正在从本机测试连接…"),
    ("Reading the wallet balance…", "正在读取钱包余额…"),
    // ── paths and steps ───────────────────────────────────────────────────
    ("Research mode", "研究模式"),
    ("Assisted trading", "辅助交易"),
    ("Advanced setup", "高级设置"),
    ("{} · {} steps", "{} · 共 {} 步"),
    ("Bot wallet", "机器人钱包"),
    ("Network", "网络"),
    ("Strategies", "策略"),
    ("Safety", "安全"),
    ("Permission", "交易授权"),
    ("Review & save", "检查并保存"),
    ("Mode & wallet", "模式和钱包"),
    ("Credentials", "密钥"),
    ("Network limits", "网络限额"),
    ("Profit guards", "利润门槛"),
    ("Risk limits", "风险限额"),
    ("Jito tips", "Jito 小费"),
    ("Feeds & scheduler", "数据源和调度"),
    ("Interface & execution", "界面和执行"),
    ("create one, connect one, watch one or skip", "新建、连接、只看或跳过"),
    ("public endpoints or your own providers", "公共节点或你自己的服务商"),
    ("which opportunities to scan", "扫描哪些机会"),
    ("route size and stop limits, derived for you", "单笔规模和止损限额，自动算好"),
    ("nothing is written before this", "这一步之前不会写入任何东西"),
    ("a dedicated signing wallet", "一个专用的签名钱包"),
    ("your own providers or public endpoints", "你自己的服务商或公共节点"),
    ("unlock CONFIRM with a typed phrase", "输入确认短语解锁 CONFIRM"),
    ("PAPER, CONFIRM or LIVE and the signer", "PAPER、CONFIRM 或 LIVE，以及签名钱包"),
    ("API keys and private endpoints", "API key 和私有节点"),
    ("endpoints, rates and timeouts", "节点地址、频率和超时"),
    ("routes, quote tokens and amounts", "路线、报价代币和金额"),
    ("minimum edge after every cost", "扣除所有成本后的最低利润"),
    ("sizes, fees and stop conditions", "规模、费用和停止条件"),
    ("tip policy and bounds", "小费策略和上下限"),
    ("on-chain feeds and quote pacing", "链上数据源和报价节奏"),
    ("display, storage and timeouts", "显示、存储和超时"),
    // ── start, review, save ───────────────────────────────────────────────
    ("MØBIUS-Searcher setup", "MØBIUS-Searcher 设置"),
    ("Nothing is written until you confirm the review at the end.", "在最后确认之前，不会写入任何文件。"),
    ("Current setup", "当前设置"),
    ("What would you like to do?", "你想做什么？"),
    ("Keep this setup", "保持现有设置"),
    ("Nothing is changed.", "不做任何改动。"),
    ("Change some settings", "修改部分设置"),
    ("Pick a part to change; everything else stays exactly as it is.", "选一部分来改，其余完全保持原样。"),
    ("Start over", "重新开始"),
    (
        "First-use setup from the shared defaults; this file is backed up first.",
        "从默认值重新走一遍首次设置；会先备份现有文件。",
    ),
    ("No changes", "没有改动"),
    ("start: mobius-searcher", "启动：mobius-searcher"),
    ("change later: mobius-searcher --setup", "以后修改：mobius-searcher --setup"),
    ("starting…", "正在启动…"),
    (
        "Check everything once more. Esc still leaves without writing anything.",
        "最后再核对一遍。现在按 Esc 仍然不会写入任何东西。",
    ),
    ("Review", "核对"),
    ("Save this setup?", "保存这份设置？"),
    ("Nothing was written.", "没有写入任何东西。"),
    ("Settings saved to {} (only what differs from the defaults)", "设置已保存到 {}（只写入与默认值不同的部分）"),
    ("Settings saved to {}", "设置已保存到 {}"),
    ("Secrets saved to {} (0600)", "密钥已保存到 {}（权限 0600）"),
    ("Bot wallet saved to {} (0600)", "机器人钱包已保存到 {}（权限 0600）"),
    ("Previous settings backed up to {}", "原来的设置已备份到 {}"),
    (
        "Back up the keypair file before funding it; a lost key cannot be recovered.",
        "充值之前先备份私钥文件；私钥丢失后无法找回。",
    ),
    (
        "Next: fund only the bot wallet, then approve each transaction in CONFIRM mode.",
        "下一步：只给机器人钱包充值，然后在 CONFIRM 模式下逐笔批准交易。",
    ),
    ("Start MØBIUS in {} mode now?", "现在以 {} 模式启动 MØBIUS？"),
    ("Setup complete", "设置完成"),
    ("No changes: {} already has these settings.", "没有改动：{} 已经是这些设置。"),
    ("What should MØBIUS be ready to do?", "你想让 MØBIUS 做好什么准备？"),
    (
        "Watch the market and simulate every route. Sending transactions stays locked.",
        "观察行情并模拟每条路线，发送交易保持锁定。",
    ),
    (
        "A dedicated bot wallet; every transaction waits for your approval (CONFIRM).",
        "使用专用机器人钱包；每笔交易都要等你批准（CONFIRM）。",
    ),
    (
        "Set every provider, strategy, limit and execution option yourself.",
        "自己设置每个服务商、策略、限额和执行选项。",
    ),
    // review rows
    ("Mode", "模式"),
    ("Wallet", "钱包"),
    ("Signer", "签名"),
    ("Venues", "交易所"),
    ("Route size", "单笔规模"),
    ("Stops", "止损"),
    ("Profit guard", "利润门槛"),
    ("Files", "文件"),
    ("new", "新建"),
    ("connected", "已连接"),
    ("none", "无"),
    ("watch only", "只看，不能签名"),
    ("virtual PAPER equity {} SOL", "虚拟 PAPER 资金 {} SOL"),
    ("virtual PAPER equity", "虚拟 PAPER 资金"),
    ("${} daily loss", "单日亏损 ${}"),
    ("{} failures in a row", "连续失败 {} 次"),
    ("{} bps slippage", "滑点 {} bps"),
    ("≥ ${} and ≥ {} bps after all costs", "扣除所有成本后 ≥ ${} 且 ≥ {} bps"),
    ("simulation only, sending locked", "只模拟，发送锁定"),
    ("sending unlocked for --mode confirm / live", "可用 --mode confirm / live 发送"),
    ("transaction submission unlocked", "交易发送已解锁"),
    ("Jupiter keyless", "Jupiter 免 key"),
    ("private RPC", "私有 RPC"),
    ("public RPC", "公共 RPC"),
    ("market data only", "只取行情"),
    ("none enabled", "一个都没开"),
    ("round-trip", "来回套利"),
    ("cross-DEX", "跨 DEX"),
    ("triangular", "三角套利"),
    ("at most {}% of equity", "最多占权益 {}%"),
    ("signing keypair", "签名私钥"),
    ("${} daily stop", "单日止损 ${}"),
    // ── the hub ───────────────────────────────────────────────────────────
    ("Review and save", "检查并保存"),
    ("See every change before anything is written.", "写入之前先看全部改动。"),
    ("Nothing to change", "没有要改的"),
    ("Leave without writing anything.", "不写入任何东西，直接退出。"),
    ("Which part do you want to change?", "你想改哪一部分？"),
    ("Network & API keys", "网络和 API key"),
    ("Markets & venues", "市场和交易所"),
    ("Safety limits", "安全限额"),
    ("Mode & transaction permission", "模式和交易授权"),
    ("Advanced", "高级"),
    ("Profit guards, risk details, Jito, feeds, interface.", "利润门槛、风险细节、Jito、数据源、界面。"),
    ("Keep current", "保持当前"),
    ("Update keys and endpoints", "更新 key 和节点地址"),
    ("Hidden input; Enter keeps each stored value, '-' clears it.", "输入不显示；回车保留已存的值，输入 '-' 清除。"),
    ("Change the proxy", "更改代理"),
    ("Test the connection", "测试连接"),
    (
        "Solana RPC, WebSocket, Jupiter, Jito and venues, from here.",
        "从本机测试 Solana RPC、WebSocket、Jupiter、Jito 和交易所。",
    ),
    ("Routes now start at {}", "现在单笔起步为 {}"),
    ("Adjust API endpoints and rate limits?", "调整 API 节点和请求频率吗？"),
    ("Adjust the profit guards?", "调整利润门槛吗？"),
    ("Adjust the risk limits?", "调整风险限额吗？"),
    ("Adjust the Jito tip policy?", "调整 Jito 小费策略吗？"),
    ("Adjust feeds and the quote scheduler?", "调整数据源和报价调度吗？"),
    ("Adjust the interface and execution settings?", "调整界面和执行设置吗？"),
    ("Keep the current values", "保持当前值"),
    ("Recommended unless you know what to change.", "除非你清楚要改什么，建议保持。"),
    ("Customize", "自定义"),
    ("Go through each value; Enter keeps the one shown.", "逐项设置；回车保留显示的值。"),
    ("No venues are configured.", "还没有配置交易所。"),
    ("no markets", "没有市场"),
    ("on", "开"),
    ("off", "关"),
    ("Back", "返回"),
    ("Keep the venues as they are.", "交易所保持原样。"),
    ("Which venue do you want to change?", "你想改哪个交易所？"),
    ("Use {} market data?", "使用 {} 的行情数据吗？"),
    ("Trading on venues stays off until their order connectors exist.", "交易所的下单功能在接口完成之前保持关闭。"),
    ("Operating mode", "运行模式"),
    ("Simulate only; nothing is ever sent.", "只模拟，永远不发送。"),
    ("Send only the transactions you approve one by one.", "只发送你逐笔批准的交易。"),
    ("Send automatically within the risk limits.", "在风险限额内自动发送。"),
    ("PAPER: sending is locked again", "PAPER：发送已重新锁定"),
    ("{} needs a signing wallet.", "{} 需要一个签名钱包。"),
    ("Type exactly: {}", "请准确输入：{}"),
    ("Type {} to unlock transaction submission", "输入 {} 以解锁交易发送"),
    (
        "This mode can send real transactions. The key itself is never copied into the config.",
        "这个模式会发送真实交易。私钥本身永远不会复制进配置文件。",
    ),
    // ── guided steps ──────────────────────────────────────────────────────
    (
        "Use a wallet that exists only for the bot, never your main wallet.",
        "用一个只给机器人用的钱包，绝不要用你的主钱包。",
    ),
    (
        "A new key stays in memory until you save, and is never shown or logged.",
        "新私钥在保存前只存在内存里，永远不会显示或写进日志。",
    ),
    (
        "Assisted trading works best with your own RPC. Public endpoints are fine for a first test.",
        "辅助交易最好用你自己的 RPC；先试一试的话公共节点也可以。",
    ),
    (
        "Public endpoints are enough to start; they are rate limited, so scans run slower.",
        "公共节点足够起步；它们有频率限制，扫描会慢一些。",
    ),
    ("Public endpoints", "公共节点"),
    ("Keyless: the built-in mainnet RPC and Jupiter access.", "免 key：内置的主网 RPC 和 Jupiter 访问。"),
    ("My own providers", "我自己的服务商"),
    (
        "Add a Jupiter API key and a private RPC. They are stored only in .env.",
        "填入 Jupiter API key 和私有 RPC，只保存在 .env 里。",
    ),
    ("How should MØBIUS reach Solana and Jupiter?", "MØBIUS 怎样连接 Solana 和 Jupiter？"),
    ("Credentials already in .env stay in use.", ".env 里已有的密钥继续使用。"),
    ("Route sizes come from the safety policy in the next step.", "单笔规模由下一步的安全档位决定。"),
    ("Which opportunities should MØBIUS scan?", "MØBIUS 要扫描哪些机会？"),
    ("Core routes", "核心路线"),
    ("SOL round-trips and price gaps between DEXes.", "SOL 来回套利和 DEX 之间的价差。"),
    ("Everything", "全部"),
    ("Also triangular cycles; needs noticeably more API requests.", "再加上三角套利；API 请求会明显增多。"),
    ("Round-trips only", "只做来回套利"),
    ("The fewest requests; a light first look.", "请求最少，适合先轻量看看。"),
    ("MØBIUS sizes every route for you; there is no amount to pick.", "MØBIUS 会替你决定每笔规模，不用自己选金额。"),
    (
        "Guards reject bigger routes and pause on repeated failures or daily losses.",
        "超出规模的路线会被拒绝；连续失败或当日亏损到限额时会暂停。",
    ),
    ("How cautiously should the bot start?", "机器人以多谨慎的方式起步？"),
    ("Guarded", "稳健"),
    ("Balanced", "均衡"),
    ("Routes up to {}% of equity", "单笔最多占权益 {}%"),
    ("pause after {} failures", "连续失败 {} 次暂停"),
    ("${} daily loss stop", "单日亏损 ${} 停止"),
    (
        "Routes start at {} SOL; wider slippage than {} bps is rejected",
        "单笔从 {} SOL 起步；滑点超过 {} bps 的路线会被拒绝",
    ),
    (
        "CONFIRM shows every transaction and sends nothing without your approval.",
        "CONFIRM 会展示每笔交易，没有你的批准什么都不发送。",
    ),
    ("The kill switch and every risk guard stay active.", "急停开关和所有风控保持生效。"),
    ("Allow MØBIUS to submit transactions you approve?", "允许 MØBIUS 发送你批准的交易吗？"),
    ("Unlock CONFIRM", "解锁 CONFIRM"),
    ("You will type a short phrase to confirm.", "需要输入一个短语来确认。"),
    ("Stay in PAPER for now", "暂时保持 PAPER"),
    ("Keep the wallet; switch later with --setup.", "保留钱包；以后用 --setup 切换。"),
    ("Type ENABLE CONFIRM to unlock approved transactions", "输入 ENABLE CONFIRM 以解锁你批准的交易"),
    ("CONFIRM unlocked: every transaction still needs your approval", "CONFIRM 已解锁：每笔交易仍需你批准"),
    ("Staying in PAPER; the bot wallet is kept for later.", "保持 PAPER；机器人钱包留着以后用。"),
    // ── wallet ────────────────────────────────────────────────────────────
    ("How should the bot wallet be prepared?", "机器人钱包怎么准备？"),
    ("Keep {}", "保留 {}"),
    ("Signing keypair {}", "签名私钥 {}"),
    ("Watch only; no signing access.", "只看，不能签名。"),
    ("Create a new bot wallet", "新建机器人钱包"),
    (
        "A fresh Solana keypair, saved privately (0600) when you save.",
        "新的 Solana 私钥，保存时以私有权限（0600）写入。",
    ),
    ("Use an existing keypair file", "使用已有私钥文件"),
    ("A Solana CLI keypair JSON that you control.", "你自己掌握的 Solana CLI 私钥 JSON 文件。"),
    ("Watch an address only", "只看某个地址"),
    ("Follow its balances; no signing access.", "跟踪它的余额，不能签名。"),
    ("No wallet for now", "暂不使用钱包"),
    ("Simulate with virtual PAPER equity.", "用虚拟 PAPER 资金模拟。"),
    ("Keeping wallet {}", "保留钱包 {}"),
    ("New bot wallet {}", "新机器人钱包 {}"),
    ("Written to {} when you save.", "保存时写入 {}。"),
    ("Enter the path of a keypair file", "请输入私钥文件路径"),
    ("Keypair file", "私钥文件"),
    ("Using bot wallet {}", "使用机器人钱包 {}"),
    ("Not a Solana address: {}", "不是有效的 Solana 地址：{}"),
    ("Wallet address to watch", "要跟踪的钱包地址"),
    ("base58 public key", "base58 公钥"),
    ("Watching {}; signing stays disabled", "正在跟踪 {}；签名保持关闭"),
    ("Virtual PAPER equity: {} SOL", "虚拟 PAPER 资金：{} SOL"),
    ("Balance {} SOL", "余额 {} SOL"),
    (
        "Balance unavailable right now; `mobius-searcher --doctor` shows it later.",
        "暂时读不到余额；稍后可用 `mobius-searcher --doctor` 查看。",
    ),
    (
        "Fund it before sending: at least {} SOL for fees, plus what it may trade. Scan with a Solana wallet app or copy the address.",
        "发送交易之前先充值：至少 {} SOL 用于手续费，再加上准备交易的金额。用 Solana 钱包 App 扫码，或复制下面的地址。",
    ),
    // ── proxy and connection test ─────────────────────────────────────────
    ("Automatic", "自动"),
    ("system proxy {}", "系统代理 {}"),
    ("no proxy found, connects directly", "没找到代理，直接连接"),
    ("No proxy", "不用代理"),
    ("always connect directly", "总是直接连接"),
    ("HTTP proxy {}", "HTTP 代理 {}"),
    ("Environment variables, else the system settings. Now: {}.", "先看环境变量，再看系统设置。当前：{}。"),
    ("How should MØBIUS connect to the internet?", "MØBIUS 怎样连接互联网？"),
    ("Detect the proxy automatically", "自动检测代理"),
    ("Connect directly", "直接连接"),
    ("Ignore every proxy setting.", "忽略所有代理设置。"),
    ("Use this HTTP proxy", "使用指定的 HTTP 代理"),
    ("An http://host:port proxy for every connection.", "所有连接都走一个 http://主机:端口 代理。"),
    ("Enter it as http://host:port", "请按 http://主机:端口 的格式输入"),
    ("HTTP proxy", "HTTP 代理"),
    ("{} did not answer. What now?", "{} 没有响应。接下来怎么办？"),
    ("Try another proxy setting", "换一种代理设置"),
    ("Re-enter keys and endpoints", "重新输入 key 和节点地址"),
    ("Hidden input; Enter keeps each stored value.", "输入不显示；回车保留已存的值。"),
    ("Test again", "再测一次"),
    ("After fixing something outside MØBIUS.", "在 MØBIUS 之外修好问题之后。"),
    ("Continue anyway", "仍然继续"),
    (
        "Save as is; `mobius-searcher --doctor` tests it again later.",
        "按现状保存；以后可用 `mobius-searcher --doctor` 再测。",
    ),
    (
        "Saved without a working connection; `mobius-searcher --doctor` tests it again.",
        "在连接不通的情况下保存了；可用 `mobius-searcher --doctor` 再测。",
    ),
    // ── credentials ───────────────────────────────────────────────────────
    ("Jupiter API key (optional)", "Jupiter API key（可选）"),
    ("Private RPC URL (optional)", "私有 RPC 地址（可选）"),
    ("Private RPC WebSocket URL (optional)", "私有 RPC WebSocket 地址（可选）"),
    ("Jito UUID (optional)", "Jito UUID（可选）"),
    ("Pyth / Hermes API key (optional)", "Pyth / Hermes API key（可选）"),
    ("enter keeps the current value", "回车保留当前值"),
    ("enter = keyless access", "回车 = 免 key 访问"),
    ("enter = public RPC", "回车 = 公共 RPC"),
    ("enter = public WebSocket", "回车 = 公共 WebSocket"),
    ("enter = none", "回车 = 不设置"),
    ("enter = on-chain oracle", "回车 = 链上预言机"),
    ("'-' clears", "输入 '-' 清除"),
    ("Must start with {}  ('-' clears the stored value)", "必须以 {} 开头（输入 '-' 清除已存的值）"),
    ("enter keeps {}", "回车保留 {}"),
    // ── advanced values ───────────────────────────────────────────────────
    ("{} (SOL)", "{}（SOL）"),
    ("{} (comma-separated)", "{}（逗号分隔）"),
    (
        "LIVE and CONFIRM send real transactions and need a signing wallet and a typed phrase.",
        "LIVE 和 CONFIRM 会发送真实交易，需要签名钱包并输入确认短语。",
    ),
    (
        "All optional. Values are hidden while typed and stored only in .env (0600).",
        "全部可选。输入时不显示，只保存在 .env（0600）里。",
    ),
    (
        "Endpoints, request rates and timeouts. The defaults fit Jupiter's free plan.",
        "节点地址、请求频率和超时。默认值适合 Jupiter 免费套餐。",
    ),
    ("Each strategy can be switched on or off; amounts are in SOL.", "每个策略都能单独开关；金额单位是 SOL。"),
    (
        "A route must clear these after fees, tips, slippage and a safety buffer.",
        "扣除手续费、小费、滑点和安全缓冲之后，路线必须达到这些门槛。",
    ),
    ("Hard limits checked before any route is simulated or sent.", "任何路线在模拟或发送之前都要先过这些硬性限额。"),
    ("Only used when a bundle is sent through Jito.", "只在通过 Jito 发送 bundle 时使用。"),
    (
        "Pool and oracle feeds let the scheduler re-quote only when something moved.",
        "有了池子和预言机数据源，调度器只在价格变动时重新报价。",
    ),
    ("Glyphs, colours, frame rate, data directory and execution timeouts.", "字形、颜色、帧率、数据目录和执行超时。"),
    ("Virtual PAPER equity", "虚拟 PAPER 资金"),
    ("Scan SOL → quote → SOL round-trips?", "扫描 SOL → 报价代币 → SOL 来回套利？"),
    ("Round-trip quote token", "来回套利的报价代币"),
    ("Round-trip amount", "来回套利金额"),
    ("Round-trip scheduler weight", "来回套利的调度权重"),
    ("Scan price gaps between DEXes?", "扫描 DEX 之间的价差？"),
    ("Cross-DEX quote token", "跨 DEX 的报价代币"),
    ("Cross-DEX amount", "跨 DEX 金额"),
    ("DEX labels", "DEX 名称"),
    ("Cross-DEX scheduler weight", "跨 DEX 的调度权重"),
    ("Scan triangular cycles?", "扫描三角套利？"),
    ("Triangular cycle", "三角套利路径"),
    ("Triangular amount", "三角套利金额"),
    ("Triangular scheduler weight", "三角套利的调度权重"),
    ("Minimum absolute profit", "最低绝对利润"),
    ("Minimum profit (bps)", "最低利润（bps）"),
    ("Minimum profit (USD)", "最低利润（USD）"),
    ("Expected slippage cost share (bps)", "预期滑点成本占比（bps）"),
    ("Additional safety buffer", "额外安全缓冲"),
    ("Safety buffer (bps)", "安全缓冲（bps）"),
    ("Compute-unit margin (bps)", "计算单元余量（bps）"),
    ("Maximum CU price (micro-lamports)", "最高 CU 单价（micro-lamports）"),
    ("Protect the final leg with a minimum output?", "为最后一腿设置最低输出保护？"),
    ("Maximum trade size", "最大单笔规模"),
    ("Maximum trade share of equity (bps)", "单笔占权益上限（bps）"),
    ("Maximum daily loss (USD)", "单日最大亏损（USD）"),
    ("Maximum consecutive failures", "最多连续失败次数"),
    ("Maximum slippage (bps)", "最大滑点（bps）"),
    ("Maximum quote age (ms)", "报价最长有效期（ms）"),
    ("Maximum simulation age (ms)", "模拟结果最长有效期（ms）"),
    ("Maximum priority fee (lamports)", "最高优先费（lamports）"),
    ("Maximum Jito tip (lamports)", "最高 Jito 小费（lamports）"),
    ("SOL reserved for fees and rent", "为手续费和租金预留的 SOL"),
    ("Maximum open executions", "最多同时执行数"),
    ("Maximum slot lag", "最大 slot 延迟"),
    ("Tip policy", "小费策略"),
    ("Fixed", "固定"),
    ("The same tip on every bundle.", "每个 bundle 用同样的小费。"),
    ("Percentile", "按百分位"),
    ("Follow recently landed tips.", "跟随最近成功上链的小费。"),
    ("Profit share", "按利润分成"),
    ("A share of the expected profit.", "预期利润的一部分。"),
    ("Fixed Jito tip (lamports)", "固定 Jito 小费（lamports）"),
    ("Landed-tip percentile", "成功小费的百分位"),
    ("Tip share of expected profit (bps)", "小费占预期利润比例（bps）"),
    ("Minimum Jito tip (lamports)", "最低 Jito 小费（lamports）"),
    ("Add Jito's dont-front account?", "加入 Jito 的 dont-front 防抢跑账户？"),
    ("Use on-chain pool and oracle feeds?", "使用链上池子和预言机数据源？"),
    ("Oracle source", "预言机来源"),
    (
        "No Pyth key is configured; Hermes falls back to on-chain prices.",
        "没有配置 Pyth key；Hermes 会退回使用链上价格。",
    ),
    ("Minimum feed sample interval (ms)", "数据源最短采样间隔（ms）"),
    ("Network-stat polling interval (ms)", "网络状态轮询间隔（ms）"),
    ("Quote scheduler", "报价调度器"),
    ("Maximum quote requests in flight", "同时进行的报价请求上限"),
    ("Full-route refresh floor (seconds)", "全路线刷新最短间隔（秒）"),
    ("Data directory", "数据目录"),
    ("Glyph mode", "字形模式"),
    ("Color mode", "颜色模式"),
    ("TUI frames per second", "界面每秒帧数"),
    ("Maximum graphs", "最多图表数"),
    ("Enable mouse controls?", "启用鼠标操作？"),
    ("Prefer one atomic transaction?", "优先用一笔原子交易？"),
    ("Manual confirmation timeout (ms)", "人工确认超时（ms）"),
    ("Bundle landing timeout (ms)", "Bundle 上链超时（ms）"),
    ("Jupiter base URL", "Jupiter 基础地址"),
    ("Jupiter requests per second", "Jupiter 每秒请求数"),
    ("Jupiter burst", "Jupiter 突发请求数"),
    ("Jupiter timeout (ms)", "Jupiter 超时（ms）"),
    ("Jupiter slippage (`rtse` or bps)", "Jupiter 滑点（`rtse` 或 bps）"),
    ("Fallback Solana RPC URL", "备用 Solana RPC 地址"),
    ("Fallback Solana RPC WebSocket URL", "备用 Solana RPC WebSocket 地址"),
    ("RPC requests per second", "RPC 每秒请求数"),
    ("RPC burst", "RPC 突发请求数"),
    ("Simulations per second", "每秒模拟次数"),
    ("RPC timeout (ms)", "RPC 超时（ms）"),
    ("Jito block-engine URL", "Jito block-engine 地址"),
    ("Jito tip-floor URL", "Jito tip-floor 地址"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn zh<T>(f: impl FnOnce() -> T) -> T {
        set(Lang::Zh);
        let out = f();
        set(Lang::En);
        out
    }

    #[test]
    fn english_is_the_identity_and_unknown_text_passes_through() {
        assert_eq!(tr("Research mode"), "Research mode");
        zh(|| {
            assert_eq!(
                tr("9N67XSEmZkYMtrRHvBLn3fBycGDGh47o2opNANJeHr7p"),
                "9N67XSEmZkYMtrRHvBLn3fBycGDGh47o2opNANJeHr7p"
            )
        });
    }

    #[test]
    fn templates_fill_values_and_translate_known_ones() {
        zh(|| {
            assert_eq!(tr("Research mode · 5 steps"), "研究模式 · 共 5 步");
            assert_eq!(
                tr("Routes start at 0.01 SOL; wider slippage than 50 bps is rejected"),
                "单笔从 0.01 SOL 起步；滑点超过 50 bps 的路线会被拒绝"
            );
            assert_eq!(tr("Round-trip amount (SOL)"), "来回套利金额（SOL）");
            assert_eq!(tr("Start MØBIUS in PAPER mode now?"), "现在以 PAPER 模式启动 MØBIUS？");
        });
    }

    #[test]
    fn dotted_lines_are_translated_part_by_part() {
        zh(|| {
            assert_eq!(
                tr("$2 daily loss · 3 failures in a row · 50 bps slippage"),
                "单日亏损 $2 · 连续失败 3 次 · 滑点 50 bps"
            );
            assert_eq!(tr("0.01 SOL · at most 1% of equity"), "0.01 SOL · 最多占权益 1%");
            assert_eq!(
                tr("↑↓ move · 1–3 jump · enter select · esc cancel"),
                "↑↓ 移动 · 1–3 直选 · 回车选择 · Esc 取消"
            );
        });
    }

    #[test]
    fn the_table_is_consistent() {
        let mut seen = std::collections::BTreeSet::new();
        for (en, zh) in ZH {
            assert!(seen.insert(*en), "duplicate entry {en:?}");
            assert_eq!(en.matches("{}").count(), zh.matches("{}").count(), "placeholders differ: {en:?}");
            assert!(!en.contains("{}{}"), "adjacent placeholders cannot be matched: {en:?}");
        }
        // the typed unlock phrases must never be translated
        zh(|| {
            assert_eq!(tr("ENABLE CONFIRM"), "ENABLE CONFIRM");
            assert_eq!(tr("ENABLE LIVE"), "ENABLE LIVE");
        });
    }
}
