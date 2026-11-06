//! WebSocket endpoint `/v0` and the stats endpoint `/v0/stats`.
//!
//! `presence` is pure and returns addressed deliveries; the [`Hub`] holds one bounded queue per
//! connection and sends them. A connection whose queue is full is dropped, so a slow reader can
//! never block the others. The std mutexes are never held across an `.await`.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::Config;
use crate::metrics::{today_utc, Metrics};
use crate::presence::{ConnId, Delivery, Presence};
use crate::protocol::{parse_app_msg, parse_update_entry, AppInfo, AppMsg, ErrorCode, ProtoError, ServerMsg};
use crate::routes::Routes;

const QUEUE_CAPACITY: usize = 1024;
const MAX_AXUM_MESSAGE: usize = 1 << 20;
const ERROR_WINDOW: Duration = Duration::from_secs(10);
const MAX_ERRORS_IN_WINDOW: usize = 20;
const CLOSE_POLICY: u16 = 1008;
const CLOSE_UNSUPPORTED: u16 = 1003;
/// How long the writer may take to flush the last frames once the reader is done.
const WRITER_GRACE: Duration = Duration::from_secs(5);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One bounded outgoing queue per connection.
#[derive(Default)]
pub struct Hub {
    queues: Mutex<HashMap<ConnId, mpsc::Sender<ServerMsg>>>,
}

impl Hub {
    pub fn register(&self, conn: ConnId) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        lock(&self.queues).insert(conn, tx);
        rx
    }

    /// Dropping the sender lets the writer drain what is queued and then see the channel close.
    pub fn unregister(&self, conn: ConnId) {
        lock(&self.queues).remove(&conn);
    }

    pub fn is_registered(&self, conn: ConnId) -> bool {
        lock(&self.queues).contains_key(&conn)
    }

    /// Never blocks. A connection whose queue is full (or already gone) is unregistered.
    pub fn deliver(&self, deliveries: Vec<Delivery>) {
        if deliveries.is_empty() {
            return;
        }
        let mut queues = lock(&self.queues);
        for d in deliveries {
            let Some(tx) = queues.get(&d.to) else { continue };
            match tx.try_send(d.msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(conn = %conn_name(d.to), "outgoing queue full, closing connection");
                    queues.remove(&d.to);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    queues.remove(&d.to);
                }
            }
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub presence: Mutex<Presence>,
    pub hub: Hub,
    pub metrics: Mutex<Metrics>,
    pub ip_counts: Mutex<HashMap<IpAddr, u32>>,
    /// The next connection number; connection IDs are `c` + its hex.
    pub next_conn: AtomicU64,
}

impl AppState {
    pub fn new(config: Config, routes: Routes, metrics: Metrics) -> Arc<AppState> {
        let presence = Presence::new(routes, config.limits);
        Arc::new(AppState {
            config,
            presence: Mutex::new(presence),
            hub: Hub::default(),
            metrics: Mutex::new(metrics),
            ip_counts: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        })
    }
}

fn conn_name(conn: ConnId) -> String {
    format!("c{conn:x}")
}

/// Background tasks: expire silent riders every second and record rider-minutes every minute.
/// The std mutexes are locked, used and released before anything is awaited.
pub fn spawn_tickers(state: Arc<AppState>) {
    let expiry = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let deliveries = lock(&expiry.presence).expire(std::time::Instant::now());
            expiry.hub.deliver(deliveries);
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await; // the first tick is immediate; counting starts after a full minute
        loop {
            tick.tick().await;
            let live = lock(&state.presence).live_counts();
            if let Err(e) = lock(&state.metrics).add_minute(&live, &today_utc()) {
                tracing::warn!("metrics write failed: {e}");
            }
        }
    });
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v0", get(ws_handler))
        .route("/v0/stats", get(stats_handler))
        .with_state(state)
}

async fn stats_handler(State(state): State<Arc<AppState>>) -> Response {
    let live = lock(&state.presence).live_counts();
    let stats = lock(&state.metrics).stats(&live, &today_utc());
    match stats {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            tracing::error!("stats query failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Releases one slot of the per-IP count when dropped, wherever the connection ends.
struct IpGuard {
    state: Arc<AppState>,
    ip: IpAddr,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut counts = lock(&self.state.ip_counts);
        if let Some(n) = counts.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

fn client_ip(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if state.config.trust_proxy {
        // The rightmost entry is the one our own proxy added.
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok());
        if let Some(ip) = forwarded {
            return ip;
        }
    }
    peer.ip()
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = client_ip(&state, peer, &headers);
    {
        let mut counts = lock(&state.ip_counts);
        let n = counts.entry(ip).or_insert(0);
        if *n >= state.config.max_connections_per_ip {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        *n += 1;
    }
    // Created before the upgrade, so the slot is also released if the upgrade never completes.
    let guard = IpGuard {
        state: state.clone(),
        ip,
    };
    ws.max_message_size(MAX_AXUM_MESSAGE)
        .on_upgrade(move |socket| handle_socket(state, socket, guard))
}

async fn handle_socket(state: Arc<AppState>, socket: WebSocket, _ip_guard: IpGuard) {
    let conn = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let rx = state.hub.register(conn);
    let (sink, mut stream) = socket.split();
    let close_code = Arc::new(AtomicU16::new(CLOSE_POLICY));
    let writer = tokio::spawn(write_loop(sink, rx, state.config.ping_interval, close_code.clone()));

    let mut session = Session {
        state: state.clone(),
        conn,
        app: None,
        recent_errors: VecDeque::new(),
    };
    session.read_loop(&mut stream, &close_code).await;

    // Leave presence first (others get `left`), then close our queue; the writer drains it, sends
    // the close frame and ends. An unresponsive peer must not hold the task forever.
    let deliveries = lock(&state.presence).disconnect(conn);
    state.hub.deliver(deliveries);
    state.hub.unregister(conn);
    let abort = writer.abort_handle();
    if tokio::time::timeout(WRITER_GRACE, writer).await.is_err() {
        abort.abort();
    }
    tracing::debug!(conn = %conn_name(conn), "connection closed");
}

async fn write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<ServerMsg>,
    ping_interval: Duration,
    close_code: Arc<AtomicU16>,
) {
    let mut ping = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                let Ok(text) = serde_json::to_string(&msg) else { continue };
                if sink.send(Message::text(text)).await.is_err() {
                    return;
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
        }
    }
    let frame = CloseFrame {
        code: close_code.load(Ordering::Relaxed),
        reason: "".into(),
    };
    let _ = sink.send(Message::Close(Some(frame))).await;
    let _ = sink.close().await;
}

enum Flow {
    Continue,
    Close,
}

struct Session {
    state: Arc<AppState>,
    conn: ConnId,
    /// Set once `hello` was accepted.
    app: Option<String>,
    /// When the recent non-fatal errors happened.
    recent_errors: VecDeque<Instant>,
}

impl Session {
    async fn read_loop(&mut self, stream: &mut futures_util::stream::SplitStream<WebSocket>, close_code: &AtomicU16) {
        let hello_timeout = self.state.config.hello_timeout;
        let pong_timeout = self.state.config.pong_timeout;
        let hello_deadline = Instant::now() + hello_timeout;
        let mut pong_deadline = Instant::now() + pong_timeout;
        loop {
            let hello_wait = async {
                if self.app.is_none() {
                    tokio::time::sleep_until(hello_deadline).await
                } else {
                    std::future::pending().await
                }
            };
            let frame = tokio::select! {
                f = stream.next() => f,
                _ = hello_wait => {
                    self.fatal(ProtoError::new(ErrorCode::HelloRequired, "no hello within the time limit"));
                    return;
                }
                _ = tokio::time::sleep_until(pong_deadline) => {
                    tracing::debug!(conn = %conn_name(self.conn), "pong timeout");
                    return;
                }
            };
            match frame {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(Message::Pong(_))) => pong_deadline = Instant::now() + pong_timeout,
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Binary(_))) => {
                    close_code.store(CLOSE_UNSUPPORTED, Ordering::Relaxed);
                    self.state.hub.unregister(self.conn);
                    return;
                }
                Some(Ok(Message::Text(text))) => {
                    if let Flow::Close = self.handle_text(text.as_str()) {
                        return;
                    }
                }
            }
        }
    }

    fn send_self(&self, msg: ServerMsg) {
        self.state.hub.deliver(vec![Delivery { to: self.conn, msg }]);
    }

    /// Sends a fatal error and unregisters, so the writer flushes it and then closes the socket.
    fn fatal(&self, err: ProtoError) {
        self.send_self(err.into());
        self.state.hub.unregister(self.conn);
    }

    /// Sends the error to this connection. Returns `Close` if it was fatal, or if it pushed the
    /// connection over the error rate (more than 20 non-fatal errors within 10 s).
    fn report(&mut self, err: ProtoError, ref_: &Option<String>) -> Flow {
        let err = err.with_ref(ref_.clone());
        if err.code.fatal() {
            self.fatal(err);
            return Flow::Close;
        }
        let now = Instant::now();
        self.recent_errors.push_back(now);
        while self
            .recent_errors
            .front()
            .is_some_and(|t| now.duration_since(*t) > ERROR_WINDOW)
        {
            self.recent_errors.pop_front();
        }
        if self.recent_errors.len() > MAX_ERRORS_IN_WINDOW {
            self.fatal(ProtoError::new(ErrorCode::TooManyErrors, "too many errors").with_ref(ref_.clone()));
            return Flow::Close;
        }
        self.send_self(err.into());
        Flow::Continue
    }

    fn handle_text(&mut self, text: &str) -> Flow {
        let limits = self.state.config.limits;
        if text.len() > limits.max_message_bytes as usize {
            return self.report(
                ProtoError::new(
                    ErrorCode::MessageTooLarge,
                    format!("messages may be at most {} bytes", limits.max_message_bytes),
                ),
                &None,
            );
        }
        let parsed = parse_app_msg(text);
        if self.app.is_none() {
            return self.handle_first(text, parsed);
        }
        let env = match parsed {
            Ok(env) => env,
            Err(e) => {
                let ref_ = e.ref_.clone();
                return self.report(e, &ref_);
            }
        };
        self.dispatch(env.ref_, env.msg)
    }

    /// Before `hello`, anything but a valid `hello` is `hello_required`; a `hello` that is itself
    /// invalid reports its own error (for example `invalid_app_id`).
    fn handle_first(&mut self, text: &str, parsed: Result<crate::protocol::Envelope, ProtoError>) -> Flow {
        match parsed {
            Ok(env) => match env.msg {
                AppMsg::Hello(info) => self.hello(info),
                _ => self.report(
                    ProtoError::new(ErrorCode::HelloRequired, "the first message must be hello"),
                    &env.ref_,
                ),
            },
            Err(e) => {
                let is_hello = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "hello"))
                    .unwrap_or(false);
                let ref_ = e.ref_.clone();
                if is_hello {
                    self.report(e, &ref_)
                } else {
                    self.report(
                        ProtoError::new(ErrorCode::HelloRequired, "the first message must be hello"),
                        &ref_,
                    )
                }
            }
        }
    }

    fn hello(&mut self, info: AppInfo) -> Flow {
        let state = &self.state;
        if let Err(e) = lock(&state.metrics).record_hello(&info.id, &info.version, &today_utc()) {
            tracing::warn!(app = %info.id, "recording hello failed: {e}");
        }
        tracing::info!(app = %info.id, conn = %conn_name(self.conn), "hello");
        self.app = Some(info.id.clone());
        lock(&state.presence).connect(self.conn, info);
        self.send_self(ServerMsg::Welcome {
            protocol: "0.1",
            connection: conn_name(self.conn),
            limits: state.config.limits,
        });
        Flow::Continue
    }

    fn dispatch(&mut self, ref_: Option<String>, msg: AppMsg) -> Flow {
        let state = self.state.clone();
        let conn = self.conn;
        let now = std::time::Instant::now();
        let result = match msg {
            AppMsg::Hello(_) => Err(ProtoError::new(ErrorCode::HelloRepeated, "hello was already sent")),
            AppMsg::Subscribe { route, room } => lock(&state.presence).subscribe(conn, &route, &room),
            AppMsg::Unsubscribe { route, room } => {
                lock(&state.presence).unsubscribe(conn, &route, &room);
                Ok(Vec::new())
            }
            AppMsg::Join(join) => lock(&state.presence).join(conn, join, now),
            AppMsg::Leave { rider } => lock(&state.presence).leave(conn, &rider),
            AppMsg::Update(entries) => return self.update(entries, ref_, now),
        };
        match result {
            Ok(deliveries) => {
                state.hub.deliver(deliveries);
                Flow::Continue
            }
            Err(e) => self.report(e, &ref_),
        }
    }

    fn update(&mut self, entries: Vec<serde_json::Value>, ref_: Option<String>, now: std::time::Instant) -> Flow {
        let mut errors = Vec::new();
        let mut valid = Vec::new();
        for v in &entries {
            match parse_update_entry(v) {
                Ok(e) => valid.push(e),
                Err(e) => errors.push(e),
            }
        }
        let (deliveries, more) = lock(&self.state.presence).update(self.conn, valid, now);
        self.state.hub.deliver(deliveries);
        errors.extend(more);
        for e in errors {
            if let Flow::Close = self.report(e, &ref_) {
                return Flow::Close;
            }
        }
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::LeftReason;

    #[test]
    fn full_queue_closes_connection() {
        let hub = Hub::default();
        let _rx = hub.register(1);
        let msg = || Delivery {
            to: 1,
            msg: ServerMsg::Left {
                route: "r".into(),
                room: "public".into(),
                id: "r1".into(),
                reason: LeftReason::Leave,
            },
        };
        for _ in 0..QUEUE_CAPACITY {
            hub.deliver(vec![msg()]);
            assert!(hub.is_registered(1));
        }
        hub.deliver(vec![msg()]);
        assert!(!hub.is_registered(1));
        // Delivering to a connection that is gone is fine.
        hub.deliver(vec![msg()]);
    }
}
