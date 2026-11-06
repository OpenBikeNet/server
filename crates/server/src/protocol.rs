//! Wire types of protocol v0 (`protocol/spec/v0.md`): parsing of app messages and serialization of
//! server messages.
//!
//! App messages are read by hand from a `serde_json::Value`, because serde-derive errors do not
//! separate `invalid_message` (missing field or wrong type) from `invalid_value` (out of range).

use serde::Serialize;
use serde_json::{Map, Value};

pub const DEFAULT_ROOM: &str = "public";

const MAX_REF: usize = 64;
const MAX_RIDER: usize = 64;
const MAX_NAME: usize = 32;
const MAX_VERSION: usize = 32;
const MAX_APP_NAME: usize = 32;
const MAX_APP_URL: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    HelloRequired,
    HelloRepeated,
    InvalidMessage,
    UnknownType,
    InvalidAppId,
    UnknownRoute,
    InvalidRoom,
    RiderExists,
    UnknownRider,
    InvalidValue,
    RateLimited,
    LimitExceeded,
    MessageTooLarge,
    TooManyErrors,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::HelloRequired => "hello_required",
            ErrorCode::HelloRepeated => "hello_repeated",
            ErrorCode::InvalidMessage => "invalid_message",
            ErrorCode::UnknownType => "unknown_type",
            ErrorCode::InvalidAppId => "invalid_app_id",
            ErrorCode::UnknownRoute => "unknown_route",
            ErrorCode::InvalidRoom => "invalid_room",
            ErrorCode::RiderExists => "rider_exists",
            ErrorCode::UnknownRider => "unknown_rider",
            ErrorCode::InvalidValue => "invalid_value",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::LimitExceeded => "limit_exceeded",
            ErrorCode::MessageTooLarge => "message_too_large",
            ErrorCode::TooManyErrors => "too_many_errors",
        }
    }

    /// True if the server closes the connection after this error.
    pub fn fatal(self) -> bool {
        matches!(
            self,
            ErrorCode::HelloRequired | ErrorCode::InvalidAppId | ErrorCode::MessageTooLarge | ErrorCode::TooManyErrors
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProtoError {
    pub code: ErrorCode,
    pub message: String,
    pub rider: Option<String>,
    pub ref_: Option<String>,
}

impl ProtoError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ProtoError {
            code,
            message: message.into(),
            rider: None,
            ref_: None,
        }
    }

    pub fn with_ref(mut self, ref_: Option<String>) -> Self {
        self.ref_ = ref_;
        self
    }

    pub fn with_rider(mut self, rider: Option<String>) -> Self {
        self.rider = rider;
        self
    }
}

fn invalid_message(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidMessage, message)
}

fn invalid_value(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidValue, message)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    pub id: String,
    pub version: String,
    pub name: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiderState {
    pub lap: u32,
    pub distance: f64,
    pub speed: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cadence: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heart_rate: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub rider: String,
    pub route: String,
    pub room: String,
    pub name: String,
    pub state: RiderState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateEntry {
    pub rider: String,
    pub state: RiderState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMsg {
    Hello(AppInfo),
    Subscribe {
        route: String,
        room: String,
    },
    Unsubscribe {
        route: String,
        room: String,
    },
    Join(Join),
    /// The raw rider entries; each is checked on its own with [`parse_update_entry`].
    Update(Vec<Value>),
    Leave {
        rider: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub ref_: Option<String>,
    pub msg: AppMsg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub max_update_hz: u32,
    pub rider_timeout_s: u32,
    pub max_riders: u32,
    pub max_subscriptions: u32,
    pub max_message_bytes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_update_hz: 4,
            rider_timeout_s: 10,
            max_riders: 200,
            max_subscriptions: 16,
            max_message_bytes: 65536,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftReason {
    Leave,
    Timeout,
    Disconnect,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiderView {
    pub id: String,
    pub app: String,
    pub name: String,
    #[serde(flatten)]
    pub state: RiderState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiderUpdate {
    pub id: String,
    #[serde(flatten)]
    pub state: RiderState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        protocol: &'static str,
        connection: String,
        limits: Limits,
    },
    Snapshot {
        route: String,
        room: String,
        riders: Vec<RiderView>,
    },
    Joined {
        route: String,
        room: String,
        rider: RiderView,
    },
    Updated {
        route: String,
        room: String,
        riders: Vec<RiderUpdate>,
    },
    Left {
        route: String,
        room: String,
        id: String,
        reason: LeftReason,
    },
    Error {
        code: ErrorCode,
        message: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        ref_: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rider: Option<String>,
        fatal: bool,
    },
}

impl From<ProtoError> for ServerMsg {
    fn from(e: ProtoError) -> Self {
        ServerMsg::Error {
            code: e.code,
            fatal: e.code.fatal(),
            message: e.message,
            ref_: e.ref_,
            rider: e.rider,
        }
    }
}

fn is_lower_alnum(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// `^[a-z0-9][a-z0-9-]{min_rest,max_rest}$`
fn is_slug(s: &str, min_rest: usize, max_rest: usize) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || !is_lower_alnum(b[0]) {
        return false;
    }
    let rest = &b[1..];
    (min_rest..=max_rest).contains(&rest.len()) && rest.iter().all(|&c| is_lower_alnum(c) || c == b'-')
}

/// `^[a-z0-9][a-z0-9-]{1,31}$`
pub fn is_valid_app_id(s: &str) -> bool {
    is_slug(s, 1, 31)
}

/// `^[a-z0-9][a-z0-9-]{0,31}$`
pub fn is_valid_room(s: &str) -> bool {
    is_slug(s, 0, 31)
}

/// `^[a-z0-9][a-z0-9-]{1,63}$`
pub fn is_valid_route_id(s: &str) -> bool {
    is_slug(s, 1, 63)
}

/// Parses one app message. Checks every field except the `update` entries and whether the route
/// exists. Errors carry the message's `ref` when it could be read.
pub fn parse_app_msg(text: &str) -> Result<Envelope, ProtoError> {
    let value: Value = serde_json::from_str(text).map_err(|_| invalid_message("not valid JSON"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_message("message must be a JSON object"))?;
    let ref_ = read_ref(obj)?;
    parse_body(obj)
        .map(|msg| Envelope {
            ref_: ref_.clone(),
            msg,
        })
        .map_err(|e| e.with_ref(ref_))
}

fn read_ref(obj: &Map<String, Value>) -> Result<Option<String>, ProtoError> {
    match obj.get("ref") {
        None => Ok(None),
        Some(Value::String(s)) if s.chars().count() <= MAX_REF => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(invalid_value("ref is longer than 64 characters")),
        Some(_) => Err(invalid_message("ref must be a string")),
    }
}

fn parse_body(obj: &Map<String, Value>) -> Result<AppMsg, ProtoError> {
    let ty = match obj.get("type") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(invalid_message("type must be a string")),
    };
    match ty {
        "hello" => parse_hello(obj),
        "subscribe" => Ok(AppMsg::Subscribe {
            route: route_field(obj)?,
            room: room_field(obj)?,
        }),
        "unsubscribe" => Ok(AppMsg::Unsubscribe {
            route: route_field(obj)?,
            room: room_field(obj)?,
        }),
        "join" => Ok(AppMsg::Join(Join {
            rider: rider_field(obj)?,
            route: route_field(obj)?,
            room: room_field(obj)?,
            name: name_field(obj)?,
            state: state_fields(obj)?,
        })),
        "update" => match obj.get("riders") {
            Some(Value::Array(a)) => Ok(AppMsg::Update(a.clone())),
            _ => Err(invalid_message("riders must be an array")),
        },
        "leave" => Ok(AppMsg::Leave {
            rider: rider_field(obj)?,
        }),
        other => Err(ProtoError::new(
            ErrorCode::UnknownType,
            format!("unknown type {:?}", truncate(other)),
        )),
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(32).collect()
}

/// A required string field; wrong type or missing is `invalid_message`.
fn req_str<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a str, ProtoError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(invalid_message(format!("{key} must be a string"))),
    }
}

/// An optional string field; wrong type is `invalid_message`.
fn opt_str<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, ProtoError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(invalid_message(format!("{key} must be a string"))),
    }
}

fn parse_hello(obj: &Map<String, Value>) -> Result<AppMsg, ProtoError> {
    let app = obj
        .get("app")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_message("app must be an object"))?;
    let id = req_str(app, "id")?;
    if !is_valid_app_id(id) {
        return Err(ProtoError::new(
            ErrorCode::InvalidAppId,
            "app.id must match ^[a-z0-9][a-z0-9-]{1,31}$",
        ));
    }
    let version = req_str(app, "version")?;
    let len = version.chars().count();
    if len == 0 || len > MAX_VERSION {
        return Err(invalid_value("app.version must be 1 to 32 characters"));
    }
    let name = opt_str(app, "name")?;
    if name.is_some_and(|n| n.chars().count() > MAX_APP_NAME) {
        return Err(invalid_value("app.name is longer than 32 characters"));
    }
    let url = opt_str(app, "url")?;
    if url.is_some_and(|u| u.chars().count() > MAX_APP_URL) {
        return Err(invalid_value("app.url is longer than 200 characters"));
    }
    Ok(AppMsg::Hello(AppInfo {
        id: id.to_string(),
        version: version.to_string(),
        name: name.map(str::to_string),
        url: url.map(str::to_string),
    }))
}

fn route_field(obj: &Map<String, Value>) -> Result<String, ProtoError> {
    let route = req_str(obj, "route")?;
    if !is_valid_route_id(route) {
        return Err(ProtoError::new(
            ErrorCode::UnknownRoute,
            "route is not a valid route ID",
        ));
    }
    Ok(route.to_string())
}

fn room_field(obj: &Map<String, Value>) -> Result<String, ProtoError> {
    match opt_str(obj, "room")? {
        None => Ok(DEFAULT_ROOM.to_string()),
        Some(room) if is_valid_room(room) => Ok(room.to_string()),
        Some(_) => Err(ProtoError::new(
            ErrorCode::InvalidRoom,
            "room must match ^[a-z0-9][a-z0-9-]{0,31}$",
        )),
    }
}

fn rider_field(obj: &Map<String, Value>) -> Result<String, ProtoError> {
    let rider = req_str(obj, "rider")?;
    let len = rider.chars().count();
    if len == 0 || len > MAX_RIDER {
        return Err(invalid_value("rider must be 1 to 64 characters"));
    }
    Ok(rider.to_string())
}

fn name_field(obj: &Map<String, Value>) -> Result<String, ProtoError> {
    let name = req_str(obj, "name")?;
    let len = name.chars().count();
    if len == 0 || len > MAX_NAME {
        return Err(invalid_value("name must be 1 to 32 characters"));
    }
    if name.chars().any(char::is_control) {
        return Err(invalid_value("name must not contain control characters"));
    }
    Ok(name.to_string())
}

/// A number that must be a whole number within `min..=max`. A non-number is `invalid_message`,
/// a fraction or an out-of-range number is `invalid_value`.
fn int_value(v: &Value, key: &str, min: f64, max: f64) -> Result<f64, ProtoError> {
    let n = v
        .as_f64()
        .filter(|_| v.is_number())
        .ok_or_else(|| invalid_message(format!("{key} must be a number")))?;
    if n.fract() != 0.0 {
        return Err(invalid_value(format!("{key} must be an integer")));
    }
    if n < min || n > max {
        return Err(invalid_value(format!("{key} must be {min} to {max}")));
    }
    Ok(n)
}

fn float_value(v: &Value, key: &str, min: f64, max: f64) -> Result<f64, ProtoError> {
    let n = v
        .as_f64()
        .filter(|_| v.is_number())
        .ok_or_else(|| invalid_message(format!("{key} must be a number")))?;
    if !n.is_finite() || n < min || n > max {
        return Err(invalid_value(format!("{key} must be {min} to {max}")));
    }
    Ok(n)
}

fn required<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a Value, ProtoError> {
    obj.get(key).ok_or_else(|| invalid_message(format!("{key} is missing")))
}

/// `lap`, `distance`, `speed` and the optional sensor fields. The upper bound of `distance` is the
/// route length, which the caller checks.
fn state_fields(obj: &Map<String, Value>) -> Result<RiderState, ProtoError> {
    let lap = int_value(required(obj, "lap")?, "lap", 0.0, f64::from(u32::MAX))? as u32;
    let distance = float_value(required(obj, "distance")?, "distance", 0.0, f64::MAX)?;
    let speed = float_value(required(obj, "speed")?, "speed", 0.0, 30.0)?;
    let optional = |key: &str, min: f64, max: f64| -> Result<Option<u16>, ProtoError> {
        obj.get(key)
            .map(|v| int_value(v, key, min, max).map(|n| n as u16))
            .transpose()
    };
    Ok(RiderState {
        lap,
        distance,
        speed,
        power: optional("power", 0.0, 3000.0)?,
        cadence: optional("cadence", 0.0, 250.0)?,
        heart_rate: optional("heart_rate", 20.0, 250.0)?,
    })
}

/// Parses one entry of an `update`. The error carries `rider` when the entry has a string `rider`.
pub fn parse_update_entry(v: &Value) -> Result<UpdateEntry, ProtoError> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid_message("update entry must be an object"))?;
    let rider_name = obj.get("rider").and_then(Value::as_str).map(str::to_string);
    let parse = || -> Result<UpdateEntry, ProtoError> {
        Ok(UpdateEntry {
            rider: rider_field(obj)?,
            state: state_fields(obj)?,
        })
    };
    parse().map_err(|e| e.with_rider(rider_name))
}
