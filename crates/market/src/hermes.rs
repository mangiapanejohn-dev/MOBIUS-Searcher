//! Pyth Hermes price stream (Server-Sent Events). Since 2026-08-26 Hermes
//! requires an API key (keyless requests get 401), so the default oracle
//! source is the on-chain Pyth price account. This adapter produces the same
//! [`OracleUpdate`]s, so switching delivery changes nothing downstream.

use crate::feed::Emit;
use crate::hot::{HotSink, HotTick, OracleSource, OracleUpdate};
use searcher_core::event::LogLevel;
use searcher_core::model::{MarketSample, SampleSide};
use searcher_core::{Event, Ts, UsdPrice};
use searcher_telemetry::backoff_delay;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub struct HermesConfig {
    pub base_url: String,
    /// Bearer key; never printed.
    pub api_key: String,
    /// (symbol, feed id)
    pub feeds: Vec<(Arc<str>, [u8; 32])>,
    /// Minimum interval between recorded samples per feed.
    pub emit_interval: Duration,
}

impl fmt::Debug for HermesConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HermesConfig({}, key=***, {} feeds)", self.base_url, self.feeds.len())
    }
}

impl HermesConfig {
    pub fn stream_url(&self) -> String {
        let ids: Vec<String> = self.feeds.iter().map(|(_, id)| format!("ids[]=0x{}", hex(id))).collect();
        format!("{}/v2/updates/price/stream?{}&parsed=true", self.base_url.trim_end_matches('/'), ids.join("&"))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Decode one SSE `data:` payload (`{"binary":…,"parsed":[{id, price:{price,
/// conf, expo, publish_time}, metadata:{slot}}]}`) into updates for known feeds.
pub fn parse_update(data: &str, feeds: &[(Arc<str>, [u8; 32])], received: Instant, now: Ts) -> Vec<OracleUpdate> {
    let Ok(v) = serde_json::from_str::<Value>(data) else { return Vec::new() };
    let Some(parsed) = v.get("parsed").and_then(Value::as_array) else { return Vec::new() };
    parsed
        .iter()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.trim_start_matches("0x").to_ascii_lowercase();
            let (symbol, _) = feeds.iter().find(|(_, f)| hex(f) == id)?;
            let px = p.get("price")?;
            let num = |k: &str| px.get(k).and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or(x.as_f64()));
            let expo = px.get("expo")?.as_i64()? as i32;
            let scale = 10f64.powi(expo);
            let price = num("price")? * scale;
            (price.is_finite() && price > 0.0).then(|| OracleUpdate {
                source: OracleSource::Hermes,
                symbol: symbol.clone(),
                price,
                conf: num("conf").unwrap_or(0.0) * scale,
                publish_time: px.get("publish_time").and_then(Value::as_i64).unwrap_or(0),
                slot: p.get("metadata").and_then(|m| m.get("slot")).and_then(Value::as_u64),
                received,
                received_ts: now,
            })
        })
        .collect()
}

/// Stream Hermes updates into the HOT sink (and throttled samples into the
/// recorder). Reconnects with backoff; a 401 is reported once and retried
/// slowly (the key may be added to the environment later).
pub async fn run_hermes(cfg: HermesConfig, hot: HotSink, emit: Emit, mut shutdown: watch::Receiver<bool>) {
    let mut b = reqwest::Client::builder().pool_idle_timeout(Duration::from_secs(600));
    if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy()
        && let Ok(proxy) = reqwest::Proxy::all(p)
    {
        b = b.proxy(proxy);
    }
    if searcher_telemetry::proxy::direct() {
        b = b.no_proxy();
    }
    let http = match b.build() {
        Ok(h) => h,
        Err(e) => {
            emit(Event::Error { ts: Ts::now(), service: "hermes".into(), message: format!("client: {e}") });
            return;
        }
    };
    let url = cfg.stream_url();
    let mut attempt = 0u32;
    let mut last_emit: HashMap<Arc<str>, Ts> = HashMap::new();
    let mut reported_401 = false;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let req = http.get(&url).bearer_auth(&cfg.api_key).header("accept", "text/event-stream");
        match req.send().await {
            Ok(mut resp) if resp.status().is_success() => {
                attempt = 0;
                emit(Event::Log {
                    ts: Ts::now(),
                    level: LogLevel::Info,
                    message: "Pyth Hermes stream connected".into(),
                });
                let mut buf = String::new();
                loop {
                    let chunk = tokio::select! {
                        _ = shutdown.changed() => return,
                        c = tokio::time::timeout(Duration::from_secs(30), resp.chunk()) => c,
                    };
                    let Ok(Ok(Some(bytes))) = chunk else { break };
                    let received = Instant::now();
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(end) = buf.find("\n\n") {
                        let event: String = buf.drain(..end + 2).collect();
                        for line in event.lines() {
                            let Some(data) = line.strip_prefix("data:") else { continue };
                            for u in parse_update(data.trim(), &cfg.feeds, received, Ts::now()) {
                                let due = last_emit
                                    .get(&u.symbol)
                                    .is_none_or(|t| Ts::now().0 - t.0 >= cfg.emit_interval.as_micros() as i64);
                                if due {
                                    last_emit.insert(u.symbol.clone(), Ts::now());
                                    emit(Event::Sample(MarketSample {
                                        ts: u.received_ts,
                                        pair: u.symbol.to_string(),
                                        price: UsdPrice::new((u.price * 1e6).round() as u64),
                                        side: SampleSide::Oracle,
                                        source: OracleSource::Hermes.label().into(),
                                        size_atoms: 0,
                                    }));
                                }
                                hot(HotTick::Oracle(u));
                            }
                        }
                    }
                }
            }
            Ok(resp) if resp.status().as_u16() == 401 => {
                if !reported_401 {
                    reported_401 = true;
                    emit(Event::Error {
                        ts: Ts::now(),
                        service: "hermes".into(),
                        message: "Pyth Hermes: 401 unauthorized (check PYTH_API_KEY); retrying every 5 min".into(),
                    });
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                }
                continue;
            }
            Ok(resp) => emit(Event::Error {
                ts: Ts::now(),
                service: "hermes".into(),
                message: format!("http {}", resp.status()),
            }),
            Err(e) => emit(Event::Error { ts: Ts::now(), service: "hermes".into(), message: format!("connect: {e}") }),
        }
        let d = backoff_delay(attempt, Duration::from_secs(1), Duration::from_secs(60), None, 0.5);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(d) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape from the Pyth docs ("Hermes REST API Response Example").
    const SAMPLE: &str = r#"{"binary":{"encoding":"hex","data":["504e41"]},"parsed":[
        {"id":"ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
         "price":{"price":"11229063700","conf":"1463400","expo":-8,"publish_time":1789739652},
         "ema_price":{"price":"11230000000","conf":"1500000","expo":-8,"publish_time":1789739652},
         "metadata":{"slot":448102885,"proof_available_time":1789739653,"prev_publish_time":1789739651}},
        {"id":"0000000000000000000000000000000000000000000000000000000000000001",
         "price":{"price":"1","conf":"0","expo":0,"publish_time":1}}]}"#;

    fn feeds() -> Vec<(Arc<str>, [u8; 32])> {
        let id =
            searcher_core::config::parse_feed_id("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d")
                .unwrap();
        vec![(Arc::from("SOL/USD"), id)]
    }

    #[test]
    fn parses_known_feeds_into_oracle_updates() {
        let u = parse_update(SAMPLE, &feeds(), Instant::now(), Ts(1_789_739_653_500_000));
        assert_eq!(u.len(), 1, "unknown feed ids are ignored");
        let u = &u[0];
        assert_eq!((u.source, &*u.symbol), (OracleSource::Hermes, "SOL/USD"));
        assert!((u.price - 112.290637).abs() < 1e-9);
        assert!((u.conf - 0.014634).abs() < 1e-9);
        assert_eq!((u.publish_time, u.slot), (1_789_739_652, Some(448_102_885)));
        assert_eq!(u.age_at_receipt_ms(), 1_500);
        assert!(parse_update("not json", &feeds(), Instant::now(), Ts(0)).is_empty());
    }

    #[test]
    fn stream_url_and_debug_never_show_the_key() {
        let c = HermesConfig {
            base_url: "https://hermes.pyth.network/".into(),
            api_key: "secret".into(),
            feeds: feeds(),
            emit_interval: Duration::from_millis(500),
        };
        assert_eq!(
            c.stream_url(),
            "https://hermes.pyth.network/v2/updates/price/stream?ids[]=0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d&parsed=true"
        );
        assert!(!format!("{c:?}").contains("secret"));
    }
}
