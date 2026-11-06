# Reference Server v0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the OpenBikeNet v0 server: WebSocket presence for shared routes, per-app statistics at `/v0/stats`, and an `obn-fake-app` CLI for testing.

**Architecture:** A Rust workspace with a lib+bin crate `obn-server` and a bin crate `obn-fake-app`. Pure modules (`protocol`, `routes`, `presence`, `metrics`) are tested without a network. `ws` wires them to axum. `presence` returns addressed deliveries (`Vec<Delivery>`), and a `Hub` of per-connection channels sends them. The protocol repo is a git submodule at `protocol/`, which supplies the routes and the test examples.

**Tech Stack:** Rust (edition 2021, MSRV 1.87), axum 0.8 (`ws`), tokio 1, serde/serde_json, rusqlite 0.40 (`bundled`), time 0.3, tracing; tokio-tungstenite 0.29 and futures-util 0.3 for tests and the fake app. These are the versions OpenRadlr's server uses.

**Spec:** `protocol/spec/v0.md` (protocol repo, branch `spec-v0-draft`) and `docs/design.md` (this repo).

## Global Constraints

- **Prerequisite:** the protocol plan (`protocol/docs/superpowers/plans/2026-11-06-protocol-v0-artifacts.md`) is done. Until the protocol branch is pushed, add the submodule from the local path (`git submodule add ../protocol protocol` run from `/root/projects/OpenBikeNet/server`, then check out `spec-v0-draft` in it). Switch the URL to `https://github.com/OpenBikeNet/protocol` before pushing.
- Work on the branch `design-v0`. Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Do not push.
- Workspace settings are copied from OpenRadlr (`/root/projects/Open-Ride/server/Cargo.toml`): `resolver = "2"`, `license = "AGPL-3.0-or-later"`, `rust-version = "1.87"`, `unsafe_code = "forbid"`, clippy `all` warn, and the same release profile. Copy its `rustfmt.toml` too.
- Every change passes `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
- Spec values: protocol minor version `"0.1"`; limits `max_update_hz` 4, `rider_timeout_s` 10, `max_riders` 200, `max_subscriptions` 16, `max_message_bytes` 65536; 10 connections per IP; `hello` within 10 s; ping every 15 s; close after 30 s without a pong; default room `public`; server rider IDs and connection IDs at most 16 characters.
- Choices the spec leaves open, decided here: rider IDs are `r` + the lowercase hex of a counter starting at 1 (`r1`, `r2`, …), and connection IDs are `c` + hex. A rider entry is rate-limited if it comes less than `0.8 × 1000 / max_update_hz` ms (200 ms) after that rider's last accepted state; the 0.8 allows for network jitter. More than 20 non-fatal errors within 10 s means `too_many_errors`. Each connection's outgoing queue holds 1024 messages, and a connection whose queue is full is closed.
- Error mapping (applies everywhere): a missing field or wrong JSON type → `invalid_message`; a number that is out of range or not an integer where an integer is required, a string that is too long or too short, or a name with control characters → `invalid_value`; a bad app ID format → `invalid_app_id`; a bad room format → `invalid_room`; an unknown or malformed route ID → `unknown_route`.
- Privacy (spec 10): log app IDs and connection IDs, never names, rider values or IP addresses. Only `metrics` writes to disk.

## Review Focus

1. **Two apps use the same rider value** (both call their rider `u-1`). Riders are scoped per connection, so the two must not collide. Test `same_rider_value_on_two_connections` (Task 3).
2. **A rider exactly on the start line at the wrong end** (`distance` 250 on the 250 m velodrome) must be `invalid_value`, while 249.99 is fine. Test `distance_equal_to_length_is_rejected` (Task 4).
3. **An app reconnects after a drop.** Others get `left` with `disconnect`, and the rejoined rider gets a new ID. Test `reconnect_gives_new_id` (Task 6).
4. **A slow app stops reading.** The server must not block other connections: `Hub::deliver` drops it instead. Test `full_queue_closes_connection` (Task 6).
5. **Joining a rider on a second route without leaving first** must give `rider_exists`, not a silent move. Test `join_twice_on_other_route` (Task 3).

---

### Task 1: Workspace, submodule, CI and `protocol` module

**Files:**
- Create: `Cargo.toml` (workspace), `rustfmt.toml`, `.gitignore` (`target/`, `*.sqlite`), `.gitmodules` (via `git submodule add`)
- Create: `crates/server/Cargo.toml` (package `obn-server`, lib + `[[bin]] obn-server`), `crates/server/src/lib.rs`, `crates/server/src/main.rs` (empty `fn main() {}` for now)
- Create: `crates/server/src/protocol.rs`
- Test: `crates/server/tests/protocol_examples.rs`
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces (`obn_server::protocol`):
  - `enum ErrorCode` with the 14 codes of spec 7.6, serialized in snake_case; `fn as_str(self) -> &'static str`; `fn fatal(self) -> bool` (true for `hello_required`, `invalid_app_id`, `message_too_large`, `too_many_errors`)
  - `struct ProtoError { code: ErrorCode, message: String, rider: Option<String>, ref_: Option<String> }`
  - `struct AppInfo { id: String, version: String, name: Option<String>, url: Option<String> }`
  - `struct RiderState { lap: u32, distance: f64, speed: f64, power: Option<u16>, cadence: Option<u16>, heart_rate: Option<u16> }` (Serialize; `None` fields skipped)
  - `struct Join { rider: String, route: String, room: String, name: String, state: RiderState }`
  - `struct UpdateEntry { rider: String, state: RiderState }`
  - `enum AppMsg { Hello(AppInfo), Subscribe { route: String, room: String }, Unsubscribe { route: String, room: String }, Join(Join), Update(Vec<serde_json::Value>), Leave { rider: String } }`
  - `struct Envelope { ref_: Option<String>, msg: AppMsg }`
  - `fn parse_app_msg(text: &str) -> Result<Envelope, ProtoError>`: checks all fields except the `update` entries and route existence; a missing `room` becomes `"public"`
  - `fn parse_update_entry(v: &serde_json::Value) -> Result<UpdateEntry, ProtoError>`: sets `rider` on the error when the entry has a string `rider`
  - `struct Limits { max_update_hz: u32, rider_timeout_s: u32, max_riders: u32, max_subscriptions: u32, max_message_bytes: u32 }`, Serialize, `impl Default` with the spec values
  - `enum LeftReason { Leave, Timeout, Disconnect }` (snake_case)
  - `struct RiderView { id: String, app: String, name: String, #[serde(flatten)] state: RiderState }`, `struct RiderUpdate { id: String, #[serde(flatten)] state: RiderState }`
  - `enum ServerMsg` (`#[serde(tag = "type", rename_all = "snake_case")]`): `Welcome { protocol: &'static str, connection: String, limits: Limits }`, `Snapshot { route, room, riders: Vec<RiderView> }`, `Joined { route, room, rider: RiderView }`, `Updated { route, room, riders: Vec<RiderUpdate> }`, `Left { route, room, id, reason: LeftReason }`, `Error { code: ErrorCode, message: String, #[serde(rename = "ref")] ref_: Option<String>, rider: Option<String>, fatal: bool }` (`None` fields skipped)
  - `impl From<ProtoError> for ServerMsg`
  - `fn is_valid_app_id(s: &str) -> bool`, `fn is_valid_room(s: &str) -> bool`, `fn is_valid_route_id(s: &str) -> bool` (hand-written checks for the spec patterns, no regex crate)

- [ ] **Step 1: Set up the workspace, the submodule and CI**

`git submodule add ../protocol protocol` (see Global Constraints), workspace files, and dependencies: `serde`, `serde_json` now; the rest when a task needs them. `ci.yml` is OpenRadlr's `server` job with the same pinned action SHAs (`/root/projects/Open-Ride/.github/workflows/ci.yml`), plus `with: submodules: true` on checkout and no `working-directory`.

Run: `cargo build --workspace`
Expected: builds.

- [ ] **Step 2: Write the failing tests in `tests/protocol_examples.rs`**

```rust
const EXAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../protocol/examples");

#[test]
fn valid_app_examples_parse() // every file in examples/app: parse_app_msg is Ok,
                              // and for update every entry passes parse_update_entry

#[test]
fn invalid_app_examples_give_their_code() // for every examples/invalid/app/<code>--*.json:
    // the first error from parse_app_msg, or else from the update entries,
    // has code.as_str() == <code>

#[test]
fn server_messages_match_examples() // build welcome (connection "c9Qe4", Limits::default()),
    // snapshot, joined, updated, left, error with the values of examples/server/*.json,
    // serialize, compare as serde_json::Value to the file

#[test]
fn ref_is_kept_on_errors() // parse_app_msg(r#"{"type":"leave","ref":"7"}"#)
    // -> Err with code InvalidMessage and ref_ == Some("7")

#[test]
fn room_defaults_to_public() // subscribe without room -> room == "public"
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p obn-server --test protocol_examples`
Expected: compile errors (the module doesn't exist yet).

- [ ] **Step 4: Implement `protocol.rs`**

Parse to `serde_json::Value` first. Pull out `ref` (a string of at most 64 characters, otherwise `invalid_value`) and `type` (missing or not a string → `invalid_message`; unknown → `unknown_type`), then read the fields by hand following the error mapping in Global Constraints. Don't use serde-derive `Deserialize` for app messages: its errors don't separate `invalid_message` from `invalid_value`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p obn-server --test protocol_examples`
Expected: PASS. Then run fmt and clippy as in Global Constraints and expect them clean.

- [ ] **Step 6: Commit**

```bash
git add .
git commit -m "Add workspace, protocol submodule and protocol module"
```

### Task 2: `routes` module

**Files:**
- Create: `crates/server/src/routes.rs`

**Interfaces:**
- Consumes: `protocol::is_valid_route_id`.
- Produces:
  - `struct Route { id: String, name: String, length: f64, #[serde(rename = "loop")] looped: bool, profile: Vec<[f64; 2]> }` (Deserialize)
  - `struct Routes` with `fn load_dir(dir: &Path) -> Result<Routes, String>`, `fn from_routes(routes: Vec<Route>) -> Result<Routes, String>` and `fn get(&self, id: &str) -> Option<&Route>`
  - `fn check(route: &Route) -> Result<(), String>`: the profile rules of spec 4.1 (first distance 0, last distance = `length`, strictly increasing distances, equal end elevations on a loop, valid ID)

- [ ] **Step 1: Write the failing unit tests (in `routes.rs`)**

- `loads_velodrome_from_protocol_repo`: `load_dir("<manifest>/../../protocol/routes")` has `velodrome-250` with `length == 250.0` and `looped`.
- `rejects_bad_profiles`: `check` gives `Err` for each of the five defects in the protocol repo's `tools/fixtures/invalid-routes/` (load them from there).
- `rejects_file_name_not_matching_id` and `rejects_duplicate_ids`.

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-server routes`
Expected: compile errors, or FAIL.

- [ ] **Step 3: Implement `routes.rs`**

`load_dir` reads every `*.json` file, deserializes it, runs `check`, compares the file stem with `id`, and errors on duplicates. Error strings name the file.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p obn-server routes`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src
git commit -m "Add routes module"
```

### Task 3: `presence`: connections, subscriptions, join, leave, disconnect

**Files:**
- Create: `crates/server/src/presence.rs`
- Modify: `docs/design.md` (Modules: replace the "Fan-out: each room has a `tokio::sync::broadcast` channel…" paragraph with: `presence` returns the messages addressed to each connection, and the `ws` hub sends them)

**Interfaces:**
- Consumes: `protocol::{AppInfo, Join, RiderState, RiderView, ServerMsg, LeftReason, ProtoError, ErrorCode, Limits}`, `routes::Routes`.
- Produces:
  - `type ConnId = u64`
  - `struct Delivery { to: ConnId, msg: ServerMsg }`
  - `struct AppCounts { connections: u32, riders: u32 }`
  - `struct Presence` with:
    - `fn new(routes: Routes, limits: Limits) -> Self`
    - `fn connect(&mut self, conn: ConnId, app: AppInfo)`
    - `fn subscribe(&mut self, conn: ConnId, route: &str, room: &str) -> Result<Vec<Delivery>, ProtoError>` (the snapshot for `conn`)
    - `fn unsubscribe(&mut self, conn: ConnId, route: &str, room: &str)`
    - `fn join(&mut self, conn: ConnId, join: Join, now: Instant) -> Result<Vec<Delivery>, ProtoError>`
    - `fn leave(&mut self, conn: ConnId, rider: &str) -> Result<Vec<Delivery>, ProtoError>`
    - `fn disconnect(&mut self, conn: ConnId) -> Vec<Delivery>`
    - `fn live_counts(&self) -> BTreeMap<String, AppCounts>` (keyed by app ID)

  Errors leave `ref_` as `None`; the caller sets it. Subscribers are kept in a `BTreeSet<ConnId>` so delivery order is deterministic. Riders without a `connect` are a programming error (`debug_assert`).

- [ ] **Step 1: Write the failing unit tests**

Use a helper `fn presence() -> Presence` (with the velodrome loaded from the submodule, as in Task 2) and `fn join_msg(rider, name) -> Join` at distance 0.

- `late_subscriber_gets_snapshot`: conn 1 joins `u-1` "Anna"; conn 2 subscribes → one `Snapshot` to 2 with one rider `{id: "r1", app: <conn 1 app id>, name: "Anna"}`.
- `snapshot_excludes_own_riders`: conn 1 joins, conn 1 subscribes → the snapshot has no riders.
- `join_notifies_other_subscribers_only`: conns 1 and 2 are subscribed, 1 joins → exactly one `Joined`, to 2.
- `rooms_are_separate`: a subscriber to room `a` gets nothing for a join in `public`.
- `same_rider_value_on_two_connections`: conns 1 and 2 both join `u-1` → both Ok, with IDs `r1` and `r2`.
- `join_twice_on_other_route`: a second join of `u-1` on conn 1 (with a second test route loaded through `Routes::from_routes`) → `RiderExists`.
- `join_unknown_route` → `UnknownRoute`; `subscribe_unknown_route` → `UnknownRoute`.
- `join_limit`: with `Limits { max_riders: 2, .. }`, the third join → `LimitExceeded`; likewise `max_subscriptions` for subscribe.
- `leave_notifies_with_reason_leave`; `leave_unknown_rider` → `UnknownRider`.
- `disconnect_removes_riders_and_subscriptions`: conn 1 has two riders and conn 2 is subscribed → two `Left { reason: Disconnect }` to 2; afterwards `live_counts` has no entry for conn 1's app.
- `unsubscribe_stops_events`.
- `live_counts_per_app`: two connections with the same app ID and three riders → `{connections: 2, riders: 3}`.

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-server presence`
Expected: FAIL (does not compile).

- [ ] **Step 3: Implement these methods of `Presence`**

`join` checks the rider state against the route (`distance < length`, else `InvalidValue` with `rider` set). Each rider stores its `RiderState`, the time of its last state (`now`), and its server ID.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p obn-server presence`
Expected: PASS.

- [ ] **Step 5: Update `docs/design.md` as in Files, then commit**

```bash
git add crates/server/src docs/design.md
git commit -m "Add presence: subscriptions, join, leave, disconnect"
```

### Task 4: `presence`: update, rate limit, timeout

**Files:**
- Modify: `crates/server/src/presence.rs`

**Interfaces:**
- Consumes: Task 3, `protocol::{UpdateEntry, RiderUpdate}`.
- Produces:
  - `fn update(&mut self, conn: ConnId, entries: Vec<UpdateEntry>, now: Instant) -> (Vec<Delivery>, Vec<ProtoError>)`: one `Updated` per route+room per call, sent to every subscriber except `conn`, holding the accepted entries of that room
  - `fn expire(&mut self, now: Instant) -> Vec<Delivery>`: removes riders whose last state is older than `rider_timeout_s`, with reason `Timeout`

- [ ] **Step 1: Write the failing unit tests**

- `update_batches_per_room`: conn 1 has riders in `public` (×2) and `a` (×1), and conn 2 is subscribed to both → one call gives two `Updated` to conn 2, one with 2 riders and one with 1.
- `update_no_echo`: conn 1 is subscribed itself and gets nothing.
- `distance_equal_to_length_is_rejected`: distance 250.0 → one `InvalidValue` error with `rider == Some("u-1")`, and no delivery; 249.99 → delivered.
- `bad_entry_does_not_block_others`: two entries, one with an unknown rider → one `UnknownRider` error, and the other is delivered.
- `rate_limit`: updates at t0, t0+150 ms and t0+200 ms → the second gives `RateLimited`, the third is accepted.
- `timeout_after_rider_timeout_s`: a join at t0; `expire(t0 + 9.9 s)` → nothing; `expire(t0 + 10 s)` → `Left { reason: Timeout }`. An update at t0 + 5 s pushes the timeout to t0 + 15 s.
- `optional_fields_only_when_sent`: an entry without `power` gives a `RiderUpdate` with `state.power == None`.

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-server presence`
Expected: the new tests FAIL.

- [ ] **Step 3: Implement `update` and `expire`**

- [ ] **Step 4: Run the tests**

Run: `cargo test -p obn-server presence`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/presence.rs
git commit -m "Add presence updates, rate limit and timeouts"
```

### Task 5: `metrics` module

**Files:**
- Create: `crates/server/src/metrics.rs`
- Modify: `crates/server/Cargo.toml` (`rusqlite = { version = "0.40", features = ["bundled"] }`, `time = "0.3"`)

**Interfaces:**
- Consumes: `presence::AppCounts`.
- Produces:
  - `struct Metrics` with:
    - `fn open(path: &Path) -> rusqlite::Result<Self>`, `fn open_in_memory() -> rusqlite::Result<Self>` (both create the tables)
    - `fn record_hello(&self, app: &str, version: &str, day: &str) -> rusqlite::Result<()>`: sets `first_seen` if it's a new app, sets `last_seen`, records the version for that day
    - `fn add_minute(&self, live: &BTreeMap<String, AppCounts>, day: &str) -> rusqlite::Result<()>`: adds each app's `riders` to that day's rider-minutes, and sets `last_seen = day` for apps with `connections > 0`
    - `fn stats(&self, live: &BTreeMap<String, AppCounts>, today: &str) -> rusqlite::Result<Stats>`
  - `struct Stats { apps: Vec<AppStats> }` and `struct AppStats { id, versions: Vec<String>, connections: u32, riders: u32, first_seen: String, last_seen: String, rider_minutes_30d: u64 }`, Serialize; apps sorted by ID, versions sorted
  - `fn today_utc() -> String` (`time::OffsetDateTime::now_utc().date()`, formatted as `YYYY-MM-DD`)

Tables: `apps(id TEXT PRIMARY KEY, first_seen TEXT, last_seen TEXT)`, `versions(app TEXT, version TEXT, last_day TEXT, PRIMARY KEY(app, version))`, `minutes(app TEXT, day TEXT, minutes INTEGER, PRIMARY KEY(app, day))`. The 30-day window is `day >= date(today, '-29 days')` in SQL, and the same for versions.

- [ ] **Step 1: Write the failing unit tests**

- `stats_matches_spec_example`: `record_hello("openradlr", "0.3.1", "2026-11-20")`, then `add_minute` once on 2026-11-20 with `{openradlr: {connections: 1, riders: 412}}` and once on 2026-11-21 with `{openradlr: {connections: 1, riders: 400}}`, then `stats(live = {openradlr: {connections: 1, riders: 4}}, "2026-11-21")` → serialized, it equals `protocol/examples/stats.json`.
- `minutes_older_than_30_days_drop_out`: minutes on 2026-10-01 don't count for today 2026-11-21.
- `apps_without_live_connection_show_zero`.
- `reopen_keeps_data`: write to a temp file, reopen it, the stats are the same.

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-server metrics`
Expected: FAIL.

- [ ] **Step 3: Implement `metrics.rs`**

- [ ] **Step 4: Run the tests**

Run: `cargo test -p obn-server metrics`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server
git commit -m "Add per-app metrics in SQLite"
```

### Task 6: `ws`: config, hub, connection handling, `/v0/stats`

**Files:**
- Create: `crates/server/src/config.rs`, `crates/server/src/ws.rs`
- Modify: `crates/server/Cargo.toml` (axum with `ws`, tokio, futures-util, tracing; dev: tokio-tungstenite)
- Test: `crates/server/tests/ws.rs`

**Interfaces:**
- Consumes: all earlier modules.
- Produces:
  - `struct Config { bind: SocketAddr, routes_dir: PathBuf, db_path: PathBuf, trust_proxy: bool, hello_timeout: Duration, ping_interval: Duration, pong_timeout: Duration, max_connections_per_ip: u32, limits: Limits }` with `fn from_env() -> Result<Config, String>` (`OBN_BIND` default `0.0.0.0:8080`, `OBN_ROUTES_DIR` default `protocol/routes`, `OBN_DB` default `obn.sqlite`, `OBN_TRUST_PROXY=1` turns on trust in the `X-Forwarded-For` header) and `impl Default` with the spec values
  - `struct Hub` with `fn register(&self, conn: ConnId) -> mpsc::Receiver<ServerMsg>` (capacity 1024), `fn unregister(&self, conn: ConnId)`, `fn deliver(&self, deliveries: Vec<Delivery>)` (uses `try_send`; when a queue is full, it unregisters that connection, so its writer sees the channel close and closes the socket)
  - `struct AppState { config, presence: Mutex<Presence>, hub: Hub, metrics: Mutex<Metrics>, ip_counts: Mutex<HashMap<IpAddr, u32>>, next_conn: AtomicU64 }`, where `Mutex` is `std::sync::Mutex`, never held across `.await`
  - `fn router(state: Arc<AppState>) -> axum::Router` with `GET /v0` (WebSocket upgrade) and `GET /v0/stats` (JSON from `metrics.stats(presence.live_counts(), today_utc())`). The caller serves it with `into_make_service_with_connect_info::<SocketAddr>()`.

Each connection does the following:
- Per-IP limit at upgrade: over the limit → HTTP 429.
- Set axum's `max_message_size` to 1 MiB and check `text.len() > limits.max_message_bytes` yourself, so that `message_too_large` can be sent before the close.
- The writer task drains the `Hub` receiver into the socket. The reader loop handles messages in this order: the hello timeout (`hello_required`), then `hello` → `presence.connect` + `metrics.record_hello` + `Welcome`, then dispatch.
- For `Update`, parse each entry with `parse_update_entry`, send its errors, and pass the valid ones to `presence.update`.
- Every `ProtoError` gets the envelope's `ref` and goes to the connection itself. A fatal error is sent, then the socket is closed with close code 1008.
- More than 20 non-fatal errors in 10 s → `too_many_errors` (fatal). A binary frame → close 1003.
- Pings every `ping_interval`; close if no pong for `pong_timeout`.
- On any exit: `presence.disconnect` → `hub.deliver`, `hub.unregister`, and decrement the IP count.

- [ ] **Step 1: Write the failing integration tests in `tests/ws.rs`**

Use a helper `async fn start(config: Config) -> SocketAddr`: it binds `127.0.0.1:0`, uses in-memory metrics and the submodule routes, and spawns `axum::serve`. Another helper `async fn app(addr, id) -> Client` connects and does hello/welcome. Tests use `hello_timeout: 300 ms` and similar short timings.

- `full_lifecycle`: A subscribes; B hellos, subscribes, joins `u-1`, and sends one update; A gets `Joined` then `Updated`; B sends leave; A gets `Left { reason: "leave" }`.
- `welcome_has_limits`: the `welcome` limits equal `Limits::default()` serialized, and `protocol == "0.1"`.
- `no_hello_closes`: connect, send nothing → `error hello_required, fatal: true`, then close.
- `first_message_not_hello_closes`: subscribe first → `hello_required`.
- `invalid_app_id_closes`.
- `second_hello_is_not_fatal`: a second `hello` → `error hello_repeated, fatal: false`, and the connection stays usable (a following subscribe gets a snapshot).
- `oversized_message_closes`: 70 000 bytes of text → `message_too_large`.
- `error_carries_ref`: `{"type":"leave","rider":"x","ref":"9"}` → `unknown_rider` with `ref == "9"`.
- `reconnect_gives_new_id`: B joins (gets ID `r1` at A) and drops its socket; A gets `Left { reason: "disconnect" }`; B reconnects and rejoins `u-1` → A gets `Joined` with `id == "r2"`.
- `per_ip_limit`: with `max_connections_per_ip: 2`, the third upgrade fails with HTTP 429.
- `stats_endpoint`: after B's hello as `openradlr` with one joined rider, `GET /v0/stats` (use a raw tokio `TcpStream` with an HTTP/1.1 request, or add `reqwest` as a dev-dependency) shows `connections: 1, riders: 1`.
- `full_queue_closes_connection` (unit test in `ws.rs`): register conn 1, deliver 1025 messages without reading → conn 1 is no longer registered and the 1025th `deliver` doesn't panic.

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-server --test ws`
Expected: FAIL (does not compile).

- [ ] **Step 3: Implement `config.rs` and `ws.rs`**

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace`
Expected: PASS. Then fmt and clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/server
git commit -m "Add WebSocket handling, hub and stats endpoint"
```

### Task 7: Binary, tickers and README

**Files:**
- Modify: `crates/server/src/main.rs`, `crates/server/src/lib.rs`
- Modify: `README.md` (status line plus a "Running it" section)

**Interfaces:**
- Consumes: `Config::from_env`, `Routes::load_dir`, `Metrics::open`, `router`, `AppState`.
- Produces: `pub fn spawn_tickers(state: Arc<AppState>)` in `ws.rs`: every 1 s `presence.expire(Instant::now())` → `hub.deliver`; every 60 s `metrics.add_minute(presence.live_counts(), today_utc())`.

- [ ] **Step 1: Write the failing test**

`ticker_expires_riders` in `tests/ws.rs`: with `limits.rider_timeout_s: 1`, B joins and then sends nothing. A gets `Left { reason: "timeout" }` within 3 s.

- [ ] **Step 2: Run it**

Run: `cargo test -p obn-server --test ws ticker_expires_riders`
Expected: FAIL (no `Left` arrives).

- [ ] **Step 3: Implement `spawn_tickers` and `main`**

`main` sets up `tracing_subscriber` with `env-filter` (default `info`), loads the config, routes and metrics (an error → print it and exit 1), calls `spawn_tickers`, and serves with a graceful shutdown on Ctrl-C/SIGTERM. Log the address and the loaded route IDs at startup.

- [ ] **Step 4: Run all tests and a manual start**

Run: `cargo test --workspace`
Expected: PASS.
Run: `cargo run -p obn-server`, and in another shell `curl -s localhost:8080/v0/stats`
Expected: `{"apps":[]}`.

- [ ] **Step 5: Update the README and commit**

README: change "Status: not started" to "Status: early draft, implements protocol v0", and add a "Running it" section (clone with `--recurse-submodules`, `cargo run -p obn-server`, the four `OBN_*` variables with their defaults, `cargo run -p obn-fake-app -- --riders 3`).

```bash
git add crates/server README.md
git commit -m "Add server binary with timeout and metrics tickers"
```

### Task 8: `obn-fake-app` CLI

**Files:**
- Create: `crates/fake-app/Cargo.toml` (package `obn-fake-app`; deps tokio, tokio-tungstenite, futures-util, serde_json)
- Create: `crates/fake-app/src/main.rs`

**Interfaces:**
- Consumes: nothing from `obn-server` on purpose. It builds JSON with `serde_json::json!`, so it reads like what another developer would write.
- Produces: the CLI `obn-fake-app [--url ws://127.0.0.1:8080/v0] [--route velodrome-250] [--length 250] [--riders 3] [--speed 11.0] [--app fake-app]`. It sends `hello` (version from `CARGO_PKG_VERSION`), `subscribe`, a `join` for each rider (`bot-1`…, names `Bot 1`…, spaced evenly around the lap), then one batched `update` per second. It prints a line for each `joined`/`left`/`error` it receives, and on Ctrl-C sends `leave` for every rider and exits.
  `fn advance(lap: u32, distance: f64, speed: f64, dt: f64, length: f64) -> (u32, f64)`

- [ ] **Step 1: Write the failing unit tests for `advance`**

- `advance(0, 100.0, 10.0, 1.0, 250.0) == (0, 110.0)`
- `advance(0, 245.0, 10.0, 1.0, 250.0) == (1, 5.0)`
- `advance(2, 249.0, 10.0, 1.0, 250.0) == (3, 9.0)`

- [ ] **Step 2: Run them**

Run: `cargo test -p obn-fake-app`
Expected: FAIL.

- [ ] **Step 3: Implement `advance` and the CLI (args parsed by hand, no clap)**

- [ ] **Step 4: Run the tests and a manual end-to-end check**

Run: `cargo test --workspace`
Expected: PASS.
Run the server, then `cargo run -p obn-fake-app -- --riders 2 --app fake-a` and, in a third shell, `cargo run -p obn-fake-app -- --riders 1 --app fake-b`.
Expected: the second fake app prints `joined` for `Bot 1` and `Bot 2` of `fake-a`, and `curl -s localhost:8080/v0/stats` lists `fake-a` with `riders: 2` and `fake-b` with `riders: 1`. After Ctrl-C on `fake-a`, the other one prints two `left … leave` lines.

- [ ] **Step 5: Commit**

```bash
git add crates/fake-app Cargo.toml Cargo.lock
git commit -m "Add obn-fake-app CLI"
```
