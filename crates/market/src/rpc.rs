//! Minimal Solana JSON-RPC client: exactly the methods the searcher needs,
//! rate limited, with telemetry. Provider URLs often embed API keys, so the
//! URL is never printed.

use searcher_core::model::ServiceId;
use searcher_core::{Address, Ts};
use searcher_telemetry::{LimiterConfig, RateLimiter, Stopwatch, Telemetry};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("rpc rate limited; backing off {0:?}")]
    RateLimited(Duration),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("http {0}")]
    Http(u16),
    #[error("timeout")]
    Timeout,
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
}

pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    /// HOT calls (simulation, ATA checks, feed seeding).
    limiter: Arc<RateLimiter>,
    /// WARM/COLD polls (block height, balance, fees, TPS): their own bucket so
    /// they can never delay a HOT call.
    bg_limiter: Arc<RateLimiter>,
    sim_limiter: Arc<RateLimiter>,
    telemetry: Arc<Telemetry>,
    id: AtomicU64,
}

impl fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RpcClient(url=<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochInfo {
    pub slot: u64,
    pub block_height: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SimulateOutcome {
    pub context_slot: Option<u64>,
    pub err: Option<Value>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
    pub fee: Option<u64>,
    pub pre_balances: Option<Vec<u64>>,
    pub post_balances: Option<Vec<u64>>,
    /// Lamports of accounts requested via `accounts` config (post-state).
    pub post_account_lamports: Vec<Option<u64>>,
    pub latency_ms: u32,
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<RpcErrBody>,
}

#[derive(Deserialize)]
struct RpcErrBody {
    code: i64,
    message: String,
}

impl RpcClient {
    pub fn new(
        url: &str,
        cfg: LimiterConfig,
        sim_rps: f64,
        timeout: Duration,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, RpcError> {
        let mut b = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("mobius-searcher/", env!("CARGO_PKG_VERSION")))
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(30));
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy() {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| RpcError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() {
            b = b.no_proxy();
        }
        let http = b.build().map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            url: url.to_string(),
            limiter: Arc::new(RateLimiter::new("rpc", cfg)),
            bg_limiter: Arc::new(RateLimiter::new("rpc.background", LimiterConfig::new(1.0, 2))),
            sim_limiter: Arc::new(RateLimiter::new("rpc.simulate", LimiterConfig::new(sim_rps, 1))),
            telemetry,
            id: AtomicU64::new(1),
        })
    }

    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    pub fn sim_limiter(&self) -> &Arc<RateLimiter> {
        &self.sim_limiter
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<(Value, u32), RpcError> {
        self.call_on(&self.limiter, method, params).await
    }

    /// A WARM/COLD poll: separate bucket, never ahead of HOT calls.
    pub async fn call_background(&self, method: &str, params: Value) -> Result<(Value, u32), RpcError> {
        self.call_on(&self.bg_limiter, method, params).await
    }

    async fn call_on(&self, limiter: &RateLimiter, method: &str, params: Value) -> Result<(Value, u32), RpcError> {
        let asked = Instant::now();
        limiter.acquire().await;
        self.telemetry.latency.duration_us(&format!("rpc.token_wait_us.{}", limiter.name()), asked.elapsed());
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let sw = Stopwatch::start();
        let resp = match self.http.post(&self.url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                let err = if e.is_timeout() { RpcError::Timeout } else { RpcError::Transport(redact(&e.to_string())) };
                self.telemetry.record_err(ServiceId::Rpc, Some(sw.ms()), format!("{method}: {err}"));
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| RpcError::Transport(redact(&e.to_string())))?;
        let ms = sw.ms();
        let parsed: Option<RpcResponse> = serde_json::from_str(&text).ok();
        let rpc_429 = parsed.as_ref().and_then(|p| p.error.as_ref()).is_some_and(|e| e.code == 429);
        if status == 429 || rpc_429 {
            let d = limiter.on_rate_limited(Instant::now(), None);
            self.telemetry.record_rate_limited(ServiceId::Rpc, d.as_millis() as u64);
            return Err(RpcError::RateLimited(d));
        }
        limiter.on_success();
        if !(200..300).contains(&status) {
            self.telemetry.record_err(ServiceId::Rpc, Some(ms), format!("{method}: http {status}"));
            return Err(RpcError::Http(status));
        }
        let Some(parsed) = parsed else {
            self.telemetry.record_err(ServiceId::Rpc, Some(ms), format!("{method}: undecodable body"));
            return Err(RpcError::Decode(format!("{method}: undecodable body")));
        };
        if let Some(e) = parsed.error {
            self.telemetry.record_err(ServiceId::Rpc, Some(ms), format!("{method}: {} {}", e.code, e.message));
            return Err(RpcError::Rpc { code: e.code, message: e.message });
        }
        self.telemetry.record_ok(ServiceId::Rpc, ms);
        Ok((parsed.result.unwrap_or(Value::Null), ms))
    }

    pub async fn get_slot(&self) -> Result<u64, RpcError> {
        let (v, _) = self.call("getSlot", json!([{"commitment": "processed"}])).await?;
        v.as_u64().ok_or_else(|| RpcError::Decode("getSlot".into()))
    }

    pub async fn get_epoch_info(&self) -> Result<EpochInfo, RpcError> {
        let (v, _) = self.call_background("getEpochInfo", json!([{"commitment": "processed"}])).await?;
        let g = |k: &str| v.get(k).and_then(Value::as_u64).ok_or_else(|| RpcError::Decode(format!("getEpochInfo.{k}")));
        Ok(EpochInfo { slot: g("absoluteSlot")?, block_height: g("blockHeight")?, epoch: g("epoch")? })
    }

    pub async fn get_balance(&self, a: &Address) -> Result<u64, RpcError> {
        let (v, _) = self.call_background("getBalance", json!([a.to_string(), {"commitment": "processed"}])).await?;
        v.get("value").and_then(Value::as_u64).ok_or_else(|| RpcError::Decode("getBalance".into()))
    }

    /// Existence (and lamports) of up to 100 accounts, without their data.
    pub async fn get_accounts_lamports(&self, addrs: &[Address]) -> Result<Vec<Option<u64>>, RpcError> {
        let keys: Vec<String> = addrs.iter().take(100).map(|a| a.to_string()).collect();
        let (v, _) = self
            .call(
                "getMultipleAccounts",
                json!([keys, {"encoding": "base64", "commitment": "processed", "dataSlice": {"offset": 0, "length": 0}}]),
            )
            .await?;
        let arr =
            v.get("value").and_then(Value::as_array).ok_or_else(|| RpcError::Decode("getMultipleAccounts".into()))?;
        Ok(arr.iter().map(|a| a.get("lamports").and_then(Value::as_u64)).collect())
    }

    /// Context slot plus owner program and data of up to 100 accounts (`None` = missing).
    #[allow(clippy::type_complexity)]
    pub async fn get_account_datas(
        &self,
        addrs: &[Address],
    ) -> Result<(Option<u64>, Vec<Option<(String, Vec<u8>)>>), RpcError> {
        use base64::Engine;
        let keys: Vec<String> = addrs.iter().take(100).map(|a| a.to_string()).collect();
        let (v, _) =
            self.call("getMultipleAccounts", json!([keys, {"encoding": "base64", "commitment": "confirmed"}])).await?;
        let arr =
            v.get("value").and_then(Value::as_array).ok_or_else(|| RpcError::Decode("getMultipleAccounts".into()))?;
        let slot = v.get("context").and_then(|c| c.get("slot")).and_then(Value::as_u64);
        Ok((
            slot,
            arr.iter()
                .map(|a| {
                    let owner = a.get("owner")?.as_str()?.to_string();
                    let b64 = a.get("data")?.get(0)?.as_str()?;
                    Some((owner, base64::engine::general_purpose::STANDARD.decode(b64).ok()?))
                })
                .collect(),
        ))
    }

    /// Per-slot minimum priority fee (µlamports/CU) paid by transactions that
    /// write-locked any of `addrs`, over the node's recent slots (≤ 150).
    pub async fn get_recent_prioritization_fees(&self, addrs: &[Address]) -> Result<Vec<u64>, RpcError> {
        let keys: Vec<String> = addrs.iter().take(128).map(|a| a.to_string()).collect();
        let (v, _) = self.call_background("getRecentPrioritizationFees", json!([keys])).await?;
        let arr = v.as_array().ok_or_else(|| RpcError::Decode("getRecentPrioritizationFees".into()))?;
        Ok(arr.iter().filter_map(|e| e.get("prioritizationFee")?.as_u64()).collect())
    }

    /// Latest performance sample: (transactions, non-vote transactions, seconds).
    pub async fn get_recent_performance_sample(&self) -> Result<Option<(u64, u64, u64)>, RpcError> {
        let (v, _) = self.call_background("getRecentPerformanceSamples", json!([1])).await?;
        let s = v.as_array().and_then(|a| a.first());
        Ok(s.and_then(|s| {
            let g = |k: &str| s.get(k).and_then(Value::as_u64);
            Some((g("numTransactions")?, g("numNonVoteTransactions").unwrap_or(0), g("samplePeriodSecs")?))
        }))
    }

    /// `simulateTransaction` with `sigVerify=false` + `replaceRecentBlockhash`.
    /// Uses its own (slower) bucket so simulations cannot starve the feed.
    pub async fn simulate(&self, tx_base64: &str, watch: &[Address]) -> Result<SimulateOutcome, RpcError> {
        self.sim_limiter.acquire().await;
        let mut cfg = json!({
            "encoding": "base64",
            "sigVerify": false,
            "replaceRecentBlockhash": true,
            "commitment": "processed",
            "innerInstructions": false,
        });
        if !watch.is_empty() {
            cfg["accounts"] =
                json!({"addresses": watch.iter().map(|a| a.to_string()).collect::<Vec<_>>(), "encoding": "base64"});
        }
        let (v, ms) = self.call("simulateTransaction", json!([tx_base64, cfg])).await?;
        Ok(parse_simulate(&v, ms))
    }

    /// Simulate a *signed* transaction exactly as it would be sent:
    /// `sigVerify=true`, no blockhash replacement. Used as the final gate
    /// before sending.
    pub async fn simulate_signed(&self, tx_base64: &str) -> Result<SimulateOutcome, RpcError> {
        self.sim_limiter.acquire().await;
        let cfg = json!({
            "encoding": "base64",
            "sigVerify": true,
            "replaceRecentBlockhash": false,
            "commitment": "processed",
        });
        let (v, ms) = self.call("simulateTransaction", json!([tx_base64, cfg])).await?;
        Ok(parse_simulate(&v, ms))
    }

    pub async fn recent_prioritization_fees(&self, accounts: &[Address]) -> Result<Vec<(u64, u64)>, RpcError> {
        let keys: Vec<String> = accounts.iter().take(128).map(|a| a.to_string()).collect();
        let (v, _) = self.call("getRecentPrioritizationFees", json!([keys])).await?;
        let arr = v.as_array().ok_or_else(|| RpcError::Decode("getRecentPrioritizationFees".into()))?;
        Ok(arr.iter().filter_map(|e| Some((e.get("slot")?.as_u64()?, e.get("prioritizationFee")?.as_u64()?))).collect())
    }
}

pub fn parse_simulate(v: &Value, latency_ms: u32) -> SimulateOutcome {
    let val = v.get("value").cloned().unwrap_or(Value::Null);
    let u64s = |k: &str| -> Option<Vec<u64>> {
        val.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect())
    };
    SimulateOutcome {
        context_slot: v.get("context").and_then(|c| c.get("slot")).and_then(Value::as_u64),
        err: val.get("err").filter(|e| !e.is_null()).cloned(),
        logs: val
            .get("logs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        units_consumed: val.get("unitsConsumed").and_then(Value::as_u64),
        fee: val.get("fee").and_then(Value::as_u64),
        pre_balances: u64s("preBalances"),
        post_balances: u64s("postBalances"),
        post_account_lamports: val
            .get("accounts")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|acc| acc.get("lamports").and_then(Value::as_u64)).collect())
            .unwrap_or_default(),
        latency_ms,
    }
}

/// Strip anything that looks like a URL (may contain an API key) from errors.
fn redact(s: &str) -> String {
    s.split_whitespace().map(|w| if w.contains("://") { "<url>" } else { w }).collect::<Vec<_>>().join(" ")
}

/// Wall-clock helper for callers.
pub fn now() -> Ts {
    Ts::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simulation_success_and_failure() {
        let ok = json!({"context": {"slot": 448000608u64}, "value": {
            "err": null, "logs": ["Program log: a", "Program log: b"], "unitsConsumed": 183412,
            "fee": 5000, "preBalances": [10, 20], "postBalances": [9, 20],
            "accounts": [{"lamports": 9, "data": ["", "base64"]}]
        }});
        let s = parse_simulate(&ok, 84);
        assert_eq!(s.context_slot, Some(448_000_608));
        assert!(s.err.is_none());
        assert_eq!(s.units_consumed, Some(183_412));
        assert_eq!(s.fee, Some(5000));
        assert_eq!(s.pre_balances.as_deref(), Some(&[10u64, 20][..]));
        assert_eq!(s.post_account_lamports, vec![Some(9)]);
        assert_eq!(s.logs.len(), 2);

        let fail = json!({"context": {"slot": 1}, "value": {
            "err": {"InstructionError": [3, {"Custom": 6001}]}, "logs": ["Program log: Error: SlippageToleranceExceeded"],
            "unitsConsumed": 91000, "accounts": null
        }});
        let s = parse_simulate(&fail, 10);
        assert!(s.err.is_some());
        assert!(s.pre_balances.is_none());
        assert!(s.post_account_lamports.is_empty());
    }

    #[test]
    fn redacts_urls() {
        assert_eq!(
            redact("error sending request for url (https://x.io/?api-key=abc)"),
            "error sending request for url <url>"
        );
    }
}
