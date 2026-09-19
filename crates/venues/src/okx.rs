//! OKX v5 REST: public market data, and signed private requests (balance,
//! place / query / cancel order). Private requests carry
//! `OK-ACCESS-SIGN = Base64(HMAC-SHA256(timestamp + METHOD + path?query + body, secret))`
//! with an ISO-8601 millisecond timestamp that must be within 30 s of OKX's
//! clock, so the client keeps an offset learned from `/api/v5/public/time`.
//! Demo trading uses the same host with a demo API key and
//! `x-simulated-trading: 1`.

use crate::book::{Book, Level};
use crate::{Balance, Instrument, OrderKind, OrderRequest, OrderState, OrderStatus, TradePermit};
use base64::Engine;
use hmac::{Hmac, Mac};
use searcher_core::config::VenueConfig;
use searcher_telemetry::{LimiterConfig, RateLimiter};
use serde_json::{Value, json};
use sha2::Sha256;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum OkxError {
    #[error("okx transport: {0}")]
    Transport(String),
    #[error("okx http {status}: {body}")]
    Http { status: u16, body: String },
    /// Envelope `code` ≠ "0" (e.g. 51001 unknown instrument, 50113 bad signature).
    #[error("okx {code}: {msg}")]
    Api { code: String, msg: String },
    /// Per-order result `sCode` ≠ "0" (e.g. 51008 insufficient balance).
    #[error("okx order rejected {code}: {msg}")]
    Rejected { code: String, msg: String },
    #[error("okx decode: {0}")]
    Decode(String),
    #[error("okx: {0}")]
    Refused(String),
}

/// API key, secret and passphrase. Never printed.
#[derive(Clone)]
pub struct Credentials {
    key: String,
    secret: String,
    passphrase: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(***)")
    }
}

impl Credentials {
    pub fn new(key: String, secret: String, passphrase: String) -> Self {
        Self { key, secret, passphrase }
    }

    /// From the environment variables the venue names. `Ok(None)` when none
    /// is set (public data only); an error naming the missing ones when only
    /// some are.
    pub fn from_env(v: &VenueConfig) -> Result<Option<Credentials>, String> {
        let names = [&v.api_key_env, &v.secret_env, &v.passphrase_env];
        let vals: Vec<Option<String>> = names
            .iter()
            .map(|n| (!n.is_empty()).then(|| std::env::var(n).ok()).flatten().filter(|x| !x.trim().is_empty()))
            .collect();
        if vals.iter().all(Option::is_none) {
            return Ok(None);
        }
        let missing: Vec<&str> =
            names.iter().zip(&vals).filter(|(_, v)| v.is_none()).map(|(n, _)| n.as_str()).collect();
        if !missing.is_empty() {
            return Err(format!("OKX credentials incomplete: {} not set", missing.join(", ")));
        }
        let mut it = vals.into_iter().flatten();
        Ok(Some(Credentials::new(it.next().unwrap(), it.next().unwrap(), it.next().unwrap())))
    }
}

/// `Base64(HMAC-SHA256(ts + method + path + body, secret))`.
pub fn sign(secret: &str, ts: &str, method: &str, path: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(ts.as_bytes());
    mac.update(method.as_bytes());
    mac.update(path.as_bytes());
    mac.update(body.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// ISO-8601 UTC with milliseconds, e.g. `2020-12-08T09:08:57.715Z`.
pub fn timestamp(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `data` of a `{code, msg, data}` envelope, or the API error it carries.
pub fn envelope(body: &str) -> Result<Vec<Value>, OkxError> {
    let v: Value = serde_json::from_str(body).map_err(|e| OkxError::Decode(format!("{e}: {}", clip(body))))?;
    let code = v.get("code").and_then(Value::as_str).unwrap_or("?");
    if code != "0" {
        let msg = v.get("msg").and_then(Value::as_str).unwrap_or("").to_string();
        return Err(OkxError::Api { code: code.to_string(), msg });
    }
    Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
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

pub fn parse_instrument(body: &str) -> Result<Instrument, OkxError> {
    let data = envelope(body)?;
    let d = data.first().ok_or_else(|| OkxError::Decode("no instrument".into()))?;
    Ok(Instrument {
        inst_id: text(d, "instId").into(),
        base: text(d, "baseCcy").into(),
        quote: text(d, "quoteCcy").into(),
        tick_sz: text(d, "tickSz").into(),
        lot_sz: text(d, "lotSz").into(),
        min_sz: text(d, "minSz").into(),
        live: text(d, "state") == "live",
    })
}

pub fn parse_book(body: &str) -> Result<Book, OkxError> {
    let data = envelope(body)?;
    let d = data.first().ok_or_else(|| OkxError::Decode("no book".into()))?;
    let side = |k: &str| -> Vec<Level> {
        d.get(k)
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|l| {
                        let px = l.get(0)?.as_str()?.parse().ok()?;
                        let sz = l.get(1)?.as_str()?.parse().ok()?;
                        Some(Level { px, sz })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Book { asks: side("asks"), bids: side("bids"), ts_ms: text(d, "ts").parse().unwrap_or(0) })
}

pub fn parse_order(body: &str) -> Result<OrderStatus, OkxError> {
    let data = envelope(body)?;
    let d = data.first().ok_or_else(|| OkxError::Decode("no order".into()))?;
    let state = match text(d, "state") {
        "live" => OrderState::Live,
        "partially_filled" => OrderState::PartiallyFilled,
        "filled" => OrderState::Filled,
        "canceled" | "mmp_canceled" => OrderState::Canceled,
        other => OrderState::Other(other.into()),
    };
    Ok(OrderStatus {
        order_id: text(d, "ordId").into(),
        client_id: text(d, "clOrdId").into(),
        state,
        filled: num(d, "accFillSz"),
        avg_px: num(d, "avgPx"),
        fee: num(d, "fee"),
        fee_ccy: text(d, "feeCcy").into(),
    })
}

pub fn parse_balances(body: &str) -> Result<Vec<Balance>, OkxError> {
    let data = envelope(body)?;
    let details = data.first().and_then(|d| d.get("details")).and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(details
        .iter()
        .map(|b| Balance { ccy: text(b, "ccy").into(), available: num(b, "availBal"), total: num(b, "cashBal") })
        .collect())
}

/// `ordId` of a place/cancel answer, or the per-order rejection.
pub fn parse_order_ack(body: &str) -> Result<String, OkxError> {
    // a rejected order comes back with envelope code 1 and the reason in data[0]
    let v: Value = serde_json::from_str(body).map_err(|e| OkxError::Decode(e.to_string()))?;
    let first = v.get("data").and_then(Value::as_array).and_then(|d| d.first().cloned());
    if let Some(d) = &first {
        let s = text(d, "sCode");
        if !s.is_empty() && s != "0" {
            return Err(OkxError::Rejected { code: s.into(), msg: text(d, "sMsg").into() });
        }
    }
    envelope(body)?;
    first.map(|d| text(&d, "ordId").to_string()).ok_or_else(|| OkxError::Decode("no ordId".into()))
}

/// JSON body of `POST /api/v5/trade/order` (spot, cash).
pub fn order_body(req: &OrderRequest) -> String {
    let ord_type = match req.kind {
        OrderKind::Market => "market",
        OrderKind::Limit => "limit",
        OrderKind::Ioc => "ioc",
        OrderKind::PostOnly => "post_only",
    };
    let mut b = json!({
        "instId": req.inst_id,
        "tdMode": "cash",
        "clOrdId": req.client_id,
        "side": req.side.as_str(),
        "ordType": ord_type,
        "sz": req.size,
    });
    if req.kind == OrderKind::Market {
        // spot market size is in base units, like every other kind here
        b["tgtCcy"] = json!("base_ccy");
    } else if let Some(px) = &req.price {
        b["px"] = json!(px);
    }
    b.to_string()
}

pub struct OkxClient {
    http: reqwest::Client,
    base: String,
    creds: Option<Credentials>,
    demo: bool,
    /// OKX clock − ours, ms.
    offset_ms: AtomicI64,
    public: RateLimiter,
    private: RateLimiter,
}

impl OkxClient {
    pub fn new(v: &VenueConfig, creds: Option<Credentials>) -> Result<Self, OkxError> {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(concat!("mobius/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy() {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| OkxError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() {
            b = b.no_proxy();
        }
        Ok(Self {
            http: b.build().map_err(|e| OkxError::Transport(e.to_string()))?,
            base: v.rest_url.trim_end_matches('/').to_string(),
            creds,
            demo: v.demo,
            offset_ms: AtomicI64::new(0),
            // OKX: market data 20 / 2 s per IP; orders 60 / 2 s per instrument. Stay well under.
            public: RateLimiter::new("okx.public", LimiterConfig::new(5.0, 5)),
            private: RateLimiter::new("okx.private", LimiterConfig::new(5.0, 3)),
        })
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    pub fn has_credentials(&self) -> bool {
        self.creds.is_some()
    }

    /// Headers of a signed request at `now_ms` (our clock; the learned offset is applied).
    pub fn signed_headers(
        &self,
        method: &str,
        path: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<Vec<(&'static str, String)>, OkxError> {
        let c = self.creds.as_ref().ok_or_else(|| OkxError::Refused("no API credentials configured".into()))?;
        let ts = timestamp(now_ms + self.offset_ms.load(Ordering::Relaxed));
        let mut h = vec![
            ("OK-ACCESS-KEY", c.key.clone()),
            ("OK-ACCESS-SIGN", sign(&c.secret, &ts, method, path, body)),
            ("OK-ACCESS-TIMESTAMP", ts),
            ("OK-ACCESS-PASSPHRASE", c.passphrase.clone()),
        ];
        if self.demo {
            h.push(("x-simulated-trading", "1".into()));
        }
        Ok(h)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<String, OkxError> {
        let resp = req.send().await.map_err(|e| OkxError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| OkxError::Transport(e.to_string()))?;
        if status != 200 {
            // OKX still returns a JSON envelope on most errors: surface its code
            if let Err(e @ OkxError::Api { .. }) = envelope(&body) {
                return Err(e);
            }
            return Err(OkxError::Http { status, body: clip(&body) });
        }
        Ok(body)
    }

    async fn get_public(&self, path: &str) -> Result<String, OkxError> {
        self.public.acquire().await;
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }

    async fn private(&self, method: &str, path: &str, body: &str) -> Result<String, OkxError> {
        self.private.acquire().await;
        let headers = self.signed_headers(method, path, body, now_ms())?;
        let url = format!("{}{path}", self.base);
        let mut req = if method == "POST" {
            self.http.post(url).header("content-type", "application/json").body(body.to_string())
        } else {
            self.http.get(url)
        };
        for (k, v) in headers {
            req = req.header(k, v);
        }
        self.send(req).await
    }

    /// Learn OKX's clock offset (private requests are refused beyond 30 s of skew).
    pub async fn sync_time(&self) -> Result<i64, OkxError> {
        let t0 = now_ms();
        let body = self.get_public("/api/v5/public/time").await?;
        let t1 = now_ms();
        let server: i64 = envelope(&body)?
            .first()
            .and_then(|d| text(d, "ts").parse().ok())
            .ok_or_else(|| OkxError::Decode("time".into()))?;
        let offset = server - (t0 + t1) / 2;
        self.offset_ms.store(offset, Ordering::Relaxed);
        Ok(offset)
    }

    pub async fn instrument(&self, inst_id: &str) -> Result<Instrument, OkxError> {
        parse_instrument(&self.get_public(&format!("/api/v5/public/instruments?instType=SPOT&instId={inst_id}")).await?)
    }

    pub async fn book(&self, inst_id: &str, depth: u32) -> Result<Book, OkxError> {
        parse_book(&self.get_public(&format!("/api/v5/market/books?instId={inst_id}&sz={depth}")).await?)
    }

    /// Trading-account balances (read-only; needs credentials).
    pub async fn balances(&self) -> Result<Vec<Balance>, OkxError> {
        parse_balances(&self.private("GET", "/api/v5/account/balance", "").await?)
    }

    fn check_permit(&self, permit: &TradePermit) -> Result<(), OkxError> {
        if permit.is_demo() != self.demo {
            return Err(OkxError::Refused(format!(
                "a {} permit cannot trade on a {} client",
                if permit.is_demo() { "demo" } else { "real-account" },
                if self.demo { "demo" } else { "real-account" }
            )));
        }
        Ok(())
    }

    pub async fn place(&self, permit: &TradePermit, req: &OrderRequest) -> Result<String, OkxError> {
        self.check_permit(permit)?;
        if req.kind != OrderKind::Market && req.price.is_none() {
            return Err(OkxError::Refused(format!("{:?} order without a price", req.kind)));
        }
        parse_order_ack(&self.private("POST", "/api/v5/trade/order", &order_body(req)).await?)
    }

    pub async fn order(&self, inst_id: &str, order_id: &str) -> Result<OrderStatus, OkxError> {
        parse_order(&self.private("GET", &format!("/api/v5/trade/order?instId={inst_id}&ordId={order_id}"), "").await?)
    }

    pub async fn cancel(&self, permit: &TradePermit, inst_id: &str, order_id: &str) -> Result<(), OkxError> {
        self.check_permit(permit)?;
        let body = json!({ "instId": inst_id, "ordId": order_id }).to_string();
        parse_order_ack(&self.private("POST", "/api/v5/trade/cancel-order", &body).await?).map(|_| ())
    }
}

/// What a read-only account check found (`--doctor`).
#[derive(Clone, Debug, PartialEq)]
pub struct AccountProbe {
    pub demo: bool,
    /// OKX clock − ours, ms (requests fail beyond ±30 s).
    pub offset_ms: i64,
    /// Non-zero balances, largest first by amount (not by value).
    pub balances: Vec<Balance>,
}

/// Check the venue's credentials without trading: sync the clock, then read
/// balances with a signed request. `Ok(None)` when no credentials are set.
pub async fn probe_account(v: &VenueConfig) -> Result<Option<AccountProbe>, OkxError> {
    let Some(creds) = Credentials::from_env(v).map_err(OkxError::Refused)? else { return Ok(None) };
    let c = OkxClient::new(v, Some(creds))?;
    let offset_ms = c.sync_time().await?;
    let mut balances: Vec<Balance> = c.balances().await?.into_iter().filter(|b| b.total != 0.0).collect();
    balances.sort_by(|a, b| b.total.total_cmp(&a.total));
    Ok(Some(AccountProbe { demo: c.is_demo(), offset_ms, balances }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/okx/");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}{name}")).unwrap()
    }

    // Expected values computed independently with Python's hmac/hashlib
    // (made-up secret, not a real key).
    #[test]
    fn signatures_match_an_independent_implementation() {
        let secret = "22582BD0CFF14C41EDBF1AB98506286D";
        let ts = "2020-12-08T09:08:57.715Z";
        assert_eq!(
            sign(secret, ts, "GET", "/api/v5/account/balance?ccy=BTC", ""),
            "HiZhvSfMtWJA3uUIVXV3a/bSXNPCWvYFXoGCVS8V4zY="
        );
        let body = r#"{"instId":"BTC-USDT","tdMode":"cash","clOrdId":"b15","side":"buy","ordType":"limit","px":"2.15","sz":"2"}"#;
        assert_eq!(
            sign(secret, ts, "POST", "/api/v5/trade/order", body),
            "dI6rrL9rXW/HdaPKJ/6LC1OgvH4/PYju6R3CqixMTNQ="
        );
        assert_eq!(timestamp(1_607_418_537_715), ts);
    }

    #[test]
    fn signed_headers_carry_the_demo_flag_and_never_debug_print_secrets() {
        let mut v = VenueConfig::okx();
        let creds = Credentials::new("k".into(), "SECRET".into(), "PASS".into());
        assert!(!format!("{creds:?}").contains("SECRET"));
        let c = OkxClient::new(&v, Some(creds.clone())).unwrap();
        let h = c.signed_headers("GET", "/api/v5/account/balance", "", 1_607_418_537_715).unwrap();
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("OK-ACCESS-TIMESTAMP"), Some("2020-12-08T09:08:57.715Z"));
        assert_eq!(get("x-simulated-trading"), Some("1"), "demo is the default");
        v.demo = false;
        let live = OkxClient::new(&v, Some(creds)).unwrap();
        let h = live.signed_headers("GET", "/x", "", 0).unwrap();
        assert!(h.iter().all(|(n, _)| *n != "x-simulated-trading"));
        let none = OkxClient::new(&v, None).unwrap();
        assert!(matches!(none.signed_headers("GET", "/x", "", 0), Err(OkxError::Refused(_))));
    }

    #[test]
    fn public_answers_parse_from_recorded_responses() {
        let i = parse_instrument(&fixture("instrument-sol-usdt.json")).unwrap();
        assert_eq!(
            (i.base.as_str(), i.quote.as_str(), i.tick_sz.as_str(), i.lot_sz.as_str()),
            ("SOL", "USDT", "0.01", "0.000001")
        );
        assert_eq!((i.min_sz.as_str(), i.live), ("0.01", true));
        match parse_instrument(&fixture("instrument-unknown.json")) {
            Err(OkxError::Api { code, .. }) => assert_eq!(code, "51001"),
            other => panic!("{other:?}"),
        }
        let b = parse_book(&fixture("books-sol-usdt.json")).unwrap();
        assert_eq!((b.asks.len(), b.bids.len()), (5, 5));
        assert!(b.asks[0].px > b.bids[0].px && b.asks[0].px <= b.asks[1].px && b.bids[0].px >= b.bids[1].px);
        assert!(b.ts_ms > 1_700_000_000_000);
    }

    #[test]
    fn orders_are_built_and_answers_parsed() {
        let req = OrderRequest {
            inst_id: "SOL-USDT".into(),
            side: Side::Buy,
            kind: OrderKind::Ioc,
            size: "0.5".into(),
            price: Some("111.38".into()),
            client_id: "mobius1".into(),
        };
        let b: Value = serde_json::from_str(&order_body(&req)).unwrap();
        assert_eq!(
            b,
            json!({"instId":"SOL-USDT","tdMode":"cash","clOrdId":"mobius1","side":"buy","ordType":"ioc","sz":"0.5","px":"111.38"})
        );
        let market = OrderRequest { kind: OrderKind::Market, price: None, side: Side::Sell, ..req };
        let b: Value = serde_json::from_str(&order_body(&market)).unwrap();
        assert_eq!(
            (b["ordType"].as_str(), b["tgtCcy"].as_str(), b.get("px")),
            (Some("market"), Some("base_ccy"), None)
        );

        let ok = r#"{"code":"0","msg":"","data":[{"clOrdId":"mobius1","ordId":"312269865356374016","tag":"","sCode":"0","sMsg":""}]}"#;
        assert_eq!(parse_order_ack(ok).unwrap(), "312269865356374016");
        let rejected = r#"{"code":"1","msg":"Operation failed.","data":[{"clOrdId":"mobius1","ordId":"","sCode":"51008","sMsg":"Order failed. Insufficient balance."}]}"#;
        match parse_order_ack(rejected) {
            Err(OkxError::Rejected { code, .. }) => assert_eq!(code, "51008"),
            other => panic!("{other:?}"),
        }
        let status = r#"{"code":"0","msg":"","data":[{"ordId":"1","clOrdId":"mobius1","state":"filled","accFillSz":"0.5","avgPx":"111.37","fee":"-0.0005","feeCcy":"SOL"}]}"#;
        let s = parse_order(status).unwrap();
        assert_eq!((s.state, s.filled, s.avg_px, s.fee_ccy.as_str()), (OrderState::Filled, 0.5, 111.37, "SOL"));
        let bal = r#"{"code":"0","msg":"","data":[{"details":[{"ccy":"USDT","availBal":"100.5","cashBal":"120"}]}]}"#;
        assert_eq!(parse_balances(bal).unwrap(), vec![Balance { ccy: "USDT".into(), available: 100.5, total: 120.0 }]);
    }

    #[test]
    fn a_permit_of_the_other_kind_is_refused() {
        let mut v = VenueConfig::okx();
        v.trading = true;
        let demo_permit = TradePermit::check(searcher_core::model::Mode::Paper, false, &v).unwrap();
        v.demo = false;
        let live_client = OkxClient::new(&v, None).unwrap();
        assert!(matches!(live_client.check_permit(&demo_permit), Err(OkxError::Refused(_))));
    }

    #[test]
    fn credentials_come_from_the_named_variables() {
        let mut v = VenueConfig::okx();
        v.api_key_env = "MOBIUS_TEST_OKX_K".into();
        v.secret_env = "MOBIUS_TEST_OKX_S".into();
        v.passphrase_env = "MOBIUS_TEST_OKX_P".into();
        assert!(Credentials::from_env(&v).unwrap().is_none(), "none set: public only");
        // SAFETY: test-only variables with unique names
        unsafe { std::env::set_var("MOBIUS_TEST_OKX_K", "k") };
        let e = Credentials::from_env(&v).unwrap_err();
        assert!(e.contains("MOBIUS_TEST_OKX_S") && e.contains("MOBIUS_TEST_OKX_P"), "{e}");
        unsafe {
            std::env::set_var("MOBIUS_TEST_OKX_S", "s");
            std::env::set_var("MOBIUS_TEST_OKX_P", "p");
        }
        assert!(Credentials::from_env(&v).unwrap().is_some());
    }
}
