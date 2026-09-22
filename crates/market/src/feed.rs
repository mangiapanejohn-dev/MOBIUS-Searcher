//! Chain feeds: one WebSocket with `slotSubscribe` plus `accountSubscribe`
//! for watched pool / oracle accounts (reconnect with backoff), a block-height
//! poller, a network-stats poller and a generic text stream (Jito tips). All
//! only *publish*; nothing here blocks on consumers.

use crate::accounts::{PYTH_RECEIVER_PROGRAM, decode_pool, decode_pyth_price_update, pool_program};
use crate::hot::{HotSink, HotTick, OracleSource, OracleUpdate};
use crate::rpc::RpcClient;
use futures_util::{SinkExt, StreamExt};
use searcher_core::config::PoolKind;
use searcher_core::event::{LogLevel, NetworkStats};
use searcher_core::metrics::MetricId;
use searcher_core::model::{MarketSample, SampleSide, ServiceId};
use searcher_core::{Address, Event, Ts, UsdPrice};
use searcher_telemetry::{LatencyBook, Telemetry, backoff_delay};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Latest chain view shared with the pipeline (lock-free reads).
#[derive(Debug, Default)]
pub struct ChainState {
    pub slot: AtomicU64,
    pub slot_ts_us: AtomicU64,
    pub block_height: AtomicU64,
}

impl ChainState {
    pub fn slot(&self) -> Option<u64> {
        let s = self.slot.load(Ordering::Relaxed);
        (s > 0).then_some(s)
    }

    pub fn block_height(&self) -> Option<u64> {
        let s = self.block_height.load(Ordering::Relaxed);
        (s > 0).then_some(s)
    }

    pub fn slot_age_ms(&self, now: Ts) -> Option<u64> {
        let t = self.slot_ts_us.load(Ordering::Relaxed);
        (t > 0).then(|| Ts(t as i64).age_ms(now))
    }

    pub fn observe_slot(&self, slot: u64) -> bool {
        let prev = self.slot.fetch_max(slot, Ordering::Relaxed);
        if slot > prev {
            self.slot_ts_us.store(Ts::now().micros() as u64, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// Proxy discovery lives in `searcher_telemetry::proxy` (shared with the HTTP
// clients); re-exported here for existing callers.
pub use searcher_telemetry::proxy::{parse_scutil_proxy, proxy_for, ws_target};

/// Connect directly, or through an HTTP CONNECT tunnel when a proxy is configured.
async fn connect_ws(url: &str) -> Result<Ws, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some((ph, pp)) = proxy_for(url) else {
        return tokio_tungstenite::connect_async(url).await.map(|(ws, _)| ws).map_err(|e| e.to_string());
    };
    let (th, tp) = ws_target(url).ok_or("bad ws url")?;
    let mut tcp = tokio::net::TcpStream::connect((ph.as_str(), pp)).await.map_err(|e| format!("proxy connect: {e}"))?;
    tcp.write_all(format!("CONNECT {th}:{tp} HTTP/1.1\r\nHost: {th}:{tp}\r\n\r\n").as_bytes())
        .await
        .map_err(|e| format!("proxy write: {e}"))?;
    let mut head = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
        let n = tcp.read(&mut b).await.map_err(|e| format!("proxy read: {e}"))?;
        if n == 0 {
            return Err("proxy closed during CONNECT".into());
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if status.split_whitespace().nth(1).is_none_or(|c| c != "200") {
        return Err(format!("proxy refused CONNECT: {}", status.lines().next().unwrap_or("")));
    }
    tokio_tungstenite::client_async_tls(url, tcp).await.map(|(ws, _)| ws).map_err(|e| e.to_string())
}

/// Connectivity check (`--doctor`): connect (through the configured proxy),
/// optionally send `request`, and wait for the first text message that is
/// not the reply to it. Returns (connect time, time to that message).
pub async fn probe_ws(url: &str, request: Option<&str>, timeout: Duration) -> Result<(Duration, Duration), String> {
    let t0 = Instant::now();
    let mut ws =
        tokio::time::timeout(timeout, connect_ws(url)).await.map_err(|_| "connect timed out".to_string())??;
    let connected = t0.elapsed();
    if let Some(r) = request {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(r.into())).await.map_err(|e| e.to_string())?;
    }
    let deadline = t0 + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => {
                return Err(format!(
                    "connected in {} ms, no message within {} s",
                    connected.as_millis(),
                    timeout.as_secs()
                ));
            }
            Ok(None) => return Err("closed by the server".into()),
            Ok(Some(Err(e))) => return Err(e.to_string()),
            Ok(Some(Ok(m))) => {
                let Ok(text) = m.to_text() else { continue };
                // skip the subscription acknowledgement (`{"result": <id>, "id": …}`)
                let ack = serde_json::from_str::<Value>(text)
                    .is_ok_and(|v| v.get("id").is_some() && v.get("method").is_none());
                if !text.is_empty() && !ack {
                    let _ = ws.close(None).await;
                    return Ok((connected, t0.elapsed()));
                }
            }
        }
    }
}

pub fn parse_slot_notification(v: &Value) -> Option<u64> {
    if v.get("method")?.as_str()? != "slotNotification" {
        return None;
    }
    v.get("params")?.get("result")?.get("slot")?.as_u64()
}

/// One account the chain feed subscribes to.
#[derive(Clone, Debug)]
pub enum Watch {
    /// DEX pool: mid price of `base` in `quote`.
    Pool {
        dex: String,
        kind: PoolKind,
        address: Address,
        pair: String,
        base: Address,
        quote: Address,
        base_decimals: u8,
        quote_decimals: u8,
    },
    /// Pyth `PriceUpdateV2` account for `feed_id`.
    Oracle { symbol: String, address: Address, feed_id: [u8; 32] },
}

impl Watch {
    pub fn address(&self) -> Address {
        match self {
            Watch::Pool { address, .. } | Watch::Oracle { address, .. } => *address,
        }
    }

    fn owner(&self) -> &'static str {
        match self {
            Watch::Pool { kind, .. } => pool_program(*kind),
            Watch::Oracle { .. } => PYTH_RECEIVER_PROGRAM,
        }
    }

    /// Source label shown next to the price.
    pub fn source(&self) -> String {
        match self {
            Watch::Pool { dex, .. } => format!("{dex} pool"),
            Watch::Oracle { .. } => "Pyth".into(),
        }
    }
}

/// Watches for the configured pools and oracles (pairs named `BASE/QUOTE`).
pub fn watches_from_config(
    feeds: &searcher_core::config::FeedsConfig,
    tokens: &searcher_core::token::TokenRegistry,
) -> Result<Vec<Watch>, String> {
    let mut out = Vec::new();
    for p in &feeds.pools {
        let (b, q) = (
            tokens.get(&p.base).ok_or(format!("unknown token {}", p.base))?,
            tokens.get(&p.quote).ok_or(format!("unknown token {}", p.quote))?,
        );
        out.push(Watch::Pool {
            dex: p.dex.clone(),
            kind: p.kind,
            address: p.address.parse().map_err(|e| format!("{}: {e}", p.dex))?,
            pair: format!("{}/{}", b.symbol, q.symbol),
            base: b.mint,
            quote: q.mint,
            base_decimals: b.decimals,
            quote_decimals: q.decimals,
        });
    }
    for o in &feeds.oracles {
        out.push(Watch::Oracle {
            symbol: o.symbol.clone(),
            address: o.address.parse().map_err(|e| format!("{}: {e}", o.symbol))?,
            feed_id: searcher_core::config::parse_feed_id(&o.feed_id).ok_or(format!("{}: bad feed id", o.symbol))?,
        });
    }
    Ok(out)
}

#[derive(Default)]
struct WatchState {
    last_emit: Option<Ts>,
    last_price: Option<f64>,
    /// Latest decoded pool mid (always tracked, emitted or not).
    mid: Option<(Ts, f64)>,
    /// Previous update's receipt, for the update-interval series.
    last_received: Option<Instant>,
    disabled: bool,
}

/// Pool mids older than this do not count towards the pool spread.
const SPREAD_FRESH_US: i64 = 15_000_000;
/// Re-emit an unchanged price at least this often (keeps its age honest).
const HEARTBEAT_US: i64 = 5_000_000;
const SUB_ID_BASE: u64 = 100;

/// When and against which chain state an account update arrived.
#[derive(Clone, Copy, Debug)]
pub struct Receipt {
    pub received: Instant,
    pub now: Ts,
    /// Slot of the account state (notification / RPC context).
    pub slot: Option<u64>,
    /// Chain head (slotSubscribe) seen at receipt.
    pub head_slot: Option<u64>,
}

impl Receipt {
    pub fn at(now: Ts) -> Self {
        Self { received: Instant::now(), now, slot: None, head_slot: None }
    }
}

/// Subscriptions and decoding for the watched accounts: JSON in, events out.
/// Every decoded update also yields an unthrottled [`HotTick`] for the
/// scheduler; UI/recorder samples and the pool spread are throttled to
/// `emit_interval` (an unchanged price is re-emitted on a heartbeat).
pub struct AccountFeed {
    watches: Vec<Watch>,
    state: Vec<WatchState>,
    subs: HashMap<u64, usize>,
    emit_interval_us: i64,
    last_spread: Option<Ts>,
    latency: Option<Arc<LatencyBook>>,
    names: Vec<(Arc<str>, Arc<str>)>,
}

impl AccountFeed {
    pub fn new(watches: Vec<Watch>, emit_interval: Duration) -> Self {
        let state = watches.iter().map(|_| WatchState::default()).collect();
        let names = watches
            .iter()
            .map(|w| match w {
                Watch::Pool { dex, pair, .. } => (Arc::from(dex.as_str()), Arc::from(pair.as_str())),
                Watch::Oracle { symbol, .. } => (Arc::from("Pyth"), Arc::from(symbol.as_str())),
            })
            .collect();
        Self {
            watches,
            state,
            subs: HashMap::new(),
            emit_interval_us: emit_interval.as_micros() as i64,
            last_spread: None,
            latency: None,
            names,
        }
    }

    /// Record decode time, update intervals, slot lag and oracle age.
    pub fn with_latency(mut self, book: Arc<LatencyBook>) -> Self {
        self.latency = Some(book);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.watches.is_empty()
    }

    pub fn addresses(&self) -> Vec<Address> {
        self.watches.iter().map(Watch::address).collect()
    }

    /// `accountSubscribe` requests (request id = 100 + watch index).
    pub fn subscribe_requests(&mut self) -> Vec<String> {
        self.subs.clear();
        self.watches
            .iter()
            .enumerate()
            .map(|(i, w)| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": SUB_ID_BASE + i as u64,
                    "method": "accountSubscribe",
                    "params": [w.address().to_string(), {"encoding": "base64", "commitment": "confirmed"}],
                })
                .to_string()
            })
            .collect()
    }

    /// Handle one WebSocket message (subscription acks and notifications).
    pub fn on_message(&mut self, v: &Value, now: Ts) -> Vec<Event> {
        self.on_message_at(v, Receipt::at(now)).0
    }

    /// As [`on_message`](Self::on_message), with receipt details; also returns the hot tick.
    pub fn on_message_at(&mut self, v: &Value, mut rx: Receipt) -> (Vec<Event>, Option<HotTick>) {
        if let (Some(id), Some(sub)) = (v.get("id").and_then(Value::as_u64), v.get("result").and_then(Value::as_u64)) {
            if let Some(i) = id.checked_sub(SUB_ID_BASE).map(|i| i as usize).filter(|i| *i < self.watches.len()) {
                self.subs.insert(sub, i);
            }
            return (Vec::new(), None);
        }
        if v.get("method").and_then(Value::as_str) != Some("accountNotification") {
            return (Vec::new(), None);
        }
        let params = v.get("params");
        let Some(i) =
            params.and_then(|p| p.get("subscription")).and_then(Value::as_u64).and_then(|s| self.subs.get(&s))
        else {
            return (Vec::new(), None);
        };
        let i = *i;
        let result = params.and_then(|p| p.get("result"));
        rx.slot = rx.slot.or_else(|| result.and_then(|r| r.get("context")?.get("slot")?.as_u64()));
        let value = result.and_then(|r| r.get("value"));
        let owner = value.and_then(|x| x.get("owner")).and_then(Value::as_str).unwrap_or("");
        let data = value.and_then(|x| x.get("data")).and_then(|d| d.get(0)).and_then(Value::as_str).and_then(|b| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b).ok()
        });
        match data {
            Some(d) => self.on_account_at(i, owner, &d, rx),
            None => (Vec::new(), None),
        }
    }

    /// Decode one account update for watch `i`.
    pub fn on_account(&mut self, i: usize, owner: &str, data: &[u8], now: Ts) -> Vec<Event> {
        self.on_account_at(i, owner, data, Receipt::at(now)).0
    }

    /// Decode one account update for watch `i`. The first wrong owner, pair or
    /// feed id disables that watch with a single error event.
    pub fn on_account_at(&mut self, i: usize, owner: &str, data: &[u8], rx: Receipt) -> (Vec<Event>, Option<HotTick>) {
        let now = rx.now;
        let Some(w) = self.watches.get(i).cloned() else { return (Vec::new(), None) };
        if self.state[i].disabled {
            return (Vec::new(), None);
        }
        let fail = |st: &mut WatchState, why: String| {
            st.disabled = true;
            let e = Event::Error {
                ts: now,
                service: "chain feed".into(),
                message: format!("{} disabled: {why}", w.source()),
            };
            (vec![e], None)
        };
        if owner != w.owner() {
            return fail(&mut self.state[i], format!("owner {owner} is not {}", w.owner()));
        }
        let (dex, pair_name) = self.names[i].clone();
        let (price, pair, side, hot) = match &w {
            Watch::Pool { kind, base, quote, base_decimals, quote_decimals, pair, .. } => {
                let dec = |m: &Address| {
                    if m == base {
                        Some(*base_decimals)
                    } else if m == quote {
                        Some(*quote_decimals)
                    } else {
                        None
                    }
                };
                match decode_pool(*kind, data, dec).and_then(|m| m.price_of(base, quote)) {
                    Some(p) => {
                        let hot = HotTick::Pool {
                            watch: i,
                            dex,
                            pair: pair_name,
                            mid: p,
                            slot: rx.slot,
                            head_slot: rx.head_slot,
                            received: rx.received,
                            received_ts: now,
                        };
                        (p, pair.clone(), SampleSide::Mid, hot)
                    }
                    None => return fail(&mut self.state[i], format!("not a {pair} {kind:?} pool")),
                }
            }
            Watch::Oracle { symbol, feed_id, .. } => match decode_pyth_price_update(data) {
                Some(q) if q.feed_id == *feed_id => {
                    let hot = HotTick::Oracle(OracleUpdate {
                        source: OracleSource::OnChainPyth,
                        symbol: pair_name,
                        price: q.price,
                        conf: q.conf,
                        publish_time: q.publish_time,
                        slot: Some(q.posted_slot),
                        received: rx.received,
                        received_ts: now,
                    });
                    (q.price, symbol.clone(), SampleSide::Oracle, hot)
                }
                Some(_) => return fail(&mut self.state[i], format!("feed id is not {symbol}")),
                None => return fail(&mut self.state[i], "not a Pyth price update".into()),
            },
        };
        if let Some(book) = &self.latency {
            book.since_us("pool.decode_us", rx.received);
            let name = &self.names[i];
            let label = match &w {
                Watch::Pool { .. } => name.0.to_string(),
                Watch::Oracle { .. } => format!("Pyth.{}", name.1),
            };
            if let Some(prev) = self.state[i].last_received {
                book.duration_us(&format!("feed.interval_us.{label}"), rx.received - prev);
            }
            match &hot {
                HotTick::Pool { slot: Some(s), head_slot: Some(h), .. } => {
                    book.value(&format!("pool.slot_lag_slots.{}", name.0), *h as i64 - *s as i64);
                }
                HotTick::Oracle(o) => {
                    book.value(&format!("oracle.publish_age_ms.{}", o.symbol), o.age_at_receipt_ms());
                }
                _ => {}
            }
        }
        self.state[i].last_received = Some(rx.received);
        let mut out = Vec::new();
        let st = &mut self.state[i];
        if side == SampleSide::Mid {
            st.mid = Some((now, price));
        }
        let since = st.last_emit.map(|t| now.0 - t.0);
        let due =
            since.is_none_or(|d| d >= self.emit_interval_us && (st.last_price != Some(price) || d >= HEARTBEAT_US));
        if due {
            st.last_emit = Some(now);
            st.last_price = Some(price);
            out.push(Event::Sample(MarketSample {
                ts: now,
                pair: pair.clone(),
                price: UsdPrice::new((price * 1e6).round() as u64),
                side,
                source: w.source(),
                size_atoms: 0,
            }));
            if side == SampleSide::Oracle && pair == "SOL/USD" {
                out.push(Event::Metric { ts: now, metric: MetricId::OraclePrice, value: price });
            }
        }
        if side == SampleSide::Mid && self.last_spread.is_none_or(|t| now.0 - t.0 >= self.emit_interval_us) {
            let mids: Vec<f64> = self
                .watches
                .iter()
                .zip(&self.state)
                .filter(|(w, _)| matches!(w, Watch::Pool { pair: p, .. } if *p == pair))
                .filter_map(|(_, s)| s.mid.filter(|(t, _)| now.0 - t.0 < SPREAD_FRESH_US).map(|(_, p)| p))
                .collect();
            if !mids.is_empty() {
                self.last_spread = Some(now);
                let mean = mids.iter().sum::<f64>() / mids.len() as f64;
                out.push(Event::Metric { ts: now, metric: MetricId::PoolMid, value: mean });
            }
            if mids.len() >= 2 {
                let (lo, hi) = mids.iter().fold((f64::MAX, f64::MIN), |(l, h), p| (l.min(*p), h.max(*p)));
                out.push(Event::Metric { ts: now, metric: MetricId::PoolSpread, value: (hi - lo) / lo * 10_000.0 });
            }
        }
        (out, Some(hot))
    }
}

/// Chain WebSocket: `slotSubscribe` plus one `accountSubscribe` per watched
/// account, seeded over RPC on every (re)connect (subscriptions only push
/// changes). Slot events are throttled; ChainState is always updated.
#[allow(clippy::too_many_arguments)]
pub async fn run_chain_ws(
    ws_url: String,
    mut accounts: AccountFeed,
    rpc: Arc<RpcClient>,
    chain: Arc<ChainState>,
    telemetry: Arc<Telemetry>,
    emit: Emit,
    hot: Option<HotSink>,
    mut shutdown: watch::Receiver<bool>,
) {
    let forward = |(events, tick): (Vec<Event>, Option<HotTick>)| {
        // hot first: the scheduler must not wait behind recording
        if let (Some(h), Some(t)) = (&hot, tick) {
            h(t);
        }
        events.into_iter().for_each(|e| emit(e));
    };
    use tokio_tungstenite::tungstenite::Message;
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let connect = tokio::time::timeout(Duration::from_secs(10), connect_ws(ws_url.as_str()));
        match connect.await {
            Ok(Ok(mut ws)) => {
                telemetry.set_connected(ServiceId::WebSocket, true);
                let mut subs = vec![r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#.to_string()];
                subs.extend(accounts.subscribe_requests());
                let mut sent = true;
                for sub in subs {
                    if ws.send(Message::Text(sub.into())).await.is_err() {
                        sent = false;
                        break;
                    }
                }
                if !sent {
                    telemetry.record_err(ServiceId::WebSocket, None, "subscribe send failed");
                } else {
                    attempt = 0;
                    if !accounts.is_empty() {
                        emit(Event::Log {
                            ts: Ts::now(),
                            level: LogLevel::Info,
                            message: format!("chain feed: slot + {} accounts subscribed", accounts.addresses().len()),
                        });
                        if let Ok((slot, datas)) = rpc.get_account_datas(&accounts.addresses()).await {
                            let rx =
                                Receipt { received: Instant::now(), now: Ts::now(), slot, head_slot: chain.slot() };
                            for (i, a) in datas.into_iter().enumerate() {
                                if let Some((owner, data)) = a {
                                    forward(accounts.on_account_at(i, &owner, &data, rx));
                                }
                            }
                        }
                    }
                    let mut last_emit = std::time::Instant::now() - Duration::from_secs(1);
                    let mut last_msg = std::time::Instant::now();
                    loop {
                        tokio::select! {
                            _ = shutdown.changed() => { let _ = ws.close(None).await; return; }
                            msg = tokio::time::timeout(Duration::from_secs(15), ws.next()) => {
                                match msg {
                                    Ok(Some(Ok(Message::Text(t)))) => {
                                        let received = Instant::now();
                                        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                                        if let Some(slot) = parse_slot_notification(&v) {
                                            let gap = last_msg.elapsed().as_millis() as u32;
                                            last_msg = std::time::Instant::now();
                                            telemetry.record_ok(ServiceId::WebSocket, gap);
                                            if chain.observe_slot(slot) && last_emit.elapsed() >= Duration::from_millis(400) {
                                                last_emit = std::time::Instant::now();
                                                emit(Event::Slot { ts: Ts::now(), slot });
                                            }
                                        } else {
                                            let rx = Receipt { received, now: Ts::now(), slot: None, head_slot: chain.slot() };
                                            forward(accounts.on_message_at(&v, rx));
                                        }
                                    }
                                    Ok(Some(Ok(Message::Ping(p)))) => {
                                        let _ = ws.send(Message::Pong(p)).await;
                                    }
                                    Ok(Some(Ok(_))) => {}
                                    Ok(Some(Err(e))) => { telemetry.record_err(ServiceId::WebSocket, None, format!("ws: {e}")); break; }
                                    Ok(None) => { telemetry.record_err(ServiceId::WebSocket, None, "ws closed"); break; }
                                    Err(_) => { telemetry.record_err(ServiceId::WebSocket, None, "ws: no message for 15s"); break; }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => telemetry.record_err(ServiceId::WebSocket, None, format!("ws connect: {e}")),
            Err(_) => telemetry.record_err(ServiceId::WebSocket, None, "ws connect timeout"),
        }
        telemetry.set_connected(ServiceId::WebSocket, false);
        let d = backoff_delay(attempt, Duration::from_secs(1), Duration::from_secs(60), None, rand_unit());
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(d) => {}
        }
    }
}

/// A server-push WebSocket delivering text messages (e.g. the Jito tip
/// stream). Reconnects with backoff; a silent socket is dropped after `idle`.
pub async fn run_text_stream(
    url: String,
    idle: Duration,
    on_text: impl FnMut(&str) + Send,
    shutdown: watch::Receiver<bool>,
) {
    run_subscribed_stream(url, Vec::new(), idle, on_text, shutdown).await
}

/// [`run_text_stream`] that sends `subscribe` (e.g. an exchange's subscribe
/// request) after every (re)connect.
pub async fn run_subscribed_stream(
    url: String,
    subscribe: Vec<String>,
    idle: Duration,
    mut on_text: impl FnMut(&str) + Send,
    mut shutdown: watch::Receiver<bool>,
) {
    use tokio_tungstenite::tungstenite::Message;
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            return;
        }
        if let Ok(Ok(mut ws)) = tokio::time::timeout(Duration::from_secs(10), connect_ws(&url)).await {
            let mut subscribed = true;
            for m in &subscribe {
                if ws.send(Message::Text(m.clone().into())).await.is_err() {
                    subscribed = false;
                    break;
                }
            }
            if subscribed {
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => { let _ = ws.close(None).await; return; }
                        msg = tokio::time::timeout(idle, ws.next()) => match msg {
                            Ok(Some(Ok(Message::Text(t)))) => { attempt = 0; on_text(&t); }
                            Ok(Some(Ok(Message::Ping(p)))) => { let _ = ws.send(Message::Pong(p)).await; }
                            Ok(Some(Ok(_))) => {}
                            _ => break,
                        }
                    }
                }
            }
        }
        let d = backoff_delay(attempt, Duration::from_secs(2), Duration::from_secs(120), None, rand_unit());
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(d) => {}
        }
    }
}

/// Nearest-rank percentile of an unsorted sample.
pub fn percentile(values: &[u64], p: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    Some(v[rank.min(v.len()) - 1])
}

/// Polls priority fees on the watched accounts every tick and network TPS
/// every third tick; emits `Network` plus the PriorityFee / NetworkTps metrics.
pub async fn run_network_poller(
    rpc: Arc<RpcClient>,
    fee_accounts: Vec<Address>,
    emit: Emit,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stats = NetworkStats::default();
    let mut n = 0u64;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                let mut changed = false;
                if let Ok(fees) = rpc.get_recent_prioritization_fees(&fee_accounts).await && !fees.is_empty() {
                    stats.fee_p50 = percentile(&fees, 50.0);
                    stats.fee_p75 = percentile(&fees, 75.0);
                    stats.fee_p90 = percentile(&fees, 90.0);
                    stats.fee_slots = fees.len() as u32;
                    changed = true;
                }
                if n.is_multiple_of(3)
                    && let Ok(Some((txs, non_vote, secs))) = rpc.get_recent_performance_sample().await
                    && secs > 0
                {
                    stats.tps = Some(txs as f64 / secs as f64);
                    stats.non_vote_tps = Some(non_vote as f64 / secs as f64);
                    changed = true;
                }
                n += 1;
                if changed {
                    let ts = Ts::now();
                    stats.ts = ts;
                    if let Some(f) = stats.fee_p75 {
                        emit(Event::Metric { ts, metric: MetricId::PriorityFee, value: f as f64 });
                    }
                    if let Some(t) = stats.tps {
                        emit(Event::Metric { ts, metric: MetricId::NetworkTps, value: t });
                    }
                    emit(Event::Network(stats.clone()));
                }
            }
        }
    }
}

/// Polls `getEpochInfo` for block height (blockhash expiry) and as a slot
/// fallback when the WebSocket is down.
pub async fn run_block_height_poller(
    rpc: Arc<RpcClient>,
    chain: Arc<ChainState>,
    emit: Emit,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                if let Ok(info) = rpc.get_epoch_info().await {
                    chain.block_height.fetch_max(info.block_height, Ordering::Relaxed);
                    let ts = Ts::now();
                    if chain.observe_slot(info.slot) {
                        emit(Event::Slot { ts, slot: info.slot });
                    }
                    emit(Event::BlockHeight { ts, height: info.block_height });
                }
            }
        }
    }
}

fn rand_unit() -> f64 {
    // cheap jitter without pulling rand into this crate's API
    (Ts::now().micros() % 1_000) as f64 / 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_notification_parsing() {
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"parent":75,"root":44,"slot":76},"subscription":0}}"#,
        )
        .unwrap();
        assert_eq!(parse_slot_notification(&v), Some(76));
        let ack: Value = serde_json::from_str(r#"{"jsonrpc":"2.0","result":0,"id":1}"#).unwrap();
        assert_eq!(parse_slot_notification(&ack), None);
    }

    #[test]
    fn ws_targets() {
        assert_eq!(ws_target("wss://api.mainnet-beta.solana.com"), Some(("api.mainnet-beta.solana.com".into(), 443)));
        assert_eq!(ws_target("ws://127.0.0.1:8900/path?x=1"), Some(("127.0.0.1".into(), 8900)));
        assert_eq!(ws_target("wss://user:pw@host.io/k"), Some(("host.io".into(), 443)));
        assert_eq!(ws_target("nonsense"), None);
    }

    #[test]
    fn scutil_proxy_parsing() {
        let out = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7897\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 1\n  HTTPSPort : 7897\n  HTTPSProxy : 127.0.0.1\n  SOCKSEnable : 0\n}\n";
        assert_eq!(parse_scutil_proxy(out, true), Some(("127.0.0.1".into(), 7897)));
        assert_eq!(parse_scutil_proxy(out, false), Some(("127.0.0.1".into(), 7897)));
        let off = "<dictionary> {\n  HTTPSEnable : 0\n  HTTPSPort : 7897\n  HTTPSProxy : 127.0.0.1\n}\n";
        assert_eq!(parse_scutil_proxy(off, true), None);
        assert_eq!(parse_scutil_proxy("<dictionary> {\n}\n", true), None);
    }

    fn fixture(name: &str) -> (String, String, f64) {
        let json = match name {
            "whirlpool" => include_str!("../../../fixtures/accounts/whirlpool.json"),
            "raydium_clmm" => include_str!("../../../fixtures/accounts/raydium_clmm.json"),
            _ => include_str!("../../../fixtures/accounts/pyth_sol_usd.json"),
        };
        let v: Value = serde_json::from_str(json).unwrap();
        (
            v["data_base64"].as_str().unwrap().into(),
            v["owner"].as_str().unwrap().into(),
            v["expected_price"].as_f64().unwrap(),
        )
    }

    fn notification(sub: u64, owner: &str, b64: &str) -> Value {
        serde_json::json!({"jsonrpc":"2.0","method":"accountNotification","params":{"subscription":sub,
            "result":{"context":{"slot":1},"value":{"owner":owner,"data":[b64,"base64"],"lamports":1,"executable":false}}}})
    }

    fn feed() -> AccountFeed {
        let sol: Address = "So11111111111111111111111111111111111111112".parse().unwrap();
        let usdc: Address = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let pool = |dex: &str, kind| Watch::Pool {
            dex: dex.into(),
            kind,
            address: Address([1; 32]),
            pair: "SOL/USDC".into(),
            base: sol,
            quote: usdc,
            base_decimals: 9,
            quote_decimals: 6,
        };
        let feed_id =
            searcher_core::config::parse_feed_id("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d")
                .unwrap();
        let mut f = AccountFeed::new(
            vec![
                pool("Whirlpool", PoolKind::Whirlpool),
                pool("Raydium CLMM", PoolKind::RaydiumClmm),
                Watch::Oracle { symbol: "SOL/USD".into(), address: Address([2; 32]), feed_id },
            ],
            Duration::from_millis(500),
        );
        assert_eq!(f.subscribe_requests().len(), 3);
        // acks: request id 100+i → subscription id 7+i
        for i in 0..3u64 {
            let ack = serde_json::json!({"jsonrpc":"2.0","result":7 + i,"id":100 + i});
            assert!(f.on_message(&ack, Ts(0)).is_empty());
        }
        f
    }

    #[test]
    fn account_notifications_become_samples_spread_and_oracle_metric() {
        let mut f = feed();
        let (wd, wo, wp) = fixture("whirlpool");
        let (rd, ro, rp) = fixture("raydium_clmm");
        let (pd, po, pp) = fixture("pyth");
        let ev = f.on_message(&notification(7, &wo, &wd), Ts(1_000_000));
        let [Event::Sample(s), Event::Metric { metric: MetricId::PoolMid, .. }] = ev.as_slice() else {
            panic!("{ev:?}")
        };
        assert_eq!((s.side, s.source.as_str(), s.pair.as_str()), (SampleSide::Mid, "Whirlpool pool", "SOL/USDC"));
        assert!((s.price.f64() - wp).abs() < 1e-5);
        // second pool after the emit interval → sample + mid + spread (two fresh mids)
        let ev = f.on_message(&notification(8, &ro, &rd), Ts(1_600_000));
        let spread = ev.iter().find_map(|e| match e {
            Event::Metric { metric: MetricId::PoolSpread, value, .. } => Some(*value),
            _ => None,
        });
        let want = (wp - rp).abs() / wp.min(rp) * 10_000.0;
        assert!((spread.expect("spread") - want).abs() < 1e-6, "{spread:?} vs {want}");
        assert!(
            ev.iter().any(|e| matches!(e, Event::Metric { metric: MetricId::PoolMid, value, .. }
            if (value - (wp + rp) / 2.0).abs() < 1e-9)),
            "pool mid is the mean of the fresh mids"
        );
        // oracle → sample + OraclePrice metric
        let ev = f.on_message(&notification(9, &po, &pd), Ts(1_700_000));
        assert!(ev.iter().any(|e| matches!(e, Event::Sample(s) if s.side == SampleSide::Oracle && s.source == "Pyth")));
        assert!(ev.iter().any(
            |e| matches!(e, Event::Metric { metric: MetricId::OraclePrice, value, .. } if (value - pp).abs() < 1e-9)
        ));
        // unknown subscription id → ignored
        assert!(f.on_message(&notification(99, &wo, &wd), Ts(2_000_000)).is_empty());
    }

    #[test]
    fn samples_are_throttled_with_heartbeat() {
        let mut f = feed();
        let (wd, wo, _) = fixture("whirlpool");
        let n = |f: &mut AccountFeed, t: i64| {
            f.on_message(&notification(7, &wo, &wd), Ts(t)).iter().filter(|e| matches!(e, Event::Sample(_))).count()
        };
        assert_eq!(n(&mut f, 0), 1, "first update is emitted");
        assert_eq!(n(&mut f, 200_000), 0, "inside the interval");
        assert_eq!(n(&mut f, 900_000), 0, "unchanged price, before the heartbeat");
        assert_eq!(n(&mut f, 5_000_000), 1, "heartbeat re-emits an unchanged price");
    }

    #[test]
    fn wrong_owner_or_feed_disables_the_watch_once() {
        let mut f = feed();
        let (wd, _, _) = fixture("whirlpool");
        let ev = f.on_message(&notification(7, "11111111111111111111111111111111", &wd), Ts(0));
        assert!(matches!(ev.as_slice(), [Event::Error { .. }]), "{ev:?}");
        let (_, wo, _) = fixture("whirlpool");
        assert!(f.on_message(&notification(7, &wo, &wd), Ts(10_000_000)).is_empty(), "stays disabled");
        // a Whirlpool account on the oracle watch: right owner check fails first
        let ev = f.on_message(&notification(9, &wo, &wd), Ts(0));
        assert!(matches!(ev.as_slice(), [Event::Error { .. }]));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let v = [0, 0, 10, 20, 30, 40, 50, 60, 70, 1000];
        assert_eq!(percentile(&v, 50.0), Some(30));
        assert_eq!(percentile(&v, 90.0), Some(70));
        assert_eq!(percentile(&v, 100.0), Some(1000));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn chain_state_is_monotonic() {
        let c = ChainState::default();
        assert_eq!(c.slot(), None);
        assert!(c.observe_slot(10));
        assert!(!c.observe_slot(9));
        assert_eq!(c.slot(), Some(10));
    }
}
