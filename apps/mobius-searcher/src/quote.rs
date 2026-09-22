//! `--quote MARKET --size N`: what buying and selling N base units costs
//! right now on every enabled venue that lists MARKET, after fees (and gas
//! on chains), with each answer's source and age. Read-only; no keys.

use searcher_core::config::Config;
use searcher_venues::Side;
use searcher_venues::market::{Market, Price, Source};

fn row(p: &Price) -> String {
    let side = match p.side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    };
    let src = match p.source {
        Source::Book => "order book",
        Source::Quoter => "QuoterV2",
    };
    let partial = if p.filled + 1e-12 < p.size { format!("  only {:.4} fillable", p.filled) } else { String::new() };
    format!(
        "  {:<10} {:<5} {:>12.4} {:>12.4} {:>10.4}   {:<10} {:<22} {:>5} ms{partial}",
        p.venue, side, p.avg_px, p.net_px, p.fee_quote, src, p.as_of, p.latency_ms
    )
}

pub async fn run(cfg: &Config, market: &str, size: f64) -> anyhow::Result<()> {
    let want = market.to_ascii_uppercase().replace('-', "/");
    let mut markets = Vec::new();
    for (name, v) in cfg.venues.iter().filter(|(_, v)| v.enabled) {
        match Market::load(name, v).await {
            Ok(ms) => markets.extend(ms.into_iter().filter(|m| m.market() == want)),
            Err(e) => eprintln!("  {name}: {e}"),
        }
    }
    if markets.is_empty() {
        let enabled: Vec<&str> = cfg.venues.iter().filter(|(_, v)| v.enabled).map(|(n, _)| n.as_str()).collect();
        anyhow::bail!(
            "no enabled venue lists {want} (enabled: {}); turn venues on in your config, e.g. `[venues.base] enabled = true`",
            enabled.join(", ")
        );
    }
    println!("{want} · size {size} · prices in quote currency per 1 base");
    println!("  net = after the taker fee (order books) or pool fee + estimated gas of one swap (chains)");
    println!(
        "  {:<10} {:<5} {:>12} {:>12} {:>10}   {:<10} {:<22} {:>8}",
        "venue", "side", "avg", "net", "fee/gas", "source", "as of", "latency"
    );
    let mut prices = Vec::new();
    for m in &markets {
        for side in [Side::Buy, Side::Sell] {
            match m.price(side, size).await {
                Ok(p) => {
                    println!("{}", row(&p));
                    prices.push(p);
                }
                Err(e) => {
                    println!("  {:<10} {:<5} error: {e}", m.venue(), if side == Side::Buy { "buy" } else { "sell" })
                }
            }
        }
    }
    let full = |p: &&Price| p.filled + 1e-12 >= p.size;
    let best_buy =
        prices.iter().filter(|p| p.side == Side::Buy).filter(full).min_by(|a, b| a.net_px.total_cmp(&b.net_px));
    let best_sell =
        prices.iter().filter(|p| p.side == Side::Sell).filter(full).max_by(|a, b| a.net_px.total_cmp(&b.net_px));
    if let (Some(b), Some(s)) = (best_buy, best_sell) {
        let edge_bp = (s.net_px / b.net_px - 1.0) * 10_000.0;
        println!(
            "\n  cheapest buy {} @ {:.4} · best sell {} @ {:.4} · buy→sell {:+.1} bp",
            b.venue, b.net_px, s.venue, s.net_px, edge_bp
        );
        if b.venue != s.venue {
            println!("  (across venues this ignores moving funds between them: withdrawal fees, bridging, time)");
        }
    }
    Ok(())
}
