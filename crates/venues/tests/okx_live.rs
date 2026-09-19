//! Against the real OKX public API (no key, no orders). Ignored by default:
//! `cargo test -p searcher-venues --test okx_live -- --ignored`

use searcher_core::config::VenueConfig;
use searcher_venues::{OkxClient, Side, fill_against_book};

#[tokio::test]
#[ignore]
async fn public_market_data_and_a_paper_fill() {
    let v = VenueConfig::okx();
    let c = OkxClient::new(&v, None).unwrap();
    let offset = c.sync_time().await.unwrap();
    assert!(offset.abs() < 30_000, "clock offset {offset} ms");

    let i = c.instrument("SOL-USDT").await.unwrap();
    assert!(i.live && i.base == "SOL" && i.quote == "USDT");
    let size = i.size(0.5).unwrap();
    assert_eq!(size, "0.5");

    let b = c.book("SOL-USDT", 20).await.unwrap();
    let (ask, bid) = (b.asks[0].px, b.bids[0].px);
    assert!(ask > bid && bid > 0.0, "crossed or empty book: {bid} / {ask}");
    let f = fill_against_book(&b, Side::Buy, 0.5, None, v.taker_fee_bps);
    assert_eq!(f.filled, 0.5);
    assert!(f.avg_px >= ask, "a buy pays at least the best ask");
    println!(
        "offset {offset} ms · SOL-USDT {bid}/{ask} · paper buy 0.5 @ {:.4} over {} level(s), fee {:.4} USDT",
        f.avg_px, f.levels, f.fee
    );
    assert!(c.instrument("NOPE-USDT").await.is_err());
}
