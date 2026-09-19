//! Connect the chain WebSocket (through HTTPS_PROXY if set) with the default
//! pool/oracle watches and print what arrives for ~12 s.
use searcher_core::config::FeedsConfig;
use searcher_core::token::TokenRegistry;
use searcher_market::feed::{AccountFeed, watches_from_config};
use searcher_market::{ChainState, RpcClient, feed};
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or("wss://api.mainnet-beta.solana.com".into());
    println!("proxy: {}", if feed::proxy_for(&url).is_some() { "HTTP CONNECT tunnel" } else { "direct" });
    let chain = Arc::new(ChainState::default());
    let tel = Arc::new(Telemetry::new());
    let rpc = Arc::new(
        RpcClient::new(
            "https://api.mainnet-beta.solana.com",
            LimiterConfig::new(2.0, 2),
            0.5,
            Duration::from_secs(5),
            tel.clone(),
        )
        .unwrap(),
    );
    let watches = watches_from_config(&FeedsConfig::default(), &TokenRegistry::defaults()).unwrap();
    let feed = AccountFeed::new(watches, Duration::from_millis(500));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let emit: feed::Emit = Arc::new(|e| match e {
        searcher_core::Event::Slot { .. } => {}
        e => println!("{e:?}"),
    });
    let h = tokio::spawn(feed::run_chain_ws(url, feed, rpc, chain.clone(), tel.clone(), emit, None, rx));
    tokio::time::sleep(Duration::from_secs(12)).await;
    let _ = tx.send(true);
    let _ = h.await;
    let s = tel.snapshot(searcher_core::ServiceId::WebSocket, searcher_core::Ts::now());
    println!(
        "latest slot {:?} · ws state {:?} · msgs {} · last error {:?}",
        chain.slot(),
        s.state,
        s.requests,
        s.last_error
    );
}
