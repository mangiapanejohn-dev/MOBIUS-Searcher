//! Against Binance's public endpoints (no key, no orders). Ignored by default:
//! `cargo test -p searcher-venues --test binance_live -- --ignored --nocapture`

use searcher_core::config::VenueConfig;
use searcher_venues::{BinanceClient, BinanceError, Side, fill_against_book};

#[tokio::test]
#[ignore]
async fn market_data_from_the_public_mirror_and_a_paper_fill() {
    let v = VenueConfig::binance();
    let c = BinanceClient::new(&v, None).unwrap();
    let i = c.instrument("SOLUSDT").await.unwrap();
    assert!(i.live && i.base == "SOL" && i.quote == "USDT" && i.min_notional > 0.0);
    let b = c.book("SOLUSDT", 20).await.unwrap();
    let (bid, ask) = (b.bids[0].px, b.asks[0].px);
    assert!(ask > bid && bid > 0.0, "crossed or empty book: {bid}/{ask}");
    let f = fill_against_book(&b, Side::Sell, 0.5, None, v.taker_fee_bps);
    assert_eq!(f.filled, 0.5);
    assert!(f.avg_px <= bid);
    println!("SOLUSDT {bid}/{ask} · min notional {} USDT · paper sell 0.5 @ {:.4}", i.min_notional, f.avg_px);
    assert!(matches!(c.instrument("NOPEUSDT").await, Err(BinanceError::Api { code: -1121, .. })));

    // the main API: served (another location) or refused with 451 (this one) — never a silent failure
    let mut main = v.clone();
    main.rest_url = "https://api.binance.com".into();
    match BinanceClient::new(&main, None).unwrap().instrument("SOLUSDT").await {
        Ok(_) => println!("api.binance.com: served from this location"),
        Err(BinanceError::Restricted) => println!("api.binance.com: HTTP 451, not available from this location"),
        Err(e) => panic!("unexpected: {e}"),
    }
}
