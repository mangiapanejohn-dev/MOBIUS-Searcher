//! `--trade FILE`: one rule of the lab with real money.
//!
//! The rule decides as it does on paper, at the close of each bar; what
//! differs is the fill: a real SOL/USDC swap, built by Jupiter, assembled,
//! simulated, signed, simulated again as signed, sent as an ordinary
//! transaction (to the RPC and, through Jito, straight to the leader) again
//! and again until it is confirmed or its blockhash has expired, and then
//! **read back from the wallet's balances**. The account only ever records
//! what the wallet shows: a swap that was sent and did not land changes
//! nothing.
//!
//! Not a bundle: one sent once lives only until the next Jito leader, and on
//! 2026-10-04 the first two this runner sent were both dropped. A swap with
//! a minimum output needs no bundle; what it risks by landing and failing is
//! its fee.
//!
//! What keeps it small:
//! * a budget in USD, set aside as USDC once at the start, never topped up;
//! * a stop: when the budget's value is down by `stop_total_loss`, everything
//!   it holds is sold and the run ends for good;
//! * it sells only SOL it bought and buys only with its own USDC;
//! * a bar seen late (the machine slept) is not acted on.
//!
//! What it cannot do: act while the machine sleeps or is off (the stop
//! included), or undo a fall between two bars. It can lose money; the file
//! must say so in words (`acknowledge = "ALLOW LOSS"`).

use super::rules::{Account, Bar, Ctx, Order, Stops, orders};
use super::{Experiment, Plan};
use anyhow::{Context, Result, bail};
use base64::Engine;
use searcher_core::config::Config;
use searcher_core::model::{DexFilter, Mode, RoutingMode, SlippageSpec};
use searcher_core::{Address, address::well_known};
use searcher_execution::assemble::{AssembledTx, AssemblyParams, compose_single, reserialize};
use searcher_execution::wallet::Wallet;
use searcher_jito::{JitoClient, SendPermit};
use searcher_jupiter::{ApiKey, BuildRequest, JupiterClient};
use searcher_market::RpcClient;
use searcher_telemetry::{LimiterConfig, Telemetry};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// A budget above this is refused: this is for trying a rule, not for size.
pub const MAX_BUDGET_USD: f64 = 25.0;
/// And one under this: a swap costs about a fifth of a cent whatever its size.
pub const MIN_BUDGET_USD: f64 = 1.0;
/// The words the file must carry.
pub const ACK: &str = "ALLOW LOSS";
/// A swap's priority fee, lamports: what Jupiter suggests, between these. It
/// is what gets an ordinary transaction into a block.
const MIN_PRIORITY_FEE: u64 = 2_000;
const MAX_PRIORITY_FEE: u64 = 7_000;
/// Ceiling of a swap's tip, lamports (Jito forwards nothing under 1,000),
/// whatever the config allows the arbitrage: at a few USD a larger one is a
/// visible share of the trade.
const MAX_TIP: u64 = 4_000;
/// Lamports of a sold lot kept back to pay for the sale (signature, priority
/// fee, tip): a sale never spends SOL the run did not buy.
const SELL_HOLDBACK: u64 = 5_000 + MAX_PRIORITY_FEE + MAX_TIP;
/// A swap sent this long ago, ms, can no longer land: its blockhash lives about a minute.
pub const SETTLED_AFTER_MS: i64 = 120_000;
/// Rent of a token account, lamports: what opening one costs the wallet.
const TOKEN_ACCOUNT_RENT: u64 = 2_039_280;
/// SOL the wallet must keep beyond its fee reserve when the budget is set
/// aside: the rent of a USDC account it may have to open, and fees.
const FUND_HEADROOM: u64 = 3_000_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Live {
    /// USD the rule may use, set aside as USDC once.
    pub budget_usd: f64,
    /// Share of the budget: down this much, everything is sold and the run ends.
    pub stop_total_loss: f64,
    /// Tolerance of a swap against its quote, basis points (its on-chain minimum output).
    #[serde(default = "default_slippage")]
    pub slippage_bps: u16,
    /// Must be `ALLOW LOSS`.
    #[serde(default)]
    pub acknowledge: String,
    /// The only DEXes a swap may route through, by Jupiter's names. Empty: any.
    #[serde(default)]
    pub dexes: Vec<String>,
}

fn default_slippage() -> u16 {
    30
}

impl Live {
    pub fn check(&self) -> Result<(), String> {
        if !(self.budget_usd >= MIN_BUDGET_USD && self.budget_usd <= MAX_BUDGET_USD) {
            return Err(format!("[live] budget_usd must be at least {MIN_BUDGET_USD} and at most {MAX_BUDGET_USD}"));
        }
        if !(self.stop_total_loss > 0.0 && self.stop_total_loss <= 1.0) {
            return Err("[live] stop_total_loss must be above 0 and at most 1 (a share of the budget)".into());
        }
        if !(1..=300).contains(&self.slippage_bps) {
            return Err("[live] slippage_bps must be 1 to 300".into());
        }
        Ok(())
    }

    /// The words, asked for where something can be sent (a backtest, a paper
    /// run and a dry run of the same file go without).
    pub fn consent(&self) -> Result<(), String> {
        if self.acknowledge != ACK {
            return Err(format!(
                "[live] this rule can lose money: there is no on-chain protection as there is for the arbitrage. \
                 To run it, write acknowledge = \"{ACK}\" in the file"
            ));
        }
        Ok(())
    }
}

/// What became of a swap.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    /// Nothing was sent; why.
    NotSent(String),
    /// A dry run: built and simulated, neither signed nor sent. The quoted
    /// output, in atoms, and the wallet's lamports after it in the simulation.
    Simulated { out: u64, lamports_after: u64, note: String },
    /// Sent. Whether it landed is read from the wallet.
    Sent(String),
}

/// The wallet and the venue, as the trader needs them (the tests have their own).
pub trait Chain {
    /// The wallet's lamports and USDC atoms.
    fn balances(&self) -> impl Future<Output = Result<(u64, u64), String>> + Send;
    /// Swap `amount` of the input: lamports when selling SOL, USDC atoms when buying it.
    fn swap(&self, sell_sol: bool, amount: u64) -> impl Future<Output = Sent> + Send;
    /// Lamports the wallet must keep for fees.
    fn fee_reserve(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub enum What {
    /// Setting the budget aside: SOL into USDC.
    Fund,
    Buy,
    Sell {
        lot: usize,
    },
}

/// A swap that was sent: the wallet before it, to tell what it did.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Pending {
    pub what: What,
    pub lamports: u64,
    pub usdc: u64,
    pub ts: i64,
    pub close: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct State {
    pub account: Account,
    /// The budget has been set aside as USDC.
    pub funded: bool,
    /// The run is over, and why.
    pub ended: Option<String>,
    pub pending: Option<Pending>,
}

/// One bar's (or the start's) work on the state, with what to say about it.
pub struct Trader<'a, C: Chain> {
    pub chain: &'a C,
    pub live: &'a Live,
    pub state: &'a mut State,
    /// Lines for the journal, in order.
    pub said: Vec<String>,
    /// Seconds to wait between looks at the wallet after a send (0 in tests).
    pub settle_wait: Duration,
    /// Writes the state down. Called before a swap is sent and after it is
    /// accounted for, so that a program stopped in between knows on its next
    /// start that a swap was under way and reads the wallet.
    pub save: Option<&'a dyn Fn(&State)>,
}

impl<C: Chain> Trader<'_, C> {
    fn say(&mut self, line: String) {
        self.said.push(line);
    }

    /// What a sent swap did, from the wallet now against the wallet before.
    /// `true`: accounted for (it landed, or it is certain it did not).
    fn settle(&mut self, p: &Pending, (lamports, usdc): (u64, u64), last_look: bool) -> bool {
        let (d_sol, d_usdc) = (lamports as i128 - p.lamports as i128, usdc as i128 - p.usdc as i128);
        let lots = self.state.account.lots.len();
        let line = match p.what {
            What::Fund if d_usdc > 0 => {
                self.state.funded = true;
                self.state.account.cash = self.live.budget_usd.min(usdc as f64 / 1e6);
                format!(
                    "budget set aside: {:.6} SOL became {:.4} USDC; the rule has {:.4} USD",
                    -d_sol as f64 / 1e9,
                    d_usdc as f64 / 1e6,
                    self.state.account.cash
                )
            }
            What::Buy if d_usdc < 0 && d_sol > 0 => {
                let (usd, sol) = (-d_usdc as f64 / 1e6, d_sol as f64 / 1e9);
                self.state.account.bought(usd, sol, p.ts, p.close);
                format!("bought {sol:.6} SOL for {usd:.4} USDC ({:.2} a SOL, every cost inside)", usd / sol)
            }
            What::Sell { lot } if d_usdc > 0 && lot < lots => {
                let usd = d_usdc as f64 / 1e6;
                let (paid, sol) = (self.state.account.lots[lot].usd, self.state.account.lots[lot].sol);
                self.state.account.sold(lot, usd, p.ts, p.close);
                format!("sold {sol:.6} SOL for {usd:.4} USDC; this trade {:+.4} USD", usd - paid)
            }
            _ if last_look => "the swap that was under way did not change the wallet: it did not land".to_string(),
            _ => return false,
        };
        self.say(line);
        true
    }

    /// Send one swap and account for it. `true` when the account changed.
    async fn act(&mut self, what: What, amount: u64, ts: i64, close: f64) -> bool {
        let (lamports, usdc) = match self.chain.balances().await {
            Ok(b) => b,
            Err(e) => {
                self.say(format!("the wallet could not be read ({e}): nothing sent"));
                return false;
            }
        };
        let sell_sol = !matches!(what, What::Buy);
        let amount = if sell_sol { amount } else { amount.min(usdc) };
        if amount == 0 {
            self.say("nothing to swap".into());
            return false;
        }
        let pending = Pending { what, lamports, usdc, ts, close };
        self.state.pending = Some(pending.clone());
        self.write();
        let changed = match self.chain.swap(sell_sol, amount).await {
            Sent::NotSent(why) => {
                self.say(format!("not sent: {why}"));
                self.state.pending = None;
                false
            }
            Sent::Simulated { out, lamports_after, note } => {
                self.say(format!(
                    "dry run, nothing signed or sent: {note}; quoted output {out} atoms; the wallet would hold {:.6} SOL after it",
                    lamports_after as f64 / 1e9
                ));
                self.state.pending = None;
                false
            }
            Sent::Sent(note) => {
                self.say(format!("sent: {note}"));
                // a landed swap shows in the wallet within seconds
                let mut landed = false;
                for _ in 0..6 {
                    tokio::time::sleep(self.settle_wait).await;
                    if let Ok(now) = self.chain.balances().await
                        && self.settle(&pending, now, false)
                    {
                        self.state.pending = None;
                        landed = true;
                        break;
                    }
                }
                if !landed {
                    self.say("the wallet has not changed yet: looked at again at the next bar".into());
                }
                landed
            }
        };
        self.write();
        changed
    }

    fn write(&self) {
        if let Some(save) = self.save {
            save(self.state);
        }
    }

    /// A swap left open by the last bar (or by a stop of the program): the
    /// wallet says what it did. Unchanged, it counts as not landed only once
    /// its blockhash has expired; until then it stays open.
    pub async fn reconcile(&mut self, now: i64) {
        let Some(p) = self.state.pending.clone() else { return };
        match self.chain.balances().await {
            Ok(wallet) => {
                if self.settle(&p, wallet, now - p.ts >= SETTLED_AFTER_MS) {
                    self.state.pending = None;
                    self.write();
                } else {
                    self.say("a swap sent less than two minutes ago does not show in the wallet yet: waiting".into());
                }
            }
            Err(e) => self.say(format!("the wallet could not be read ({e}): the open swap stays open")),
        }
    }

    /// Set the budget aside as USDC, once: what the wallet lacks is bought with SOL.
    pub async fn fund(&mut self, bid: f64, ts: i64) {
        if self.state.funded || self.state.ended.is_some() {
            return;
        }
        let (lamports, usdc) = match self.chain.balances().await {
            Ok(b) => b,
            Err(e) => return self.say(format!("the wallet could not be read ({e}): the budget is not set aside yet")),
        };
        let have = usdc as f64 / 1e6;
        if have >= self.live.budget_usd {
            self.state.funded = true;
            self.state.account.cash = self.live.budget_usd;
            return self.say(format!(
                "the wallet holds {have:.4} USDC: {:.2} of it is the rule's budget",
                self.live.budget_usd
            ));
        }
        // a little more than the price says, so that slippage does not leave it short
        let need = ((self.live.budget_usd - have) / bid * 1e9 * 1.003).ceil() as u64;
        let keep = self.chain.fee_reserve() + FUND_HEADROOM;
        if lamports < need + keep {
            return self.say(format!(
                "the wallet holds {:.6} SOL; setting {:.2} USD aside needs {:.6} and {:.6} must stay for fees and rent: nothing sent",
                lamports as f64 / 1e9,
                self.live.budget_usd - have,
                need as f64 / 1e9,
                keep as f64 / 1e9
            ));
        }
        self.say(format!(
            "setting the budget aside: selling {:.6} SOL for about {:.2} USDC",
            need as f64 / 1e9,
            self.live.budget_usd - have
        ));
        self.act(What::Fund, need, ts, bid).await;
    }

    pub fn say_closing(&mut self) {
        self.say("closing by hand: selling what the run holds".into());
    }

    fn equity(&self, bid: f64) -> f64 {
        self.state.account.cash + self.state.account.sol() * bid
    }

    /// Sell every lot the run holds. `true` when none is left.
    pub async fn sell_all(&mut self, ts: i64, close: f64) -> bool {
        while let Some(sol) = self.state.account.lots.last().map(|l| l.sol) {
            let amount = ((sol * 1e9).round() as u64).saturating_sub(SELL_HOLDBACK);
            let i = self.state.account.lots.len() - 1;
            if !self.act(What::Sell { lot: i }, amount, ts, close).await {
                return false;
            }
        }
        true
    }

    /// The close of a bar: the stop first, then what the rule wants.
    pub async fn on_bar(&mut self, plan: &Plan, e: &Experiment, bars: &[Bar], other: &[f64], bid: f64, ts: i64) {
        let Some(bar) = bars.last().copied() else { return };
        if self.state.ended.is_some() {
            return;
        }
        self.reconcile(ts).await;
        if self.state.pending.is_some() {
            return; // nothing on top of an open swap
        }
        self.fund(bid, ts).await;
        if !self.state.funded {
            return;
        }
        let equity = self.equity(bid);
        let floor = self.live.budget_usd * (1.0 - self.live.stop_total_loss);
        if equity <= floor || self.state.account.frozen {
            if !self.state.account.frozen {
                self.say(format!(
                    "STOP: the budget's value is {equity:.4} USD, at or under {floor:.4} ({:.0} % down): selling what is held, the run ends",
                    self.live.stop_total_loss * 100.0
                ));
            }
            self.state.account.frozen = true;
            if self.sell_all(ts, bar.close).await {
                self.state.ended =
                    Some(format!("stopped at {:.4} USD of {:.2}", self.state.account.cash, self.live.budget_usd));
                let line = format!("the run has ended: {}", self.state.ended.as_deref().unwrap_or_default());
                self.say(line);
            }
            return;
        }
        let ctx = Ctx { model: e.model.as_deref(), other };
        // the total stop is the one above; a daily pause of the file applies as on paper
        let stops = Stops { daily_loss: plan.stops.daily_loss, total_loss: None };
        let (wanted, _) = orders(&e.rule, &stops, plan.capital, &mut self.state.account, (bars, ctx));
        // sells first, highest lot first, so the lot numbers stay the ones the rule meant
        let mut sells: Vec<usize> =
            wanted.iter().filter_map(|o| if let Order::Sell { lot } = o { Some(*lot) } else { None }).collect();
        sells.sort_unstable_by(|a, b| b.cmp(a));
        for lot in sells {
            let Some(sol) = self.state.account.lots.get(lot).map(|l| l.sol) else { continue };
            let amount = ((sol * 1e9).round() as u64).saturating_sub(SELL_HOLDBACK);
            self.act(What::Sell { lot }, amount, ts, bar.close).await;
        }
        for o in &wanted {
            let Order::Buy { usd } = *o else { continue };
            let atoms = (usd.min(self.state.account.cash) * 1e6).floor() as u64;
            self.act(What::Buy, atoms, ts, bar.close).await;
        }
    }
}

// ───────────────────────────── mainnet ─────────────────────────────

/// The real wallet and Jupiter + Jito. Without a signer it only simulates.
pub struct Mainnet {
    rpc: Arc<RpcClient>,
    jupiter: JupiterClient,
    jito: Arc<JitoClient>,
    taker: Address,
    signer: Option<(SendPermit, Wallet)>,
    slippage_bps: u16,
    cu_price_percentile: String,
    max_tip: u64,
    max_priority_fee: u64,
    fee_reserve: u64,
    dexes: Vec<String>,
}

/// Where a signed transaction is handed over and its fate is read (the tests have their own).
trait Wire {
    /// Hand it over, by every way there is. Returns the ways that refused it (none: all took it).
    fn send(&self, tx_b64: &str) -> impl Future<Output = Vec<String>> + Send;
    /// `None`: not in a confirmed block (yet). `Some(None)`: confirmed.
    /// `Some(Some(err))`: confirmed, and it failed.
    fn status(&self, signature: &str) -> impl Future<Output = Result<Option<Option<String>>, String>> + Send;
    fn block_height(&self) -> impl Future<Output = Result<u64, String>> + Send;
}

/// What became of a transaction that was handed over.
#[derive(Debug, PartialEq)]
enum Fate {
    Confirmed,
    /// In a block, and it failed there: its fee is paid, nothing else happened.
    Failed(String),
    /// Its blockhash expired: it can no longer land.
    Expired,
    /// Neither seen nor known to be expired when the looking stopped.
    Unknown,
}

/// Looks at a transaction before giving up on knowing (two minutes at two seconds).
const ROUNDS: usize = 60;

/// Hand a signed transaction over again and again until it is confirmed or
/// its blockhash has expired. Its signature lets it land at most once, however
/// often and by whichever way it is sent. Also returns who refused it first.
async fn deliver<W: Wire>(
    wire: &W,
    tx_b64: &str,
    signature: &str,
    last_valid: u64,
    every: Duration,
) -> (Fate, Vec<String>) {
    let mut refused = Vec::new();
    let seen = |s: Result<Option<Option<String>>, String>| match s {
        Ok(Some(None)) => Some(Fate::Confirmed),
        Ok(Some(Some(err))) => Some(Fate::Failed(err)),
        _ => None,
    };
    for _ in 0..ROUNDS {
        // a refusal is not the end: the next round sends again, and the status says what happened
        let no = wire.send(tx_b64).await;
        if refused.is_empty() {
            refused = no;
        }
        tokio::time::sleep(every).await;
        if let Some(fate) = seen(wire.status(signature).await) {
            return (fate, refused);
        }
        if wire.block_height().await.is_ok_and(|h| h > last_valid) {
            // it may have landed in the last block it could
            return (seen(wire.status(signature).await).unwrap_or(Fate::Expired), refused);
        }
    }
    (Fate::Unknown, refused)
}

/// The DEXes a quote may use: the ones the file allows (any, when it names
/// none) without the ones whose route just failed. `None`: none is left.
fn routes(allowed: &[String], without: &[String]) -> Option<DexFilter> {
    if allowed.is_empty() {
        return Some(if without.is_empty() { DexFilter::Any } else { DexFilter::Exclude(without.to_vec()) });
    }
    let left: Vec<String> = allowed.iter().filter(|d| !without.contains(d)).cloned().collect();
    (!left.is_empty()).then_some(DexFilter::Only(left))
}

/// Lamports a simulated swap takes from the wallet beyond what it swaps and
/// its fees: none, unless it opens a token account at the wallet's expense
/// (its rent is about a quarter of a dollar, on a swap of two). Jupiter's
/// routes through a third token were seen not to (2026-10-04); this is the
/// check that it stays so. For a buy it is what is missing even from the
/// least the route promised, so only a shortfall larger than the slippage
/// shows.
fn beyond_fees(sell_sol: bool, amount: u64, min_out: u64, fee: u64, (before, after): (u64, u64)) -> u64 {
    if sell_sol {
        before.saturating_sub(after).saturating_sub(amount + fee)
    } else {
        min_out.saturating_sub(after.saturating_sub(before) + fee)
    }
}

/// One entry of `getSignatureStatuses`, as [`Wire::status`] gives it.
fn confirmed(status: &Value) -> Option<Option<String>> {
    let level = status.get("confirmationStatus")?.as_str()?;
    // `processed` may still be dropped with its fork
    (level == "confirmed" || level == "finalized")
        .then(|| status.get("err").filter(|e| !e.is_null()).map(Value::to_string))
}

/// Quotes asked for one swap before giving up on it for this bar.
const QUOTES: usize = 3;

/// Why a quote did not become a swap.
enum No {
    /// Its simulation failed (the quote went stale, or its route does not
    /// hold what it quoted): another quote, without these DEXes, may do.
    Simulation {
        via: Vec<String>,
        why: String,
    },
    Other(String),
}

impl From<String> for No {
    fn from(why: String) -> No {
        No::Other(why)
    }
}

/// Why a transaction was not sent.
#[derive(Debug)]
enum Unsent {
    /// Its simulation as signed failed: the quote went stale, another may do.
    Simulation(String),
    Other(String),
}

fn mint(s: &str) -> Address {
    well_known::addr(s)
}

impl Mainnet {
    /// `dry_run`: no key is read, nothing can be signed.
    pub fn new(cfg: &Config, live: &Live, dry_run: bool) -> Result<Mainnet> {
        let telemetry = Arc::new(Telemetry::new());
        let rpc = Arc::new(RpcClient::new(
            &cfg.rpc.resolved_url(),
            LimiterConfig::new(cfg.rpc.rps, cfg.rpc.burst),
            cfg.rpc.simulate_rps,
            Duration::from_millis(cfg.rpc.timeout_ms),
            telemetry.clone(),
        )?);
        // without the key: the session that trades the arbitrage holds the key's budget
        let jupiter = JupiterClient::new(
            &cfg.jupiter.base_url,
            None::<ApiKey>,
            LimiterConfig::new(0.4, 1),
            Duration::from_millis(cfg.jupiter.timeout_ms.max(8_000)),
            telemetry.clone(),
        )?;
        let jito = Arc::new(JitoClient::new(
            &cfg.jito.block_engine_url,
            &cfg.jito.tip_floor_url,
            std::env::var(&cfg.jito.uuid_env).ok(),
            cfg.jito.rps,
            telemetry,
        )?);
        let configured: Option<Address> = cfg
            .wallet
            .pubkey
            .as_deref()
            .map(|p| p.parse())
            .transpose()
            .map_err(|e| anyhow::anyhow!("wallet.pubkey: {e}"))?;
        let (taker, signer) = if dry_run {
            (configured.context("a dry run needs [wallet] pubkey in your config (the wallet to simulate as)")?, None)
        } else {
            let Some(permit) = SendPermit::check(Mode::Live, cfg.execution.live_enabled) else {
                bail!("--trade sends real transactions: it needs execution.live_enabled = true in your config");
            };
            let path = cfg.wallet.keypair_path.clone().context("--trade needs [wallet] keypair_path in your config")?;
            let wallet =
                Wallet::load(std::path::Path::new(&path), configured.as_ref()).context("loading the wallet")?;
            (wallet.pubkey(), Some((permit, wallet)))
        };
        Ok(Mainnet {
            rpc,
            jupiter,
            jito,
            taker,
            signer,
            slippage_bps: live.slippage_bps,
            cu_price_percentile: cfg.jupiter.compute_unit_price_percentile.clone(),
            max_tip: cfg.risk.max_jito_tip_lamports,
            max_priority_fee: cfg.risk.max_priority_fee_lamports.min(MAX_PRIORITY_FEE),
            fee_reserve: cfg.risk.min_wallet_sol_for_fees_lamports,
            dexes: live.dexes.clone(),
        })
    }

    pub fn taker(&self) -> Address {
        self.taker
    }

    async fn try_swap(&self, sell_sol: bool, amount: u64, without: &[String]) -> Result<Sent, No> {
        let (sol, usdc) = (mint(well_known::WSOL_MINT), mint(well_known::USDC_MINT));
        let req = BuildRequest {
            input_mint: if sell_sol { sol } else { usdc },
            output_mint: if sell_sol { usdc } else { sol },
            amount,
            taker: self.taker,
            slippage: SlippageSpec::Fixed(self.slippage_bps),
            mode: RoutingMode::Normal,
            dex_filter: routes(&self.dexes, without)
                .ok_or_else(|| "every DEX the file allows has failed in simulation".to_string())?,
            cu_price_percentile: self.cu_price_percentile.clone(),
            max_accounts: Some(30),
            // the longest a blockhash lives: the sending goes on until it expires
            blockhash_slots_to_expiry: 150,
            for_jito_bundle: true,
        };
        let built = self.jupiter.build(&req, 0).await.map_err(|e| format!("Jupiter: {e}"))?;
        let leg = &built.leg;
        // the least Jito forwards, or the going rate, never above the ceiling
        let floor = self.jito.tip_floor().await.map(|f| f.p50).unwrap_or(1_000);
        let tip = floor.clamp(1_000, self.max_tip.clamp(1_000, MAX_TIP));
        let tip_account: Address =
            searcher_jito::KNOWN_TIP_ACCOUNTS[0].parse().map_err(|_| "tip account".to_string())?;
        let none = HashSet::new();
        let params = |cu_limit: u32, cu_price: u64, blockhash: [u8; 32]| AssemblyParams {
            payer: self.taker,
            cu_limit,
            cu_price_micro: cu_price,
            tip: Some((tip_account, tip)),
            dont_front: None,
            existing_atas: &none,
            blockhash,
        };
        let price = leg.cu_price_micro.unwrap_or(0);
        // without a priority fee: at the largest limit it would not be the one paid
        let probe = compose_single(
            &[&built.instructions],
            &params(searcher_core::units::MAX_COMPUTE_UNITS_PER_TX, 0, built.instructions.blockhash),
        )
        .map_err(|e| format!("assembling: {e}"))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&probe.wire);
        let before =
            self.rpc.get_balance(&self.taker).await.map_err(|e| format!("the wallet could not be read: {e}"))?;
        let sim = self.rpc.simulate(&b64, &[self.taker]).await.map_err(|e| format!("simulation unavailable: {e}"))?;
        let via = leg.dex_labels().join(" + ");
        if let Some(err) = &sim.err {
            let why = format!("via {via} ({err}: {})", sim.logs.last().cloned().unwrap_or_default());
            return Err(No::Simulation { via: leg.dex_labels(), why });
        }
        let Some(after) = sim.post_account_lamports.first().copied().flatten() else {
            return Ok(Sent::NotSent("the simulation did not show the wallet after the swap".into()));
        };
        // the account is kept from what the wallet shows: a buy must arrive there as SOL
        // itself, and no more of it than was quoted (a wrapped-SOL account the wallet
        // already has is closed by the swap, and would be counted as bought)
        if !sell_sol {
            let arrived = after as i128 - before as i128;
            if arrived <= 0 || arrived > leg.out_amount as i128 * 101 / 100 {
                return Ok(Sent::NotSent(format!(
                    "in the simulation the buy changes the wallet by {arrived} lamports against {} quoted: \
                     the run could not tell what it bought",
                    leg.out_amount
                )));
            }
        }
        // the swap may cost the wallet what it swaps and its fees, nothing else: a route that
        // would is asked for again without its DEXes. The one account a wallet may have to
        // open is its first for USDC.
        let extra = beyond_fees(sell_sol, amount, leg.min_out, 5_000 + tip, (before, after));
        if extra > 0 && !(sell_sol && extra <= TOKEN_ACCOUNT_RENT && !self.holds_a_usdc_account().await?) {
            let why =
                format!("via {via} (it would cost the wallet {extra} lamports beyond what it swaps and its fees)");
            return Err(No::Simulation { via: leg.dex_labels(), why });
        }
        let units = sim.units_consumed.ok_or_else(|| "the simulation gave no compute units".to_string())?;
        let cu_limit = ((units as f64 * 1.2) as u32 + 10_000).min(searcher_core::units::MAX_COMPUTE_UNITS_PER_TX);
        // the priority fee is the limit times the price: what Jupiter suggests, within the floor and the ceiling
        let per_cu = |lamports: u64| lamports.saturating_mul(1_000_000) / u64::from(cu_limit.max(1));
        let price = price.max(per_cu(MIN_PRIORITY_FEE)).min(per_cu(self.max_priority_fee));
        let note = format!(
            "{} {} for at least {} {} (quoted {}) via {via}, {} CU, priority fee {} + tip {} lamports",
            if sell_sol { "sell" } else { "spend" },
            if sell_sol {
                format!("{:.6} SOL", amount as f64 / 1e9)
            } else {
                format!("{:.4} USDC", amount as f64 / 1e6)
            },
            if sell_sol {
                format!("{:.4}", leg.min_out as f64 / 1e6)
            } else {
                format!("{:.6}", leg.min_out as f64 / 1e9)
            },
            if sell_sol { "USDC" } else { "SOL" },
            if sell_sol {
                format!("{:.4}", leg.out_amount as f64 / 1e6)
            } else {
                format!("{:.6}", leg.out_amount as f64 / 1e9)
            },
            units,
            price * u64::from(cu_limit) / 1_000_000,
            tip
        );
        let Some((_, wallet)) = &self.signer else {
            return Ok(Sent::Simulated { out: leg.out_amount, lamports_after: after, note });
        };
        if before < self.fee_reserve {
            return Ok(Sent::NotSent(format!(
                "the wallet holds {before} lamports, under the fee reserve of {}",
                self.fee_reserve
            )));
        }
        // a blockhash of the node it is simulated and sent through: the one Jupiter gives
        // is one its own node knows, and ours may not have it yet
        let (blockhash, last_valid) = self.latest_blockhash().await?;
        let tx = compose_single(&[&built.instructions], &params(cu_limit, price, blockhash))
            .map_err(|e| format!("assembling: {e}"))?;
        let (signature, fate, refused) = match self.sign_and_send(wallet, tx, last_valid).await {
            Ok(sent) => sent,
            Err(Unsent::Simulation(why)) => {
                return Err(No::Simulation { via: leg.dex_labels(), why: format!("via {via}, as signed ({why})") });
            }
            Err(Unsent::Other(why)) => return Ok(Sent::NotSent(why)),
        };
        let fate = match fate {
            Fate::Confirmed => "confirmed".to_string(),
            Fate::Failed(err) => format!("it landed and failed ({err}): its fee is paid, nothing was swapped"),
            Fate::Expired => "it expired without landing".to_string(),
            Fate::Unknown => "nothing was heard of it in two minutes".to_string(),
        };
        let refused =
            if refused.is_empty() { String::new() } else { format!("; refused by {}", refused.join(" and ")) };
        Ok(Sent::Sent(format!("{note}; {fate}; signature {signature}{refused}")))
    }

    /// The node's newest confirmed blockhash and the last block height it is valid at.
    async fn latest_blockhash(&self) -> Result<([u8; 32], u64), String> {
        let (v, _) = self
            .rpc
            .call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))
            .await
            .map_err(|e| format!("blockhash: {e}"))?;
        let hash: Option<Address> = v.pointer("/value/blockhash").and_then(Value::as_str).and_then(|s| s.parse().ok());
        let last = v.pointer("/value/lastValidBlockHeight").and_then(Value::as_u64);
        hash.map(|h| h.0).zip(last).ok_or_else(|| "blockhash: not in the node's answer".to_string())
    }

    async fn holds_a_usdc_account(&self) -> Result<bool, String> {
        let of = json!({"mint": well_known::USDC_MINT});
        let how = json!({"encoding": "jsonParsed", "commitment": "processed"});
        let (v, _) = self
            .rpc
            .call("getTokenAccountsByOwner", json!([self.taker.to_string(), of, how]))
            .await
            .map_err(|e| format!("the wallet's USDC account could not be read: {e}"))?;
        Ok(v.get("value").and_then(Value::as_array).is_some_and(|a| !a.is_empty()))
    }

    /// Sign, simulate the exact bytes as signed (any error and nothing is
    /// sent), then send until it is confirmed or its blockhash has expired.
    /// Returns the signature, what became of it, and who refused it.
    async fn sign_and_send(
        &self,
        wallet: &Wallet,
        mut tx: AssembledTx,
        last_valid: u64,
    ) -> Result<(String, Fate, Vec<String>), Unsent> {
        // sending goes on until the blockhash expires: it must have a while left
        let height = self.block_height().await.map_err(Unsent::Other)?;
        if height + 40 > last_valid {
            return Err(Unsent::Other(format!(
                "the quote's blockhash is nearly spent (block {height} of {last_valid})"
            )));
        }
        wallet.sign(&mut tx.tx).map_err(|e| Unsent::Other(format!("signing: {e}")))?;
        let signature = tx.tx.signatures.first().map(|s| s.to_string()).unwrap_or_default();
        let wire = reserialize(&tx.tx).map_err(|e| Unsent::Other(format!("serialising: {e}")))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(wire);
        let last = self
            .rpc
            .simulate_signed(&b64)
            .await
            .map_err(|e| Unsent::Other(format!("final simulation unavailable: {e}")))?;
        if let Some(err) = &last.err {
            return Err(Unsent::Simulation(format!("{err}: {}", last.logs.last().cloned().unwrap_or_default())));
        }
        let (fate, refused) = deliver(self, &b64, &signature, last_valid, Duration::from_secs(2)).await;
        Ok((signature, fate, refused))
    }
}

impl Wire for Mainnet {
    /// To the RPC and, through Jito, straight to the leader: the same
    /// signature, so at most one of them lands it.
    async fn send(&self, tx_b64: &str) -> Vec<String> {
        let Some((permit, _)) = &self.signer else { return vec!["everyone (no signer)".into()] };
        // simulated as signed a moment ago, and sent again by `deliver`: no preflight, no retries of the node's own
        let opts = json!({"encoding": "base64", "skipPreflight": true, "maxRetries": 0});
        let (jito, rpc) = tokio::join!(
            self.jito.send_transaction(permit, tx_b64),
            self.rpc.call("sendTransaction", json!([tx_b64, opts]))
        );
        let mut refused = Vec::new();
        if let Err(e) = jito {
            refused.push(format!("Jito ({e})"));
        }
        if let Err(e) = rpc {
            refused.push(format!("the RPC ({e})"));
        }
        refused
    }

    async fn status(&self, signature: &str) -> Result<Option<Option<String>>, String> {
        let (v, _) = self
            .rpc
            .call("getSignatureStatuses", json!([[signature], {"searchTransactionHistory": false}]))
            .await
            .map_err(|e| e.to_string())?;
        Ok(v.get("value").and_then(|a| a.get(0)).and_then(confirmed))
    }

    async fn block_height(&self) -> Result<u64, String> {
        let (h, _) = self
            .rpc
            .call("getBlockHeight", json!([{"commitment": "confirmed"}]))
            .await
            .map_err(|e| format!("block height: {e}"))?;
        h.as_u64().ok_or_else(|| "block height".to_string())
    }
}

impl Chain for Mainnet {
    async fn balances(&self) -> Result<(u64, u64), String> {
        let lamports = self.rpc.get_balance(&self.taker).await.map_err(|e| e.to_string())?;
        let usdc =
            self.rpc.token_balance(&self.taker, &mint(well_known::USDC_MINT)).await.map_err(|e| e.to_string())?;
        Ok((lamports, usdc))
    }

    async fn swap(&self, sell_sol: bool, amount: u64) -> Sent {
        let (mut without, mut failed): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for _ in 0..QUOTES {
            // said in the journal: which routes did not hold
            let also = |note: String| {
                if failed.is_empty() {
                    note
                } else {
                    format!("{note}; before it, failed in simulation: {}", failed.join("; "))
                }
            };
            match self.try_swap(sell_sol, amount, &without).await {
                Ok(Sent::NotSent(why)) | Err(No::Other(why)) => return Sent::NotSent(also(why)),
                Ok(Sent::Simulated { out, lamports_after, note }) => {
                    return Sent::Simulated { out, lamports_after, note: also(note) };
                }
                Ok(Sent::Sent(note)) => return Sent::Sent(also(note)),
                Err(No::Simulation { via, why }) => {
                    without.extend(via);
                    failed.push(why);
                }
            }
        }
        Sent::NotSent(format!("{QUOTES} quotes failed in simulation: {}", failed.join("; ")))
    }

    fn fee_reserve(&self) -> u64 {
        self.fee_reserve
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::parse;
    use parking_lot::Mutex;

    /// A wallet and a venue that fills at `price` (USD a SOL), taking `fee` lamports a swap.
    struct Mock {
        wallet: Mutex<(u64, u64)>,
        price: Mutex<f64>,
        sends: Mutex<Vec<(bool, u64)>>,
        /// What the next swaps do instead of landing.
        script: Mutex<Vec<Sent>>,
        /// A sent swap that does not land.
        lost: Mutex<bool>,
    }

    impl Mock {
        fn new(sol: f64, usdc: f64, price: f64) -> Mock {
            Mock {
                wallet: Mutex::new(((sol * 1e9) as u64, (usdc * 1e6) as u64)),
                price: Mutex::new(price),
                sends: Mutex::new(Vec::new()),
                script: Mutex::new(Vec::new()),
                lost: Mutex::new(false),
            }
        }
    }

    const FEE: u64 = 7_000;

    impl Chain for Mock {
        async fn balances(&self) -> Result<(u64, u64), String> {
            Ok(*self.wallet.lock())
        }
        async fn swap(&self, sell_sol: bool, amount: u64) -> Sent {
            if let Some(s) = self.script.lock().pop() {
                return s;
            }
            self.sends.lock().push((sell_sol, amount));
            if *self.lost.lock() {
                return Sent::Sent("sent, lost".into());
            }
            let price = *self.price.lock();
            let mut w = self.wallet.lock();
            if sell_sol {
                w.0 -= amount + FEE;
                w.1 += (amount as f64 / 1e9 * price * 1e6) as u64;
            } else {
                w.1 -= amount;
                w.0 = w.0 + (amount as f64 / 1e6 / price * 1e9) as u64 - FEE;
            }
            Sent::Sent("landed".into())
        }
        fn fee_reserve(&self) -> u64 {
            5_000_000
        }
    }

    const FILE: &str = r#"
[live]
budget_usd = 2.0
stop_total_loss = 0.5
acknowledge = "ALLOW LOSS"

[[experiment]]
name = "dip"
rule = "dip"
window = 4
k = 1.0
stop = 0.05
"#;

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Bar { ts: i as i64 * 900_000, open: c, high: c, low: c, close: c, volume: 1.0 })
            .collect()
    }

    /// Runs the plan bar by bar over `closes`, the venue filling at each close.
    async fn run(mock: &Mock, state: &mut State, closes: &[f64]) -> Vec<String> {
        let plan = parse(FILE).unwrap();
        let live = plan.live.clone().unwrap();
        let b = bars(closes);
        let mut said = Vec::new();
        for i in 0..b.len() {
            *mock.price.lock() = b[i].close;
            let mut t =
                Trader { chain: mock, live: &live, state, said: Vec::new(), settle_wait: Duration::ZERO, save: None };
            t.on_bar(&plan, &plan.experiments[0], &b[..=i], &[], b[i].close, b[i].ts).await;
            said.extend(t.said);
        }
        said
    }

    #[test]
    fn the_file_must_say_the_words_and_keep_the_budget_small() {
        let err = |text: &str| format!("{:#}", parse(text).unwrap_err());
        // without the words the file is still a plan (for a backtest, paper, a dry run): nothing may be sent
        let consent = |text: &str| parse(text).unwrap().live.unwrap().consent();
        assert!(consent(&FILE.replace("acknowledge = \"ALLOW LOSS\"", "")).unwrap_err().contains("can lose money"));
        assert!(consent(&FILE.replace("ALLOW LOSS", "allow loss")).unwrap_err().contains("ALLOW LOSS"));
        assert_eq!(consent(FILE), Ok(()));
        assert!(err(&FILE.replace("budget_usd = 2.0", "budget_usd = 26.0")).contains("at most 25"));
        assert!(err(&FILE.replace("budget_usd = 2.0", "budget_usd = 0.5")).contains("at least 1"));
        assert!(err(&FILE.replace("stop_total_loss = 0.5", "stop_total_loss = 0.0")).contains("stop_total_loss"));
        assert!(
            err(&FILE.replace("stop_total_loss = 0.5", "stop_total_loss = 0.5\nleverage = 3")).contains("rules file")
        );
        let plan = parse(FILE).unwrap();
        assert_eq!((plan.capital, plan.live.unwrap().slippage_bps), (2.0, 30), "the budget is the rule's capital");
    }

    #[tokio::test]
    async fn the_budget_is_set_aside_once_from_sol() {
        let mock = Mock::new(0.188, 0.0, 120.0);
        let mut st = State::default();
        let said = run(&mock, &mut st, &[120.0, 120.0]).await;
        assert!(st.funded, "{said:?}");
        assert_eq!(mock.sends.lock().len(), 1, "one swap, at the first bar only: {said:?}");
        let (sell_sol, lamports) = mock.sends.lock()[0];
        assert!(sell_sol && (lamports as f64 / 1e9 * 120.0 - 2.0).abs() < 0.01, "about 2 USD of SOL: {lamports}");
        assert!((st.account.cash - 2.0).abs() < 1e-9, "never more than the budget: {}", st.account.cash);
        assert!(said.iter().any(|l| l.contains("budget set aside")), "{said:?}");
    }

    #[tokio::test]
    async fn usdc_already_in_the_wallet_is_used_and_a_poor_wallet_sends_nothing() {
        let mock = Mock::new(0.188, 5.0, 120.0);
        let mut st = State::default();
        run(&mock, &mut st, &[120.0]).await;
        assert!(st.funded && mock.sends.lock().is_empty() && st.account.cash == 2.0);
        // 0.02 SOL: 2 USD of it and the reserve do not both fit
        let mock = Mock::new(0.02, 0.0, 120.0);
        let mut st = State::default();
        let said = run(&mock, &mut st, &[120.0]).await;
        assert!(!st.funded && mock.sends.lock().is_empty(), "{said:?}");
        assert!(said[0].contains("must stay for fees"), "{said:?}");
    }

    #[tokio::test]
    async fn a_dip_is_bought_and_sold_with_what_the_wallet_shows() {
        let mock = Mock::new(0.188, 0.0, 100.0);
        let mut st = State::default();
        // flat, a fall far under the average (bought), back over it (sold)
        let said = run(&mock, &mut st, &[100.0, 100.0, 100.0, 100.0, 96.0, 97.0, 101.0, 101.0]).await;
        assert_eq!(st.account.trades.len(), 1, "{said:?}");
        let t = &st.account.trades[0];
        // in at 96, out at 101, a fee of 7,000 lamports each way inside the amounts
        assert!((t.usd - 2.0).abs() < 1e-6, "{t:?}");
        assert!((t.net - (2.0 * 101.0 / 96.0 - 2.0)).abs() < 0.01, "{t:?}");
        assert!(st.account.lots.is_empty() && st.pending.is_none());
        assert!((st.account.cash - (2.0 + t.net)).abs() < 1e-9);
        // the wallet's own SOL is what it was, less the funding sale and dust
        let sends = mock.sends.lock().clone();
        assert_eq!(sends.iter().map(|s| s.0).collect::<Vec<_>>(), [true, false, true], "fund, buy, sell");
        let bought_lamports = (2.0 / 96.0 * 1e9) as u64 - FEE;
        assert_eq!(
            sends[2].1,
            bought_lamports - SELL_HOLDBACK,
            "it sells the SOL it bought, less the holdback for fees"
        );
    }

    #[tokio::test]
    async fn the_stop_sells_everything_and_ends_the_run_for_good() {
        let mock = Mock::new(0.188, 0.0, 100.0);
        let mut st = State::default();
        // bought at 96; the price halves: the budget is worth less than half
        let said = run(&mock, &mut st, &[100.0, 100.0, 100.0, 100.0, 96.0, 97.0, 96.5, 45.0, 44.0, 120.0, 30.0]).await;
        assert!(st.ended.as_deref().is_some_and(|e| e.starts_with("stopped")), "{said:?}");
        assert!(st.account.lots.is_empty() && st.account.frozen);
        assert!(st.account.cash < 1.0 && st.account.cash > 0.8, "sold near 45: {}", st.account.cash);
        let n = mock.sends.lock().len();
        assert_eq!(n, 3, "fund, buy, the stop's sale; nothing after the run ended: {said:?}");
        assert!(said.iter().any(|l| l.starts_with("STOP")));
    }

    #[tokio::test]
    async fn the_rule_own_stop_sells_before_the_budget_stop() {
        let mock = Mock::new(0.188, 0.0, 100.0);
        let mut st = State::default();
        // bought at 96; 90 is more than 5 % under it: the rule sells; the run goes on
        run(&mock, &mut st, &[100.0, 100.0, 100.0, 100.0, 96.0, 97.0, 96.5, 90.0, 90.0]).await;
        assert_eq!(st.account.trades.len(), 1);
        assert!(st.account.trades[0].net < 0.0 && st.ended.is_none() && !st.account.frozen);
    }

    #[tokio::test]
    async fn a_swap_that_is_not_sent_or_only_simulated_changes_nothing() {
        let mock = Mock::new(0.188, 0.0, 120.0);
        mock.script.lock().push(Sent::NotSent("the simulation failed".into()));
        let mut st = State::default();
        let said = run(&mock, &mut st, &[120.0]).await;
        assert!(!st.funded && st.pending.is_none() && st.account == Account::default(), "{said:?}");
        assert!(said.iter().any(|l| l.contains("not sent: the simulation failed")));
        mock.script.lock().push(Sent::Simulated { out: 1_990_000, lamports_after: 0, note: "sell".into() });
        let said = run(&mock, &mut st, &[120.0]).await;
        assert!(!st.funded && st.pending.is_none(), "{said:?}");
        assert!(said.iter().any(|l| l.contains("dry run")));
        assert_eq!(*mock.wallet.lock(), (188_000_000, 0), "the wallet was never touched");
    }

    #[tokio::test]
    async fn a_swap_that_was_sent_and_lost_is_left_open_then_closed_as_not_landed() {
        let mock = Mock::new(0.188, 0.0, 120.0);
        *mock.lost.lock() = true;
        let mut st = State::default();
        let said = run(&mock, &mut st, &[120.0]).await;
        assert!(st.pending.is_some() && !st.funded, "sent, the wallet unchanged: open. {said:?}");
        // the next bar finds the wallet as it was: it did not land; the budget is tried again
        *mock.lost.lock() = false;
        let said = run(&mock, &mut st, &[120.0, 120.0]).await;
        assert!(said[0].contains("waiting"), "looked at in the same minute it stays open: {said:?}");
        assert!(said[1].contains("did not land"), "{said:?}");
        assert!(st.funded && st.pending.is_none());
        assert_eq!(mock.sends.lock().len(), 2, "sent twice in all: the one lost, the one that landed");
    }

    #[tokio::test]
    async fn a_swap_left_open_by_a_stopped_program_is_accounted_for_from_the_wallet() {
        // the program stopped after sending the buy; the wallet shows it landed
        let mock = Mock::new(0.2, 0.0, 100.0);
        let mut st = State { funded: true, ..State::default() };
        st.account.cash = 2.0;
        st.pending = Some(Pending { what: What::Buy, lamports: 180_000_000, usdc: 2_000_000, ts: 5, close: 100.0 });
        let plan = parse(FILE).unwrap();
        let live = plan.live.clone().unwrap();
        let mut t = Trader {
            chain: &mock,
            live: &live,
            state: &mut st,
            said: Vec::new(),
            settle_wait: Duration::ZERO,
            save: None,
        };
        t.reconcile(6).await; // a second later: what landed is seen at once
        assert!(st.pending.is_none());
        assert_eq!(st.account.lots.len(), 1);
        assert!((st.account.lots[0].sol - 0.02).abs() < 1e-9 && st.account.cash == 0.0, "{:?}", st.account);
    }

    #[tokio::test]
    async fn an_open_swap_is_not_given_up_while_it_may_still_land() {
        // sent, the program stopped, started again at once: the wallet is as it was
        let mock = Mock::new(0.18, 2.0, 100.0);
        let wallet = *mock.wallet.lock();
        let mut st = State { funded: true, ..State::default() };
        st.account.cash = 2.0;
        st.pending = Some(Pending { what: What::Buy, lamports: wallet.0, usdc: wallet.1, ts: 1_000, close: 100.0 });
        let plan = parse(FILE).unwrap();
        let live = plan.live.clone().unwrap();
        let mut t = Trader {
            chain: &mock,
            live: &live,
            state: &mut st,
            said: Vec::new(),
            settle_wait: Duration::ZERO,
            save: None,
        };
        t.reconcile(1_000 + SETTLED_AFTER_MS - 1).await;
        assert!(t.state.pending.is_some() && t.said[0].contains("waiting"), "{:?}", t.said);
        // a bar closing meanwhile sends nothing on top of it
        let b = bars(&[100.0, 100.0, 100.0, 100.0, 90.0]);
        t.on_bar(&plan, &plan.experiments[0], &b, &[], 90.0, 1_000 + SETTLED_AFTER_MS - 1).await;
        assert!(mock.sends.lock().is_empty() && t.state.pending.is_some());
        t.reconcile(1_000 + SETTLED_AFTER_MS).await;
        assert!(t.said.last().unwrap().contains("did not land"), "{:?}", t.said);
        assert!(st.pending.is_none() && st.account.lots.is_empty() && st.account.cash == 2.0, "{:?}", st.account);
    }

    /// Mainnet, read-only (nothing is signed): a buy arrives in the wallet as
    /// SOL itself, the quote less the signature and the tip. Simulated as a
    /// public exchange wallet that holds USDC and no wrapped SOL.
    /// `cargo test -p mobius-searcher --lib a_buy_arrives -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn a_buy_arrives_as_sol_in_the_wallet_on_mainnet() {
        let mut cfg = Config::default();
        cfg.wallet.pubkey = Some("5VCwKtCXgCJ6kit5FybXjvriW3xELsFDhYrPSqtJNmcD".into());
        let live = parse(FILE).unwrap().live.unwrap();
        let chain = Mainnet::new(&cfg, &live, true).unwrap();
        let (lamports, usdc) = chain.balances().await.unwrap();
        assert!(usdc >= 2_000_000, "the wallet this simulates as holds {usdc} USDC atoms: pick another");
        match chain.swap(false, 2_000_000).await {
            Sent::Simulated { out, lamports_after, note } => {
                let arrived = lamports_after as i64 - lamports as i64;
                println!("{note}\nquoted {out} lamports, arrived {arrived}");
                assert!(arrived > out as i64 * 99 / 100 - 5_000 - MAX_TIP as i64, "arrived {arrived} of {out} quoted");
            }
            other => panic!("{other:?}"),
        }
        // the quote asked for after a route failed in simulation: without that route's DEXes
        let first = match chain.try_swap(false, 2_000_000, &[]).await {
            Ok(Sent::Simulated { note, .. }) => note,
            _ => panic!("the first quote"),
        };
        let via: Vec<String> =
            first.split(" via ").nth(1).unwrap().split(',').next().unwrap().split(" + ").map(String::from).collect();
        match chain.try_swap(false, 2_000_000, &via).await {
            Ok(Sent::Simulated { note, .. }) => {
                println!("first {first}\nwithout {via:?}: {note}");
                assert!(via.iter().all(|d| !note.contains(d.as_str())), "{note}");
            }
            Ok(other) => panic!("{other:?}"),
            Err(No::Simulation { why, .. }) | Err(No::Other(why)) => panic!("{why}"),
        }
        // the two reads the sending relies on, against the real node
        let height = chain.block_height().await.unwrap();
        assert!(height > 300_000_000, "{height}");
        // a transaction of the last seconds (Jupiter's program is in one every block): confirmed, failed or not
        let jupiter = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
        let (v, _) = chain.rpc.call("getSignaturesForAddress", json!([jupiter, {"limit": 1}])).await.unwrap();
        let newest = v[0]["signature"].as_str().unwrap().to_string();
        let status = chain.status(&newest).await;
        println!("block height {height}; {newest}: {status:?}");
        assert!(matches!(status, Ok(Some(_))), "a fresh transaction is in a confirmed block: {status:?}");
        assert_eq!(chain.status("1111111111111111111111111111111111111111111111111111111111111111").await, Ok(None));
    }

    /// A local validator and test SOL only: the real sending against a real
    /// node. A transaction is signed, sent, seen confirmed and paid for once;
    /// sent again it is not paid for twice; one that fails on chain is told
    /// apart; one that fails its simulation as signed is never sent.
    /// `solana-test-validator --rpc-port 18899 --faucet-port 19900 --ledger /tmp/mobius-ledger`
    /// `MOBIUS_TEST_VALIDATOR=http://127.0.0.1:18899 cargo test -p mobius-searcher --lib on_a_local_validator -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn the_sending_lands_a_transaction_once_on_a_local_validator() {
        let url = std::env::var("MOBIUS_TEST_VALIDATOR").expect("MOBIUS_TEST_VALIDATOR: a local validator's RPC URL");
        assert!(url.contains("127.0.0.1") || url.contains("localhost"), "a local validator only: {url}");
        let key = std::env::temp_dir().join(format!("mobius-trade-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&key);
        searcher_execution::GeneratedWallet::new().write_new(&key).unwrap();
        let mut cfg = Config::default();
        (cfg.rpc.url, cfg.rpc.url_env) = (url, String::new());
        cfg.execution.live_enabled = true;
        (cfg.wallet.keypair_path, cfg.wallet.pubkey) = (Some(key.to_string_lossy().into()), None);
        cfg.jito.block_engine_url = "http://127.0.0.1:1".into(); // nobody there: Jito refuses, the node takes it
        let live = parse(FILE).unwrap().live.unwrap();
        let chain = Mainnet::new(&cfg, &live, false).unwrap();
        std::fs::remove_file(&key).unwrap();
        let (_, wallet) = chain.signer.as_ref().unwrap();
        let (me, other) = (wallet.pubkey(), searcher_execution::GeneratedWallet::new().pubkey());
        let balance = || async { chain.rpc.get_balance(&me).await.unwrap() };

        chain.rpc.call("requestAirdrop", json!([me.to_string(), 1_000_000_000u64])).await.unwrap();
        for _ in 0..60 {
            if balance().await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_eq!(balance().await, 1_000_000_000, "test SOL from the validator's faucet");

        // a transfer of `lamports` to `other`, assembled as a swap is: limit, price, instructions
        let (chain, none) = (&chain, &HashSet::new());
        let transfer = |lamports: u64| async move {
            let (blockhash, last_valid) = chain.latest_blockhash().await.unwrap();
            let params = AssemblyParams {
                payer: me,
                cu_limit: 10_000,
                cu_price_micro: 100_000, // 1,000 lamports of priority fee
                tip: Some((other, lamports)),
                dont_front: None,
                existing_atas: none,
                blockhash,
            };
            (compose_single(&[], &params).unwrap(), last_valid)
        };
        const FEE: u64 = 5_000 + 1_000;

        // 1. the way a swap goes: signed, simulated as signed, sent, confirmed
        let (tx, last_valid) = transfer(1_000_000).await;
        let (signature, fate, refused) = chain.sign_and_send(wallet, tx, last_valid).await.unwrap();
        println!("1. {signature}: {fate:?}; refused by {refused:?}");
        assert_eq!(fate, Fate::Confirmed);
        assert!(refused.len() == 1 && refused[0].starts_with("Jito"), "only Jito, which is not there: {refused:?}");
        assert_eq!(balance().await, 1_000_000_000 - 1_000_000 - FEE);
        assert_eq!(chain.rpc.get_balance(&other).await.unwrap(), 1_000_000);

        // 2. the same signed bytes handed over again and again land once
        let (mut tx, last_valid) = transfer(1_000_000).await;
        wallet.sign(&mut tx.tx).unwrap();
        let signature = tx.tx.signatures[0].to_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(reserialize(&tx.tx).unwrap());
        let every = Duration::from_millis(500);
        for round in 0..3 {
            let (fate, _) = deliver(chain, &b64, &signature, last_valid, every).await;
            assert_eq!(fate, Fate::Confirmed, "round {round}");
        }
        println!("2. {signature}: delivered three times, paid for once");
        assert_eq!(balance().await, 1_000_000_000 - 2 * (1_000_000 + FEE));
        assert_eq!(chain.rpc.get_balance(&other).await.unwrap(), 2_000_000);

        // 3. one that fails its simulation as signed (more than the wallet holds) is never sent
        let (tx, last_valid) = transfer(5_000_000_000).await;
        let unsent = chain.sign_and_send(wallet, tx, last_valid).await.unwrap_err();
        println!("3. not sent: {unsent:?}");
        assert!(matches!(unsent, Unsent::Simulation(_)), "{unsent:?}");
        assert_eq!(balance().await, 1_000_000_000 - 2 * (1_000_000 + FEE), "nothing was paid");

        // 4. sent all the same, it lands and fails: the fee is paid, nothing moves, and it is told apart
        let (mut tx, last_valid) = transfer(5_000_000_000).await;
        wallet.sign(&mut tx.tx).unwrap();
        let signature = tx.tx.signatures[0].to_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(reserialize(&tx.tx).unwrap());
        let (fate, _) = deliver(chain, &b64, &signature, last_valid, every).await;
        println!("4. {signature}: {fate:?}");
        assert!(matches!(&fate, Fate::Failed(err) if err.contains("InstructionError")), "{fate:?}");
        assert_eq!(balance().await, 1_000_000_000 - 2 * (1_000_000 + FEE) - FEE);
        assert_eq!(chain.rpc.get_balance(&other).await.unwrap(), 2_000_000);
    }

    /// A wire that answers each look from a script; the last answer repeats.
    struct Script {
        sends: Mutex<usize>,
        refuses: bool,
        statuses: Mutex<Vec<Result<Option<Option<String>>, String>>>,
        heights: Mutex<Vec<u64>>,
    }

    impl Script {
        fn new(statuses: Vec<Result<Option<Option<String>>, String>>, heights: Vec<u64>) -> Script {
            Script {
                sends: Mutex::new(0),
                refuses: false,
                statuses: Mutex::new(statuses),
                heights: Mutex::new(heights),
            }
        }
    }

    fn next<T: Clone>(script: &Mutex<Vec<T>>) -> T {
        let mut s = script.lock();
        if s.len() > 1 { s.remove(0) } else { s[0].clone() }
    }

    impl Wire for Script {
        async fn send(&self, _tx: &str) -> Vec<String> {
            *self.sends.lock() += 1;
            if self.refuses { vec!["Jito (429)".into()] } else { Vec::new() }
        }
        async fn status(&self, _signature: &str) -> Result<Option<Option<String>>, String> {
            next(&self.statuses)
        }
        async fn block_height(&self) -> Result<u64, String> {
            Ok(next(&self.heights))
        }
    }

    async fn fate(wire: &Script) -> Fate {
        deliver(wire, "tx", "sig", 200, Duration::ZERO).await.0
    }

    #[tokio::test]
    async fn a_transaction_is_sent_again_until_it_is_confirmed() {
        let wire = Script::new(vec![Ok(None), Ok(None), Ok(Some(None))], vec![100]);
        assert_eq!(fate(&wire).await, Fate::Confirmed);
        assert_eq!(*wire.sends.lock(), 3, "once a round until it shows");
        // a node that refuses it, or a status that cannot be read, does not end the trying
        let mut wire = Script::new(vec![Err("timeout".into()), Ok(None), Ok(Some(None))], vec![100]);
        wire.refuses = true;
        let (fate, refused) = deliver(&wire, "tx", "sig", 200, Duration::ZERO).await;
        assert_eq!((fate, refused), (Fate::Confirmed, vec!["Jito (429)".to_string()]), "and who refused is said");
    }

    #[tokio::test]
    async fn one_that_lands_and_fails_is_told_apart() {
        let wire = Script::new(vec![Ok(Some(Some("slippage".into())))], vec![100]);
        assert_eq!(fate(&wire).await, Fate::Failed("slippage".into()));
        assert_eq!(*wire.sends.lock(), 1);
    }

    #[tokio::test]
    async fn past_its_blockhash_it_is_given_up_after_one_last_look() {
        let wire = Script::new(vec![Ok(None)], vec![150, 200, 201]);
        assert_eq!(fate(&wire).await, Fate::Expired);
        assert_eq!(*wire.sends.lock(), 3, "still valid at block 200, its last");
        // it landed in the last block it could: the last look sees it
        let wire = Script::new(vec![Ok(None), Ok(Some(None))], vec![201]);
        assert_eq!(fate(&wire).await, Fate::Confirmed);
        // neither seen nor expired (the height cannot be trusted): it stops, and says it does not know
        let wire = Script::new(vec![Ok(None)], vec![100]);
        assert_eq!(fate(&wire).await, Fate::Unknown);
        assert_eq!(*wire.sends.lock(), ROUNDS);
    }

    #[test]
    fn a_swap_may_cost_the_wallet_its_fees_and_nothing_else() {
        let (fee, sol) = (9_000, 188_116_340);
        // a sale of 16,594,000 lamports: the wallet is down by them and the fee
        assert_eq!(beyond_fees(true, 16_594_000, 0, fee, (sol, sol - 16_594_000 - fee)), 0);
        // a route that opened a token account at the wallet's expense would show as its rent
        let after = sol - 16_594_000 - fee - TOKEN_ACCOUNT_RENT;
        assert_eq!(beyond_fees(true, 16_594_000, 0, fee, (sol, after)), TOKEN_ACCOUNT_RENT);
        // a buy quoted 16,550,000, at least 16,500,000: the quote less the fee arrives
        assert_eq!(beyond_fees(false, 2_000_000, 16_500_000, fee, (sol, sol + 16_550_000 - fee)), 0);
        assert_eq!(beyond_fees(false, 2_000_000, 16_500_000, fee, (sol, sol + 16_500_000 - fee)), 0, "at its minimum");
        let after = sol + 16_550_000 - fee - TOKEN_ACCOUNT_RENT;
        assert_eq!(beyond_fees(false, 2_000_000, 16_500_000, fee, (sol, after)), TOKEN_ACCOUNT_RENT - 50_000);
    }

    #[test]
    fn a_quote_keeps_to_the_dexes_the_file_allows_without_the_ones_that_failed() {
        let (any, some): (Vec<String>, Vec<String>) = (Vec::new(), vec!["Whirlpool".into(), "Meteora DLMM".into()]);
        let failed = vec!["Whirlpool".to_string()];
        assert_eq!(routes(&any, &[]), Some(DexFilter::Any));
        assert_eq!(routes(&any, &failed), Some(DexFilter::Exclude(failed.clone())));
        assert_eq!(routes(&some, &[]), Some(DexFilter::Only(some.clone())));
        assert_eq!(routes(&some, &failed), Some(DexFilter::Only(vec!["Meteora DLMM".into()])));
        assert_eq!(routes(&some, &some), None, "none is left");
    }

    #[test]
    fn only_a_confirmed_status_counts() {
        let ok = json!({"slot": 9, "confirmations": null, "err": null, "confirmationStatus": "finalized"});
        assert_eq!(confirmed(&ok), Some(None));
        let failed =
            json!({"slot": 9, "err": {"InstructionError": [5, {"Custom": 6001}]}, "confirmationStatus": "confirmed"});
        assert!(confirmed(&failed).unwrap().unwrap().contains("6001"));
        let early = json!({"slot": 9, "err": null, "confirmationStatus": "processed"});
        assert_eq!(confirmed(&early), None, "a processed block may still be dropped");
        assert_eq!(confirmed(&Value::Null), None, "not seen");
    }

    #[tokio::test]
    async fn the_state_is_written_down_before_a_swap_is_sent() {
        // every write of the state: the first must already name the swap under way
        let mock = Mock::new(0.188, 0.0, 120.0);
        let plan = parse(FILE).unwrap();
        let live = plan.live.clone().unwrap();
        let written: Mutex<Vec<(bool, bool, usize)>> = Mutex::new(Vec::new());
        let save = |s: &State| written.lock().push((s.pending.is_some(), s.funded, mock.sends.lock().len()));
        let mut st = State::default();
        let mut t = Trader {
            chain: &mock,
            live: &live,
            state: &mut st,
            said: Vec::new(),
            settle_wait: Duration::ZERO,
            save: Some(&save),
        };
        t.fund(120.0, 1).await;
        let w = written.lock().clone();
        assert_eq!(w.first(), Some(&(true, false, 0)), "pending is on disk before anything is sent: {w:?}");
        assert_eq!(w.last(), Some(&(false, true, 1)), "and cleared once the wallet shows the swap: {w:?}");
    }

    #[test]
    fn the_state_survives_being_saved() {
        let mut st = State { funded: true, ended: Some("stopped".into()), ..State::default() };
        st.pending = Some(Pending { what: What::Sell { lot: 0 }, lamports: 1, usdc: 2, ts: 3, close: 4.0 });
        let back: State = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(st, back);
    }
}
