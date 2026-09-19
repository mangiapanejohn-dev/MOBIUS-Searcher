//! OKX public market data for the Markets page: the pair's ticker (24h
//! stats), candles of the selected bar, last trades and a ticker strip. Host,
//! markets and watchlist come from the `[venues.<name>]` of kind okx.
//! Display only: never used for trading decisions and never recorded. Polled
//! over the public REST API on a background thread, through the same proxy
//! as the chain WebSocket.

use parking_lot::RwLock;
use searcher_core::Ts;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Where the Markets page gets OKX data (an enabled `[venues.*]` of kind okx).
#[derive(Clone, Debug, PartialEq)]
pub struct OkxSource {
    /// REST host, e.g. `https://www.okx.com` (regional hosts work too).
    pub rest_url: String,
    /// Instruments the page cycles through (`p`), first = default.
    pub markets: Vec<String>,
    /// Instruments of the ticker strip.
    pub watchlist: Vec<String>,
}

impl Default for OkxSource {
    /// The built-in OKX venue.
    fn default() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        Self {
            rest_url: "https://www.okx.com".into(),
            markets: v(&["SOL-USDT", "SOL-USDC", "SOL-USD"]),
            watchlist: v(&["BTC-USDT", "ETH-USDT", "SOL-USDT", "XRP-USDT", "JUP-USDT", "USDC-USDT"]),
        }
    }
}

impl OkxSource {
    fn market_api(&self) -> String {
        format!("{}/api/v5/market", self.rest_url.trim_end_matches('/'))
    }
}

/// Candle bars: label, OKX `bar` parameter, length in µs. Daily candles open
/// at 00:00 UTC, the same day boundary as the 24h change.
pub const BARS: [(&str, &str, i64); 7] = [
    ("1s", "1s", 1_000_000),
    ("1m", "1m", 60_000_000),
    ("5m", "5m", 300_000_000),
    ("15m", "15m", 900_000_000),
    ("1h", "1H", 3_600_000_000),
    ("4h", "4H", 14_400_000_000),
    ("1D", "1Dutc", 86_400_000_000),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ticker {
    pub inst: String,
    pub last: f64,
    /// Price at 00:00 UTC (OKX's reference for the daily change).
    pub sod_utc0: f64,
    pub high24h: f64,
    pub low24h: f64,
    /// 24h volume in the base currency.
    pub vol24h: f64,
    /// 24h turnover in the quote currency.
    pub vol_ccy24h: f64,
    pub ts: Ts,
}

impl Ticker {
    /// Change since 00:00 UTC as `(absolute, percent)`.
    pub fn change(&self) -> (f64, f64) {
        let d = self.last - self.sod_utc0;
        (d, if self.sod_utc0 > 0.0 { d / self.sod_utc0 * 100.0 } else { 0.0 })
    }

    pub fn base(&self) -> &str {
        self.inst.split('-').next().unwrap_or("")
    }

    pub fn quote(&self) -> &str {
        self.inst.split('-').nth(1).unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candle {
    pub start: Ts,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Base-currency volume (0 when the source has none).
    pub vol: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trade {
    pub ts: Ts,
    pub px: f64,
    pub sz: f64,
    pub buy: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CexState {
    pub ticker: Option<Ticker>,
    /// Oldest first; belong to `(pair, bar)` = `candles_for`.
    pub candles: Vec<Candle>,
    pub candles_for: Option<(usize, usize)>,
    /// Newest first; belong to pair `trades_for`.
    pub trades: Vec<Trade>,
    pub trades_for: Option<usize>,
    pub strip: Vec<Ticker>,
    pub last_ok: Option<Ts>,
    pub error: Option<String>,
}

/// Handle to the poller; dropping it stops the thread.
pub struct Cex {
    pub source: OkxSource,
    pub state: Arc<RwLock<CexState>>,
    pair: Arc<AtomicUsize>,
    bar: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Cex {
    /// Start polling `source` on a background thread.
    pub fn start(source: OkxSource, pair: usize, bar: usize) -> Cex {
        let cex = Cex::fixed(source, CexState::default());
        cex.select(pair, bar);
        let (src, state, p, b, stop) =
            (cex.source.clone(), cex.state.clone(), cex.pair.clone(), cex.bar.clone(), cex.stop.clone());
        let _ = std::thread::Builder::new().name("okx".into()).spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                rt.block_on(poll(src, state, p, b, stop));
            }
        });
        cex
    }

    /// Fixed data and no polling (tests, snapshots).
    pub fn fixed(source: OkxSource, state: CexState) -> Cex {
        Cex {
            source,
            state: Arc::new(RwLock::new(state)),
            pair: Arc::new(AtomicUsize::new(0)),
            bar: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn select(&self, pair: usize, bar: usize) {
        self.pair.store(pair, Ordering::Relaxed);
        self.bar.store(bar, Ordering::Relaxed);
    }
}

impl Drop for Cex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

async fn poll(
    source: OkxSource,
    state: Arc<RwLock<CexState>>,
    pair: Arc<AtomicUsize>,
    bar: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) {
    let base = source.market_api();
    let markets = &source.markets;
    // Same proxy as the chain WebSocket: the env vars, else the macOS system
    // proxy (reqwest's own system lookup went direct here and timed out).
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(8));
    if let Some(p) = searcher_market::feed::proxy_for("wss://www.okx.com")
        .and_then(|(h, p)| reqwest::Proxy::https(format!("http://{h}:{p}")).ok())
    {
        builder = builder.proxy(p);
    }
    let Ok(client) = builder.build() else { return };
    let get = |url: String| {
        let client = client.clone();
        async move {
            let body = client.get(url).send().await.map_err(|e| format!("okx: {e}"))?.text().await;
            body.map_err(|e| format!("okx: {e}"))
        }
    };
    let record = |r: Result<(), String>| {
        let mut s = state.write();
        match r {
            Ok(()) => {
                s.last_ok = Some(Ts::now());
                s.error = None;
            }
            Err(e) => s.error = Some(e),
        }
    };
    // Each stream refetches on its interval, or at once when the selection it
    // depends on changes. `due()`: None = stop, Some(None) = not yet.
    let every = |period: Duration, key: fn(usize, usize) -> (usize, usize)| {
        let (pair, bar, stop) = (pair.clone(), bar.clone(), stop.clone());
        let mut last: Option<(Instant, (usize, usize))> = None;
        move || -> Option<Option<(usize, usize)>> {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let k = key(pair.load(Ordering::Relaxed), bar.load(Ordering::Relaxed));
            if last.is_some_and(|(t, lk)| lk == k && t.elapsed() < period) {
                return Some(None);
            }
            last = Some((Instant::now(), k));
            Some(Some(k))
        }
    };
    let tick = Duration::from_millis(100);
    let ticker = async {
        let mut due = every(Duration::from_secs(1), |p, _| (p, 0));
        while let Some(k) = due() {
            if let Some(inst) = k.and_then(|(p, _)| markets.get(p)) {
                let r = get(format!("{base}/ticker?instId={inst}")).await.and_then(|b| parse_ticker(&b));
                record(r.map(|t| state.write().ticker = Some(t)));
            }
            tokio::time::sleep(tick).await;
        }
    };
    // Candles: the full history once per selection, then only the newest few
    // merged in (the full reload repeats when a gap appears).
    let candles = async {
        let mut due = every(Duration::from_secs(1), |p, b| (p, b));
        let mut loaded = None;
        while let Some(k) = due() {
            if let Some((p, b, inst, (_, param, bar_us))) =
                k.and_then(|(p, b)| Some((p, b, markets.get(p)?, BARS.get(b)?)))
            {
                let full = loaded != Some((p, b));
                let limit = if full { HISTORY } else { 5 };
                let r = get(format!("{base}/candles?instId={inst}&bar={param}&limit={limit}")).await;
                let r = r.and_then(|b| parse_candles(&b)).map(|c| {
                    let mut s = state.write();
                    if full {
                        s.candles = c;
                        s.candles_for = Some((p, b));
                        true
                    } else {
                        s.candles_for == Some((p, b)) && merge_candles(&mut s.candles, c, *bar_us)
                    }
                });
                loaded = match r {
                    Ok(true) => Some((p, b)),
                    Ok(false) => None,
                    Err(_) => loaded,
                };
                record(r.map(drop));
            }
            tokio::time::sleep(tick).await;
        }
    };
    let trades = async {
        let mut due = every(Duration::from_secs(2), |p, _| (p, 0));
        while let Some(k) = due() {
            if let Some((p, inst)) = k.and_then(|(p, _)| Some((p, markets.get(p)?))) {
                let r = get(format!("{base}/trades?instId={inst}&limit=40")).await;
                record(r.and_then(|b| parse_trades(&b)).map(|t| {
                    let mut s = state.write();
                    s.trades = t;
                    s.trades_for = Some(p);
                }));
            }
            tokio::time::sleep(tick).await;
        }
    };
    let strip = async {
        let mut due = every(Duration::from_secs(10), |_, _| (0, 0));
        while let Some(k) = due() {
            if k.is_some() {
                let all = futures_util::future::join_all(
                    source.watchlist.iter().map(|inst| get(format!("{base}/ticker?instId={inst}"))),
                )
                .await;
                let mut out = Vec::new();
                for r in all.into_iter().map(|r| r.and_then(|b| parse_ticker(&b))) {
                    match r {
                        Ok(t) => out.push(t),
                        Err(e) => record(Err(e)),
                    }
                }
                if !out.is_empty() {
                    state.write().strip = out;
                }
            }
            tokio::time::sleep(tick).await;
        }
    };
    tokio::join!(ticker, candles, trades, strip);
}

/// Candles kept per selection (OKX's maximum per request).
const HISTORY: usize = 300;

/// Merge newer candles (oldest first) into `into`, replacing ones with the
/// same start. `false` when they do not connect to what is there (a gap):
/// the caller reloads the full history.
pub fn merge_candles(into: &mut Vec<Candle>, newer: Vec<Candle>, bar_us: i64) -> bool {
    let (Some(last), Some(first)) = (into.last(), newer.first()) else { return newer.is_empty() };
    if first.start.0 > last.start.0 + bar_us {
        return false;
    }
    for c in newer {
        match into.binary_search_by_key(&c.start, |x| x.start) {
            Ok(i) => into[i] = c,
            Err(i) if i == into.len() => into.push(c),
            Err(_) => {}
        }
    }
    let excess = into.len().saturating_sub(HISTORY);
    into.drain(..excess);
    true
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0.0)
}

fn ms(s: Option<&Value>) -> Ts {
    Ts(s.and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0) * 1_000)
}

/// The `data` array of an OKX response (`code` must be "0").
fn data(body: &str) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("okx: bad response ({e})"))?;
    if v.get("code").and_then(Value::as_str) != Some("0") {
        return Err(format!("okx: {}", v.get("msg").and_then(Value::as_str).unwrap_or("error")));
    }
    Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
}

pub fn parse_ticker(body: &str) -> Result<Ticker, String> {
    let d = data(body)?;
    let t = d.first().ok_or("okx: empty ticker")?;
    Ok(Ticker {
        inst: t.get("instId").and_then(Value::as_str).unwrap_or_default().to_string(),
        last: num(t, "last"),
        sod_utc0: num(t, "sodUtc0"),
        high24h: num(t, "high24h"),
        low24h: num(t, "low24h"),
        vol24h: num(t, "vol24h"),
        vol_ccy24h: num(t, "volCcy24h"),
        ts: ms(t.get("ts")),
    })
}

/// Candles oldest first (OKX sends newest first).
pub fn parse_candles(body: &str) -> Result<Vec<Candle>, String> {
    let f = |r: &[Value], i: usize| r.get(i).and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let mut out: Vec<Candle> = data(body)?
        .iter()
        .filter_map(|r| r.as_array())
        .map(|r| Candle {
            start: ms(r.first()),
            open: f(r, 1),
            high: f(r, 2),
            low: f(r, 3),
            close: f(r, 4),
            vol: f(r, 5),
        })
        .collect();
    out.reverse();
    Ok(out)
}

/// Trades newest first.
pub fn parse_trades(body: &str) -> Result<Vec<Trade>, String> {
    Ok(data(body)?
        .iter()
        .map(|t| Trade {
            ts: ms(t.get("ts")),
            px: num(t, "px"),
            sz: num(t, "sz"),
            buy: t.get("side").and_then(Value::as_str) == Some("buy"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live OKX smoke test: `cargo test -p searcher-tui live_okx -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_okx() {
        let cex = Cex::start(OkxSource::default(), 0, 1);
        let t0 = Instant::now();
        let mut first = [None; 4];
        while t0.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(100));
            let s = cex.state.read();
            let have = [s.ticker.is_some(), s.candles_for.is_some(), !s.trades.is_empty(), !s.strip.is_empty()];
            for (f, h) in first.iter_mut().zip(have) {
                if h && f.is_none() {
                    *f = Some(t0.elapsed());
                }
            }
            if first.iter().all(Option::is_some) {
                break;
            }
        }
        println!("first ticker / candles / trades / strip: {first:?}");
        let s = cex.state.read();
        println!("after {:?}: ticker {:?}\ncandles {} error {:?}", t0.elapsed(), s.ticker, s.candles.len(), s.error);
        assert_eq!(s.ticker.as_ref().map(|t| t.inst.as_str()), Some("SOL-USDT"));
        assert_eq!((s.candles_for, s.candles.len()), (Some((0, 1)), 300));
        assert!(s.candles.windows(2).all(|w| w[1].start.0 - w[0].start.0 == 60_000_000), "1m candles, oldest first");
        assert!(!s.trades.is_empty() && s.strip.len() == OkxSource::default().watchlist.len());
    }

    #[test]
    fn ticker_parsing_and_daily_change() {
        let body = r#"{"code":"0","data":[{"instType":"SPOT","instId":"SOL-USD","last":"111.92","lastSz":"0.1","askPx":"111.9","askSz":"16.8","bidPx":"111.88","bidSz":"2.7","open24h":"106.22","high24h":"114.31","low24h":"105.33","volCcy24h":"3351763.45","vol24h":"30238.87","ts":"1789812710466","sodUtc0":"112.74","sodUtc8":"111.19"}],"msg":""}"#;
        let t = parse_ticker(body).unwrap();
        assert_eq!((t.base(), t.quote()), ("SOL", "USD"));
        assert_eq!((t.high24h, t.low24h, t.vol24h), (114.31, 105.33, 30238.87));
        assert_eq!(t.ts, Ts(1_789_812_710_466_000));
        let (d, p) = t.change();
        assert!((d + 0.82).abs() < 1e-9 && (p + 0.7273).abs() < 1e-3, "OKX shows -0.82 (-0.73%): {d} {p}");
    }

    #[test]
    fn merging_updates_the_live_candle_appends_new_ones_and_detects_gaps() {
        let c =
            |m: i64, close: f64| Candle { start: Ts(m * 60_000_000), open: 1.0, high: 2.0, low: 0.5, close, vol: 1.0 };
        let mut v: Vec<Candle> = (0..HISTORY as i64).map(|m| c(m, 1.0)).collect();
        let last = HISTORY as i64 - 1;
        assert!(merge_candles(&mut v, vec![c(last - 1, 1.1), c(last, 1.2), c(last + 1, 1.3)], 60_000_000));
        assert_eq!(v.len(), HISTORY, "capped");
        assert_eq!((v[HISTORY - 2].close, v[HISTORY - 1].close), (1.2, 1.3));
        assert_eq!(v[0].start, Ts(60_000_000), "oldest dropped");
        assert!(!merge_candles(&mut v, vec![c(last + 3, 9.0)], 60_000_000), "gap → reload");
        assert_eq!(v.last().unwrap().close, 1.3);
    }

    #[test]
    fn candles_come_back_oldest_first() {
        let body = r#"{"code":"0","data":[["1789812660000","111.89","111.93","111.89","111.93","291.99","32678.8","32678.8","0"],["1789812600000","111.88","111.89","111.86","111.88","22.22","2485.9","2485.9","1"]],"msg":""}"#;
        let c = parse_candles(body).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].start, Ts(1_789_812_600_000_000));
        assert_eq!((c[1].open, c[1].high, c[1].low, c[1].close, c[1].vol), (111.89, 111.93, 111.89, 111.93, 291.99));
    }

    #[test]
    fn trades_and_errors() {
        let body = r#"{"code":"0","data":[{"instId":"SOL-USDT","tradeId":"1","px":"111.9","sz":"5.36","side":"buy","ts":"1789812711708"},{"instId":"SOL-USDT","tradeId":"0","px":"111.89","sz":"1","side":"sell","ts":"1789812711000"}],"msg":""}"#;
        let t = parse_trades(body).unwrap();
        assert_eq!(t[0], Trade { ts: Ts(1_789_812_711_708_000), px: 111.9, sz: 5.36, buy: true });
        assert!(!t[1].buy);
        assert_eq!(
            parse_ticker(r#"{"code":"51001","data":[],"msg":"Instrument ID does not exist"}"#).unwrap_err(),
            "okx: Instrument ID does not exist"
        );
        assert!(parse_candles("<html>").is_err());
    }
}
