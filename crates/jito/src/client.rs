//! Jito Block Engine JSON-RPC client (docs.jito.wtf/lowlatencytxnsend).

use searcher_core::event::TipFloor;
use searcher_core::model::{Mode, ServiceId};
use searcher_core::{Address, Ts};
use searcher_telemetry::{LimiterConfig, RateLimiter, Stopwatch, Telemetry};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum JitoError {
    #[error("jito rate limited; backing off {0:?}")]
    RateLimited(Duration),
    #[error("jito rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("http {0}: {1}")]
    Http(u16, String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("bundle must contain 1..=5 transactions, got {0}")]
    BundleSize(usize),
}

/// Proof that the process is allowed to send transactions. Only obtainable
/// when the mode sends transactions AND `execution.live_enabled` is true.
#[derive(Debug)]
pub struct SendPermit(());

impl SendPermit {
    pub fn check(mode: Mode, live_enabled: bool) -> Option<SendPermit> {
        (mode.sends_transactions() && live_enabled).then_some(SendPermit(()))
    }
}

#[derive(Clone)]
struct Uuid(String);

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Uuid(***)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InflightStatus {
    Invalid,
    Pending,
    Failed,
    Landed { slot: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleStatus {
    pub bundle_id: String,
    pub slot: Option<u64>,
    pub confirmation_status: Option<String>,
    pub err: Option<String>,
    pub transactions: Vec<String>,
}

pub struct JitoClient {
    http: reqwest::Client,
    base_url: String,
    tip_floor_url: String,
    uuid: Option<Uuid>,
    limiter: Arc<RateLimiter>,
    telemetry: Arc<Telemetry>,
}

/// The 8 mainnet tip accounts as returned by `getTipAccounts` on 2026-09-18.
/// Only used if the live call fails; the live list always wins.
pub const KNOWN_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

impl JitoClient {
    pub fn new(
        base_url: &str,
        tip_floor_url: &str,
        uuid: Option<String>,
        rps: f64,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, JitoError> {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("mobius-searcher/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy() {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| JitoError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() {
            b = b.no_proxy();
        }
        let http = b.build().map_err(|e| JitoError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            tip_floor_url: tip_floor_url.to_string(),
            uuid: uuid.filter(|u| !u.trim().is_empty()).map(Uuid),
            limiter: Arc::new(RateLimiter::new("jito", LimiterConfig::new(rps, 1))),
            telemetry,
        })
    }

    async fn rpc(&self, path: &str, method: &str, params: Value) -> Result<Value, JitoError> {
        self.limiter.acquire().await;
        let sw = Stopwatch::start();
        let mut req = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(u) = &self.uuid {
            req = req.header("x-jito-auth", &u.0);
        }
        let resp = req.send().await.map_err(|e| {
            let err = JitoError::Transport(e.to_string());
            self.telemetry.record_err(ServiceId::Jito, Some(sw.ms()), format!("{method}: {err}"));
            err
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| JitoError::Transport(e.to_string()))?;
        let ms = sw.ms();
        if status == 429 {
            let d = self.limiter.on_rate_limited(Instant::now(), None);
            self.telemetry.record_rate_limited(ServiceId::Jito, d.as_millis() as u64);
            return Err(JitoError::RateLimited(d));
        }
        self.limiter.on_success();
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            self.telemetry.record_err(ServiceId::Jito, Some(ms), format!("{method}: http {status} undecodable"));
            JitoError::Http(status, truncate(&text, 200))
        })?;
        if let Some(e) = v.get("error") {
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = e.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            self.telemetry.record_err(ServiceId::Jito, Some(ms), format!("{method}: {code} {message}"));
            return Err(JitoError::Rpc { code, message });
        }
        if !(200..300).contains(&status) {
            self.telemetry.record_err(ServiceId::Jito, Some(ms), format!("{method}: http {status}"));
            return Err(JitoError::Http(status, truncate(&text, 200)));
        }
        self.telemetry.record_ok(ServiceId::Jito, ms);
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn get_tip_accounts(&self) -> Result<Vec<Address>, JitoError> {
        let v = self.rpc("/api/v1/getTipAccounts", "getTipAccounts", json!([])).await?;
        parse_tip_accounts(&v)
    }

    /// REST tip floor. Values are SOL (floats) on the wire → converted to lamports here.
    pub async fn tip_floor(&self) -> Result<TipFloor, JitoError> {
        self.limiter.acquire().await;
        let sw = Stopwatch::start();
        let resp = self.http.get(&self.tip_floor_url).send().await.map_err(|e| {
            self.telemetry.record_err(ServiceId::Jito, Some(sw.ms()), format!("tip_floor: {e}"));
            JitoError::Transport(e.to_string())
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| JitoError::Transport(e.to_string()))?;
        if status == 429 {
            let d = self.limiter.on_rate_limited(Instant::now(), None);
            self.telemetry.record_rate_limited(ServiceId::Jito, d.as_millis() as u64);
            return Err(JitoError::RateLimited(d));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| JitoError::Decode(format!("tip_floor: {e}")))?;
        let tf = parse_tip_floor(&v, Ts::now())?;
        self.telemetry.record_ok(ServiceId::Jito, sw.ms());
        Ok(tf)
    }

    /// `sendBundle` with explicit base64 encoding (the API default is base58).
    pub async fn send_bundle(&self, _permit: &SendPermit, txs_base64: &[String]) -> Result<String, JitoError> {
        if txs_base64.is_empty() || txs_base64.len() > 5 {
            return Err(JitoError::BundleSize(txs_base64.len()));
        }
        let v = self.rpc("/api/v1/bundles", "sendBundle", json!([txs_base64, {"encoding": "base64"}])).await?;
        v.as_str().map(str::to_string).ok_or_else(|| JitoError::Decode("sendBundle result".into()))
    }

    pub async fn inflight_statuses(&self, ids: &[String]) -> Result<Vec<(String, InflightStatus)>, JitoError> {
        let v = self.rpc("/api/v1/getInflightBundleStatuses", "getInflightBundleStatuses", json!([ids])).await?;
        Ok(parse_inflight(&v))
    }

    pub async fn bundle_statuses(&self, ids: &[String]) -> Result<Vec<BundleStatus>, JitoError> {
        let v = self.rpc("/api/v1/getBundleStatuses", "getBundleStatuses", json!([ids])).await?;
        Ok(parse_bundle_statuses(&v))
    }
}

pub fn parse_tip_accounts(v: &Value) -> Result<Vec<Address>, JitoError> {
    let arr = v.as_array().ok_or_else(|| JitoError::Decode("getTipAccounts".into()))?;
    let out: Vec<Address> = arr.iter().filter_map(|a| a.as_str()?.parse().ok()).collect();
    if out.is_empty() {
        return Err(JitoError::Decode("getTipAccounts: empty".into()));
    }
    Ok(out)
}

fn sol_to_lamports(v: Option<&Value>) -> u64 {
    // External float (SOL) → integer lamports at the adapter edge.
    v.and_then(Value::as_f64).filter(|f| f.is_finite() && *f >= 0.0).map(|f| (f * 1e9).round() as u64).unwrap_or(0)
}

pub fn parse_tip_floor(v: &Value, ts: Ts) -> Result<TipFloor, JitoError> {
    let e = v.as_array().and_then(|a| a.first()).ok_or_else(|| JitoError::Decode("tip_floor: empty".into()))?;
    Ok(TipFloor {
        ts,
        p25: sol_to_lamports(e.get("landed_tips_25th_percentile")),
        p50: sol_to_lamports(e.get("landed_tips_50th_percentile")),
        p75: sol_to_lamports(e.get("landed_tips_75th_percentile")),
        p95: sol_to_lamports(e.get("landed_tips_95th_percentile")),
        p99: sol_to_lamports(e.get("landed_tips_99th_percentile")),
        ema_p50: sol_to_lamports(e.get("ema_landed_tips_50th_percentile")),
    })
}

pub fn parse_inflight(v: &Value) -> Vec<(String, InflightStatus)> {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let id = s.get("bundle_id")?.as_str()?.to_string();
                    let st = match s.get("status")?.as_str()? {
                        "Landed" => {
                            InflightStatus::Landed { slot: s.get("landed_slot").and_then(Value::as_u64).unwrap_or(0) }
                        }
                        "Pending" => InflightStatus::Pending,
                        "Failed" => InflightStatus::Failed,
                        _ => InflightStatus::Invalid,
                    };
                    Some((id, st))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_bundle_statuses(v: &Value) -> Vec<BundleStatus> {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    if s.is_null() {
                        return None;
                    }
                    let err = s.get("err").and_then(|e| match e.get("Ok") {
                        Some(_) => None,
                        None if e.is_null() => None,
                        None => Some(e.to_string()),
                    });
                    Some(BundleStatus {
                        bundle_id: s.get("bundle_id")?.as_str()?.to_string(),
                        slot: s.get("slot").and_then(Value::as_u64),
                        confirmation_status: s.get("confirmation_status").and_then(Value::as_str).map(str::to_string),
                        err,
                        transactions: s
                            .get("transactions")
                            .and_then(Value::as_array)
                            .map(|t| t.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..s.floor_char_boundary(n)]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_permit_requires_mode_and_gate() {
        assert!(SendPermit::check(Mode::Paper, true).is_none());
        assert!(SendPermit::check(Mode::Live, false).is_none());
        assert!(SendPermit::check(Mode::Confirm, false).is_none());
        assert!(SendPermit::check(Mode::Live, true).is_some());
    }

    #[test]
    fn tip_floor_is_sol_on_the_wire() {
        let v: Value = serde_json::from_str(
            r#"[{"time":"2026-09-18T06:00:00Z","landed_tips_25th_percentile":1.0e-6,"landed_tips_50th_percentile":1.126e-6,
                "landed_tips_75th_percentile":1.2e-5,"landed_tips_95th_percentile":0.000555,"landed_tips_99th_percentile":0.001,
                "ema_landed_tips_50th_percentile":2.3e-6}]"#,
        )
        .unwrap();
        let t = parse_tip_floor(&v, Ts(0)).unwrap();
        assert_eq!((t.p25, t.p50, t.p75, t.p95, t.p99, t.ema_p50), (1_000, 1_126, 12_000, 555_000, 1_000_000, 2_300));
        assert!(parse_tip_floor(&serde_json::json!([]), Ts(0)).is_err());
    }

    #[test]
    fn known_tip_accounts_parse() {
        let v = serde_json::json!(KNOWN_TIP_ACCOUNTS);
        assert_eq!(parse_tip_accounts(&v).unwrap().len(), 8);
    }

    #[test]
    fn statuses_parse() {
        let v = serde_json::json!({"context":{"slot":1},"value":[
            {"bundle_id":"a","status":"Landed","landed_slot":42},
            {"bundle_id":"b","status":"Pending","landed_slot":null},
            {"bundle_id":"c","status":"Invalid","landed_slot":null},
            {"bundle_id":"d","status":"Failed","landed_slot":null}]});
        let s = parse_inflight(&v);
        assert_eq!(s[0].1, InflightStatus::Landed { slot: 42 });
        assert_eq!(s[1].1, InflightStatus::Pending);
        assert_eq!(s[2].1, InflightStatus::Invalid);
        assert_eq!(s[3].1, InflightStatus::Failed);
        let v = serde_json::json!({"context":{"slot":1},"value":[
            {"bundle_id":"a","transactions":["sig1"],"slot":42,"confirmation_status":"confirmed","err":{"Ok":null}}, null]});
        let b = parse_bundle_statuses(&v);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].err, None);
        assert_eq!(b[0].confirmation_status.as_deref(), Some("confirmed"));
    }
}
