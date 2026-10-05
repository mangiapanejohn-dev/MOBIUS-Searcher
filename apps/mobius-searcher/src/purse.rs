//! The wallet as the TUI's Wallet page shows it: what it holds, what came
//! and went, and sending SOL or USDC from it to another address.
//!
//! Reading needs only the address (`[wallet] pubkey`). Sending reads the key
//! file, and only at the moment a transfer the operator confirmed is sent; it
//! needs `execution.live_enabled`, as everything that sends does. A transfer
//! goes the way a swap of `--trade` goes: assembled, signed, simulated as
//! signed (any error and nothing is sent), then sent until it is confirmed or
//! its blockhash has expired.
//!
//! Nothing here is sent that was not checked first and shown to the operator:
//! `Send` is acted on only for the very request the last `Check` reviewed.

use crate::lab::trade::{Fate, Mainnet, Unsent};
use parking_lot::Mutex;
use searcher_core::address::well_known;
use searcher_core::config::Config;
use searcher_core::ix::{RawInstruction, ata_create_idempotent_ix, system_transfer_ix, token_transfer_checked_ix};
use searcher_core::{Address, Ts};
use searcher_execution::assemble::{ata, on_curve};
use searcher_market::RpcClient;
use searcher_telemetry::{LimiterConfig, Telemetry};
use searcher_tui::wallet::{
    Asset, Moved, Recipient, Review, SendRequest, Sending, WalletAction, WalletPort, WalletView,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Lamports of a signature.
const SIGNATURE_FEE: u64 = 5_000;
/// Lamports of priority fee a transfer pays: enough to be taken, not worth thinking about.
const PRIORITY_FEE: u64 = 2_000;
/// The least an account of no data may hold: a first transfer to a new address must bring it.
const WALLET_RENT: u64 = 890_880;
/// What opening a token account locks in it.
const TOKEN_ACCOUNT_RENT: u64 = 2_039_280;
/// Transactions of the history shown.
const HISTORY: usize = 12;
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Addresses kept of those it sent to.
const RECIPIENTS: usize = 6;

/// The addresses this wallet sent to before, newest first (a file of the data directory).
fn recipients(path: &Path) -> Vec<Recipient> {
    let kept: Vec<(String, i64, u32)> =
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    kept.into_iter().map(|(address, last, times)| Recipient { address, last, times }).collect()
}

/// `address` was sent to at `now`: it is the first of them, counted once more.
fn remember(path: &Path, address: &str, now: i64) -> Vec<Recipient> {
    let mut all = recipients(path);
    let times = all.iter().find(|r| r.address == address).map_or(0, |r| r.times) + 1;
    all.retain(|r| r.address != address);
    all.insert(0, Recipient { address: address.to_string(), last: now, times });
    all.truncate(RECIPIENTS);
    let kept: Vec<(&str, i64, u32)> = all.iter().map(|r| (r.address.as_str(), r.last, r.times)).collect();
    // (not kept, it is only typed again next time)
    let _ = serde_json::to_string(&kept).map(|json| std::fs::write(path, json));
    all
}

/// A request, checked: what is sent and how.
#[derive(Clone, Debug, PartialEq)]
struct Plan {
    asset: Asset,
    to: Address,
    atoms: u64,
    /// USDC: the wallet's token account it leaves from and the recipient's it goes to.
    accounts: Option<(Address, Address)>,
    /// USDC: the recipient's account is opened first.
    opens: bool,
}

impl Plan {
    fn cu_limit(&self) -> u32 {
        match (self.asset, self.opens) {
            (Asset::Sol, _) => 2_000,
            (Asset::Usdc, false) => 20_000,
            (Asset::Usdc, true) => 120_000,
        }
    }

    fn instructions(&self, me: Address) -> Vec<RawInstruction> {
        let Some((from, dest)) = self.accounts else { return vec![system_transfer_ix(me, self.to, self.atoms)] };
        let mint = well_known::addr(well_known::USDC_MINT);
        let mut ixs = Vec::new();
        if self.opens {
            ixs.push(ata_create_idempotent_ix(me, dest, self.to, mint));
        }
        ixs.push(token_transfer_checked_ix(from, mint, dest, me, self.atoms, 6));
        ixs
    }
}

/// `text` as atoms of something counted in `decimals`: digits and one point, nothing lost to rounding.
fn atoms(text: &str, decimals: usize) -> Option<u64> {
    let (whole, part) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if (whole.is_empty() && part.is_empty()) || !digits(whole) || !digits(part) || part.len() > decimals {
        return None;
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let part: u64 = format!("{part:0<decimals$}").parse().ok()?;
    whole.checked_mul(10u64.pow(decimals as u32))?.checked_add(part)
}

fn sol(lamports: u64) -> f64 {
    lamports as f64 / 1e9
}

/// One `getTransaction` (jsonParsed), as it changed `wallet`.
fn moved(tx: &Value, wallet: &str, signature: &str) -> Option<Moved> {
    let keys = tx.pointer("/transaction/message/accountKeys")?.as_array()?;
    let key = |k: &Value| k.get("pubkey").and_then(Value::as_str).or(k.as_str()).map(str::to_string);
    let at = keys.iter().position(|k| key(k).as_deref() == Some(wallet));
    let meta = tx.get("meta")?;
    let lamports = |which: &str| at.and_then(|i| meta.get(which)?.get(i)?.as_u64());
    let usdc = |which: &str| -> i128 {
        let held = meta.get(which).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        held.iter()
            .filter(|b| {
                b.get("owner").and_then(Value::as_str) == Some(wallet)
                    && b.get("mint").and_then(Value::as_str) == Some(well_known::USDC_MINT)
            })
            .filter_map(|b| b.pointer("/uiTokenAmount/amount")?.as_str()?.parse::<i128>().ok())
            .sum()
    };
    // USDC that arrives names only the wallet's token account: the wallet itself is then no key, and no SOL of its moved
    let lamports_moved = match (lamports("preBalances"), lamports("postBalances")) {
        (Some(pre), Some(post)) => post as i128 - pre as i128,
        _ => 0,
    };
    let usdc_moved = usdc("postTokenBalances") - usdc("preTokenBalances");
    if at.is_none() && usdc_moved == 0 {
        return None;
    }
    // the first key pays the fee
    let fee = if at == Some(0) { meta.get("fee").and_then(Value::as_u64).unwrap_or(0) } else { 0 };
    Some(Moved {
        at: tx.get("blockTime").and_then(Value::as_i64).unwrap_or(0) * 1000,
        sol: lamports_moved as f64 / 1e9,
        usdc: usdc_moved as f64 / 1e6,
        fee: sol(fee),
        failed: meta.get("err").is_some_and(|e| !e.is_null()),
        signature: signature.to_string(),
    })
}

pub struct Purse {
    cfg: Config,
    zh: bool,
    address: Address,
    rpc: Arc<RpcClient>,
    view: Arc<Mutex<WalletView>>,
    /// The request the operator was shown, and what sending it means.
    reviewed: Mutex<Option<(SendRequest, Plan)>>,
    /// Transactions already read, by signature: read once.
    seen: Mutex<HashMap<String, Moved>>,
    /// Where the addresses it sent to are kept.
    recipients: PathBuf,
}

impl Purse {
    /// `None`: no wallet is configured.
    pub fn new(cfg: &Config, zh: bool) -> Option<Purse> {
        let address: Address = cfg.wallet.pubkey.as_deref()?.parse().ok()?;
        let rpc = RpcClient::new(
            &cfg.rpc.resolved_url(),
            LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
            cfg.rpc.simulate_rps,
            Duration::from_millis(cfg.rpc.timeout_ms),
            Arc::new(Telemetry::new()),
        )
        .ok()?;
        let say = |en: &str, cn: &str| Some(if zh { cn } else { en }.to_string());
        let cannot_send = if !cfg.execution.live_enabled {
            say(
                "Nothing can be sent from here: execution.live_enabled is not true in your config, so this program sends no real transaction.",
                "这里不能转出：配置里 execution.live_enabled 不是 true，程序不会发出任何真实交易。",
            )
        } else if cfg.wallet.keypair_path.is_none() {
            say(
                "Nothing can be sent from here: your config names no key file ([wallet] keypair_path). The wallet can be read, not signed for.",
                "这里不能转出：配置里没有私钥文件（[wallet] keypair_path），只能查看，不能签名。",
            )
        } else {
            None
        };
        let view = WalletView {
            address: Some(address.to_string()),
            qr: crate::setup_ui::qr_matrix(&address.to_string()).unwrap_or_default(),
            reserve: sol(cfg.risk.min_wallet_sol_for_fees_lamports),
            cannot_send,
            recipients: recipients(&cfg.data_dir().join("recipients.json")),
            ..Default::default()
        };
        Some(Purse {
            cfg: cfg.clone(),
            zh,
            address,
            rpc: Arc::new(rpc),
            view: Arc::new(Mutex::new(view)),
            reviewed: Mutex::new(None),
            seen: Mutex::new(HashMap::new()),
            recipients: cfg.data_dir().join("recipients.json"),
        })
    }

    fn say(&self, en: impl Into<String>, cn: impl Into<String>) -> String {
        if self.zh { cn.into() } else { en.into() }
    }

    async fn balances(&self) -> Result<(u64, u64), String> {
        let lamports = self.rpc.get_balance(&self.address).await.map_err(|e| e.to_string())?;
        let usdc = self
            .rpc
            .token_balance(&self.address, &well_known::addr(well_known::USDC_MINT))
            .await
            .map_err(|e| e.to_string())?;
        Ok((lamports, usdc))
    }

    /// Read what the wallet holds into the view.
    pub async fn refresh(&self) {
        let read = self.balances().await;
        let mut v = self.view.lock();
        match read {
            Ok((lamports, usdc)) => {
                (v.sol, v.usdc, v.error) = (Some(sol(lamports)), Some(usdc as f64 / 1e6), None);
                v.read_at = Some(Ts::now().millis());
            }
            Err(e) => {
                v.error = Some(self.say(format!("the wallet could not be read: {e}"), format!("钱包读取失败：{e}")))
            }
        }
    }

    /// Read what came and went into the view: the wallet's own transactions
    /// and those of its USDC account (USDC that arrives names only that).
    pub async fn history(&self) {
        let usdc_account = ata(&self.address, &well_known::addr(well_known::USDC_MINT));
        // (confirmed, not the node's default of finalized: what was just sent is in the list at once)
        let mut recent: Vec<(i64, String)> = Vec::new();
        for of in [self.address, usdc_account] {
            let asked = self.rpc.call_background(
                "getSignaturesForAddress",
                json!([of.to_string(), {"limit": HISTORY, "commitment": "confirmed"}]),
            );
            let Ok((v, _)) = asked.await else { continue };
            for s in v.as_array().map(Vec::as_slice).unwrap_or_default() {
                if let Some(sig) = s.get("signature").and_then(Value::as_str) {
                    recent.push((s.get("blockTime").and_then(Value::as_i64).unwrap_or(0), sig.to_string()));
                }
            }
        }
        recent.sort_by(|a, b| b.cmp(a));
        recent.dedup_by(|a, b| a.1 == b.1);
        recent.truncate(HISTORY);
        let me = self.address.to_string();
        let how = json!({"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"});
        for (_, sig) in &recent {
            let known = self.seen.lock().contains_key(sig);
            if !known
                && let Ok((tx, _)) = self.rpc.call_background("getTransaction", json!([sig, how])).await
                && let Some(m) = moved(&tx, &me, sig)
            {
                self.seen.lock().insert(sig.clone(), m);
            }
            // shown as they are read: the list fills in
            let seen = self.seen.lock();
            self.view.lock().moved = recent.iter().filter_map(|(_, s)| seen.get(s).cloned()).collect();
        }
    }

    /// The account at `a` as the node gives it (jsonParsed), `None` when there is none.
    async fn account(&self, a: &Address) -> Result<Option<Value>, String> {
        let how = json!({"encoding": "jsonParsed", "commitment": "confirmed"});
        let (v, _) = self.rpc.call("getAccountInfo", json!([a.to_string(), how])).await.map_err(|e| e.to_string())?;
        Ok(v.get("value").filter(|x| !x.is_null()).cloned())
    }

    /// The wallet's USDC account that holds the most, and how much.
    async fn usdc_account(&self) -> Result<Option<(Address, u64)>, String> {
        let of = json!({"mint": well_known::USDC_MINT});
        let how = json!({"encoding": "jsonParsed", "commitment": "confirmed"});
        let (v, _) = self
            .rpc
            .call("getTokenAccountsByOwner", json!([self.address.to_string(), of, how]))
            .await
            .map_err(|e| e.to_string())?;
        let accounts = v.get("value").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        Ok(accounts
            .iter()
            .filter_map(|a| {
                let at: Address = a.get("pubkey")?.as_str()?.parse().ok()?;
                let amount = a.pointer("/account/data/parsed/info/tokenAmount/amount")?.as_str()?.parse().ok()?;
                Some((at, amount))
            })
            .max_by_key(|a: &(Address, u64)| a.1))
    }

    /// Check a request against the chain: what sending it would do, or why it cannot be sent.
    async fn check(&self, req: &SendRequest) -> Result<(Review, Plan), String> {
        if let Some(why) = self.view.lock().cannot_send.clone() {
            return Err(why);
        }
        let unit = req.asset.name();
        let to: Address = req.to.trim().parse().map_err(|_| {
            self.say(
                "That is not a Solana address (32 to 44 characters of base58). Paste it again: one wrong character and it is another address.",
                "这不是一个 Solana 地址（32–44 位 base58 字符）。请重新粘贴：错一位就是另一个地址。",
            )
        })?;
        if to == self.address {
            return Err(self.say("That is this wallet's own address.", "这是这个钱包自己的地址。"));
        }
        let amount = atoms(req.amount.trim(), req.asset.decimals()).filter(|a| *a > 0).ok_or_else(|| {
            self.say(
                format!(
                    "The amount is not a number of {unit} (at most {} decimals, more than zero).",
                    req.asset.decimals()
                ),
                format!("数量不是有效的 {unit} 数字（最多 {} 位小数，且大于零）。", req.asset.decimals()),
            )
        })?;
        let unread =
            |e: String| self.say(format!("the chain could not be read: {e}"), format!("链上数据读取失败：{e}"));
        let (lamports, _) = self.balances().await.map_err(unread)?;
        let there = self.account(&to).await.map_err(unread)?;
        let owner = there.as_ref().and_then(|a| a.get("owner")?.as_str()).unwrap_or(well_known::SYSTEM_PROGRAM);
        if there.as_ref().is_some_and(|a| a.get("executable").and_then(Value::as_bool) == Some(true)) {
            return Err(self.say(
                "That address is a program, not a wallet. Nothing sent to it could be taken out again.",
                "这个地址是一个程序，不是钱包。转进去的钱取不出来。",
            ));
        }
        if owner == well_known::TOKEN_PROGRAM || owner == TOKEN_2022 {
            return Err(self.say(
                "That address is a token account, not a wallet. Give the wallet's own address: its USDC account is found from it.",
                "这个地址是一个代币账户，不是钱包地址。请填对方钱包的主地址，程序会自己找到它的 USDC 账户。",
            ));
        }
        if owner != well_known::SYSTEM_PROGRAM {
            return Err(self.say(
                format!("That address is an account of the program {owner}, not a wallet."),
                format!("这个地址是程序 {owner} 的账户，不是钱包。"),
            ));
        }
        if !on_curve(&to) {
            return Err(self.say(
                "That address is not a wallet's: no key can exist for it (it is a program's derived address).",
                "这个地址不是普通钱包地址：它没有对应的私钥（是程序派生地址）。",
            ));
        }
        let held = there.as_ref().and_then(|a| a.get("lamports")?.as_u64());
        let mut recipient = match held {
            Some(l) => self
                .say(format!("a wallet that holds {:.6} SOL", sol(l)), format!("已有的钱包，持有 {:.6} SOL", sol(l))),
            None => self.say(
                "an address the chain has never seen: new, or mistyped",
                "链上还没有记录的新地址：可能是新钱包，也可能填错了",
            ),
        };
        let fee = SIGNATURE_FEE + PRIORITY_FEE;
        let mut notes = Vec::new();
        let (plan, spent, left_usdc) = match req.asset {
            Asset::Sol => {
                if held.is_none() && amount < WALLET_RENT {
                    return Err(self.say(
                        format!("A new address must receive at least {:.6} SOL the first time (the least the chain lets an account hold).", sol(WALLET_RENT)),
                        format!("对方是新地址，第一次转入至少要 {:.6} SOL（链上规定的账户最低余额）。", sol(WALLET_RENT)),
                    ));
                }
                let usdc = self.view.lock().usdc.unwrap_or(0.0);
                (Plan { asset: Asset::Sol, to, atoms: amount, accounts: None, opens: false }, amount + fee, usdc)
            }
            Asset::Usdc => {
                let Some((from, has)) = self.usdc_account().await.map_err(unread)? else {
                    return Err(self.say("The wallet holds no USDC.", "钱包里没有 USDC。"));
                };
                if amount > has {
                    return Err(self.say(
                        format!("The wallet holds {:.6} USDC: less than that.", has as f64 / 1e6),
                        format!("余额不够：钱包里只有 {:.6} USDC。", has as f64 / 1e6),
                    ));
                }
                let dest = ata(&to, &well_known::addr(well_known::USDC_MINT));
                let opens = self.account(&dest).await.map_err(unread)?.is_none();
                recipient += &match (opens, self.zh) {
                    (true, false) => "; it has no USDC account yet".to_string(),
                    (true, true) => "；还没有 USDC 账户".to_string(),
                    (false, false) => "; it has a USDC account".to_string(),
                    (false, true) => "；已有 USDC 账户".to_string(),
                };
                let plan = Plan { asset: Asset::Usdc, to, atoms: amount, accounts: Some((from, dest)), opens };
                (plan, fee + if opens { TOKEN_ACCOUNT_RENT } else { 0 }, (has - amount) as f64 / 1e6)
            }
        };
        if spent > lamports {
            return Err(self.say(
                format!("The wallet holds {:.6} SOL; this takes {:.6} with its fee.", sol(lamports), sol(spent)),
                format!("SOL 不够：钱包有 {:.6} SOL，这笔连网络费要 {:.6}。", sol(lamports), sol(spent)),
            ));
        }
        let left = lamports - spent;
        if left > 0 && left < WALLET_RENT {
            return Err(self.say(
                format!("It would leave {:.6} SOL, under the {:.6} the chain lets an account hold. Send less, so that at least that much stays.", sol(left), sol(WALLET_RENT)),
                format!("转出后只剩 {:.6} SOL，低于链上规定的账户最低余额 {:.6}。请少转一点，至少留下这么多。", sol(left), sol(WALLET_RENT)),
            ));
        }
        let reserve = self.cfg.risk.min_wallet_sol_for_fees_lamports;
        if left < reserve {
            notes.push(self.say(
                format!("It leaves {:.6} SOL, under the {:.6} this program keeps for fees: the bot and the arbitrage will stop sending until there is more.", sol(left), sol(reserve)),
                format!("转出后只剩 {:.6} SOL，低于程序为手续费保留的 {:.6}：机器人和套利会因为付不起手续费而停下，直到再充入 SOL。", sol(left), sol(reserve)),
            ));
        }
        if held.is_none() {
            notes.push(self.say(
                "The chain has never seen this address. If it is not a wallet you just made, it is probably mistyped, and what is sent to a wrong address is gone.",
                "链上从来没见过这个地址。如果它不是你刚新建的钱包，多半是填错了；转到错误地址的钱找不回来。",
            ));
        }
        let review = Review {
            request: req.clone(),
            to: to.to_string(),
            amount: amount as f64 / 10f64.powi(req.asset.decimals() as i32),
            recipient,
            fee: sol(fee),
            opens_account: plan.opens.then(|| sol(TOKEN_ACCOUNT_RENT)),
            left_sol: sol(left),
            left_usdc,
            notes,
        };
        Ok((review, plan))
    }

    /// Check `req` and put the answer on the page.
    pub async fn review(&self, req: SendRequest) {
        self.view.lock().sending = Some(Sending::Checking(req.clone()));
        let answer = self.check(&req).await;
        // walked away from while it was being checked: nothing to show
        if self.view.lock().sending != Some(Sending::Checking(req.clone())) {
            return;
        }
        let shown = match answer {
            Ok((review, plan)) => {
                *self.reviewed.lock() = Some((req, plan));
                Sending::Reviewed(review)
            }
            Err(why) => Sending::Refused(req, why),
        };
        self.view.lock().sending = Some(shown);
    }

    /// Send the request the page shows as reviewed, and nothing else.
    pub async fn send(&self, req: SendRequest) {
        let review = {
            let (mut view, mut reviewed) = (self.view.lock(), self.reviewed.lock());
            let shown = match &view.sending {
                Some(Sending::Reviewed(r)) if r.request == req => r.clone(),
                _ => return,
            };
            if reviewed.as_ref().is_none_or(|(asked, _)| *asked != req) {
                return;
            }
            // from here on it is being sent: a second `Send` finds nothing reviewed
            *reviewed = None;
            view.sending = Some(Sending::Sending(shown.clone()));
            shown
        };
        let end = self.dispatch(&req, review).await;
        // an address that money arrived at is one to offer the next time
        if let Sending::Done { review, .. } = &end {
            let known = remember(&self.recipients, &review.to, Ts::now().millis());
            self.view.lock().recipients = known;
        }
        self.view.lock().sending = Some(end);
        self.refresh().await;
        self.history().await;
    }

    async fn dispatch(&self, req: &SendRequest, review: Review) -> Sending {
        let failed = |why: String, signature| Sending::Failed { review: review.clone(), why, signature };
        // checked again at the moment of sending: the chain may have moved since it was shown
        let plan = match self.check(req).await {
            Ok((_, plan)) => plan,
            Err(why) => return failed(why, None),
        };
        let chain = match Mainnet::sender(&self.cfg) {
            Ok(c) if c.taker() == self.address => c,
            Ok(_) => {
                return failed(
                    self.say(
                        "The key file is not this wallet's: nothing was sent.",
                        "私钥文件不是这个钱包的：没有发出。",
                    ),
                    None,
                );
            }
            Err(e) => {
                return failed(
                    self.say(
                        format!("The key could not be read: {e:#}. Nothing was sent."),
                        format!("私钥读取失败：{e:#}。没有发出。"),
                    ),
                    None,
                );
            }
        };
        let cu_limit = plan.cu_limit();
        let price = PRIORITY_FEE * 1_000_000 / u64::from(cu_limit);
        match chain.send_plain(plan.instructions(self.address), cu_limit, price).await {
            Ok((signature, Fate::Confirmed, _)) => Sending::Done { review: review.clone(), signature },
            Ok((signature, Fate::Failed(err), _)) => failed(
                self.say(
                    format!("It reached the chain and failed there ({err}). Its fee is paid; nothing was sent."),
                    format!("交易上链了，但执行失败（{err}）。网络费已付，钱没有转出。"),
                ),
                Some(signature),
            ),
            Ok((signature, Fate::Expired, _)) => failed(
                self.say(
                    "It did not reach the chain before it expired. Nothing was sent and nothing was paid; it can be sent again.",
                    "交易在有效期内没有上链。钱没有转出，也没有扣费；可以重新发一次。",
                ),
                Some(signature),
            ),
            Ok((signature, Fate::Unknown, _)) => failed(
                self.say(
                    "Nothing was heard of it in two minutes. Do not send it again yet: look the signature up in a block explorer first, it may still have arrived.",
                    "两分钟内没有等到结果。先不要重发：用下面的签名到区块浏览器查一下，它可能已经到账。",
                ),
                Some(signature),
            ),
            Err(Unsent::Simulation(why)) => failed(
                self.say(
                    format!("Its simulation failed, so it was not sent and nothing moved: {why}"),
                    format!("发出前的模拟没有通过，所以没有发出，钱没动：{why}"),
                ),
                None,
            ),
            Err(Unsent::Other(why)) => {
                failed(self.say(format!("It was not sent: {why}"), format!("没有发出：{why}")), None)
            }
        }
    }

    /// One of the page's wishes.
    async fn act(&self, action: WalletAction) {
        match action {
            WalletAction::Check(req) => self.review(req).await,
            WalletAction::Send(req) => self.send(req).await,
            WalletAction::Refresh => {
                self.refresh().await;
                self.history().await;
            }
            WalletAction::Clear => {
                // a transfer that is being sent stays on the page until its end is known
                let mut v = self.view.lock();
                if !matches!(v.sending, Some(Sending::Sending(_))) {
                    v.sending = None;
                    *self.reviewed.lock() = None;
                }
            }
        }
    }
}

/// The TUI's way to the wallet. Reading and sending go on on a thread of
/// their own, which ends when the page lets go of the port.
pub fn port(cfg: &Config, zh: bool) -> WalletPort {
    let Some(purse) = Purse::new(cfg, zh).map(Arc::new) else {
        return WalletPort { view: Arc::new(WalletView::default), act: Arc::new(|_| {}) };
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WalletAction>();
    let (reader, worker) = (purse.clone(), purse);
    let _ = std::thread::Builder::new().name("wallet-chain".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let watch = worker.clone();
            // what it holds every ten seconds, what came and went every minute
            let watching = tokio::spawn(async move {
                let mut round = 0u64;
                loop {
                    watch.refresh().await;
                    if round % 6 == 0 {
                        watch.history().await;
                    }
                    round = round.wrapping_add(1);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            });
            while let Some(action) = rx.recv().await {
                // forgetting is done at once; the rest take their time, each in its turn
                if action == WalletAction::Clear {
                    worker.act(action).await;
                } else {
                    let w = worker.clone();
                    tokio::spawn(async move { w.act(action).await });
                }
            }
            watching.abort();
        });
    });
    WalletPort {
        view: Arc::new(move || reader.view.lock().clone()),
        act: Arc::new(move |action| drop(tx.send(action))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_amount_is_read_to_the_last_unit_or_not_at_all() {
        assert_eq!(atoms("0.05", 9), Some(50_000_000));
        assert_eq!(atoms("2.005052", 6), Some(2_005_052));
        assert_eq!(atoms("3", 6), Some(3_000_000));
        assert_eq!(atoms(".5", 6), Some(500_000));
        assert_eq!(atoms("1.", 6), Some(1_000_000));
        assert_eq!(atoms("0.128220999", 9), Some(128_220_999));
        // more decimals than the chain counts, signs, words, nothing: refused, never rounded
        for bad in ["0.0000001", "-1", "1e3", "1,5", "", ".", "abc", "1.2.3", "99999999999999999999"] {
            assert_eq!(atoms(bad, 6), None, "{bad}");
        }
    }

    const ME: &str = "Examp1eWa11etAddressForTestsNotARea1Wa11et11";

    #[test]
    fn a_transaction_is_read_as_what_it_changed_in_the_wallet() {
        // a swap this wallet paid for: USDC out, SOL in, its fee inside the SOL
        let swap = json!({
            "blockTime": 1_790_000_000i64,
            "transaction": {"message": {"accountKeys": [{"pubkey": ME}, {"pubkey": "Other111"}]}},
            "meta": {
                "err": null, "fee": 7000,
                "preBalances": [171_411_575u64, 5], "postBalances": [187_946_575u64, 5],
                "preTokenBalances": [{"owner": ME, "mint": well_known::USDC_MINT, "uiTokenAmount": {"amount": "2005052"}}],
                "postTokenBalances": [
                    {"owner": ME, "mint": well_known::USDC_MINT, "uiTokenAmount": {"amount": "5052"}},
                    {"owner": "Other111", "mint": well_known::USDC_MINT, "uiTokenAmount": {"amount": "999"}}
                ]
            }
        });
        let m = moved(&swap, ME, "sig1").unwrap();
        assert_eq!((m.at, m.failed, m.signature.as_str()), (1_790_000_000_000, false, "sig1"));
        assert!(
            (m.sol - 0.016535).abs() < 1e-9 && (m.usdc + 2.0).abs() < 1e-9 && (m.fee - 0.000007).abs() < 1e-12,
            "{m:?}"
        );
        assert_eq!(m.kind(false), "swap");
        // SOL that another sent: this wallet is not the payer, so no fee is its own
        let received = json!({
            "blockTime": 1,
            "transaction": {"message": {"accountKeys": ["Sender111", ME]}},
            "meta": {"err": null, "fee": 5000, "preBalances": [10, 0], "postBalances": [4, 200_000_000u64]}
        });
        let m = moved(&received, ME, "sig2").unwrap();
        assert_eq!((m.sol, m.usdc, m.fee, m.kind(true)), (0.2, 0.0, 0.0, "收到"));
        // USDC that arrived names only the wallet's token account: it is told by whose the account is
        let usdc_in = json!({
            "blockTime": 1,
            "transaction": {"message": {"accountKeys": ["Sender111", "TokenAcct1"]}},
            "meta": {"err": null, "fee": 5000, "preBalances": [10, 2_039_280], "postBalances": [4, 2_039_280],
                "preTokenBalances": [],
                "postTokenBalances": [{"owner": ME, "mint": well_known::USDC_MINT, "uiTokenAmount": {"amount": "1500000"}}]}
        });
        let m = moved(&usdc_in, ME, "sig3").unwrap();
        assert_eq!((m.sol, m.usdc, m.fee, m.kind(false)), (0.0, 1.5, 0.0, "received"));
        // a transaction that has nothing of this wallet's is not its history
        let other = json!({
            "blockTime": 1,
            "transaction": {"message": {"accountKeys": ["A", "B"]}},
            "meta": {"err": null, "fee": 5000, "preBalances": [10, 0], "postBalances": [4, 1]}
        });
        assert_eq!(moved(&other, ME, "sig4"), None);
    }

    /// The Wallet page of this machine's wallet, as its terminal shows it: read from the chain, nothing sent.
    /// `WALLET=<address> cargo test -p mobius-searcher --lib wallet_page_of_this_machine -- --ignored --nocapture`
    /// (`ZH=1` in Chinese, `SIZE=120x40`, `KEYS=s`, `HTML=file` for a picture).
    #[test]
    #[ignore]
    fn the_wallet_page_of_this_machine() {
        let size = std::env::var("SIZE").unwrap_or_else(|_| "200x58".into());
        let zh = std::env::var("ZH").is_ok();
        let (w, h) = size.split_once('x').unwrap();
        let mut cfg = Config::default();
        cfg.wallet.pubkey = Some(std::env::var("WALLET").expect("WALLET: the address to look at"));
        let opts = searcher_tui::TuiOptions {
            bots: Some(crate::lab::desk::port(&cfg, None, zh)),
            wallet: Some(port(&cfg, zh)),
            zh,
            ..searcher_tui::TuiOptions::default()
        };
        let mut app = searcher_tui::App::new(&opts);
        app.glyphs = searcher_tui::theme::Glyphs::unicode();
        app.theme = searcher_tui::theme::Theme::with_depth(searcher_tui::theme::Depth::TrueColor);
        app.cex = Some(searcher_tui::cex::Cex::start(searcher_tui::cex::OkxSource::default(), 0, 1));
        // the chain is read on a thread of its own: give it the time a terminal would
        std::thread::sleep(Duration::from_secs(std::env::var("WAIT").ok().and_then(|s| s.parse().ok()).unwrap_or(12)));
        app.wallet = opts.wallet.as_ref().map(|p| searcher_tui::wallet::Wallet::fixed((p.view)()));
        let vm = searcher_tui::ViewModel::new(true);
        let key = |c| {
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )
        };
        app.on_key(key('0'), &vm);
        for c in std::env::var("KEYS").unwrap_or_default().chars() {
            app.on_key(key(c), &vm);
        }
        let buf = searcher_tui::snapshot(&mut app, &vm, w.parse().unwrap(), h.parse().unwrap());
        if let Ok(to) = std::env::var("HTML") {
            std::fs::write(to, searcher_tui::buffer_html(&buf, "wallet")).unwrap();
        }
        println!("{}", searcher_tui::buffer_text(&buf));
    }

    /// Transfers end to end on a local validator, with test SOL and a USDC of the validator's own
    /// (`scripts/wallet_local.py DIR` prepares both and prints how to run this).
    #[tokio::test]
    #[ignore]
    async fn transfers_arrive_on_a_local_validator_and_what_is_refused_sends_nothing() {
        let url = std::env::var("MOBIUS_TEST_VALIDATOR").expect("MOBIUS_TEST_VALIDATOR: a local validator's RPC URL");
        assert!(url.contains("127.0.0.1") || url.contains("localhost"), "a local validator only: {url}");
        let key = std::env::var("MOBIUS_TEST_KEY").expect("MOBIUS_TEST_KEY: the key the validator's USDC is minted by");
        let me = searcher_execution::wallet::Wallet::load(std::path::Path::new(&key), None).unwrap().pubkey();
        let mut cfg = Config::default();
        (cfg.rpc.url, cfg.rpc.url_env) = (url, String::new());
        cfg.execution.live_enabled = true;
        (cfg.wallet.keypair_path, cfg.wallet.pubkey) = (Some(key), Some(me.to_string()));
        cfg.jito.block_engine_url = "http://127.0.0.1:1".into(); // nobody there: the node takes it
        cfg.risk.min_wallet_sol_for_fees_lamports = 20_000_000;
        // what a transfer leaves behind (the addresses it went to) is kept in a directory of this test's own
        let data = std::env::temp_dir().join(format!("mobius-purse-test-{}", std::process::id()));
        std::fs::create_dir_all(&data).unwrap();
        cfg.general.data_dir = data.display().to_string();
        let purse = Purse::new(&cfg, false).unwrap();
        let usdc = well_known::addr(well_known::USDC_MINT);
        let lamports = |of: Address| {
            let rpc = purse.rpc.clone();
            async move { rpc.get_balance(&of).await.unwrap() }
        };
        let tokens = |of: Address| {
            let rpc = purse.rpc.clone();
            async move { rpc.token_balance(&of, &usdc).await.unwrap() }
        };
        let sending = || purse.view.lock().sending.clone();
        let ask = |asset, to: &str, amount: &str| SendRequest { asset, to: to.to_string(), amount: amount.to_string() };

        // test SOL from the faucet; test USDC minted by the key itself, into the wallet's own account
        purse.rpc.call("requestAirdrop", json!([me.to_string(), 2_000_000_000u64])).await.unwrap();
        for _ in 0..60 {
            if lamports(me).await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let chain = Mainnet::sender(&cfg).unwrap();
        let mine = ata(&me, &usdc);
        if tokens(me).await == 0 {
            let meta =
                |pubkey, is_signer, is_writable| searcher_core::ix::RawAccountMeta { pubkey, is_signer, is_writable };
            let mint_to = RawInstruction {
                program_id: well_known::addr(well_known::TOKEN_PROGRAM),
                accounts: vec![meta(usdc, false, true), meta(mine, false, true), meta(me, true, false)],
                data: [vec![7u8], 10_000_000u64.to_le_bytes().to_vec()].concat(),
            };
            let (_, fate, _) = chain
                .send_plain(vec![ata_create_idempotent_ix(me, mine, me, usdc), mint_to], 120_000, 1)
                .await
                .unwrap();
            assert_eq!(
                fate,
                Fate::Confirmed,
                "the wallet's USDC account, at the address it is derived to, and 10 USDC in it"
            );
        }
        purse.refresh().await;
        let (start_sol, start_usdc) = (lamports(me).await, tokens(me).await);
        println!("the wallet: {me} holds {:.6} SOL and {:.6} USDC", sol(start_sol), start_usdc as f64 / 1e6);
        assert_eq!(purse.view.lock().usdc, Some(start_usdc as f64 / 1e6));
        assert!(start_usdc >= 5_000_000 && start_sol > 1_000_000_000);

        // 1. what cannot be sent is refused with its reason, and nothing leaves
        let new = || searcher_execution::GeneratedWallet::new().pubkey().to_string();
        let (other, me_s) = (new(), me.to_string());
        for (req, why) in [
            (ask(Asset::Sol, "not-an-address", "0.1"), "not a Solana address"),
            (ask(Asset::Sol, &me_s, "0.1"), "own address"),
            (ask(Asset::Sol, &other, "0"), "not a number of SOL"),
            (ask(Asset::Sol, &other, "0.0000000001"), "not a number of SOL"),
            (ask(Asset::Sol, &other, "0.0001"), "at least 0.000891 SOL the first time"),
            (ask(Asset::Sol, &other, "5"), "this takes 5.000007 with its fee"),
            (ask(Asset::Sol, well_known::TOKEN_PROGRAM, "0.1"), "is a program"),
            (ask(Asset::Sol, &mine.to_string(), "0.1"), "is a token account"),
            (ask(Asset::Usdc, &usdc.to_string(), "1"), "is a token account"),
            (ask(Asset::Usdc, &other, "1000"), "less than that"),
            (ask(Asset::Usdc, &mine.to_string(), "1"), "is a token account"),
        ] {
            purse.review(req.clone()).await;
            match sending() {
                Some(Sending::Refused(asked, said)) => {
                    assert!(asked == req && said.contains(why), "{req:?}: {said}");
                    println!("refused: {} {} → {said}", req.amount, req.asset.name());
                }
                other => panic!("{req:?} was not refused: {other:?}"),
            }
            // refused, it cannot be sent either
            purse.send(req).await;
            assert!(matches!(sending(), Some(Sending::Refused(..))));
        }
        assert_eq!((lamports(me).await, tokens(me).await), (start_sol, start_usdc), "nothing left the wallet");

        // 2. SOL to an address the chain has never seen: said, and sent only as reviewed
        let quarter = ask(Asset::Sol, &other, "0.25");
        purse.send(quarter.clone()).await;
        assert!(!matches!(sending(), Some(Sending::Sending(_) | Sending::Done { .. })), "not reviewed: not sent");
        purse.review(quarter.clone()).await;
        let Some(Sending::Reviewed(r)) = sending() else { panic!("{:?}", sending()) };
        assert_eq!((r.to.as_str(), r.amount, r.fee, r.opens_account), (other.as_str(), 0.25, 0.000007, None));
        assert!((r.left_sol - sol(start_sol - 250_007_000)).abs() < 1e-9, "{r:?}");
        assert!(r.recipient.contains("never seen") && r.notes.iter().any(|n| n.contains("never seen")), "{r:?}");
        // another amount than the one that was shown is not sent
        purse.send(ask(Asset::Sol, &other, "0.26")).await;
        assert!(matches!(sending(), Some(Sending::Reviewed(_))));
        purse.send(quarter.clone()).await;
        let Some(Sending::Done { signature, .. }) = sending() else { panic!("{:?}", sending()) };
        println!("2. 0.25 SOL → {other}: {signature}");
        let to: Address = other.parse().unwrap();
        assert_eq!((lamports(to).await, lamports(me).await), (250_000_000, start_sol - 250_007_000));
        // sent, it is not sent a second time
        purse.send(quarter).await;
        assert_eq!(lamports(to).await, 250_000_000);

        // 3. USDC to a wallet that has no account for it: opened at this wallet's expense, which is said
        let first = ask(Asset::Usdc, &other, "1.5");
        purse.review(first.clone()).await;
        let Some(Sending::Reviewed(r)) = sending() else { panic!("{:?}", sending()) };
        assert_eq!(
            (r.amount, r.opens_account, r.left_usdc),
            (1.5, Some(0.00203928), (start_usdc - 1_500_000) as f64 / 1e6)
        );
        assert!(r.recipient.contains("holds 0.250000 SOL") && r.recipient.contains("no USDC account yet"), "{r:?}");
        purse.send(first).await;
        let Some(Sending::Done { signature, .. }) = sending() else { panic!("{:?}", sending()) };
        println!("3. 1.5 USDC → {other}, its account opened: {signature}");
        assert_eq!((tokens(to).await, tokens(me).await), (1_500_000, start_usdc - 1_500_000));
        assert_eq!(lamports(me).await, start_sol - 250_007_000 - 7_000 - TOKEN_ACCOUNT_RENT);

        // 4. and again, now that it has one: nothing to open
        let second = ask(Asset::Usdc, &other, "2.25");
        purse.review(second.clone()).await;
        let Some(Sending::Reviewed(r)) = sending() else { panic!("{:?}", sending()) };
        assert!(r.opens_account.is_none() && r.recipient.contains("has a USDC account"), "{r:?}");
        purse.send(second).await;
        let Some(Sending::Done { signature, .. }) = sending() else { panic!("{:?}", sending()) };
        println!("4. 2.25 USDC → {other}: {signature}");
        assert_eq!((tokens(to).await, tokens(me).await), (3_750_000, start_usdc - 3_750_000));
        assert_eq!(lamports(me).await, start_sol - 250_007_000 - 2 * 7_000 - TOKEN_ACCOUNT_RENT);

        // 5. what would leave the wallet under the chain's least is refused; under the program's reserve is said
        let left = lamports(me).await;
        let nearly_all = format!("{:.9}", sol(left - 7_000 - 500_000));
        purse.review(ask(Asset::Sol, &other, &nearly_all)).await;
        assert!(
            matches!(sending(), Some(Sending::Refused(_, why)) if why.contains("It would leave 0.000500 SOL")),
            "{:?}",
            sending()
        );
        let most = format!("{:.9}", sol(left - 7_000 - 10_000_000));
        purse.review(ask(Asset::Sol, &other, &most)).await;
        let Some(Sending::Reviewed(r)) = sending() else { panic!("{:?}", sending()) };
        assert!(r.notes.iter().any(|n| n.contains("under the 0.020000 this program keeps for fees")), "{r:?}");
        purse.act(WalletAction::Clear).await;
        assert_eq!(sending(), None);

        // 6. what came and went, newest first, as it changed the wallet
        purse.refresh().await;
        purse.history().await;
        let v = purse.view.lock().clone();
        assert_eq!((v.sol, v.usdc), (Some(sol(left)), Some((start_usdc - 3_750_000) as f64 / 1e6)));
        for m in &v.moved {
            println!(
                "{}  {:8}  {:+.6} SOL  {:+.6} USDC  fee {:.6}",
                Ts(m.at * 1000).hms(),
                m.kind(false),
                m.sol,
                m.usdc,
                m.fee
            );
        }
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let [fourth, third, second, ..] = v.moved.as_slice() else { panic!("{:?}", v.moved) };
        assert!(fourth.kind(false) == "sent" && near(fourth.usdc, -2.25) && near(fourth.sol, -0.000007), "{fourth:?}");
        assert!(third.kind(false) == "sent" && near(third.usdc, -1.5) && near(third.sol, -0.00204628), "{third:?}");
        assert!(second.kind(false) == "sent" && near(second.sol, -0.250007) && second.usdc == 0.0, "{second:?}");
        assert!(v.moved.iter().any(|m| m.kind(false) == "received" && m.sol >= 2.0), "the faucet's SOL: {:?}", v.moved);
        // the address the three transfers went to is remembered, for the next one
        let known = recipients(&data.join("recipients.json"));
        assert_eq!((known.len(), known[0].address.as_str(), known[0].times), (1, other.as_str(), 3));
        assert_eq!(v.recipients, known);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn addresses_sent_to_are_kept_newest_first_and_counted() {
        let path = std::env::temp_dir().join(format!("mobius-recipients-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(recipients(&path).is_empty());
        remember(&path, "A", 10);
        remember(&path, "B", 20);
        let known = remember(&path, "A", 30);
        let seen = |l: &[Recipient]| l.iter().map(|r| (r.address.clone(), r.last, r.times)).collect::<Vec<_>>();
        assert_eq!(seen(&known), [("A".to_string(), 30, 2), ("B".to_string(), 20, 1)]);
        assert_eq!(seen(&recipients(&path)), seen(&known), "as the next session reads them");
        for i in 0..10 {
            remember(&path, &format!("C{i}"), 40 + i);
        }
        assert_eq!(recipients(&path).len(), RECIPIENTS);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn what_is_sent_is_what_was_planned() {
        let a = |n| Address([n; 32]);
        let plan = Plan { asset: Asset::Sol, to: a(2), atoms: 50_000_000, accounts: None, opens: false };
        let ixs = plan.instructions(a(1));
        assert_eq!(ixs.len(), 1);
        assert_eq!(ixs[0].program_id.to_string(), well_known::SYSTEM_PROGRAM);
        assert_eq!((ixs[0].accounts[0].pubkey, ixs[0].accounts[1].pubkey), (a(1), a(2)));
        assert_eq!(u64::from_le_bytes(ixs[0].data[4..].try_into().unwrap()), 50_000_000);
        // USDC to someone who has no account for it: opened first, then the transfer into it
        let plan = Plan { asset: Asset::Usdc, to: a(2), atoms: 1_500_000, accounts: Some((a(3), a(4))), opens: true };
        let ixs = plan.instructions(a(1));
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id.to_string(), well_known::ASSOCIATED_TOKEN_PROGRAM);
        assert_eq!((ixs[0].accounts[1].pubkey, ixs[0].accounts[2].pubkey), (a(4), a(2)), "their account, theirs");
        assert_eq!(ixs[1].program_id.to_string(), well_known::TOKEN_PROGRAM);
        assert_eq!(
            (ixs[1].accounts[0].pubkey, ixs[1].accounts[2].pubkey, ixs[1].accounts[3].pubkey),
            (a(3), a(4), a(1))
        );
        assert_eq!(u64::from_le_bytes(ixs[1].data[1..9].try_into().unwrap()), 1_500_000);
        assert_eq!(Plan { opens: false, ..plan }.instructions(a(1)).len(), 1);
    }
}
