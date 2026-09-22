//! Cross-chain spread: buy an asset with N USDC on Solana (Jupiter), then
//! price selling and buying that same quantity on Solana and on each EVM
//! chain (Uniswap v3 QuoterV2, exact). Two directions per chain, after swap
//! fees, Solana transaction fees and EVM gas:
//!
//! * A: buy on Solana, sell on the EVM chain
//! * B: buy on the EVM chain, sell on Solana
//!
//! Not included, and the report says so: moving inventory between chains
//! (bridging) and the price risk while the two legs are apart.

use super::{Ctx, Priority};
use searcher_core::config::XchainAsset;
use searcher_core::{Address, Ts};
use searcher_storage::research::XchainRow;
use searcher_venues::Side;
use searcher_venues::evm::{EvmRpc, UniV3Pool};
use searcher_venues::market::Market;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Base fee of one signature plus the provider's CU price × this many CU.
const SOLANA_SWAP_CU: u64 = 200_000;

pub async fn run(ctx: Arc<Ctx>, mut shutdown: watch::Receiver<bool>) {
    let r = &ctx.cfg.research;
    let mut tick = tokio::time::interval(Duration::from_secs(r.xchain_every_s));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // (asset, venue) → market; loaded on first use, retried on failure
    let mut markets: BTreeMap<(String, String), Market> = BTreeMap::new();
    let mut round: i64 = 0;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {}
        }
        round += 1;
        for asset in &r.xchain_assets {
            for &notional in &r.xchain_notional_usd {
                if *shutdown.borrow() {
                    return;
                }
                for row in measure(&ctx, &mut markets, asset, notional, round).await {
                    if row.err.as_deref() == Some("stopped") {
                        return;
                    }
                    ctx.record("xchain", |s| s.insert_xchain(&ctx.run, &row));
                }
            }
        }
    }
}

async fn ensure_market(
    ctx: &Ctx,
    markets: &mut BTreeMap<(String, String), Market>,
    asset: &XchainAsset,
    venue: &str,
    pool: &str,
) -> Result<(), String> {
    if let std::collections::btree_map::Entry::Vacant(slot) = markets.entry((asset.symbol.clone(), venue.to_string())) {
        let v = ctx.cfg.venues.get(venue).ok_or_else(|| format!("no venue `{venue}` in [venues]"))?;
        let rpc = EvmRpc::new(&v.resolved_url(), 3.0).map_err(|e| e.to_string())?;
        let p = UniV3Pool::load(&rpc, pool).await.map_err(|e| format!("{venue} pool {pool}: {e}"))?;
        let base = p.market().split('/').next().unwrap_or_default().to_string();
        let expected = if asset.symbol == "ETH" { "WETH" } else { asset.symbol.as_str() };
        if !base.eq_ignore_ascii_case(expected) {
            return Err(format!("{venue} pool {pool} is {}, not {expected}", p.market()));
        }
        slot.insert(Market::Uniswap { venue: venue.to_string(), rpc, quoter: v.quoter.clone(), pool: p });
    }
    Ok(())
}

async fn measure(
    ctx: &Ctx,
    markets: &mut BTreeMap<(String, String), Market>,
    asset: &XchainAsset,
    notional: u32,
    round: i64,
) -> Vec<XchainRow> {
    let t0 = Ts::now();
    let base_row = |venue: &str| XchainRow {
        round,
        ts: t0.0,
        asset: asset.symbol.clone(),
        notional_usd: notional,
        venue: venue.to_string(),
        ..Default::default()
    };
    let venues: Vec<(&String, &String)> = asset.evm_pools.iter().collect();
    let fail = |e: String| venues.iter().map(|(v, _)| XchainRow { err: Some(e.clone()), ..base_row(v) }).collect();

    let Ok(mint) = asset.solana_mint.parse::<Address>() else { return fail("bad solana_mint".into()) };
    let usdc = ctx.cfg.tokens().get("USDC").map(|t| t.mint).unwrap_or_default();
    let scale = 10f64.powi(asset.solana_decimals as i32);

    // 1. Solana buy: N USDC → asset
    let buy = match ctx.gate.build(ctx.request(usdc, mint, notional as u64 * 1_000_000, None), Priority::Normal).await {
        Ok(b) => b,
        Err(e) if e == "stopped" => return fail(e),
        Err(e) => return fail(format!("solana buy: {e}")),
    };
    let qty_atoms = buy.leg.out_amount;
    if qty_atoms == 0 {
        return fail("solana buy returned 0".into());
    }
    let qty = qty_atoms as f64 / scale;
    let sol_buy_px = notional as f64 / qty;

    // 2. at the same time: Solana sell of that quantity, and every EVM chain both ways
    let sell = ctx.gate.build(ctx.request(mint, usdc, qty_atoms, None), Priority::Normal);
    let mut load_err = BTreeMap::new();
    for (venue, pool) in &venues {
        if let Err(e) = ensure_market(ctx, markets, asset, venue, pool).await {
            load_err.insert(venue.to_string(), e);
        }
    }
    let evm: Vec<Result<&Market, String>> = venues
        .iter()
        .map(|(v, _)| {
            markets
                .get(&(asset.symbol.clone(), v.to_string()))
                .ok_or_else(|| load_err.get(v.as_str()).cloned().unwrap_or_else(|| "not loaded".into()))
        })
        .collect();
    let evm_quotes = one_by_one(evm.iter().map(|m| async move {
        match m {
            Ok(m) => {
                let (b, s) = tokio::join!(m.price(Side::Buy, qty), m.price(Side::Sell, qty));
                (b, s, Ts::now())
            }
            Err(e) => (Err(e.clone()), Err(e.clone()), Ts::now()),
        }
    }));
    let (sell, evm_quotes) = tokio::join!(sell, evm_quotes);
    let sell = match sell {
        Ok(s) => s,
        Err(e) if e == "stopped" => return fail(e),
        Err(e) => return fail(format!("solana sell: {e}")),
    };
    let sol_sell_usd = sell.leg.out_amount as f64 / 1e6;
    let sol_sell_px = sol_sell_usd / qty;

    // Solana fees per swap transaction, valued at the CEX SOL price
    let sol_usd = *ctx.sol_usd.lock();
    let fee = |cu_price: Option<u64>| {
        let lamports = 5_000 + cu_price.unwrap_or(0) as u128 * SOLANA_SWAP_CU as u128 / 1_000_000;
        sol_usd.map(|p| lamports as f64 / 1e9 * p)
    };
    let (fee_buy, fee_sell) = (fee(buy.leg.cu_price_micro), fee(sell.leg.cu_price_micro));

    let mut rows = Vec::new();
    for ((venue, _), (eb, es, at)) in venues.iter().zip(evm_quotes) {
        let mut row = base_row(venue);
        row.qty = Some(qty);
        row.sol_buy_px = Some(sol_buy_px);
        row.sol_sell_px = Some(sol_sell_px);
        row.sol_fee_usd = fee_buy.zip(fee_sell).map(|(a, b)| (a + b) / 2.0);
        row.skew_ms = Some((at.0 - t0.0) / 1_000);
        let (eb, es) = match (eb, es) {
            (Ok(b), Ok(s)) => (b, s),
            (Err(e), _) | (_, Err(e)) => {
                row.err = Some(format!("evm: {e}"));
                rows.push(row);
                continue;
            }
        };
        row.evm_buy_px = Some(eb.avg_px);
        row.evm_sell_px = Some(es.avg_px);
        row.evm_block = Some(es.as_of.clone());
        // gas in ETH → USD: the ETH asset prices itself; others use the last ETH price
        if asset.symbol == "ETH" {
            ctx.eth_usd.lock().replace(es.avg_px);
        }
        let eth_usd = *ctx.eth_usd.lock();
        row.evm_gas_usd = eth_usd.map(|p| es.gas_native.max(eb.gas_native) * p);
        if let (Some(gas), Some(fb), Some(fs)) = (row.evm_gas_usd, fee_buy, fee_sell) {
            // A: pay N (+ Solana fee) for qty, sell qty on the chain (− gas)
            let cost_a = notional as f64 + fb;
            row.a_bps = Some((es.avg_px * qty - gas - cost_a) / cost_a * 1e4);
            // B: buy qty on the chain (+ gas), sell it on Solana (− Solana fee)
            let cost_b = eb.avg_px * qty + gas;
            row.b_bps = Some((sol_sell_usd - fs - cost_b) / cost_b * 1e4);
        } else {
            row.err = Some("no SOL or ETH price yet to value fees".into());
        }
        rows.push(row);
    }
    rows
}

/// Await the futures one after another, in order (a chain's buy and sell
/// already run together; chains are few).
async fn one_by_one<F: std::future::Future>(fs: impl Iterator<Item = F>) -> Vec<F::Output> {
    let handles: Vec<_> = fs.collect();
    let mut out = Vec::with_capacity(handles.len());
    for f in handles {
        out.push(f.await);
    }
    out
}
