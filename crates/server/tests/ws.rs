//! End-to-end tests over real WebSockets. Nothing here sleeps: every wait is a read with a timeout,
//! and "nothing arrives" is shown by a later message being the next one.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use obn_server::config::Config;
use obn_server::metrics::Metrics;
use obn_server::protocol::Limits;
use obn_server::routes::Routes;
use obn_server::ws::{router, spawn_tickers, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(2);

fn test_config() -> Config {
    Config {
        hello_timeout: Duration::from_millis(300),
        // A rate limit that a round trip always exceeds, so tests need not wait between updates.
        limits: Limits {
            max_update_hz: 1_000_000,
            ..Limits::default()
        },
        ..Config::default()
    }
}

async fn start(config: Config) -> SocketAddr {
    let routes = Routes::load_dir(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../protocol/routes"))).unwrap();
    let state = AppState::new(config, routes, Metrics::open_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    spawn_tickers(state.clone());
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

async fn raw(addr: SocketAddr) -> Client {
    let (ws, _) = tokio::time::timeout(WAIT, connect_async(format!("ws://{addr}/v0")))
        .await
        .expect("timeout connecting")
        .expect("connect failed");
    ws
}

async fn send(c: &mut Client, v: Value) {
    c.send(Message::text(v.to_string())).await.unwrap();
}

async fn recv_json(c: &mut Client) -> Value {
    recv_json_within(c, WAIT).await
}

async fn recv_json_within(c: &mut Client, wait: Duration) -> Value {
    loop {
        match tokio::time::timeout(wait, c.next()).await {
            Err(_) => panic!("timed out waiting for a message"),
            Ok(None) => panic!("stream ended while waiting for a message"),
            Ok(Some(Err(e))) => panic!("socket error while waiting for a message: {e}"),
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(t.as_str()).unwrap(),
            Ok(Some(Ok(Message::Close(f)))) => panic!("closed while waiting for a message: {f:?}"),
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// The next frame must be a close frame or the end of the stream.
async fn expect_closed(c: &mut Client) {
    loop {
        match tokio::time::timeout(WAIT, c.next()).await {
            Err(_) => panic!("timed out waiting for the close"),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return,
            Ok(Some(Ok(Message::Text(t)))) => panic!("expected close, got {t}"),
            Ok(Some(Ok(_))) => continue,
        }
    }
}

async fn app(addr: SocketAddr, id: &str) -> Client {
    let mut c = raw(addr).await;
    send(&mut c, json!({"type":"hello","app":{"id":id,"version":"1.0"}})).await;
    let w = recv_json(&mut c).await;
    assert_eq!(w["type"], "welcome");
    c
}

async fn subscribe(c: &mut Client) -> Value {
    send(c, json!({"type":"subscribe","route":"velodrome-250","room":"public"})).await;
    let s = recv_json(c).await;
    assert_eq!(s["type"], "snapshot", "{s}");
    s
}

fn join_msg(rider: &str) -> Value {
    json!({"type":"join","rider":rider,"route":"velodrome-250","room":"public","name":"Anna","lap":0,"distance":0,"speed":0})
}

#[tokio::test]
async fn full_lifecycle() {
    let addr = start(test_config()).await;
    let mut a = app(addr, "app-a").await;
    subscribe(&mut a).await;
    let mut b = app(addr, "app-b").await;
    subscribe(&mut b).await;
    send(&mut b, join_msg("u-1")).await;
    // Barrier: once B's snapshot arrives, the join has been processed.
    subscribe(&mut b).await;
    let j = recv_json(&mut a).await;
    assert_eq!(j["type"], "joined");
    assert_eq!(j["rider"]["app"], "app-b");
    assert_eq!(j["rider"]["name"], "Anna");
    send(
        &mut b,
        json!({"type":"update","riders":[{"rider":"u-1","lap":0,"distance":5.5,"speed":9}]}),
    )
    .await;
    let u = recv_json(&mut a).await;
    assert_eq!(u["type"], "updated");
    assert_eq!(u["riders"][0]["id"], j["rider"]["id"]);
    assert_eq!(u["riders"][0]["distance"], 5.5);
    send(&mut b, json!({"type":"leave","rider":"u-1"})).await;
    let l = recv_json(&mut a).await;
    assert_eq!(l["type"], "left");
    assert_eq!(l["reason"], "leave");
    assert_eq!(l["id"], j["rider"]["id"]);
}

#[tokio::test]
async fn welcome_has_limits() {
    let addr = start(Config::default()).await;
    let mut c = raw(addr).await;
    send(&mut c, json!({"type":"hello","app":{"id":"app-a","version":"1.0"}})).await;
    let w = recv_json(&mut c).await;
    assert_eq!(w["type"], "welcome");
    assert_eq!(w["protocol"], "0.1");
    assert_eq!(w["connection"], "c1");
    assert_eq!(w["limits"], serde_json::to_value(Limits::default()).unwrap());
}

#[tokio::test]
async fn no_hello_closes() {
    let addr = start(test_config()).await;
    let mut c = raw(addr).await;
    let e = recv_json(&mut c).await;
    assert_eq!(e["type"], "error");
    assert_eq!(e["code"], "hello_required");
    assert_eq!(e["fatal"], true);
    expect_closed(&mut c).await;
}

#[tokio::test]
async fn first_message_not_hello_closes() {
    let addr = start(test_config()).await;
    let mut c = raw(addr).await;
    send(
        &mut c,
        json!({"type":"subscribe","route":"velodrome-250","room":"public"}),
    )
    .await;
    let e = recv_json(&mut c).await;
    assert_eq!(e["code"], "hello_required");
    assert_eq!(e["fatal"], true);
    expect_closed(&mut c).await;
}

#[tokio::test]
async fn invalid_app_id_closes() {
    let addr = start(test_config()).await;
    let mut c = raw(addr).await;
    send(&mut c, json!({"type":"hello","app":{"id":"Bad App","version":"1.0"}})).await;
    let e = recv_json(&mut c).await;
    assert_eq!(e["code"], "invalid_app_id");
    assert_eq!(e["fatal"], true);
    expect_closed(&mut c).await;
}

#[tokio::test]
async fn second_hello_is_not_fatal() {
    let addr = start(test_config()).await;
    let mut c = app(addr, "app-a").await;
    send(&mut c, json!({"type":"hello","app":{"id":"app-a","version":"1.0"}})).await;
    let e = recv_json(&mut c).await;
    assert_eq!(e["code"], "hello_repeated");
    assert_eq!(e["fatal"], false);
    subscribe(&mut c).await;
}

#[tokio::test]
async fn oversized_message_closes() {
    let addr = start(test_config()).await;
    let mut c = app(addr, "app-a").await;
    c.send(Message::text("x".repeat(70_000))).await.unwrap();
    let e = recv_json(&mut c).await;
    assert_eq!(e["code"], "message_too_large");
    assert_eq!(e["fatal"], true);
    expect_closed(&mut c).await;
}

#[tokio::test]
async fn error_carries_ref() {
    let addr = start(test_config()).await;
    let mut c = app(addr, "app-a").await;
    send(&mut c, json!({"type":"leave","rider":"x","ref":"9"})).await;
    let e = recv_json(&mut c).await;
    assert_eq!(e["code"], "unknown_rider");
    assert_eq!(e["ref"], "9");
    assert_eq!(e["fatal"], false);
}

#[tokio::test]
async fn reconnect_gives_new_id() {
    let addr = start(test_config()).await;
    let mut a = app(addr, "app-a").await;
    subscribe(&mut a).await;
    let mut b = app(addr, "app-b").await;
    send(&mut b, join_msg("u-1")).await;
    let j = recv_json(&mut a).await;
    assert_eq!(j["rider"]["id"], "r1");
    drop(b);
    let l = recv_json(&mut a).await;
    assert_eq!(l["type"], "left");
    assert_eq!(l["reason"], "disconnect");
    assert_eq!(l["id"], "r1");
    let mut b = app(addr, "app-b").await;
    send(&mut b, join_msg("u-1")).await;
    let j = recv_json(&mut a).await;
    assert_eq!(j["type"], "joined");
    assert_eq!(j["rider"]["id"], "r2");
}

#[tokio::test]
async fn per_ip_limit() {
    let mut config = test_config();
    config.max_connections_per_ip = 2;
    let addr = start(config).await;
    let _a = app(addr, "app-a").await;
    let _b = app(addr, "app-b").await;
    let err = connect_async(format!("ws://{addr}/v0")).await.unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status().as_u16(), 429),
        other => panic!("expected an HTTP 429, got {other}"),
    }
}

#[tokio::test]
async fn stats_endpoint() {
    let addr = start(test_config()).await;
    let mut b = raw(addr).await;
    send(
        &mut b,
        json!({"type":"hello","app":{"id":"openradlr","version":"0.3.1"}}),
    )
    .await;
    assert_eq!(recv_json(&mut b).await["type"], "welcome");
    send(&mut b, join_msg("u-1")).await;
    // Barrier: the snapshot comes after the join was processed.
    subscribe(&mut b).await;

    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(b"GET /v0/stats HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(WAIT, tcp.read_to_end(&mut buf))
        .await
        .expect("timeout reading stats")
        .unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    let body = text.split("\r\n\r\n").nth(1).unwrap();
    let v: Value = serde_json::from_str(body).unwrap();
    let app = &v["apps"][0];
    assert_eq!(app["id"], "openradlr");
    assert_eq!(app["connections"], 1);
    assert_eq!(app["riders"], 1);
}

#[tokio::test]
async fn ticker_expires_riders() {
    let mut config = test_config();
    config.limits.rider_timeout_s = 1;
    let addr = start(config).await;
    let mut a = app(addr, "app-a").await;
    subscribe(&mut a).await;
    let mut b = app(addr, "app-b").await;
    send(&mut b, join_msg("u-1")).await;
    let j = recv_json(&mut a).await;
    assert_eq!(j["type"], "joined");
    // B sends nothing more; the ticker must expire it.
    let l = recv_json_within(&mut a, Duration::from_secs(3)).await;
    assert_eq!(l["type"], "left");
    assert_eq!(l["reason"], "timeout");
    assert_eq!(l["id"], "r1");
}
