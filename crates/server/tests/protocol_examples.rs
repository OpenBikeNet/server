use std::fs;
use std::path::{Path, PathBuf};

use obn_server::protocol::*;
use serde_json::{json, Value};

const EXAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../protocol/examples");

/// Files in `dir` (relative to the examples directory), sorted by path.
fn json_files(dir: &str) -> Vec<PathBuf> {
    let path = Path::new(EXAMPLES).join(dir);
    let mut files: Vec<PathBuf> = fs::read_dir(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no examples in {}", path.display());
    files
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// JSON numbers `0` and `0.0` are the same number on the wire, but `serde_json::Value` tells them apart.
fn normalize(v: Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().expect("finite number")),
        Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, normalize(v))).collect()),
        other => other,
    }
}

/// The first error from parsing the message, or else from its update entries.
fn first_error(text: &str) -> Option<ProtoError> {
    match parse_app_msg(text) {
        Err(e) => Some(e),
        Ok(Envelope {
            msg: AppMsg::Update(entries),
            ..
        }) => entries.iter().find_map(|v| parse_update_entry(v).err()),
        Ok(_) => None,
    }
}

#[test]
fn valid_app_examples_parse() {
    for path in json_files("app") {
        let text = read(&path);
        let env = parse_app_msg(&text).unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
        if let AppMsg::Update(entries) = env.msg {
            for entry in &entries {
                parse_update_entry(entry).unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
            }
        }
    }
}

#[test]
fn invalid_app_examples_give_their_code() {
    for path in json_files("invalid/app") {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let code = name.split("--").next().unwrap();
        let err = first_error(&read(&path)).unwrap_or_else(|| panic!("{name}: expected error {code}, got none"));
        assert_eq!(err.code.as_str(), code, "{name}: {err:?}");
    }
}

fn expected(name: &str) -> Value {
    let path = Path::new(EXAMPLES).join("server").join(name);
    serde_json::from_str(&read(&path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn check(name: &str, msg: ServerMsg) {
    let got = serde_json::to_value(&msg).unwrap();
    assert_eq!(normalize(got), normalize(expected(name)), "{name}");
}

#[test]
fn server_messages_match_examples() {
    let state = |lap, distance, speed, power| RiderState {
        lap,
        distance,
        speed,
        power,
        cadence: None,
        heart_rate: None,
    };
    let view = |s| RiderView {
        id: "r7Kx2".into(),
        app: "incyclist".into(),
        name: "Ben".into(),
        state: s,
    };
    let route = || "velodrome-250".to_string();
    let room = || "public".to_string();

    check(
        "welcome.json",
        ServerMsg::Welcome {
            protocol: "0.1",
            connection: "c9Qe4".into(),
            limits: Limits::default(),
        },
    );
    check(
        "snapshot.json",
        ServerMsg::Snapshot {
            route: route(),
            room: room(),
            riders: vec![view(state(5, 201.3, 12.1, Some(260)))],
        },
    );
    check(
        "joined.json",
        ServerMsg::Joined {
            route: route(),
            room: room(),
            rider: view(state(0, 0.0, 0.0, None)),
        },
    );
    check(
        "updated.json",
        ServerMsg::Updated {
            route: route(),
            room: room(),
            riders: vec![RiderUpdate {
                id: "r7Kx2".into(),
                state: state(5, 213.4, 12.1, Some(262)),
            }],
        },
    );
    check(
        "left.json",
        ServerMsg::Left {
            route: route(),
            room: room(),
            id: "r7Kx2".into(),
            reason: LeftReason::Timeout,
        },
    );
    check(
        "error.json",
        ServerMsg::Error {
            code: ErrorCode::InvalidValue,
            message: "distance must be below 250".into(),
            ref_: Some("42".into()),
            rider: Some("u-1842".into()),
            fatal: false,
        },
    );
}

#[test]
fn ref_is_kept_on_errors() {
    let err = parse_app_msg(r#"{"type":"leave","ref":"7"}"#).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidMessage);
    assert_eq!(err.ref_.as_deref(), Some("7"));
}

#[test]
fn room_defaults_to_public() {
    let env = parse_app_msg(r#"{"type":"subscribe","route":"velodrome-250"}"#).unwrap();
    match env.msg {
        AppMsg::Subscribe { route, room } => {
            assert_eq!(route, "velodrome-250");
            assert_eq!(room, "public");
        }
        other => panic!("unexpected {other:?}"),
    }
}
