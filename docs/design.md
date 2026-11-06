# Reference server design (protocol v0)

**Status: draft**, written together with the
[v0 specification](https://github.com/OpenBikeNet/protocol/blob/main/spec/v0.md). The spec says what
the server does. This file says how this implementation is built.

## Decisions

- **WebSocket only.** MQTT gives little for a few long-lived backend connections and would add a
  broker, a second binding and a second test set. Messages stay transport-neutral, so a binding
  can come later.
- **Stateful presence server.** Rider state lives in memory, so late joiners get a snapshot and
  riders of dropped apps are removed by the server instead of by every client. The only data on
  disk are per-app daily statistics.
- **Rust, axum + tokio**, the same stack as the OpenRadlr server, which is the first app to use it.

## Modules

One binary in a Cargo workspace. Each module has one job:

| Module | Job | Depends on |
|---|---|---|
| `protocol` | message types (serde), field validation, error codes; no I/O | – |
| `routes` | loads `routes/*.json` from the protocol repo (copied in at build time), checks the profile rules | `protocol` |
| `presence` | routes → rooms → riders; join, update, leave, timeout, snapshot; returns the events to send | `protocol`, `routes` |
| `metrics` | live counters per app ID, daily rider-minutes and versions in SQLite, `/v0/stats` | – |
| `ws` | axum WebSocket handler: hello timeout, limits, rate limiting, fan-out, pings, disconnect cleanup | all of the above |

`presence` is a plain struct with methods that take a message and return the outgoing events. It
has no async code and no clock of its own (the time is passed in), so timeouts are tested without
waiting.

Fan-out: each room has a `tokio::sync::broadcast` channel. A connection task subscribes to the
rooms its app watches and drops events for its own riders.

## Testing

- **Unit (`protocol`):** every example in the protocol repo's `examples/` parses, and each invalid
  example gives the documented error code.
- **Unit (`presence`):** late join gets a snapshot, timeout after `rider_timeout_s`, disconnect
  removes all riders of a connection, rooms are separate, no echo of own riders, `rider_exists`,
  `unknown_rider`, out-of-range values, per-rider rate limit.
- **Integration:** start the server on a random port, connect two WebSocket clients as two apps,
  check the full lifecycle of spec section 3.4, plus `/v0/stats`.
- **`obn-fake-app` CLI:** puts N bot riders on a route at a given speed. For manual tests, load
  tests and as an example for app developers.

## First app: OpenRadlr

Planned separately in the OpenRadlr repository. Outline:

- The OpenRadlr server opens one outgoing connection, sends `hello` with `app.id = "openradlr"` and
  subscribes to `velodrome-250`.
- OpenRadlr gets a 250 m flat oval route in its own look.
- Human riders on that route are sent with `join` and one batched `update` per second from the sim
  state. Bots are not sent.
- Remote riders appear as riders without physics, interpolated on the client, with their app name.
- If the connection drops it reconnects with backoff. Local riding is never affected.

## Order of work

1. Protocol repo: spec, JSON Schemas for route files and messages, examples, `routes/velodrome-250.json`,
   `apps.md`, CI that validates examples and routes.
2. This server.
3. OpenRadlr integration.
