//! One question for every venue: "buying (or selling) this much base right
//! now, at what average price, after fees?" Order-book venues answer by
//! walking their live book; Uniswap answers with QuoterV2's exact simulation.
//! Every answer carries its source and how old its data is.

use crate::evm::{EvmRpc, UniV3Pool, quote, quote_exact_output};
use crate::{BinanceClient, Instrument, OkxClient, Side, fill_against_book};
use searcher_core::config::{VenueConfig, VenueKind};
use std::time::Instant;

/// Where a price came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Walked the venue's order book (paper fill; not a guarantee).
    Book,
    /// Uniswap QuoterV2 simulation at a block (exact for that block).
    Quoter,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Price {
    pub venue: String,
    /// Canonical `BASE/QUOTE`.
    pub market: String,
    pub side: Side,
    /// Base amount asked for and amount the liquidity covers.
    pub size: f64,
    pub filled: f64,
    /// Quote per base before separate fees (pool fees are already inside).
    pub avg_px: f64,
    /// Quote per base after fees (and, on chains, the estimated gas of one
    /// swap): what a buy pays / a sell receives.
    pub net_px: f64,
    /// Taker fee (order books) or estimated gas (chains), in quote currency.
    pub fee_quote: f64,
    pub source: Source,
    /// How current the data is: the book's exchange-time age, or the block.
    pub as_of: String,
    /// Time from request to answer.
    pub latency_ms: u64,
}

/// A market on one venue, ready to price.
pub enum Market {
    Okx { venue: String, client: OkxClient, inst: Instrument, fee_bps: u32 },
    Binance { venue: String, client: BinanceClient, inst: Instrument, fee_bps: u32 },
    Uniswap { venue: String, rpc: EvmRpc, quoter: String, pool: UniV3Pool },
}

fn canonical(base: &str, quote: &str) -> String {
    format!("{}/{}", base.to_ascii_uppercase(), quote.to_ascii_uppercase())
}

impl Market {
    /// Every market a venue lists (instruments / pools loaded from the venue).
    pub async fn load(name: &str, v: &VenueConfig) -> Result<Vec<Market>, String> {
        let mut out = Vec::new();
        match v.kind {
            VenueKind::Okx => {
                for id in &v.markets {
                    let client = OkxClient::new(v, None).map_err(|e| e.to_string())?;
                    let inst = client.instrument(id).await.map_err(|e| format!("{name} {id}: {e}"))?;
                    out.push(Market::Okx { venue: name.into(), client, inst, fee_bps: v.taker_fee_bps });
                }
            }
            VenueKind::Binance => {
                for id in &v.markets {
                    let client = BinanceClient::new(v, None).map_err(|e| e.to_string())?;
                    let inst = client.instrument(id).await.map_err(|e| format!("{name} {id}: {e}"))?;
                    out.push(Market::Binance { venue: name.into(), client, inst, fee_bps: v.taker_fee_bps });
                }
            }
            VenueKind::Evm => {
                for addr in &v.pools {
                    let rpc = EvmRpc::new(&v.resolved_url(), 3.0).map_err(|e| e.to_string())?;
                    let pool = UniV3Pool::load(&rpc, addr).await.map_err(|e| format!("{name} {addr}: {e}"))?;
                    out.push(Market::Uniswap { venue: name.into(), rpc, quoter: v.quoter.clone(), pool });
                }
            }
        }
        Ok(out)
    }

    pub fn venue(&self) -> &str {
        match self {
            Market::Okx { venue, .. } | Market::Binance { venue, .. } | Market::Uniswap { venue, .. } => venue,
        }
    }

    pub fn market(&self) -> String {
        match self {
            Market::Okx { inst, .. } | Market::Binance { inst, .. } => canonical(&inst.base, &inst.quote),
            Market::Uniswap { pool, .. } => pool.market(),
        }
    }

    /// Price `size` base units on `side`.
    pub async fn price(&self, side: Side, size: f64) -> Result<Price, String> {
        let t = Instant::now();
        let from_book = |book: crate::Book, fee_bps: u32, source_ts: i64| {
            let f = fill_against_book(&book, side, size, None, fee_bps);
            let net = match side {
                Side::Buy => (f.notional + f.fee) / f.filled.max(f64::MIN_POSITIVE),
                Side::Sell => (f.notional - f.fee) / f.filled.max(f64::MIN_POSITIVE),
            };
            let as_of = if source_ts > 0 {
                format!("book {} ms old", (chrono::Utc::now().timestamp_millis() - source_ts).max(0))
            } else {
                "book (time received)".to_string()
            };
            (f.filled, f.avg_px, net, f.fee, as_of)
        };
        let (filled, avg_px, net_px, fee_quote, as_of, source) = match self {
            Market::Okx { client, inst, fee_bps, .. } => {
                let b = client.book(&inst.inst_id, 400).await.map_err(|e| e.to_string())?;
                let ts = b.ts_ms;
                let (a, b2, c, d, e) = from_book(b, *fee_bps, ts);
                (a, b2, c, d, e, Source::Book)
            }
            Market::Binance { client, inst, fee_bps, .. } => {
                let b = client.book(&inst.inst_id, 100).await.map_err(|e| e.to_string())?;
                // Binance's depth has no exchange timestamp: say so rather than invent one
                let ts = 0;
                let (a, b2, c, d, e) = from_book(b, *fee_bps, ts);
                (a, b2, c, d, e, Source::Book)
            }
            Market::Uniswap { rpc, quoter, pool, .. } => {
                let base_sym = pool.market().split('/').next().unwrap_or_default().to_string();
                let base = pool.token(&base_sym).ok_or("pool has no base token")?.clone();
                let quote_tok = if base.address == pool.token0.address { &pool.token1 } else { &pool.token0 };
                let base_raw = (size * 10f64.powi(base.decimals as i32)) as u128;
                let q_scale = 10f64.powi(quote_tok.decimals as i32);
                let q = match side {
                    // sell exactly `size` base: `amount_out` = quote received
                    Side::Sell => quote(rpc, quoter, pool, &base, base_raw).await,
                    // buy exactly `size` base: `amount_out` = quote paid
                    Side::Buy => quote_exact_output(rpc, quoter, pool, &base, base_raw).await,
                }
                .map_err(|e| e.to_string())?;
                let quote_amount = q.amount_out as f64 / q_scale;
                let px = quote_amount / size;
                // gas of one swap transaction (router overhead + 21k base), valued at
                // this price when the base is the chain's gas token (WETH)
                let gas_price = rpc.request("eth_gasPrice", serde_json::json!([])).await.map_err(|e| e.to_string())?;
                let gas_price = u128::from_str_radix(gas_price.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)
                    .unwrap_or(0) as f64;
                let gas_eth = (q.gas_estimate + 21_000 + 30_000) as f64 * gas_price / 1e18;
                let gas_quote = if base.symbol.eq_ignore_ascii_case("WETH") { gas_eth * px } else { 0.0 };
                let net = match side {
                    Side::Buy => (quote_amount + gas_quote) / size,
                    Side::Sell => (quote_amount - gas_quote) / size,
                };
                (size, px, net, gas_quote, format!("block {}", q.block), Source::Quoter)
            }
        };
        Ok(Price {
            venue: self.venue().into(),
            market: self.market(),
            side,
            size,
            filled,
            avg_px,
            net_px,
            fee_quote,
            source,
            as_of,
            latency_ms: t.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_match_across_venues() {
        assert_eq!(canonical("sol", "usdt"), "SOL/USDT");
        // OKX `SOL-USDT` and Binance `SOLUSDT` both load as base SOL, quote USDT
        let okx = Instrument {
            inst_id: "SOL-USDT".into(),
            base: "SOL".into(),
            quote: "USDT".into(),
            tick_sz: "0.01".into(),
            lot_sz: "0.000001".into(),
            min_sz: "0.01".into(),
            min_notional: 0.0,
            live: true,
        };
        let bn = Instrument { inst_id: "SOLUSDT".into(), ..okx.clone() };
        assert_eq!(canonical(&okx.base, &okx.quote), canonical(&bn.base, &bn.quote));
    }
}
