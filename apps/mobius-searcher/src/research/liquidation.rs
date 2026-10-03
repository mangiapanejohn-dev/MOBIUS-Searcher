//! `--research-liquidations`: every Morpho Blue liquidation of the last N
//! days, read back from chain: what it paid, what the winner spent on gas,
//! and for how many blocks the position had been liquidatable when the
//! winner took it. That last figure decides whether a liquidator behind a
//! public node could ever have been first. Nothing is signed or sent, and
//! no Jupiter budget is used.

use super::report::Dist;
use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use parking_lot::Mutex;
use searcher_core::Ts;
use searcher_core::config::{Config, LiquidationTarget};
use searcher_storage::ResearchStore;
use searcher_storage::research::LiqEvent;
use searcher_venues::evm::{self, EvmError, EvmRpc, Receipt, Token, UniV3Pool, word, word_f64, word_u128_of};
use searcher_venues::morpho::{Liquidation, MarketParams, liquidate_topic};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How far back "liquidatable since" is searched: about 4.5 hours of Base's
/// 2-second blocks. A position unhealthy for longer is reported as such.
const HORIZON: u64 = 8_192;
/// Blocks at the chain head left for the next run (they can still change).
const TIP_MARGIN: u64 = 10;
/// One ETH price per this many blocks, to value gas.
const ETH_PRICE_EVERY: u64 = 300;
/// Loan tokens counted as one dollar.
const DOLLARS: [&str; 5] = ["USDC", "USDT", "DAI", "USDS", "USDbC"];
/// Requests a second to the node, and how many run at once.
const RPS: f64 = 6.0;
const AT_ONCE: usize = 6;

/// Requests that had to be repeated (shown at the end of a run).
static REPEATED: AtomicUsize = AtomicUsize::new(0);

/// A request repeated after a transport error or a rate limit.
async fn retry<T>(mut f: impl AsyncFnMut() -> Result<T, EvmError>) -> Result<T, EvmError> {
    let mut wait = Duration::from_secs(1);
    for _ in 0..4 {
        match f().await {
            Err(EvmError::Transport(_)) => {}
            Err(EvmError::Rpc { code, ref message })
                if matches!(code, 429 | -32005 | -32016) || message.to_ascii_lowercase().contains("rate limit") => {}
            other => return other,
        }
        REPEATED.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(wait).await;
        wait *= 3;
    }
    f().await
}

/// The first block at whose end `probe` says yes, searching back from
/// `block − 1` and never below `floor`. Returns that block (`block` itself
/// when the position only became liquidatable inside it), whether the search
/// stopped at the floor, and how many blocks were asked.
async fn since<E>(
    mut probe: impl AsyncFnMut(u64) -> Result<bool, E>,
    block: u64,
    floor: u64,
) -> Result<(u64, bool, u32), E> {
    let mut asked = 0;
    let mut ask = async |n: u64| {
        asked += 1;
        probe(n).await
    };
    if block == 0 || floor >= block || !ask(block - 1).await? {
        return Ok((block, floor >= block, asked));
    }
    // liquidatable at `yes`; step back twice as far each time until it is not
    let (mut yes, mut step) = (block - 1, 1);
    let no = loop {
        if yes <= floor {
            return Ok((yes, true, asked));
        }
        let n = yes.saturating_sub(step).max(floor);
        if !ask(n).await? {
            break n;
        }
        (yes, step) = (n, step * 2);
    };
    let (mut lo, mut hi) = (no, yes);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if ask(mid).await? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok((hi, false, asked))
}

struct Market {
    params: MarketParams,
    loan: Token,
    collateral: Token,
}

/// One chain's node with what has been read from it so far.
struct Chain<'a> {
    rpc: EvmRpc,
    target: &'a LiquidationTarget,
    /// Prices ETH (gas) in dollars; `None` when the venue lists no such pool.
    weth_pool: Option<UniV3Pool>,
    markets: Mutex<HashMap<String, std::sync::Arc<Market>>>,
    receipts: Mutex<HashMap<String, Receipt>>,
    blocks: Mutex<HashMap<u64, (u64, u128)>>,
    eth_usd: Mutex<HashMap<u64, Option<f64>>>,
}

impl Chain<'_> {
    async fn token(&self, address: &str) -> Result<Token, EvmError> {
        match retry(async || evm::token(&self.rpc, address.to_string()).await).await {
            Err(EvmError::Decode(_)) => {
                // a symbol that is not a string: keep the address, the decimals are what matters
                let d = retry(async || self.rpc.call(address, evm::DECIMALS, "latest").await).await?;
                Ok(Token {
                    address: address.to_string(),
                    symbol: address[..10.min(address.len())].to_string(),
                    decimals: word_u128_of(word(&d, 0)?)? as u8,
                })
            }
            other => other,
        }
    }

    async fn market(&self, id: &str) -> Result<std::sync::Arc<Market>, EvmError> {
        if let Some(m) = self.markets.lock().get(id) {
            return Ok(m.clone());
        }
        let params = retry(async || MarketParams::load(&self.rpc, &self.target.morpho, id).await).await?;
        let loan = self.token(&params.loan_token).await?;
        let collateral = self.token(&params.collateral_token).await?;
        let m = std::sync::Arc::new(Market { params, loan, collateral });
        self.markets.lock().insert(id.to_string(), m.clone());
        Ok(m)
    }

    async fn block(&self, number: u64) -> Result<(u64, u128), EvmError> {
        if let Some(b) = self.blocks.lock().get(&number) {
            return Ok(*b);
        }
        let b = retry(async || self.rpc.block(number).await).await?;
        self.blocks.lock().insert(number, b);
        Ok(b)
    }

    async fn receipt(&self, tx: &str) -> Result<Receipt, EvmError> {
        if let Some(r) = self.receipts.lock().get(tx) {
            return Ok(r.clone());
        }
        let r = retry(async || self.rpc.receipt(tx).await).await?;
        let r = r.ok_or_else(|| EvmError::Decode(format!("no receipt for {tx}")))?;
        self.receipts.lock().insert(tx.to_string(), r.clone());
        Ok(r)
    }

    /// Dollars per ETH around `block` (the pool's mid; gas is small, a mid is enough).
    async fn eth_usd(&self, block: u64) -> Option<f64> {
        let bucket = block / ETH_PRICE_EVERY;
        if let Some(p) = self.eth_usd.lock().get(&bucket) {
            return *p;
        }
        let price = match &self.weth_pool {
            Some(pool) => retry(async || self.rpc.call(&pool.address, evm::SLOT0, &format!("0x{block:x}")).await)
                .await
                .ok()
                .and_then(|s0| pool.mid(word_f64(word(&s0, 0).ok()?), "WETH")),
            None => None,
        };
        self.eth_usd.lock().insert(bucket, price);
        price
    }

    /// Everything about one event beyond its log. `prev` is the same
    /// position's previous liquidation; `in_tx` how many events share the
    /// transaction (and its gas).
    async fn work_out(&self, e: &mut LiqEvent, prev: Option<&LiqEvent>, in_tx: usize) -> Result<(), EvmError> {
        let (time, base_fee) = self.block(e.block).await?;
        let rc = self.receipt(&e.tx).await?;
        let eth_usd = self.eth_usd(e.block).await;
        let morpho = &self.target.morpho;
        let m = self.market(&e.market).await?;
        let params = &m.params;
        let repaid = e.repaid.parse::<u128>().map_err(|x| EvmError::Decode(x.to_string()))? as f64;
        e.block_time = Some(time as i64);
        e.tx_index = Some(rc.index);
        e.sender = Some(rc.from.clone());
        e.loan = Some(m.loan.symbol.clone());
        e.collateral = Some(m.collateral.symbol.clone());
        e.lltv = Some(params.lltv);
        e.incentive = Some(repaid / 10f64.powi(m.loan.decimals as i32) * (params.lif() - 1.0));
        e.loan_usd = match m.loan.symbol.as_str() {
            s if DOLLARS.contains(&s) => Some(1.0),
            "WETH" => eth_usd,
            _ => None,
        };
        e.gas_eth = Some(rc.fee_wei() as f64 / 1e18 / in_tx.max(1) as f64);
        e.priority_gwei = Some(rc.effective_gas_price.saturating_sub(base_fee) as f64 / 1e9);
        e.eth_usd = eth_usd;

        // a position liquidated in steps inside one block was one opportunity
        if let Some(p) = prev.filter(|p| p.block == e.block && p.since_kind.is_some()) {
            (e.since_block, e.since_time, e.since_kind, e.probes) =
                (p.since_block, p.since_time, p.since_kind.clone(), Some(0));
            return Ok(());
        }
        let horizon = e.block.saturating_sub(HORIZON);
        let after = prev.map(|p| p.block).filter(|b| *b >= horizon);
        let rpc = &self.rpc;
        let probe = async |n: u64| retry(async || params.liquidatable(rpc, morpho, &e.borrower, n).await).await;
        let (block, at_floor, probes) = since(probe, e.block, after.unwrap_or(horizon)).await?;
        e.since_block = Some(block);
        e.probes = Some(probes);
        e.since_kind = Some(
            match (block == e.block, at_floor, after) {
                (true, false, _) => "same_block",
                (_, true, Some(_)) => "after_liquidation",
                (_, true, None) => "horizon",
                _ => "earlier",
            }
            .to_string(),
        );
        e.since_time = Some(if block == e.block { time } else { self.block(block).await?.0 } as i64);
        Ok(())
    }
}

fn event(venue: &str, l: Liquidation) -> LiqEvent {
    LiqEvent {
        venue: venue.to_string(),
        block: l.block,
        log_index: l.log_index,
        tx: l.tx,
        market: l.market,
        borrower: l.borrower,
        caller: l.caller,
        repaid: l.repaid_assets.to_string(),
        seized: l.seized_assets.to_string(),
        bad_debt: l.bad_debt_assets.to_string(),
        ..Default::default()
    }
}

/// Bring one target's recorded liquidations up to the chain head, going back
/// `days`. Returns the first block of that period and the seconds per block.
async fn replay(cfg: &Config, store: &ResearchStore, target: &LiquidationTarget, days: u32) -> Result<(u64, f64)> {
    let venue = cfg.venues.get(&target.venue).with_context(|| format!("no venue `{}` in [venues]", target.venue))?;
    let keyed = !venue.rpc_url_env.is_empty() && std::env::var(&venue.rpc_url_env).is_ok_and(|v| !v.trim().is_empty());
    let url = if keyed { venue.resolved_url() } else { target.rpc_url.clone() };
    let rpc = EvmRpc::new(&url, RPS)?;
    let chain_id = retry(async || rpc.chain_id().await).await?;
    if venue.chain_id.is_some_and(|c| c != chain_id) {
        bail!(
            "{} answers for chain {chain_id}, the venue is chain {:?}",
            searcher_core::config::display_url(&url),
            venue.chain_id
        );
    }
    let (head, head_time) = retry(async || rpc.latest_block().await).await?;
    let back = 100_000.min(head.saturating_sub(1)).max(1);
    let (old_time, _) = retry(async || rpc.block(head - back).await).await?;
    let block_s = (head_time.saturating_sub(old_time)) as f64 / back as f64;
    if block_s <= 0.0 {
        bail!("cannot tell the block time of chain {chain_id}");
    }
    let from = head.saturating_sub((days as f64 * 86_400.0 / block_s) as u64);
    let tip = head.saturating_sub(TIP_MARGIN);
    let name = &target.venue;
    println!(
        "{name}: {} · chain {chain_id} · head {head} · {block_s:.2} s per block · reading {days} days back to block {from}",
        searcher_core::config::display_url(&url)
    );

    // 1. find the events: newer blocks first, then older ones if more days are asked for than before
    let topic = liquidate_topic();
    let (mut first, mut next) = store.liq_scan(name)?.filter(|(_, next)| *next >= from).unwrap_or((from, from));
    let mut ranges = Vec::new(); // each one request; `true` = extends the searched range upwards
    let mut lo = next;
    while lo <= tip {
        let hi = (lo + target.log_window - 1).min(tip);
        ranges.push((lo, hi, true));
        lo = hi + 1;
    }
    let mut hi = first;
    while hi > from {
        let lo = hi.saturating_sub(target.log_window).max(from);
        ranges.push((lo, hi - 1, false));
        hi = lo;
    }
    let (mut found, mut shown) = (0, std::time::Instant::now());
    for (n, group) in ranges.chunks(AT_ONCE).enumerate() {
        let answers = join_all(group.iter().map(|&(lo, hi, _)| {
            let (rpc, topic) = (&rpc, &topic);
            async move { retry(async || rpc.logs(&target.morpho, topic, lo, hi).await).await }
        }))
        .await;
        for (&(lo, hi, up), logs) in group.iter().zip(answers) {
            let logs = logs.with_context(|| {
                format!("eth_getLogs {lo}..{hi} (research.liquidations log_window = {})", target.log_window)
            })?;
            let events: Vec<LiqEvent> = logs
                .iter()
                .map(Liquidation::decode)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|l| event(name, l))
                .collect();
            store.insert_liq_events(&events)?;
            found += events.len();
            if up {
                next = hi + 1;
            } else {
                first = lo;
            }
            store.set_liq_scan(name, first, next)?;
        }
        if shown.elapsed() > Duration::from_secs(20) {
            shown = std::time::Instant::now();
            let part = ((n + 1) * AT_ONCE).min(ranges.len()) as f64 / ranges.len() as f64;
            println!("  searching blocks: {:.0} % · {found} liquidations so far", part * 100.0);
        }
    }

    // 2. work out each one that is not done yet
    let weth_pool = match venue.pools.first() {
        Some(p) => match retry(async || UniV3Pool::load(&rpc, p).await).await {
            Ok(pool) if pool.token("WETH").is_some() => Some(pool),
            Ok(pool) => {
                println!("  gas is not valued in dollars: the venue's first pool is {}, not WETH", pool.market());
                None
            }
            Err(e) => {
                println!("  gas is not valued in dollars: {e}");
                None
            }
        },
        None => {
            println!("  gas is not valued in dollars: [venues.{name}] lists no pool");
            None
        }
    };
    let chain = Chain {
        rpc,
        target,
        weth_pool,
        markets: Default::default(),
        receipts: Default::default(),
        blocks: Default::default(),
        eth_usd: Default::default(),
    };
    let mut events = store.liq_events(name, from)?;
    let mut in_tx: HashMap<String, usize> = HashMap::new();
    for e in &events {
        *in_tx.entry(e.tx.clone()).or_default() += 1;
    }
    // each event's previous liquidation of the same position
    let mut last = HashMap::new();
    let prev: Vec<Option<usize>> =
        events.iter().enumerate().map(|(i, e)| last.insert((e.market.clone(), e.borrower.clone()), i)).collect();
    // A liquidation done in steps inside one block: the later steps copy
    // from the first, so they go last and in order; the rest several at once.
    let pending: Vec<usize> = (0..events.len()).filter(|i| events[*i].since_kind.is_none()).collect();
    let (steps, firsts): (Vec<usize>, Vec<usize>) =
        pending.iter().partition(|i| prev[**i].is_some_and(|p| events[p].block == events[**i].block));
    if !pending.is_empty() {
        println!("  {} liquidations in the period, {} to work out (a few requests each)", events.len(), pending.len());
    }
    let (mut worked, mut failed, mut shown) = (0, 0, std::time::Instant::now());
    for group in firsts.chunks(AT_ONCE).chain(steps.chunks(1)) {
        let done = join_all(group.iter().map(|&i| {
            let (chain, mut e, prev, shared) =
                (&chain, events[i].clone(), prev[i].map(|p| &events[p]), in_tx[&events[i].tx]);
            async move {
                e.err = chain.work_out(&mut e, prev, shared).await.err().map(|x| x.to_string());
                e
            }
        }))
        .await;
        for (&i, e) in group.iter().zip(done) {
            failed += e.err.is_some() as usize;
            store.update_liq_event(&e)?;
            events[i] = e;
            worked += 1;
        }
        if shown.elapsed() > Duration::from_secs(20) {
            shown = std::time::Instant::now();
            println!("  worked out {worked} of {} ({failed} failed)", pending.len());
        }
    }
    let repeated = REPEATED.swap(0, Ordering::Relaxed);
    if repeated > 0 {
        println!("  {repeated} requests were repeated after a rate limit or a transport error");
    }
    Ok((from, block_s))
}

pub async fn run(cfg: &Config, days: u32, json: bool) -> Result<()> {
    if cfg.research.liquidations.is_empty() {
        bail!("research.liquidations is empty: no lending market to read");
    }
    let db = cfg.data_dir().join("research.sqlite");
    let store = ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let mut reports = Vec::new();
    for target in &cfg.research.liquidations {
        let (from, block_s) =
            replay(cfg, &store, target, days).await.with_context(|| format!("liquidations on {}", target.venue))?;
        reports.push(build(&target.venue, days, block_s, &store.liq_events(&target.venue, from)?));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        for r in &reports {
            print!("\n{}", render(r));
        }
    }
    Ok(())
}

#[derive(Serialize, Default, Debug)]
pub struct LiqReport {
    pub venue: String,
    pub days: u32,
    pub block_s: f64,
    pub events: usize,
    pub transactions: usize,
    /// Events whose details could not be read, by error.
    pub failed: BTreeMap<String, usize>,
    /// Incentive in dollars, of the events whose loan token has a dollar price.
    pub incentive_usd: Dist,
    pub incentive_usd_sum: f64,
    /// loan token → events not valued
    pub not_valued: BTreeMap<String, usize>,
    pub active_days: usize,
    pub per_day_median: f64,
    /// The busiest days: (day, events, incentive in dollars).
    pub busiest: Vec<(String, usize, f64)>,
    /// collateral → (events, incentive in dollars), largest first
    pub collateral: Vec<(String, usize, f64)>,
    /// How long the position had been liquidatable: (label, events, incentive in dollars).
    pub waited: Vec<(String, usize, f64)>,
    /// By incentive size: (label, events, median wait in blocks, share taken
    /// in the same or the next block, distinct liquidators, top three liquidators' share of events).
    pub by_size: Vec<(String, usize, Option<f64>, f64, usize, f64)>,
    /// Contracts that called `liquidate`, and the accounts that signed for them.
    pub liquidators: usize,
    pub senders: usize,
    /// (top k liquidators by incentive, their share of events, their share of the incentive)
    pub top: Vec<(usize, f64, f64)>,
    pub gas_usd: Dist,
    /// The winner's gas as a percentage of the incentive (incentives of a cent or more).
    pub gas_pct: Dist,
    pub priority_gwei: Dist,
    /// Events whose incentive was below the gas the winner paid.
    pub under_gas: usize,
    /// For a slower liquidator: (waited at least this many blocks, events,
    /// events per day, incentive − winner's gas: median, sum).
    pub slower: Vec<(u64, usize, f64, Option<f64>, f64)>,
}

/// Blocks between "liquidatable" and the winner's block.
fn wait(e: &LiqEvent) -> Option<u64> {
    e.since_block.map(|s| e.block.saturating_sub(s))
}

fn usd(e: &LiqEvent) -> Option<f64> {
    Some(e.incentive? * e.loan_usd?)
}

fn gas_usd(e: &LiqEvent) -> Option<f64> {
    Some(e.gas_eth? * e.eth_usd?)
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.sort_by(|a, b| a.total_cmp(b));
    (!v.is_empty()).then(|| v[(v.len() - 1) / 2])
}

/// Share of `events` taken by the `k` liquidators with the most of them.
fn top_share<'a>(events: impl Iterator<Item = &'a LiqEvent>, k: usize) -> (usize, f64) {
    let mut by: HashMap<&str, usize> = HashMap::new();
    let mut n = 0;
    for e in events {
        *by.entry(&e.caller).or_default() += 1;
        n += 1;
    }
    let mut counts: Vec<usize> = by.values().copied().collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    (by.len(), counts.iter().take(k).sum::<usize>() as f64 / n.max(1) as f64)
}

pub fn build(venue: &str, days: u32, block_s: f64, events: &[LiqEvent]) -> LiqReport {
    let mut r = LiqReport { venue: venue.to_string(), days, block_s, events: events.len(), ..Default::default() };
    r.transactions = events.iter().map(|e| e.tx.as_str()).collect::<std::collections::HashSet<_>>().len();
    for e in events {
        if let Some(err) = &e.err {
            *r.failed.entry(err.chars().take(80).collect()).or_default() += 1;
        }
        if e.incentive.is_some() && e.loan_usd.is_none() {
            *r.not_valued.entry(e.loan.clone().unwrap_or_default()).or_default() += 1;
        }
    }
    let valued: Vec<(&LiqEvent, f64)> = events.iter().filter_map(|e| Some((e, usd(e)?))).collect();
    let total: f64 = valued.iter().map(|(_, v)| v).sum();
    r.incentive_usd = Dist::of(valued.iter().map(|(_, v)| *v).collect());
    r.incentive_usd_sum = total;

    let mut days_seen: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut collateral: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for e in events {
        let v = usd(e).unwrap_or(0.0);
        if let Some(t) = e.block_time {
            let d = days_seen.entry(Ts(t * 1_000_000).format("%Y-%m-%d")).or_default();
            *d = (d.0 + 1, d.1 + v);
        }
        if let Some(c) = &e.collateral {
            let c = collateral.entry(c.clone()).or_default();
            *c = (c.0 + 1, c.1 + v);
        }
    }
    r.active_days = days_seen.len();
    r.per_day_median = median(days_seen.values().map(|d| d.0 as f64).collect()).unwrap_or(0.0);
    r.busiest = days_seen.into_iter().map(|(d, (n, v))| (d, n, v)).collect();
    r.busiest.sort_by_key(|d| std::cmp::Reverse(d.1));
    r.busiest.truncate(4);
    r.collateral = collateral.into_iter().map(|(c, (n, v))| (c, n, v)).collect();
    r.collateral.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.1.cmp(&a.1)));
    r.collateral.truncate(8);

    for (label, lo, hi) in [
        ("inside the winner's block", 0, 0),
        ("1 block (the next one)", 1, 1),
        ("2–3 blocks", 2, 3),
        ("4–10 blocks", 4, 10),
        ("11–100 blocks", 11, 100),
        ("over 100 blocks", 101, u64::MAX),
    ] {
        let rows = || events.iter().filter(|e| wait(e).is_some_and(|w| (lo..=hi).contains(&w)));
        r.waited.push((label.to_string(), rows().count(), rows().filter_map(usd).sum()));
    }
    let unmeasured = || events.iter().filter(|e| wait(e).is_none());
    r.waited.push(("not measured".to_string(), unmeasured().count(), unmeasured().filter_map(usd).sum()));

    for (label, lo, hi) in
        [("under $1", 0.0, 1.0), ("$1–10", 1.0, 10.0), ("$10–100", 10.0, 100.0), ("over $100", 100.0, f64::MAX)]
    {
        let rows: Vec<&LiqEvent> = valued.iter().filter(|(_, v)| (lo..hi).contains(v)).map(|(e, _)| *e).collect();
        let waits: Vec<f64> = rows.iter().filter_map(|e| wait(e)).map(|w| w as f64).collect();
        let quick = waits.iter().filter(|w| **w <= 1.0).count() as f64 / waits.len().max(1) as f64;
        let (senders, top3) = top_share(rows.iter().copied(), 3);
        r.by_size.push((label.to_string(), rows.len(), median(waits), quick, senders, top3));
    }

    let mut by_caller: HashMap<&str, (usize, f64)> = HashMap::new();
    for e in events {
        let s = by_caller.entry(&e.caller).or_default();
        *s = (s.0 + 1, s.1 + usd(e).unwrap_or(0.0));
    }
    r.liquidators = by_caller.len();
    r.senders = events.iter().filter_map(|e| e.sender.as_deref()).collect::<std::collections::HashSet<_>>().len();
    let mut by_value: Vec<(usize, f64)> = by_caller.into_values().collect();
    by_value.sort_by(|a, b| b.1.total_cmp(&a.1));
    for k in [1, 3, 5] {
        let (n, v) = by_value.iter().take(k).fold((0, 0.0), |a, s| (a.0 + s.0, a.1 + s.1));
        r.top.push((k, n as f64 / events.len().max(1) as f64, if total > 0.0 { v / total } else { 0.0 }));
    }

    r.gas_usd = Dist::of(events.iter().filter_map(gas_usd).collect());
    r.priority_gwei = Dist::of(events.iter().filter_map(|e| e.priority_gwei).collect());
    let paid: Vec<(f64, f64)> = valued.iter().filter_map(|(e, v)| Some((*v, gas_usd(e)?))).collect();
    r.gas_pct = Dist::of(paid.iter().filter(|(v, _)| *v >= 0.01).map(|(v, g)| g / v * 100.0).collect());
    r.under_gas = paid.iter().filter(|(v, g)| v < g).count();

    for min in [2, 3, 5, 10, 30, 150] {
        let net: Vec<f64> = valued
            .iter()
            .filter(|(e, _)| wait(e).is_some_and(|w| w >= min))
            .filter_map(|(e, v)| Some(v - gas_usd(e)?))
            .filter(|n| *n > 0.0)
            .collect();
        r.slower.push((min, net.len(), net.len() as f64 / days.max(1) as f64, median(net.clone()), net.iter().sum()));
    }
    r
}

fn money(v: f64) -> String {
    let v = v + 0.0; // an empty sum is −0.0
    let whole = format!("{:.0}", v.abs());
    let mut out = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}${out}", if v < 0.0 { "−" } else { "" })
}

fn cols(d: &Dist, digits: usize) -> String {
    let f = |v: Option<f64>| v.map(|x| format!("{x:.digits$}")).unwrap_or_else(|| "-".into());
    format!(
        "{:>6}  {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        d.n,
        f(d.p5),
        f(d.p25),
        f(d.p50),
        f(d.p75),
        f(d.p95),
        f(d.max)
    )
}

const HEAD: &str = "     n         p5       p25       p50       p75       p95        max";

pub fn render(r: &LiqReport) -> String {
    let mut o = String::new();
    let pct = |n: usize| n as f64 / r.events.max(1) as f64 * 100.0;
    let of_total = |v: f64| if r.incentive_usd_sum > 0.0 { v / r.incentive_usd_sum * 100.0 + 0.0 } else { 0.0 };
    let _ = writeln!(
        o,
        "LIQUIDATIONS · {} · Morpho Blue · last {} days · {} events in {} transactions",
        r.venue, r.days, r.events, r.transactions
    );
    let _ = writeln!(o, "  Read back from chain; nothing here was attempted by this program.");
    let _ = writeln!(o, "  incentive = repaid × (LIF − 1): what the protocol pays at its oracle price, before gas");
    let _ =
        writeln!(o, "  and before the seized collateral is sold. Dollars where the loan token is a dollar or WETH.");
    if r.events == 0 {
        return o;
    }

    let _ = writeln!(o, "\n1. How much, and how often");
    let _ = writeln!(o, "   {:<22}{HEAD}         sum", "");
    let _ = writeln!(o, "   {:<22}{}  {:>10}", "incentive ($)", cols(&r.incentive_usd, 2), money(r.incentive_usd_sum));
    if !r.not_valued.is_empty() {
        let list: Vec<String> = r.not_valued.iter().map(|(t, n)| format!("{t} {n}")).collect();
        let _ = writeln!(o, "   not valued (loan token without a dollar price here): {}", list.join(", "));
    }
    let busy: (usize, f64) = r.busiest.iter().fold((0, 0.0), |a, d| (a.0 + d.1, a.1 + d.2));
    let _ = writeln!(
        o,
        "   calendar days with a liquidation: {} · median {} a day on those · the {} busiest hold {:.0} % of the events and {:.0} % of the incentive",
        r.active_days,
        r.per_day_median,
        r.busiest.len(),
        pct(busy.0),
        of_total(busy.1)
    );
    for (day, n, v) in &r.busiest {
        let _ = writeln!(o, "      {day}  {n:>5} events  {:>10}", money(*v));
    }
    let _ = writeln!(o, "   by collateral:");
    for (c, n, v) in &r.collateral {
        let _ = writeln!(o, "      {c:<12} {n:>5} events  {:>10}", money(*v));
    }

    let _ = writeln!(o, "\n2. How long each one had been there when the winner took it");
    let _ = writeln!(
        o,
        "   From the first block at whose end the contract itself says the position can be\n   liquidated to the winner's block. A block is {:.2} s.",
        r.block_s
    );
    let _ = writeln!(o, "   {:<28} {:>6} {:>7} {:>12} {:>7}", "waited", "events", "share", "incentive", "share");
    for (label, n, v) in &r.waited {
        let _ = writeln!(o, "   {label:<28} {n:>6} {:>6.1}% {:>12} {:>6.1}%", pct(*n), money(*v), of_total(*v));
    }
    let _ = writeln!(
        o,
        "   {:<12} {:>6} {:>13} {:>22} {:>12} {:>15}",
        "by size", "events", "median wait", "same or next block", "liquidators", "top 3 of them"
    );
    for (label, n, wait, quick, senders, top3) in &r.by_size {
        let wait = wait.map(|w| format!("{w:.0} blocks")).unwrap_or_else(|| "-".into());
        let _ = writeln!(
            o,
            "   {label:<12} {n:>6} {wait:>13} {:>21.0}% {senders:>12} {:>14.0}%",
            quick * 100.0,
            top3 * 100.0
        );
    }

    let _ = writeln!(o, "\n3. Who took them");
    let _ = writeln!(
        o,
        "   {} liquidators (the contracts that called liquidate), signed for by {} accounts.",
        r.liquidators, r.senders
    );
    for (k, n, v) in &r.top {
        let _ = writeln!(o, "   top {k}: {:>3.0} % of the events, {:>3.0} % of the incentive", n * 100.0, v * 100.0);
    }

    let _ = writeln!(o, "\n4. What the winners paid");
    let _ = writeln!(o, "   {:<22}{HEAD}", "");
    let _ = writeln!(o, "   {:<22}{}", "gas ($)", cols(&r.gas_usd, 3));
    let _ = writeln!(o, "   {:<22}{}   (incentives of a cent or more)", "gas ÷ incentive (%)", cols(&r.gas_pct, 1));
    let _ = writeln!(o, "   {:<22}{}", "priority fee (gwei)", cols(&r.priority_gwei, 4));
    let _ = writeln!(o, "   incentive below the winner's own gas: {} events ({:.0} %)", r.under_gas, pct(r.under_gas));

    let _ = writeln!(o, "\n5. What a slower liquidator would have found");
    let _ = writeln!(o, "   Events that waited at least this long and paid more than the winner's gas.");
    let _ = writeln!(
        o,
        "   {:<22} {:>6} {:>8} {:>18} {:>12}",
        "waited at least", "events", "a day", "median after gas", "sum"
    );
    for (min, n, per_day, med, sum) in &r.slower {
        let label = format!("{min} blocks ({:.0} s)", *min as f64 * r.block_s);
        let med = med.map(|m| format!("${m:.2}")).unwrap_or_else(|| "-".into());
        let _ = writeln!(o, "   {label:<22} {n:>6} {per_day:>8.1} {med:>18} {:>12}", money(*sum));
    }
    let _ = writeln!(o, "   An upper bound: someone took every one of these, the cost of selling the collateral");
    let _ = writeln!(o, "   is not counted, and a second liquidator would have had to outbid the first.");
    if !r.failed.is_empty() {
        let _ = writeln!(o, "\n   Could not be read (tried again on the next run):");
        for (e, n) in &r.failed {
            let _ = writeln!(o, "      {n:>4} × {e}");
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position liquidatable at the end of blocks `from..`, asked about like a node would be.
    async fn search(from: u64, block: u64, floor: u64) -> (u64, bool, u32) {
        since(async |n: u64| Ok::<_, ()>(n >= from), block, floor).await.unwrap()
    }

    #[tokio::test]
    async fn the_search_finds_the_first_liquidatable_block() {
        // became liquidatable inside the winner's block: one question
        assert_eq!(search(100, 100, 0).await, (100, false, 1));
        // at the end of the block before: the winner was in the next block
        assert_eq!(search(99, 100, 0).await, (99, false, 2));
        for from in [98, 97, 90, 64, 37, 2] {
            let (block, at_floor, asked) = search(from, 100, 0).await;
            assert_eq!((block, at_floor), (from, false), "from {from}");
            assert!(asked <= 14, "{asked} questions for a wait of {} blocks", 100 - from);
        }
    }

    #[tokio::test]
    async fn the_search_stops_at_the_floor() {
        // liquidatable for longer than is searched
        assert_eq!(search(0, 10_000, 10_000 - HORIZON).await.0, 10_000 - HORIZON);
        assert!(search(0, 10_000, 10_000 - HORIZON).await.1);
        // still liquidatable right after its previous liquidation in block 95
        assert_eq!(search(50, 100, 95).await, (95, true, 4));
        // healthy after block 95's liquidation, liquidatable again from 98
        assert_eq!(search(98, 100, 95).await.0, 98);
        // the previous liquidation was in the same block: nothing to ask
        assert_eq!(search(0, 100, 100).await, (100, true, 0));
    }

    #[tokio::test]
    async fn a_failed_question_is_an_error_not_an_answer() {
        let r = since(async |n: u64| if n < 98 { Err("node") } else { Ok(true) }, 100, 0).await;
        assert_eq!(r, Err("node"));
    }

    fn ev(block: u64, log: u32, sender: &str, incentive: f64, since: Option<u64>, gas_eth: f64) -> LiqEvent {
        LiqEvent {
            venue: "base".into(),
            block,
            log_index: log,
            tx: format!("0x{block}"),
            caller: format!("contract-{sender}"),
            sender: Some(sender.into()),
            collateral: Some("cbXRP".into()),
            loan: Some("USDC".into()),
            incentive: Some(incentive),
            loan_usd: Some(1.0),
            gas_eth: Some(gas_eth),
            eth_usd: Some(2_000.0),
            priority_gwei: Some(0.5),
            block_time: Some(1_790_000_000 + block as i64 * 2),
            since_block: since,
            since_kind: since.map(|_| "earlier".into()),
            ..Default::default()
        }
    }

    #[test]
    fn the_report_counts_every_event_once() {
        let mut events = vec![
            ev(100, 0, "a", 50.0, Some(100), 0.001),  // inside the block, $2 of gas
            ev(100, 1, "a", 0.5, Some(99), 0.001),    // next block; incentive below gas
            ev(200, 0, "b", 5.0, Some(197), 0.0005),  // waited 3 blocks, $4 after gas
            ev(300, 0, "c", 200.0, Some(100), 0.001), // waited 200 blocks
            ev(400, 0, "a", 20.0, None, 0.001),       // not measured
        ];
        events[4].err = Some("evm transport: timeout".into());
        // a loan token without a price is counted, not valued
        let mut odd = ev(500, 0, "d", 1.0, Some(499), 0.001);
        (odd.loan, odd.loan_usd) = (Some("cbBTC".into()), None);
        events.push(odd);

        let r = build("base", 30, 2.0, &events);
        assert_eq!((r.events, r.transactions, r.liquidators, r.senders), (6, 5, 4, 4));
        assert_eq!(r.incentive_usd.n, 5);
        assert!((r.incentive_usd_sum - 275.5).abs() < 1e-9);
        assert_eq!(r.not_valued.get("cbBTC"), Some(&1));
        assert_eq!(r.waited.iter().map(|w| w.1).sum::<usize>(), 6, "every event is in exactly one row");
        let row = |label: &str| r.waited.iter().find(|w| w.0.starts_with(label)).unwrap().1;
        assert_eq!((row("inside"), row("1 block"), row("2–3"), row("over 100"), row("not measured")), (1, 2, 1, 1, 1));
        // liquidator a: 3 of 6 events, $70.50 of $275.50; top by incentive is c
        assert!((r.top[0].1 - 1.0 / 6.0).abs() < 1e-9 && (r.top[0].2 - 200.0 / 275.5).abs() < 1e-9);
        assert_eq!(r.under_gas, 1);
        // waited ≥ 2 blocks and above gas: the $5 one ($4 after gas) and the $200 one
        assert_eq!((r.slower[0].0, r.slower[0].1), (2, 2));
        assert_eq!(r.slower[0].3, Some(4.0));
        // ≥ 150 blocks: only the $200 one
        assert_eq!(r.slower.last().unwrap().1, 1);
        assert_eq!(r.failed.len(), 1);
        let text = render(&r);
        assert!(text.contains("6 events in 5 transactions") && text.contains("not valued"), "{text}");
        assert_eq!(money(149_647.4), "$149,647");
        assert_eq!(money(12.0), "$12");
    }
}
