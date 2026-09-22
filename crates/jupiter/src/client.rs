//! Jupiter Swap API V2 client. One call = one HTTP request; rate limiting is
//! enforced before the request and 429s put the shared bucket into backoff.

use crate::adapter::{self, LegContext};
use crate::wire::{BuildResponse, ErrorBody, PriceEntry};
use searcher_core::ix::LegInstructions;
use searcher_core::model::{DexFilter, Leg, RoutingMode, ServiceId, SlippageSpec};
use searcher_core::{Address, Ts, UsdPrice};
use searcher_telemetry::{Limiter, LimiterConfig, RateLimiter, Stopwatch, Telemetry};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// API key wrapper that never prints.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(k: String) -> Option<Self> {
        let k = k.trim().to_string();
        (!k.is_empty()).then_some(ApiKey(k))
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JupiterError {
    #[error("rate limited (429); backing off {0:?}")]
    RateLimited(Duration),
    #[error("no route: {0}")]
    NoRoute(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("auth/permission error {0}: {1}")]
    Auth(u16, String),
    #[error("http {0}: {1}")]
    Http(u16, String),
    #[error("timeout")]
    Timeout,
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
}

impl JupiterError {
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, JupiterError::RateLimited(_))
    }
}

#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub input_mint: Address,
    pub output_mint: Address,
    pub amount: u64,
    pub taker: Address,
    pub slippage: SlippageSpec,
    pub mode: RoutingMode,
    pub dex_filter: DexFilter,
    pub cu_price_percentile: String,
    pub max_accounts: Option<u8>,
    pub blockhash_slots_to_expiry: u16,
    pub for_jito_bundle: bool,
}

impl BuildRequest {
    /// Identity of the request: two equal keys ask Jupiter the same question.
    pub fn key(&self) -> String {
        let q = self.query();
        q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
    }

    /// Query parameters exactly as documented for `/swap/v2/build`.
    pub fn query(&self) -> Vec<(&'static str, String)> {
        let mut q = vec![
            ("inputMint", self.input_mint.to_string()),
            ("outputMint", self.output_mint.to_string()),
            ("amount", self.amount.to_string()),
            ("taker", self.taker.to_string()),
            (
                "slippageBps",
                match self.slippage {
                    SlippageSpec::Rtse => "rtse".to_string(),
                    SlippageSpec::Fixed(b) => b.to_string(),
                },
            ),
            ("computeUnitPricePercentile", self.cu_price_percentile.clone()),
            ("blockhashSlotsToExpiry", self.blockhash_slots_to_expiry.to_string()),
            ("wrapAndUnwrapSol", "true".into()),
        ];
        if self.mode == RoutingMode::Fast {
            q.push(("mode", "fast".into()));
        }
        match &self.dex_filter {
            DexFilter::Any => {}
            DexFilter::Only(d) => q.push(("dexes", d.join(","))),
            DexFilter::Exclude(d) => q.push(("excludeDexes", d.join(","))),
        }
        if let Some(m) = self.max_accounts {
            q.push(("maxAccounts", m.to_string()));
        }
        if self.for_jito_bundle {
            q.push(("forJitoBundle", "true".into()));
        }
        q
    }
}

#[derive(Clone, Debug)]
pub struct BuiltLeg {
    pub leg: Leg,
    pub instructions: LegInstructions,
    /// Raw response body, persisted as part of the opportunity snapshot.
    pub raw: String,
    pub timing: QuoteTiming,
}

/// When a quote was requested and received (monotonic + wall clock), and
/// what chain state Jupiter built it against.
#[derive(Clone, Copy, Debug)]
pub struct QuoteTiming {
    /// Rate-limit permission granted; request handed to the HTTP client.
    pub sent: Instant,
    pub sent_ts: Ts,
    /// Response body fully received.
    pub received: Instant,
    pub received_ts: Ts,
    /// Waited for rate-limit permission before sending.
    pub token_wait: Duration,
    /// JSON decode + domain adaptation.
    pub parse: Duration,
    /// Block height Jupiter's blockhash was fetched at
    /// (`lastValidBlockHeight − blockhashSlotsToExpiry`).
    pub source_block_height: Option<u64>,
}

/// Gateway rate-limit window as last reported (`x-ratelimit-*` headers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerWindow {
    pub current: i64,
    pub remaining: i64,
    /// Unix seconds when the oldest in-window request ages out.
    pub reset_unix_s: i64,
    pub seen_at: Ts,
}

/// Timing of one HTTP exchange.
struct Exchange {
    body: String,
    request_id: Option<String>,
    ms: u32,
    sent: Instant,
    sent_ts: Ts,
    received: Instant,
    token_wait: Duration,
}

pub struct JupiterClient {
    http: reqwest::Client,
    base_url: String,
    key: Option<ApiKey>,
    limiter: Arc<Limiter>,
    telemetry: Arc<Telemetry>,
    window: parking_lot::Mutex<Option<ServerWindow>>,
    /// Wall-clock send times of recent requests (window-length estimation).
    recent_sends: parking_lot::Mutex<std::collections::VecDeque<Ts>>,
}

impl JupiterClient {
    /// Token-bucket client (round-robin scanner, tests).
    pub fn new(
        base_url: &str,
        key: Option<ApiKey>,
        limiter_cfg: LimiterConfig,
        timeout: Duration,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, JupiterError> {
        Self::with_limiter(
            base_url,
            key,
            Limiter::Bucket(RateLimiter::new("jupiter.general", limiter_cfg)),
            timeout,
            telemetry,
        )
    }

    /// Client enforcing `limiter` before every request. The connection is
    /// kept warm (HTTP/2 pings, long idle timeout): an event-driven scheduler
    /// can be quiet for minutes, and a new TLS connection costs a round trip
    /// or two (0.3–0.7 s measured through the local proxy).
    pub fn with_limiter(
        base_url: &str,
        key: Option<ApiKey>,
        limiter: Limiter,
        timeout: Duration,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, JupiterError> {
        let mut b = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("mobius-searcher/", env!("CARGO_PKG_VERSION")))
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(30))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true);
        // no HTTPS_PROXY in the environment (Ghostty, the app's terminal): use the OS proxy
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy_for(base_url) {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| JupiterError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() || searcher_telemetry::proxy::bypass_proxy_for(base_url) {
            b = b.no_proxy();
        }
        let http = b.build().map_err(|e| JupiterError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            key,
            limiter: Arc::new(limiter),
            telemetry,
            window: parking_lot::Mutex::new(None),
            recent_sends: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// Latest gateway window report, if any response carried one.
    pub fn server_window(&self) -> Option<ServerWindow> {
        *self.window.lock()
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    pub fn limiter(&self) -> &Arc<Limiter> {
        &self.limiter
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<(String, Option<String>, u32), JupiterError> {
        self.exchange(path, query, None).await.map(|x| (x.body, x.request_id, x.ms))
    }

    /// One request. `slot = Some(waited)`: the caller already took a limiter
    /// slot (after waiting `waited`); otherwise wait for one here.
    async fn exchange(
        &self,
        path: &str,
        query: &[(&str, String)],
        slot: Option<Duration>,
    ) -> Result<Exchange, JupiterError> {
        let token_wait = match slot {
            Some(waited) => waited,
            None => {
                let asked = Instant::now();
                self.limiter.acquire().await;
                asked.elapsed()
            }
        };
        let lat = &self.telemetry.latency;
        lat.duration_us("jupiter.token_wait_us", token_wait);
        lat.count("jupiter.requests", 1);
        let sent = Instant::now();
        let sent_ts = Ts::now();
        {
            let mut r = self.recent_sends.lock();
            r.push_back(sent_ts);
            while r.len() > 64 {
                r.pop_front();
            }
        }
        let sw = Stopwatch::start();
        let mut req = self.http.get(format!("{}{}", self.base_url, path)).query(query);
        if let Some(k) = &self.key {
            req = req.header("x-api-key", &k.0);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let err = if e.is_timeout() { JupiterError::Timeout } else { JupiterError::Transport(e.to_string()) };
                self.telemetry.record_err(ServiceId::Jupiter, Some(sw.ms()), err.to_string());
                return Err(err);
            }
        };
        lat.since_us("jupiter.ttfb_us", sent);
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let num = |n: &str| header(n).and_then(|v| v.parse::<i64>().ok());
        if let Some(rem) = num("x-ratelimit-remaining") {
            self.telemetry.set_quota(ServiceId::Jupiter, rem);
        }
        if let (Some(current), Some(remaining), Some(reset)) =
            (num("x-ratelimit-current"), num("x-ratelimit-remaining"), num("x-ratelimit-reset"))
        {
            let reset_in = Duration::from_millis((reset * 1000 - Ts::now().0 / 1000).clamp(0, 120_000) as u64);
            // Window length as the gateway applies it: the oldest request it
            // still counts is our `current`-th most recent send (when we are
            // its only client); it ages out at `reset` (1 s resolution).
            if current > 0 {
                let r = self.recent_sends.lock();
                if let Some(oldest) = r.iter().rev().nth(current as usize - 1) {
                    lat.value("jupiter.window_est_ms", reset * 1000 - oldest.0 / 1000);
                }
            }
            self.limiter.on_server_report(sent, Instant::now(), current, remaining, reset_in);
            *self.window.lock() = Some(ServerWindow { current, remaining, reset_unix_s: reset, seen_at: Ts::now() });
            lat.value("jupiter.window_current", current);
            lat.value("jupiter.window_capacity", current + remaining);
        }
        let request_id = header("x-api-gateway-request-id");
        let body = resp.text().await.map_err(|e| JupiterError::Transport(e.to_string()))?;
        let received = Instant::now();
        let ms = sw.ms();
        lat.duration_us("jupiter.http_us", received - sent);

        if status == 429 {
            // x-ratelimit-reset = unix seconds when a slot frees; use as a floor.
            let hint = header("x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok()).map(|reset| {
                let now_s = Ts::now().micros() / 1_000_000;
                Duration::from_secs((reset - now_s).clamp(0, 120) as u64)
            });
            let d = self.limiter.on_rate_limited(Instant::now(), hint);
            self.telemetry.record_rate_limited(ServiceId::Jupiter, d.as_millis() as u64);
            lat.count("jupiter.429", 1);
            return Err(JupiterError::RateLimited(d));
        }
        self.limiter.on_success();
        if (200..300).contains(&status) {
            self.telemetry.record_ok(ServiceId::Jupiter, ms);
            return Ok(Exchange { body, request_id, ms, sent, sent_ts, received, token_wait });
        }
        let msg = serde_json::from_str::<ErrorBody>(&body).map(|e| e.text()).unwrap_or_else(|_| truncate(&body, 300));
        let err = match status {
            400 if msg.to_ascii_lowercase().contains("no route") => JupiterError::NoRoute(msg),
            // The gateway wraps upstream outages in a 400; they are transient, not our request.
            400 if msg.contains("upstream") || msg.starts_with("503") || msg.starts_with("502") => {
                JupiterError::Http(503, msg)
            }
            400 | 422 => JupiterError::BadRequest(msg),
            401 | 403 => JupiterError::Auth(status, msg),
            _ => JupiterError::Http(status, msg),
        };
        // "no route" is a market fact, not a service failure.
        if matches!(err, JupiterError::NoRoute(_)) {
            self.telemetry.record_ok(ServiceId::Jupiter, ms);
        } else {
            self.telemetry.record_err(ServiceId::Jupiter, Some(ms), err.to_string());
        }
        Err(err)
    }

    /// `GET /swap/v2/build` → domain leg + instructions.
    pub async fn build(&self, req: &BuildRequest, index: u8) -> Result<BuiltLeg, JupiterError> {
        self.build_inner(req, index, None).await
    }

    /// `/build` for a caller that already holds a limiter slot (the
    /// event-driven scheduler acquires before dispatching).
    pub async fn build_with_slot(
        &self,
        req: &BuildRequest,
        index: u8,
        waited: Duration,
    ) -> Result<BuiltLeg, JupiterError> {
        self.build_inner(req, index, Some(waited)).await
    }

    async fn build_inner(
        &self,
        req: &BuildRequest,
        index: u8,
        slot: Option<Duration>,
    ) -> Result<BuiltLeg, JupiterError> {
        let x = self.exchange("/swap/v2/build", &req.query(), slot).await?;
        let (body, request_id, ms) = (x.body, x.request_id, x.ms);
        let parse_start = Instant::now();
        // A quote reflects the market after it was sent, never before: the
        // rate-limit wait is not part of its age.
        let quoted_at = x.sent_ts;
        let wire: BuildResponse =
            serde_json::from_str(&body).map_err(|e| JupiterError::Decode(format!("/build: {e}")))?;
        let (leg, instructions) = adapter::to_leg(
            &wire,
            LegContext {
                index,
                slippage_spec: req.slippage,
                mode: req.mode,
                dex_filter: req.dex_filter.clone(),
                quoted_at,
                latency_ms: ms,
                request_id,
                expect_input: req.input_mint,
                expect_output: req.output_mint,
            },
        )
        .map_err(|e| JupiterError::Decode(e.to_string()))?;
        let parse = parse_start.elapsed();
        self.telemetry.latency.duration_us("jupiter.parse_us", parse);
        let timing = QuoteTiming {
            sent: x.sent,
            sent_ts: x.sent_ts,
            received: x.received,
            received_ts: Ts(x.sent_ts.0 + (x.received - x.sent).as_micros() as i64),
            token_wait: x.token_wait,
            parse,
            source_block_height: leg.last_valid_block_height.checked_sub(req.blockhash_slots_to_expiry as u64),
        };
        Ok(BuiltLeg { leg, instructions, raw: body, timing })
    }

    /// `GET /swap/v2/program-id-to-label` → valid DEX labels.
    pub async fn dex_labels(&self) -> Result<BTreeMap<String, String>, JupiterError> {
        let (body, _, _) = self.get("/swap/v2/program-id-to-label", &[]).await?;
        serde_json::from_str(&body).map_err(|e| JupiterError::Decode(format!("labels: {e}")))
    }

    /// `GET /price/v3?ids=` → USD reference prices (non-executable).
    pub async fn prices(&self, mints: &[Address]) -> Result<BTreeMap<Address, UsdPrice>, JupiterError> {
        let ids = mints.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(",");
        let (body, _, _) = self.get("/price/v3", &[("ids", ids)]).await?;
        let raw: BTreeMap<String, PriceEntry> =
            serde_json::from_str(&body).map_err(|e| JupiterError::Decode(format!("price: {e}")))?;
        Ok(raw
            .into_iter()
            .filter_map(|(k, v)| {
                let a: Address = k.parse().ok()?;
                // external float → micro-USD at the adapter edge
                (v.usd_price.is_finite() && v.usd_price > 0.0)
                    .then(|| (a, UsdPrice::new((v.usd_price * 1e6).round() as u64)))
            })
            .collect())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..s.floor_char_boundary(n)]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::address::well_known;

    #[test]
    fn query_matches_documented_params() {
        let r = BuildRequest {
            input_mint: well_known::addr(well_known::WSOL_MINT),
            output_mint: well_known::addr(well_known::USDC_MINT),
            amount: 1_000_000_000,
            taker: Address([9; 32]),
            slippage: SlippageSpec::Rtse,
            mode: RoutingMode::Fast,
            dex_filter: DexFilter::Only(vec!["Raydium CLMM".into(), "Meteora DLMM".into()]),
            cu_price_percentile: "high".into(),
            max_accounts: Some(30),
            blockhash_slots_to_expiry: 150,
            for_jito_bundle: true,
        };
        let q: BTreeMap<_, _> = r.query().into_iter().collect();
        assert_eq!(q["slippageBps"], "rtse");
        assert_eq!(q["mode"], "fast");
        assert_eq!(q["dexes"], "Raydium CLMM,Meteora DLMM");
        assert_eq!(q["maxAccounts"], "30");
        assert_eq!(q["forJitoBundle"], "true");
        assert!(!q.contains_key("excludeDexes"));
        assert!(!q.contains_key("tipAmount"), "never request Jupiter's own tip");
        let mut r2 = r.clone();
        r2.mode = RoutingMode::Normal;
        r2.dex_filter = DexFilter::Exclude(vec!["Pump.fun".into()]);
        r2.slippage = SlippageSpec::Fixed(12);
        let q: BTreeMap<_, _> = r2.query().into_iter().collect();
        assert!(!q.contains_key("mode") && !q.contains_key("dexes"));
        assert_eq!(q["excludeDexes"], "Pump.fun");
        assert_eq!(q["slippageBps"], "12");
    }

    #[test]
    fn api_key_never_prints() {
        let k = ApiKey::new("jup_secret".into()).unwrap();
        assert_eq!(format!("{k:?}"), "ApiKey(***)");
        assert!(ApiKey::new("  ".into()).is_none());
    }
}
