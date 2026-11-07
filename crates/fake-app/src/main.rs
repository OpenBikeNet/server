//! A fake indoor cycling app for trying out an OpenBikeNet server. It speaks the protocol with plain
//! JSON and does not depend on `obn-server`, like any other app would.
//!
//! ```text
//! obn-fake-app [--url ws://127.0.0.1:8080/v0] [--route velodrome-250] [--length 250]
//!              [--riders 3] [--speed 11.0] [--app fake-app]
//! ```

use std::process::ExitCode;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

struct Args {
    url: String,
    route: String,
    length: f64,
    riders: u32,
    speed: f64,
    app: String,
}

const USAGE: &str =
    "usage: obn-fake-app [--url URL] [--route ID] [--length METRES] [--riders N] [--speed M/S] [--app ID]";

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        url: "ws://127.0.0.1:8080/v0".into(),
        route: "velodrome-250".into(),
        length: 250.0,
        riders: 3,
        speed: 11.0,
        app: "fake-app".into(),
    };
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        if flag == "--help" || flag == "-h" {
            return Err(USAGE.into());
        }
        let value = it.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        let bad = |what: &str| format!("{flag}: {value:?} is not {what}");
        match flag.as_str() {
            "--url" => a.url = value,
            "--route" => a.route = value,
            "--app" => a.app = value,
            "--length" => a.length = value.parse().map_err(|_| bad("a number"))?,
            "--speed" => a.speed = value.parse().map_err(|_| bad("a number"))?,
            "--riders" => a.riders = value.parse().map_err(|_| bad("a whole number"))?,
            _ => return Err(format!("unknown option {flag}\n{USAGE}")),
        }
    }
    if a.length <= 0.0 {
        return Err("--length must be positive".into());
    }
    Ok(a)
}

/// Moves a rider forward by `speed * dt` metres, rolling over into the next lap at `length`.
fn advance(lap: u32, distance: f64, speed: f64, dt: f64, length: f64) -> (u32, f64) {
    let d = distance + speed * dt;
    if d >= length {
        let laps = (d / length).floor();
        (lap + laps as u32, d - laps * length)
    } else {
        (lap, d)
    }
}

struct Bot {
    id: String,
    name: String,
    lap: u32,
    distance: f64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("obn-fake-app: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let (ws, _) = tokio_tungstenite::connect_async(args.url.as_str())
        .await
        .map_err(|e| format!("cannot connect to {}: {e}", args.url))?;
    let (mut tx, mut rx) = ws.split();
    let send = |v: Value| Message::text(v.to_string());

    tx.send(send(
        json!({"type":"hello","app":{"id":args.app,"version":env!("CARGO_PKG_VERSION")}}),
    ))
    .await
    .map_err(|e| e.to_string())?;
    tx.send(send(json!({"type":"subscribe","route":args.route,"room":"public"})))
        .await
        .map_err(|e| e.to_string())?;

    let mut bots: Vec<Bot> = (0..args.riders)
        .map(|i| Bot {
            id: format!("fake-{}", i + 1),
            name: format!("Fake {}", i + 1),
            lap: 0,
            distance: args.length * f64::from(i) / f64::from(args.riders),
        })
        .collect();
    for b in &bots {
        let join = json!({
            "type":"join","rider":b.id,"route":args.route,"room":"public","name":b.name,
            "lap":b.lap,"distance":b.distance,"speed":args.speed,
        });
        tx.send(send(join)).await.map_err(|e| e.to_string())?;
    }

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let riders: Vec<Value> = bots
                    .iter_mut()
                    .map(|b| {
                        (b.lap, b.distance) = advance(b.lap, b.distance, args.speed, 1.0, args.length);
                        json!({"rider":b.id,"lap":b.lap,"distance":b.distance,"speed":args.speed})
                    })
                    .collect();
                if riders.is_empty() {
                    continue;
                }
                tx.send(send(json!({"type":"update","riders":riders}))).await.map_err(|e| e.to_string())?;
            }
            _ = tokio::signal::ctrl_c() => {
                for b in &bots {
                    // Best effort: the server also expires riders on its own.
                    let _ = tx.send(send(json!({"type":"leave","rider":b.id}))).await;
                }
                let _ = tx.close().await;
                println!("sent leave for {} riders, bye", bots.len());
                return Ok(());
            }
            msg = rx.next() => match msg {
                None => return Err("connection closed".into()),
                Some(Err(e)) => return Err(format!("connection error: {e}")),
                Some(Ok(Message::Text(t))) => report(t.as_str()),
                Some(Ok(Message::Close(f))) => return Err(format!("server closed the connection: {f:?}")),
                Some(Ok(_)) => {}
            },
        }
    }
}

fn report(text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match v["type"].as_str() {
        Some("joined") => {
            let r = &v["rider"];
            println!(
                "joined {:?} of {} as {}",
                r["name"].as_str().unwrap_or("?"),
                r["app"].as_str().unwrap_or("?"),
                r["id"].as_str().unwrap_or("?")
            );
        }
        Some("left") => println!(
            "left {} {}",
            v["id"].as_str().unwrap_or("?"),
            v["reason"].as_str().unwrap_or("?")
        ),
        Some("error") => println!(
            "error {}: {}",
            v["code"].as_str().unwrap_or("?"),
            v["message"].as_str().unwrap_or("?")
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::advance;

    #[test]
    fn moves_forward_within_a_lap() {
        assert_eq!(advance(0, 100.0, 10.0, 1.0, 250.0), (0, 110.0));
    }

    #[test]
    fn wraps_into_the_next_lap() {
        assert_eq!(advance(0, 245.0, 10.0, 1.0, 250.0), (1, 5.0));
        assert_eq!(advance(2, 249.0, 10.0, 1.0, 250.0), (3, 9.0));
    }
}
