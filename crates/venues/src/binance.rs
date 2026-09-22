//! Binance spot REST. Market data works from the public mirror
//! (`data-api.binance.vision`, the default `rest_url`); signed requests go to
//! `rest_url` (set `https://api.binance.com` to trade where Binance serves you)
//! or, with `demo = true`, to the spot testnet. Signed requests carry
//! `signature = hex(HMAC-SHA256(query, secret))` with `timestamp` and
//! `recvWindow`, and the key in `X-MBX-APIKEY`. Binance answers HTTP 451 from
//! locations it does not serve; that is reported as such, never worked around.

use crate::book::{Book, Level};
use crate::{Balance, Instrument, OrderKind, OrderRequest, OrderState, OrderStatus, Side, TradePermit};
use hmac::{Hmac, Mac};
use searcher_core::config::VenueConfig;
use searcher_telemetry::{LimiterConfig, RateLimiter};
use serde_json::Value;
use sha2::Sha256;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

pub const TESTNET: &str = "https://testnet.binance.vision";

#[derive(Debug, thiserror::Error)]
pub enum BinanceError {
    #[error("binance transport: {0}")]
    Transport(String),
    #[error("binance is not available from this location (HTTP 451); use another venue")]
    Restricted,
    #[error("binance http {status}: {body}")]
    Http { status: u16, body: String },
    /// `{"code": -1121, "msg": "Invalid symbol."}`
    #[error("binance {code}: {msg}")]
    Api { code: i64, msg: String },
    #[error("binance decode: {0}")]
    Decode(String),
    #[error("binance: {0}")]
    Refused(String),
}

/// API key and secret. Never printed.
#[derive(Clone)]
pub struct BinanceCredentials {
    key: String,
    secret: String,
}

impl fmt::Debug for BinanceCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BinanceCredentials(***)")
    }
}

impl BinanceCredentials {
    pub fn new(key: String, secret: String) -> Self {
        Self { key, secret }
    }

    /// From the venue's `api_key_env` / `secret_env`: `Ok(None)` when neither
    /// is set, an error naming the missing one when only one is.
    pub fn from_env(v: &VenueConfig) -> Result<Option<BinanceCredentials>, String> {
        let get = |n: &str| (!n.is_empty()).then(|| std::env::var(n).ok()).flatten().filter(|x| !x.trim().is_empty());
        match (get(&v.api_key_env), get(&v.secret_env)) {
            (None, None) => Ok(None),
            (Some(k), Some(s)) => Ok(Some(BinanceCredentials::new(k, s))),
            (Some(_), None) => Err(format!("Binance credentials incomplete: {} not set", v.secret_env)),
            (None, Some(_)) => Err(format!("Binance credentials incomplete: {} not set", v.api_key_env)),
        }
    }
}

/// `hex(HMAC-SHA256(payload, secret))`.
pub fn sign(secret: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn num(v: &Value, k: &str) -> f64 {
    text(v, k).parse().unwrap_or(0.0)
}

/// The API error in a body, if it is one.
fn api_error(body: &str) -> Option<BinanceError> {
    let v: Value = serde_json::from_str(body).ok()?;
    let code = v.get("code")?.as_i64()?;
    (code != 0 || v.get("msg").is_some())
        .then(|| BinanceError::Api { code, msg: v.get("msg").and_then(Value::as_str).unwrap_or("").to_string() })
}

pub fn parse_exchange_info(body: &str) -> Result<Instrument, BinanceError> {
    if let Some(e) = api_error(body) {
        return Err(e);
    }
    let v: Value = serde_json::from_str(body).map_err(|e| BinanceError::Decode(e.to_string()))?;
    let s = v
        .get("symbols")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| BinanceError::Decode("no symbol".into()))?;
    let filter = |t: &str| {
        s.get("filters")
            .and_then(Value::as_array)
            .and_then(|fs| fs.iter().find(|f| text(f, "filterType") == t))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let (price, lot, notional) = (filter("PRICE_FILTER"), filter("LOT_SIZE"), filter("NOTIONAL"));
    Ok(Instrument {
        inst_id: text(s, "symbol").into(),
        base: text(s, "baseAsset").into(),
        quote: text(s, "quoteAsset").into(),
        tick_sz: text(&price, "tickSize").into(),
        lot_sz: text(&lot, "stepSize").into(),
        min_sz: text(&lot, "minQty").into(),
        min_notional: num(&notional, "minNotional"),
        live: text(s, "status") == "TRADING",
    })
}

/// `/api/v3/depth` has no timestamp: the book is stamped `received_ms`.
pub fn parse_depth(body: &str, received_ms: i64) -> Result<Book, BinanceError> {
    if let Some(e) = api_error(body) {
        return Err(e);
    }
    let v: Value = serde_json::from_str(body).map_err(|e| BinanceError::Decode(e.to_string()))?;
    let side = |k: &str| -> Vec<Level> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| {
                        Some(Level { px: l.get(0)?.as_str()?.parse().ok()?, sz: l.get(1)?.as_str()?.parse().ok()? })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Book { asks: side("asks"), bids: side("bids"), ts_ms: received_ms })
}

pub fn parse_order(body: &str) -> Result<OrderStatus, BinanceError> {
    if let Some(e) = api_error(body) {
        return Err(e);
    }
    let d: Value = serde_json::from_str(body).map_err(|e| BinanceError::Decode(e.to_string()))?;
    let state = match text(&d, "status") {
        "NEW" => OrderState::Live,
        "PARTIALLY_FILLED" => OrderState::PartiallyFilled,
        "FILLED" => OrderState::Filled,
        "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "REJECTED" => OrderState::Canceled,
        other => OrderState::Other(other.into()),
    };
    let filled = num(&d, "executedQty");
    let quote = num(&d, "cummulativeQuoteQty");
    let fills = d.get("fills").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(OrderStatus {
        order_id: d.get("orderId").map(|x| x.to_string()).unwrap_or_default(),
        client_id: text(&d, "clientOrderId").into(),
        state,
        filled,
        avg_px: if filled > 0.0 { quote / filled } else { 0.0 },
        // Binance reports commission paid as a positive amount; ours is negative when paid
        fee: -fills.iter().map(|f| num(f, "commission")).sum::<f64>(),
        fee_ccy: fills.first().map(|f| text(f, "commissionAsset").to_string()).unwrap_or_default(),
    })
}

pub fn parse_balances(body: &str) -> Result<Vec<Balance>, BinanceError> {
    if let Some(e) = api_error(body) {
        return Err(e);
    }
    let v: Value = serde_json::from_str(body).map_err(|e| BinanceError::Decode(e.to_string()))?;
    Ok(v.get("balances")
        .and_then(Value::as_array)
        .map(|bs| {
            bs.iter()
                .map(|b| Balance {
                    ccy: text(b, "asset").into(),
                    available: num(b, "free"),
                    total: num(b, "free") + num(b, "locked"),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Query parameters of `POST /api/v3/order` (before timestamp and signature).
pub fn order_params(req: &OrderRequest) -> Vec<(&'static str, String)> {
    let side = match req.side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    };
    let mut p = vec![("symbol", req.inst_id.clone()), ("side", side.into())];
    match req.kind {
        OrderKind::Market => p.push(("type", "MARKET".into())),
        OrderKind::Limit => p.extend([("type", "LIMIT".into()), ("timeInForce", "GTC".into())]),
        OrderKind::Ioc => p.extend([("type", "LIMIT".into()), ("timeInForce", "IOC".into())]),
        OrderKind::PostOnly => p.push(("type", "LIMIT_MAKER".into())),
    }
    p.push(("quantity", req.size.clone()));
    if req.kind != OrderKind::Market
        && let Some(px) = &req.price
    {
        p.push(("price", px.clone()));
    }
    p.push(("newClientOrderId", req.client_id.clone()));
    p.push(("newOrderRespType", "FULL".into()));
    p
}

fn query(params: &[(&str, String)]) -> String {
    params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub struct BinanceClient {
    http: reqwest::Client,
    /// Market data.
    data_url: String,
    /// Signed requests (`rest_url`, or the testnet for demo).
    private_url: String,
    creds: Option<BinanceCredentials>,
    demo: bool,
    offset_ms: AtomicI64,
    limiter: RateLimiter,
}

impl BinanceClient {
    pub fn new(v: &VenueConfig, creds: Option<BinanceCredentials>) -> Result<Self, BinanceError> {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(concat!("mobius/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy() {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| BinanceError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() {
            b = b.no_proxy();
        }
        let base = v.rest_url.trim_end_matches('/').to_string();
        Ok(Self {
            http: b.build().map_err(|e| BinanceError::Transport(e.to_string()))?,
            private_url: if v.demo { TESTNET.to_string() } else { base.clone() },
            data_url: base,
            creds,
            demo: v.demo,
            offset_ms: AtomicI64::new(0),
            // request weight limit is 6000/min per IP; stay far below
            limiter: RateLimiter::new("binance", LimiterConfig::new(5.0, 5)),
        })
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<String, BinanceError> {
        self.limiter.acquire().await;
        let resp = req.send().await.map_err(|e| BinanceError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| BinanceError::Transport(e.to_string()))?;
        match status {
            200 => Ok(body),
            451 => Err(BinanceError::Restricted),
            _ => Err(api_error(&body).unwrap_or(BinanceError::Http { status, body: clip(&body) })),
        }
    }

    /// Signed query string for `params` at `now_ms` (our clock + learned offset).
    pub fn signed_query(&self, params: &[(&str, String)], now_ms: i64) -> Result<String, BinanceError> {
        let c = self.creds.as_ref().ok_or_else(|| BinanceError::Refused("no API credentials configured".into()))?;
        let mut p = params.to_vec();
        p.push(("recvWindow", "5000".into()));
        p.push(("timestamp", (now_ms + self.offset_ms.load(Ordering::Relaxed)).to_string()));
        let q = query(&p);
        Ok(format!("{q}&signature={}", sign(&c.secret, &q)))
    }

    async fn private(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<String, BinanceError> {
        let q = self.signed_query(params, now_ms())?;
        let key = self.creds.as_ref().map(|c| c.key.clone()).unwrap_or_default();
        self.send(self.http.request(method, format!("{}{path}?{q}", self.private_url)).header("X-MBX-APIKEY", key))
            .await
    }

    /// Learn Binance's clock offset (signed requests outside recvWindow are refused).
    pub async fn sync_time(&self) -> Result<i64, BinanceError> {
        let t0 = now_ms();
        let body = self.send(self.http.get(format!("{}/api/v3/time", self.private_url))).await?;
        let t1 = now_ms();
        let v: Value = serde_json::from_str(&body).map_err(|e| BinanceError::Decode(e.to_string()))?;
        let server = v.get("serverTime").and_then(Value::as_i64).ok_or_else(|| BinanceError::Decode("time".into()))?;
        let offset = server - (t0 + t1) / 2;
        self.offset_ms.store(offset, Ordering::Relaxed);
        Ok(offset)
    }

    pub async fn instrument(&self, symbol: &str) -> Result<Instrument, BinanceError> {
        parse_exchange_info(
            &self.send(self.http.get(format!("{}/api/v3/exchangeInfo?symbol={symbol}", self.data_url))).await?,
        )
    }

    pub async fn book(&self, symbol: &str, depth: u32) -> Result<Book, BinanceError> {
        let body =
            self.send(self.http.get(format!("{}/api/v3/depth?symbol={symbol}&limit={depth}", self.data_url))).await?;
        parse_depth(&body, now_ms())
    }

    pub async fn balances(&self) -> Result<Vec<Balance>, BinanceError> {
        parse_balances(
            &self.private(reqwest::Method::GET, "/api/v3/account", &[("omitZeroBalances", "true".into())]).await?,
        )
    }

    fn check_permit(&self, permit: &TradePermit) -> Result<(), BinanceError> {
        if permit.is_demo() != self.demo {
            return Err(BinanceError::Refused(format!(
                "a {} permit cannot trade on a {} client",
                if permit.is_demo() { "demo" } else { "real-account" },
                if self.demo { "demo" } else { "real-account" }
            )));
        }
        Ok(())
    }

    pub async fn place(&self, permit: &TradePermit, req: &OrderRequest) -> Result<OrderStatus, BinanceError> {
        self.check_permit(permit)?;
        if req.kind != OrderKind::Market && req.price.is_none() {
            return Err(BinanceError::Refused(format!("{:?} order without a price", req.kind)));
        }
        parse_order(&self.private(reqwest::Method::POST, "/api/v3/order", &order_params(req)).await?)
    }

    pub async fn order(&self, symbol: &str, order_id: &str) -> Result<OrderStatus, BinanceError> {
        let p = [("symbol", symbol.to_string()), ("orderId", order_id.to_string())];
        parse_order(&self.private(reqwest::Method::GET, "/api/v3/order", &p).await?)
    }

    pub async fn cancel(
        &self,
        permit: &TradePermit,
        symbol: &str,
        order_id: &str,
    ) -> Result<OrderStatus, BinanceError> {
        self.check_permit(permit)?;
        let p = [("symbol", symbol.to_string()), ("orderId", order_id.to_string())];
        parse_order(&self.private(reqwest::Method::DELETE, "/api/v3/order", &p).await?)
    }
}

/// What a read-only account check found (`--doctor`).
pub async fn probe_account(v: &VenueConfig) -> Result<Option<crate::AccountProbe>, BinanceError> {
    let Some(creds) = BinanceCredentials::from_env(v).map_err(BinanceError::Refused)? else { return Ok(None) };
    let c = BinanceClient::new(v, Some(creds))?;
    let offset_ms = c.sync_time().await?;
    let mut balances: Vec<Balance> = c.balances().await?.into_iter().filter(|b| b.total != 0.0).collect();
    balances.sort_by(|a, b| b.total.total_cmp(&a.total));
    Ok(Some(crate::AccountProbe { demo: c.is_demo(), offset_ms, balances }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/binance/");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}{name}")).unwrap()
    }

    // The example key and payloads from Binance's own API documentation.
    #[test]
    fn signatures_match_binance_documentation() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        assert_eq!(
            sign(
                secret,
                "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559"
            ),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
        assert_eq!(
            sign(
                secret,
                "symbol=%EF%BC%91%EF%BC%92%EF%BC%93%EF%BC%94%EF%BC%95%EF%BC%96&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559"
            ),
            "e1353ec6b14d888f1164ae9af8228a3dbd508bc82eb867db8ab6046442f33ef3"
        );
    }

    #[test]
    fn signed_queries_append_window_timestamp_and_signature() {
        let v = VenueConfig::binance();
        let c = BinanceClient::new(&v, Some(BinanceCredentials::new("k".into(), "SECRET".into()))).unwrap();
        let q = c.signed_query(&[("symbol", "SOLUSDT".into())], 1_499_827_319_559).unwrap();
        let (payload, sig) = q.split_once("&signature=").unwrap();
        assert_eq!(payload, "symbol=SOLUSDT&recvWindow=5000&timestamp=1499827319559");
        assert_eq!(sig, sign("SECRET", payload));
        assert!(!format!("{:?}", BinanceCredentials::new("k".into(), "SECRET".into())).contains("SECRET"));
        assert!(c.is_demo() && c.private_url == TESTNET, "demo signs against the testnet");
        assert!(matches!(BinanceClient::new(&v, None).unwrap().signed_query(&[], 0), Err(BinanceError::Refused(_))));
    }

    #[test]
    fn public_answers_parse_from_recorded_responses() {
        let i = parse_exchange_info(&fixture("exchange-info-solusdt.json")).unwrap();
        assert_eq!((i.inst_id.as_str(), i.base.as_str(), i.quote.as_str(), i.live), ("SOLUSDT", "SOL", "USDT", true));
        assert_eq!(i.size(0.12345).as_deref(), Some("0.123"), "stepSize 0.001");
        assert_eq!(i.price(111.234, Side::Buy).as_deref(), Some("111.24"), "tickSize 0.01");
        assert_eq!(i.min_notional, 5.0);
        match parse_exchange_info(&fixture("exchange-info-unknown.json")) {
            Err(BinanceError::Api { code, .. }) => assert_eq!(code, -1121),
            other => panic!("{other:?}"),
        }
        let b = parse_depth(&fixture("depth-solusdt.json"), 42).unwrap();
        assert_eq!((b.asks.len(), b.bids.len(), b.ts_ms), (5, 5, 42));
        assert!(b.asks[0].px > b.bids[0].px);
    }

    #[test]
    fn orders_are_built_and_answers_parsed() {
        let req = OrderRequest {
            inst_id: "SOLUSDT".into(),
            side: Side::Buy,
            kind: OrderKind::Ioc,
            size: "0.5".into(),
            price: Some("111.24".into()),
            client_id: "mobius1".into(),
        };
        assert_eq!(
            query(&order_params(&req)),
            "symbol=SOLUSDT&side=BUY&type=LIMIT&timeInForce=IOC&quantity=0.5&price=111.24&newClientOrderId=mobius1&newOrderRespType=FULL"
        );
        let full = r#"{"symbol":"SOLUSDT","orderId":28,"clientOrderId":"mobius1","status":"FILLED","executedQty":"0.50000000","cummulativeQuoteQty":"55.62000000",
            "fills":[{"price":"111.24","qty":"0.5","commission":"0.00050000","commissionAsset":"SOL"}]}"#;
        let s = parse_order(full).unwrap();
        assert_eq!(
            (s.order_id.as_str(), s.state.clone(), s.filled, s.fee_ccy.as_str()),
            ("28", OrderState::Filled, 0.5, "SOL")
        );
        assert!((s.avg_px - 111.24).abs() < 1e-9 && (s.fee + 0.0005).abs() < 1e-12);
        let err = r#"{"code":-2010,"msg":"Account has insufficient balance for requested action."}"#;
        assert!(matches!(parse_order(err), Err(BinanceError::Api { code: -2010, .. })));
        let bal = r#"{"balances":[{"asset":"USDT","free":"100.5","locked":"20"}]}"#;
        assert_eq!(parse_balances(bal).unwrap(), vec![Balance { ccy: "USDT".into(), available: 100.5, total: 120.5 }]);
    }
}
