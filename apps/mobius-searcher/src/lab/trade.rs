//! `--trade FILE`: one rule of the lab with real money.
//!
//! The rule decides as it does on paper, at the close of each bar; what
//! differs is the fill: a real SOL/USDC swap, built by Jupiter, assembled,
//! simulated, signed, simulated again as signed, sent once as a Jito bundle
//! (the engine's own send path, [`execute_live`]) and then **read back from
//! the wallet's balances**. The account only ever records what the wallet
//! shows: a swap that was sent and did not land changes nothing.
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
use searcher_execution::assemble::{AssemblyParams, compose_single};
use searcher_execution::live::{BundleConfirmation, LiveBackend, LiveOutcome, LiveParams, execute_live};
use searcher_execution::wallet::Wallet;
use searcher_jito::{InflightStatus, JitoClient, SendPermit};
use searcher_jupiter::{ApiKey, BuildRequest, JupiterClient};
use searcher_market::{RpcClient, SimulateOutcome};
use searcher_telemetry::{LimiterConfig, Telemetry};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A budget above this is refused: this is for trying a rule, not for size.
pub const MAX_BUDGET_USD: f64 = 25.0;
/// And one under this: a swap costs about a fifth of a cent whatever its size.
pub const MIN_BUDGET_USD: f64 = 1.0;
/// The words the file must carry.
pub const ACK: &str = "ALLOW LOSS";
/// Ceiling of a swap's priority fee, lamports: a bundle is ordered by its tip.
const MAX_PRIORITY_FEE: u64 = 1_000;
/// Ceiling of a swap's tip, lamports, whatever the config allows the
/// arbitrage: at a few USD a larger tip is a visible share of the trade.
const MAX_TIP: u64 = 10_000;
/// Lamports of a sold lot kept back to pay for the sale (signature, priority
/// fee, tip): a sale never spends SOL the run did not buy.
const SELL_HOLDBACK: u64 = 5_000 + MAX_PRIORITY_FEE + MAX_TIP;
/// A swap sent this long ago, ms, can no longer land: its blockhash lives about a minute.
pub const SETTLED_AFTER_MS: i64 = 120_000;
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
    blockhash_slots_to_expiry: u16,
    max_tip: u64,
    max_priority_fee: u64,
    fee_reserve: u64,
    live: LiveParams,
    height: AtomicU64,
}

struct Backend<'a>(&'a Mainnet);

impl LiveBackend for Backend<'_> {
    async fn simulate_signed(&self, tx_b64: String) -> Result<SimulateOutcome, String> {
        self.0.rpc.simulate_signed(&tx_b64).await.map_err(|e| e.to_string())
    }
    async fn send_bundle(&self, permit: &SendPermit, txs: Vec<String>) -> Result<String, String> {
        self.0.jito.send_bundle(permit, &txs).await.map_err(|e| e.to_string())
    }
    async fn inflight(&self, id: String) -> Result<Option<InflightStatus>, String> {
        let v = self.0.jito.inflight_statuses(&[id]).await.map_err(|e| e.to_string())?;
        Ok(v.into_iter().next().map(|(_, s)| s))
    }
    async fn balance(&self, a: Address) -> Result<u64, String> {
        self.0.rpc.get_balance(&a).await.map_err(|e| e.to_string())
    }
    fn block_height(&self) -> Option<u64> {
        Some(self.0.height.load(Ordering::Relaxed)).filter(|h| *h > 0)
    }
    async fn bundle_status(&self, id: String) -> Result<BundleConfirmation, String> {
        let v = self.0.jito.bundle_statuses(&[id]).await.map_err(|e| e.to_string())?;
        Ok(v.into_iter().next().map(|s| (s.confirmation_status, s.err)))
    }
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
            blockhash_slots_to_expiry: cfg.jupiter.blockhash_slots_to_expiry,
            max_tip: cfg.risk.max_jito_tip_lamports,
            max_priority_fee: cfg.risk.max_priority_fee_lamports.min(MAX_PRIORITY_FEE),
            fee_reserve: cfg.risk.min_wallet_sol_for_fees_lamports,
            live: LiveParams {
                min_wallet_lamports: cfg.risk.min_wallet_sol_for_fees_lamports,
                blockhash_margin: 10,
                poll: Duration::from_millis(cfg.execution.bundle_status_poll_ms),
                timeout: Duration::from_millis(cfg.execution.bundle_timeout_ms),
            },
            height: AtomicU64::new(0),
        })
    }

    pub fn taker(&self) -> Address {
        self.taker
    }

    async fn try_swap(&self, sell_sol: bool, amount: u64) -> Result<Sent, String> {
        let (sol, usdc) = (mint(well_known::WSOL_MINT), mint(well_known::USDC_MINT));
        let req = BuildRequest {
            input_mint: if sell_sol { sol } else { usdc },
            output_mint: if sell_sol { usdc } else { sol },
            amount,
            taker: self.taker,
            slippage: SlippageSpec::Fixed(self.slippage_bps),
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            cu_price_percentile: self.cu_price_percentile.clone(),
            max_accounts: Some(30),
            blockhash_slots_to_expiry: self.blockhash_slots_to_expiry,
            for_jito_bundle: true,
        };
        let built = self.jupiter.build(&req, 0).await.map_err(|e| format!("Jupiter: {e}"))?;
        let leg = &built.leg;
        // the least the block engine takes, or the going rate, never above the configured ceiling
        let floor = self.jito.tip_floor().await.map(|f| f.p50).unwrap_or(1_000);
        let tip = floor.clamp(1_000, self.max_tip.clamp(1_000, MAX_TIP));
        let tip_account: Address =
            searcher_jito::KNOWN_TIP_ACCOUNTS[0].parse().map_err(|_| "tip account".to_string())?;
        let none = HashSet::new();
        let params = |cu_limit: u32, cu_price: u64| AssemblyParams {
            payer: self.taker,
            cu_limit,
            cu_price_micro: cu_price,
            tip: Some((tip_account, tip)),
            dont_front: None,
            existing_atas: &none,
            blockhash: built.instructions.blockhash,
        };
        let price = leg.cu_price_micro.unwrap_or(0);
        // without a priority fee: at the largest limit it would not be the one paid
        let probe = compose_single(&[&built.instructions], &params(searcher_core::units::MAX_COMPUTE_UNITS_PER_TX, 0))
            .map_err(|e| format!("assembling: {e}"))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&probe.wire);
        let before =
            self.rpc.get_balance(&self.taker).await.map_err(|e| format!("the wallet could not be read: {e}"))?;
        let sim = self.rpc.simulate(&b64, &[self.taker]).await.map_err(|e| format!("simulation unavailable: {e}"))?;
        if let Some(err) = &sim.err {
            return Ok(Sent::NotSent(format!(
                "the simulation failed ({err}): {}",
                sim.logs.last().cloned().unwrap_or_default()
            )));
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
        let units = sim.units_consumed.ok_or("the simulation gave no compute units")?;
        let cu_limit = ((units as f64 * 1.2) as u32 + 10_000).min(searcher_core::units::MAX_COMPUTE_UNITS_PER_TX);
        // the priority fee is the limit times the price: keep it under the configured ceiling
        let price = price.min(self.max_priority_fee.saturating_mul(1_000_000) / u64::from(cu_limit.max(1)));
        let note = format!(
            "{} {} for at least {} {} (quoted {}), {} CU, tip {} lamports",
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
            tip
        );
        let Some((permit, wallet)) = &self.signer else {
            return Ok(Sent::Simulated { out: leg.out_amount, lamports_after: after, note });
        };
        let tx =
            compose_single(&[&built.instructions], &params(cu_limit, price)).map_err(|e| format!("assembling: {e}"))?;
        let (h, _) = self
            .rpc
            .call("getBlockHeight", serde_json::json!([{"commitment": "processed"}]))
            .await
            .map_err(|e| format!("block height: {e}"))?;
        self.height.store(h.as_u64().ok_or("block height")?, Ordering::Relaxed);
        Ok(
            match execute_live(&Backend(self), permit, wallet, vec![tx], leg.last_valid_block_height, &self.live).await
            {
                LiveOutcome::NotSent(why) => Sent::NotSent(why),
                LiveOutcome::Landed { signatures, slot, .. } => Sent::Sent(format!(
                    "{note}; landed in slot {slot}, signature {}",
                    signatures.first().cloned().unwrap_or_default()
                )),
                LiveOutcome::Failed { bundle_id, reason, .. } => {
                    Sent::Sent(format!("{note}; bundle {bundle_id} failed: {reason}"))
                }
                LiveOutcome::TimedOut { bundle_id } => {
                    Sent::Sent(format!("{note}; bundle {bundle_id}: no answer in time"))
                }
            },
        )
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
        self.try_swap(sell_sol, amount).await.unwrap_or_else(Sent::NotSent)
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
