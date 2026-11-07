# Feedback from the first app implementation (OpenRadlr)

Date: 2026-11-07. OpenRadlr implemented protocol v0 against this server (OpenRadlr PR
[#37](https://github.com/Lethonium/OpenRadlr/pull/37), code in `server/crates/server/src/obn/`
there) and tested it live against `make dev` here plus `obn-fake-app`. These are the points it
raised, most important first. All five are resolved; each section ends with what was decided.
The spec changes are protocol v0.2 (`welcome.protocol` is now `"0.2"`).

Affects: **S** = this server, **P** = the protocol spec (`protocol/spec/v0.md`), **F** = the fake app.

## 1. A join followed by an update is rate-limited, and that can loop (S, P)

**What happened:** OpenRadlr sent `join` and, in the same second, an `update` for the new rider.
The server answered `rate_limited`.

**Cause:** in `crates/server/src/presence.rs` (around line 210) the rate limit compares against
`r.last`, which the join sets. An update within `0.8 / max_update_hz` s of the join is dropped.
The spec doesn't say a join counts as an update; section 3.4 step 4 ("send `join` … then
`update` messages") reads as if join-then-update is fine.

**Why it matters more than one dropped entry:** `rate_limited` counts towards
`too_many_errors` (`ws.rs`: more than `MAX_ERRORS_IN_WINDOW = 20` errors in 10 s is fatal). An
app that reconnects with 20+ riders and sends join + update for each is disconnected; on
reconnect it joins everyone again and is disconnected again: a loop. A group ride start does the
same.

**Options:**
- Server: a join does not start the rate-limit clock (lean), and/or `rate_limited` does not count
  towards `too_many_errors` (it is a pacing hint, not a misbehaving client).
- Spec: state explicitly whether the join's state counts as the rider's first update for
  `max_update_hz`.

OpenRadlr worked around it (sends only `join` in the frame a rider joins), so it works either way.

**Decided:** both server options. A join no longer starts the rate-limit clock (it still resets
the timeout), and `rate_limited` does not count towards `too_many_errors`. Spec 6.3 says the join
state does not count towards `max_update_hz`; spec 7.6 says `rate_limited` does not count.

## 2. Names can carry invisible and bidi characters (S, P)

The spec forbids only "control characters" (6.3), and `protocol.rs` (~line 461) checks
`char::is_control`, which is Unicode category Cc only. Format characters (Cf) pass: bidi overrides
such as U+202E, zero-width space/joiner, etc. They let one rider disguise or reverse their name in
every other app.

**Proposal:** spec: names MUST NOT contain Cc or Cf characters (maybe also unassigned / private
use), and receivers MUST display names as plain text, never as markup. Server: reject such names
with `invalid_value`. OpenRadlr now cleans incoming names itself, but every app shouldn't have to
rely on its own filter.

**Decided:** spec 6.3.1 forbids Cc, Cf, Co, Zl and Zp, except U+200D ZERO WIDTH JOINER (emoji
sequences need it), and names that are only whitespace. Unassigned characters are allowed, since
what is unassigned depends on the Unicode version. Receivers MUST show names as plain text. The
server and both message schemas use the same Unicode 15.0 ranges. Tag characters (U+E0020…)
are rejected too, which also rejects the England/Scotland/Wales flag emoji.

## 3. Unknown server message types (P)

3.2 says receivers ignore unknown *fields*, and the server answers an unknown `type` from an app
with an error. Nothing says what an app does with an unknown `type` from the server. Adding
"apps MUST ignore server messages with an unknown `type`" lets v0.x add messages without breaking
older apps. (OpenRadlr already ignores them, logging at debug.)

**Decided:** spec 3.2, broadened: apps MUST ignore unknown server message types and MUST accept
unknown `error.code` (acting on `fatal`) and `left.reason` values.

## 4. How an app detects a dead server (P)

3.5 defines server pings every 15 s and closing on missing pongs, but gives apps no rule for a
server that goes silent (half-open TCP). Suggest one line such as: "An app MAY treat a connection
with no ping for 45 seconds as closed and reconnect." OpenRadlr has no watchdog yet and would
build it on that rule.

**Decided:** browsers cannot see WebSocket pings, so v0.2 adds an app-level `ping` (spec 6.6)
that the server answers with `pong` (7.7), carrying the `ref`. Spec 3.5: an app that has received
nothing for 20 s SHOULD send `ping` and MAY treat the connection as closed if nothing arrives in
10 s; apps that see WebSocket pings MAY use a 45 s rule instead. A v0.1 server answers `ping`
with `unknown_type`, which also shows it is alive.

## 5. Fake app riders look like local bots (F)

`obn-fake-app` names its riders `Bot 1…N`. In OpenRadlr (which also has bots) a fake rider shows
as "Bot 3 · incyclist", which reads like a local bot. Names such as `Fake 1…N`, or taking the
`--app` value into account, would avoid the confusion.

**Decided:** fake riders are now `Fake 1…N` (rider values `fake-1…N`).

## Smaller observations (no action needed unless you want)

- Reconnect means new server IDs and laps from 0 (spec 3.5): other apps see `left` + `joined`.
  Fine for v0; a resume token could come later.
- OpenRadlr ignores `welcome.limits` and sends at a fixed 1 Hz, which is within the
  recommended limits. Apps sending faster should read `max_update_hz`.
