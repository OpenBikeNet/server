//! Who is where: connections, their subscriptions and riders. A plain struct without async code or
//! a clock of its own (time is passed in). Every method returns the messages addressed to each
//! connection; the `ws` hub sends them.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::protocol::{
    AppInfo, ErrorCode, Join, LeftReason, Limits, ProtoError, RiderState, RiderUpdate, RiderView, ServerMsg,
    UpdateEntry,
};
use crate::routes::Routes;

pub type ConnId = u64;

/// One message for one connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub to: ConnId,
    pub msg: ServerMsg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppCounts {
    pub connections: u32,
    pub riders: u32,
}

type RoomKey = (String, String);

#[derive(Debug)]
struct Rider {
    /// The server ID (`r1`, `r2`, ...), unique across connections.
    id: String,
    route: String,
    room: String,
    name: String,
    state: RiderState,
    /// When the last state was accepted, by `join` or `update`. Drives the timeout.
    last: Instant,
    /// When the last `update` was accepted. Drives the rate limit; a join does not count (spec 6.3).
    last_update: Option<Instant>,
}

#[derive(Debug)]
struct Conn {
    app: AppInfo,
    subscriptions: BTreeSet<RoomKey>,
    /// The riders of this connection, keyed by the app's own rider value.
    riders: BTreeMap<String, Rider>,
}

#[derive(Debug)]
pub struct Presence {
    routes: Routes,
    limits: Limits,
    conns: BTreeMap<ConnId, Conn>,
    /// Subscribers per route+room, in connection order so deliveries are deterministic.
    rooms: BTreeMap<RoomKey, BTreeSet<ConnId>>,
    next_rider: u64,
}

impl Presence {
    pub fn new(routes: Routes, limits: Limits) -> Self {
        Presence {
            routes,
            limits,
            conns: BTreeMap::new(),
            rooms: BTreeMap::new(),
            next_rider: 1,
        }
    }

    pub fn connect(&mut self, conn: ConnId, app: AppInfo) {
        self.conns.insert(
            conn,
            Conn {
                app,
                subscriptions: BTreeSet::new(),
                riders: BTreeMap::new(),
            },
        );
    }

    /// Subscribing again to a route+room already subscribed to is allowed (it does not count
    /// against the limit) and returns a fresh snapshot.
    pub fn subscribe(&mut self, conn: ConnId, route: &str, room: &str) -> Result<Vec<Delivery>, ProtoError> {
        self.check_route(route)?;
        let key = (route.to_string(), room.to_string());
        let Some(c) = self.conns.get_mut(&conn) else {
            debug_assert!(false, "subscribe without connect");
            return Ok(Vec::new());
        };
        if !c.subscriptions.contains(&key) {
            if c.subscriptions.len() >= self.limits.max_subscriptions as usize {
                return Err(ProtoError::new(
                    ErrorCode::LimitExceeded,
                    format!("at most {} subscriptions per connection", self.limits.max_subscriptions),
                ));
            }
            c.subscriptions.insert(key.clone());
        }
        self.rooms.entry(key.clone()).or_default().insert(conn);

        let riders = self
            .conns
            .iter()
            .filter(|(id, _)| **id != conn)
            .flat_map(|(_, other)| {
                other
                    .riders
                    .values()
                    .filter(|r| r.route == key.0 && r.room == key.1)
                    .map(|r| view(&other.app, r))
            })
            .collect();
        Ok(vec![Delivery {
            to: conn,
            msg: ServerMsg::Snapshot {
                route: key.0,
                room: key.1,
                riders,
            },
        }])
    }

    /// Unsubscribing from a route+room that is not subscribed to does nothing.
    pub fn unsubscribe(&mut self, conn: ConnId, route: &str, room: &str) {
        let key = (route.to_string(), room.to_string());
        if let Some(c) = self.conns.get_mut(&conn) {
            c.subscriptions.remove(&key);
        }
        self.remove_subscriber(&key, conn);
    }

    pub fn join(&mut self, conn: ConnId, join: Join, now: Instant) -> Result<Vec<Delivery>, ProtoError> {
        let length = self.check_route(&join.route)?.length;
        let rider_name = || Some(join.rider.clone());
        check_distance(length, join.state.distance, &join.rider)?;
        let Some(c) = self.conns.get_mut(&conn) else {
            debug_assert!(false, "join without connect");
            return Ok(Vec::new());
        };
        if c.riders.contains_key(&join.rider) {
            return Err(ProtoError::new(ErrorCode::RiderExists, "rider has already joined").with_rider(rider_name()));
        }
        if c.riders.len() >= self.limits.max_riders as usize {
            return Err(ProtoError::new(
                ErrorCode::LimitExceeded,
                format!("at most {} riders per connection", self.limits.max_riders),
            )
            .with_rider(rider_name()));
        }
        let rider = Rider {
            id: format!("r{:x}", self.next_rider),
            route: join.route.clone(),
            room: join.room.clone(),
            name: join.name,
            state: join.state,
            last: now,
            last_update: None,
        };
        self.next_rider += 1;
        let joined = view(&c.app, &rider);
        c.riders.insert(join.rider, rider);
        let key = (join.route.clone(), join.room.clone());
        Ok(self.to_subscribers(
            &key,
            conn,
            ServerMsg::Joined {
                route: join.route,
                room: join.room,
                rider: joined,
            },
        ))
    }

    pub fn leave(&mut self, conn: ConnId, rider: &str) -> Result<Vec<Delivery>, ProtoError> {
        let removed = self.conns.get_mut(&conn).and_then(|c| c.riders.remove(rider));
        match removed {
            Some(r) => Ok(self.left(conn, &r, LeftReason::Leave)),
            None => {
                Err(ProtoError::new(ErrorCode::UnknownRider, "rider has not joined")
                    .with_rider(Some(rider.to_string())))
            }
        }
    }

    /// Applies a batch of state entries. Each entry is checked on its own: a bad one yields an error
    /// and does not stop the others. The accepted entries go out as one `Updated` per route+room.
    pub fn update(
        &mut self,
        conn: ConnId,
        entries: Vec<UpdateEntry>,
        now: Instant,
    ) -> (Vec<Delivery>, Vec<ProtoError>) {
        let mut errors = Vec::new();
        let mut accepted: BTreeMap<RoomKey, Vec<RiderUpdate>> = BTreeMap::new();
        let min_interval = self.min_update_interval();
        if let Some(c) = self.conns.get_mut(&conn) {
            for e in entries {
                let fail = |code, msg: String| ProtoError::new(code, msg).with_rider(Some(e.rider.clone()));
                let Some(r) = c.riders.get_mut(&e.rider) else {
                    errors.push(fail(ErrorCode::UnknownRider, "rider has not joined".to_string()));
                    continue;
                };
                if let Some(route) = self.routes.get(&r.route) {
                    if let Err(err) = check_distance(route.length, e.state.distance, &e.rider) {
                        errors.push(err);
                        continue;
                    }
                }
                if r.last_update
                    .is_some_and(|t| now.saturating_duration_since(t) < min_interval)
                {
                    errors.push(fail(ErrorCode::RateLimited, "updates are coming too fast".to_string()));
                    continue;
                }
                r.last = now;
                r.last_update = Some(now);
                r.state = e.state.clone();
                accepted
                    .entry((r.route.clone(), r.room.clone()))
                    .or_default()
                    .push(RiderUpdate {
                        id: r.id.clone(),
                        state: e.state,
                    });
            }
        } else {
            debug_assert!(false, "update without connect");
        }
        let deliveries = accepted
            .into_iter()
            .flat_map(|(key, riders)| {
                let msg = ServerMsg::Updated {
                    route: key.0.clone(),
                    room: key.1.clone(),
                    riders,
                };
                self.to_subscribers(&key, conn, msg)
            })
            .collect();
        (deliveries, errors)
    }

    /// Removes the riders whose last accepted state is `rider_timeout_s` old or older.
    pub fn expire(&mut self, now: Instant) -> Vec<Delivery> {
        let timeout = Duration::from_secs(u64::from(self.limits.rider_timeout_s));
        let mut expired: Vec<(ConnId, Rider)> = Vec::new();
        for (id, c) in &mut self.conns {
            let stale: Vec<String> = c
                .riders
                .iter()
                .filter(|(_, r)| now.saturating_duration_since(r.last) >= timeout)
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                if let Some(r) = c.riders.remove(&k) {
                    expired.push((*id, r));
                }
            }
        }
        expired
            .iter()
            .flat_map(|(conn, r)| self.left(*conn, r, LeftReason::Timeout))
            .collect()
    }

    /// 80% of the nominal interval between updates, which allows for network jitter.
    fn min_update_interval(&self) -> Duration {
        Duration::from_nanos(800_000_000 / u64::from(self.limits.max_update_hz.max(1)))
    }

    /// Removes the connection with its riders and subscriptions.
    pub fn disconnect(&mut self, conn: ConnId) -> Vec<Delivery> {
        let Some(c) = self.conns.remove(&conn) else {
            return Vec::new();
        };
        for key in &c.subscriptions {
            self.remove_subscriber(key, conn);
        }
        c.riders
            .values()
            .flat_map(|r| self.left(conn, r, LeftReason::Disconnect))
            .collect()
    }

    pub fn live_counts(&self) -> BTreeMap<String, AppCounts> {
        let mut counts: BTreeMap<String, AppCounts> = BTreeMap::new();
        for c in self.conns.values() {
            let e = counts.entry(c.app.id.clone()).or_default();
            e.connections += 1;
            e.riders += c.riders.len() as u32;
        }
        counts
    }

    fn check_route(&self, route: &str) -> Result<&crate::routes::Route, ProtoError> {
        self.routes
            .get(route)
            .ok_or_else(|| ProtoError::new(ErrorCode::UnknownRoute, format!("unknown route {route:?}")))
    }

    fn remove_subscriber(&mut self, key: &RoomKey, conn: ConnId) {
        if let Some(set) = self.rooms.get_mut(key) {
            set.remove(&conn);
            if set.is_empty() {
                self.rooms.remove(key);
            }
        }
    }

    /// `msg` for every subscriber of the room except `except`.
    fn to_subscribers(&self, key: &RoomKey, except: ConnId, msg: ServerMsg) -> Vec<Delivery> {
        self.rooms
            .get(key)
            .into_iter()
            .flatten()
            .filter(|c| **c != except)
            .map(|c| Delivery {
                to: *c,
                msg: msg.clone(),
            })
            .collect()
    }

    fn left(&self, conn: ConnId, rider: &Rider, reason: LeftReason) -> Vec<Delivery> {
        let key = (rider.route.clone(), rider.room.clone());
        self.to_subscribers(
            &key,
            conn,
            ServerMsg::Left {
                route: key.0.clone(),
                room: key.1.clone(),
                id: rider.id.clone(),
                reason,
            },
        )
    }
}

fn check_distance(length: f64, distance: f64, rider: &str) -> Result<(), ProtoError> {
    if distance >= length {
        return Err(ProtoError::new(
            ErrorCode::InvalidValue,
            format!("distance must be less than the route length {length}"),
        )
        .with_rider(Some(rider.to_string())));
    }
    Ok(())
}

fn view(app: &AppInfo, r: &Rider) -> RiderView {
    RiderView {
        id: r.id.clone(),
        app: app.id.clone(),
        name: r.name.clone(),
        state: r.state.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AppInfo, ErrorCode, Join, LeftReason, Limits, RiderState, RiderView, ServerMsg};
    use crate::routes::{Route, Routes};
    use std::path::Path;
    use std::time::Instant;

    const ROUTES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../protocol/routes");

    fn app(id: &str) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            version: "1.0".to_string(),
            name: None,
            url: None,
        }
    }

    fn other_route() -> Route {
        Route {
            id: "other-100".to_string(),
            name: "Other".to_string(),
            length: 100.0,
            looped: true,
            profile: vec![[0.0, 0.0], [100.0, 0.0]],
        }
    }

    fn presence_with(limits: Limits) -> Presence {
        let mut routes = Routes::load_dir(Path::new(ROUTES_DIR)).unwrap();
        let mut all: Vec<Route> = vec![routes.get("velodrome-250").unwrap().clone(), other_route()];
        routes = Routes::from_routes(std::mem::take(&mut all)).unwrap();
        Presence::new(routes, limits)
    }

    fn presence() -> Presence {
        presence_with(Limits::default())
    }

    fn state(distance: f64) -> RiderState {
        RiderState {
            lap: 0,
            distance,
            speed: 10.0,
            power: None,
            cadence: None,
            heart_rate: None,
        }
    }

    fn join_in(rider: &str, route: &str, room: &str, name: &str) -> Join {
        Join {
            rider: rider.to_string(),
            route: route.to_string(),
            room: room.to_string(),
            name: name.to_string(),
            state: state(0.0),
        }
    }

    fn join_msg(rider: &str, name: &str) -> Join {
        join_in(rider, "velodrome-250", "public", name)
    }

    const V: &str = "velodrome-250";
    const P: &str = "public";

    #[test]
    fn late_subscriber_gets_snapshot() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "Anna"), t0).unwrap();
        let d = p.subscribe(2, V, P).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].to, 2);
        match &d[0].msg {
            ServerMsg::Snapshot { route, room, riders } => {
                assert_eq!((route.as_str(), room.as_str()), (V, P));
                assert_eq!(riders.len(), 1);
                let r: &RiderView = &riders[0];
                assert_eq!(
                    (r.id.as_str(), r.app.as_str(), r.name.as_str()),
                    ("r1", "app-one", "Anna")
                );
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn snapshot_excludes_own_riders() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
        let d = p.subscribe(1, V, P).unwrap();
        match &d[0].msg {
            ServerMsg::Snapshot { riders, .. } => assert!(riders.is_empty()),
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn join_notifies_other_subscribers_only() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(1, V, P).unwrap();
        p.subscribe(2, V, P).unwrap();
        let d = p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].to, 2);
        assert!(matches!(&d[0].msg, ServerMsg::Joined { rider, .. } if rider.id == "r1" && rider.app == "app-one"));
    }

    #[test]
    fn rooms_are_separate() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, "a").unwrap();
        let d = p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn same_rider_value_on_two_connections() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(1, V, P).unwrap();
        p.subscribe(2, V, P).unwrap();
        let t0 = Instant::now();
        let d1 = p.join(1, join_msg("u-1", "Anna"), t0).unwrap();
        let d2 = p.join(2, join_msg("u-1", "Ben"), t0).unwrap();
        assert!(matches!(&d1[0].msg, ServerMsg::Joined { rider, .. } if rider.id == "r1"));
        assert!(matches!(&d2[0].msg, ServerMsg::Joined { rider, .. } if rider.id == "r2"));
    }

    #[test]
    fn join_twice_on_other_route() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        let t0 = Instant::now();
        p.join(1, join_msg("u-1", "Anna"), t0).unwrap();
        let e = p.join(1, join_in("u-1", "other-100", P, "Anna"), t0).unwrap_err();
        assert_eq!(e.code, ErrorCode::RiderExists);
    }

    #[test]
    fn join_unknown_route() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        let e = p
            .join(1, join_in("u-1", "nowhere", P, "Anna"), Instant::now())
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownRoute);
    }

    #[test]
    fn subscribe_unknown_route() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        assert_eq!(p.subscribe(1, "nowhere", P).unwrap_err().code, ErrorCode::UnknownRoute);
    }

    #[test]
    fn join_limit() {
        let limits = Limits {
            max_riders: 2,
            ..Limits::default()
        };
        let mut p = presence_with(limits);
        p.connect(1, app("app-one"));
        let t0 = Instant::now();
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.join(1, join_msg("u-2", "B"), t0).unwrap();
        let e = p.join(1, join_msg("u-3", "C"), t0).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn subscribe_limit() {
        let limits = Limits {
            max_subscriptions: 2,
            ..Limits::default()
        };
        let mut p = presence_with(limits);
        p.connect(1, app("app-one"));
        p.subscribe(1, V, "a").unwrap();
        p.subscribe(1, V, "b").unwrap();
        assert_eq!(p.subscribe(1, V, "c").unwrap_err().code, ErrorCode::LimitExceeded);
        // Subscribing again to an existing one is not a new subscription.
        p.subscribe(1, V, "a").unwrap();
    }

    #[test]
    fn leave_notifies_with_reason_leave() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, P).unwrap();
        p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
        let d = p.leave(1, "u-1").unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].to, 2);
        assert!(matches!(&d[0].msg, ServerMsg::Left { id, reason: LeftReason::Leave, .. } if id == "r1"));
        // The rider value can be joined again.
        p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
    }

    #[test]
    fn leave_unknown_rider() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        assert_eq!(p.leave(1, "u-1").unwrap_err().code, ErrorCode::UnknownRider);
    }

    #[test]
    fn disconnect_removes_riders_and_subscriptions() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, P).unwrap();
        let t0 = Instant::now();
        p.join(1, join_msg("u-1", "Anna"), t0).unwrap();
        p.join(1, join_msg("u-2", "Ben"), t0).unwrap();
        let d = p.disconnect(1);
        assert_eq!(d.len(), 2);
        for x in &d {
            assert_eq!(x.to, 2);
            assert!(matches!(
                &x.msg,
                ServerMsg::Left {
                    reason: LeftReason::Disconnect,
                    ..
                }
            ));
        }
        let counts = p.live_counts();
        assert!(!counts.contains_key("app-one"));
        assert_eq!(counts["app-two"].connections, 1);
        assert!(p.disconnect(1).is_empty());
    }

    #[test]
    fn unsubscribe_stops_events() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, P).unwrap();
        p.unsubscribe(2, V, P);
        p.unsubscribe(2, V, P); // not subscribed any more: no-op
        let d = p.join(1, join_msg("u-1", "Anna"), Instant::now()).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn live_counts_per_app() {
        let mut p = presence();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-one"));
        let t0 = Instant::now();
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.join(1, join_msg("u-2", "B"), t0).unwrap();
        p.join(2, join_msg("u-1", "C"), t0).unwrap();
        let c = p.live_counts();
        assert_eq!(c.len(), 1);
        assert_eq!((c["app-one"].connections, c["app-one"].riders), (2, 3));
    }

    use crate::protocol::{RiderUpdate, UpdateEntry};
    use std::time::Duration;

    fn entry(rider: &str, distance: f64) -> UpdateEntry {
        UpdateEntry {
            rider: rider.to_string(),
            state: state(distance),
        }
    }

    fn updated(d: &Delivery) -> &Vec<RiderUpdate> {
        match &d.msg {
            ServerMsg::Updated { riders, .. } => riders,
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn update_batches_per_room() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.join(1, join_msg("u-2", "B"), t0).unwrap();
        p.join(1, join_in("u-3", V, "a", "C"), t0).unwrap();
        p.subscribe(2, V, P).unwrap();
        p.subscribe(2, V, "a").unwrap();
        let t1 = t0 + Duration::from_secs(1);
        let (d, errs) = p.update(1, vec![entry("u-1", 1.0), entry("u-3", 2.0), entry("u-2", 3.0)], t1);
        assert!(errs.is_empty());
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.to == 2));
        // Rooms in key order: "a" before "public".
        assert!(matches!(&d[0].msg, ServerMsg::Updated { room, .. } if room == "a"));
        assert_eq!(updated(&d[0]).len(), 1);
        assert!(matches!(&d[1].msg, ServerMsg::Updated { room, .. } if room == "public"));
        let ids: Vec<&str> = updated(&d[1]).iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2"]);
        assert_eq!(updated(&d[1])[1].state.distance, 3.0);
    }

    #[test]
    fn update_no_echo() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.subscribe(1, V, P).unwrap();
        let (d, errs) = p.update(1, vec![entry("u-1", 1.0)], t0 + Duration::from_secs(1));
        assert!(d.is_empty());
        assert!(errs.is_empty());
    }

    #[test]
    fn distance_equal_to_length_is_rejected() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.subscribe(2, V, P).unwrap();
        let t1 = t0 + Duration::from_secs(1);
        let (d, errs) = p.update(1, vec![entry("u-1", 250.0)], t1);
        assert!(d.is_empty());
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::InvalidValue);
        assert_eq!(errs[0].rider.as_deref(), Some("u-1"));
        let (d, errs) = p.update(1, vec![entry("u-1", 249.99)], t1);
        assert!(errs.is_empty());
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn bad_entry_does_not_block_others() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.subscribe(2, V, P).unwrap();
        let (d, errs) = p.update(
            1,
            vec![entry("nope", 1.0), entry("u-1", 2.0)],
            t0 + Duration::from_secs(1),
        );
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::UnknownRider);
        assert_eq!(errs[0].rider.as_deref(), Some("nope"));
        assert_eq!(d.len(), 1);
        assert_eq!(updated(&d[0]).len(), 1);
    }

    #[test]
    fn rate_limit() {
        let mut p = presence();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "A"), t0 - ms(1000)).unwrap();
        p.subscribe(2, V, P).unwrap();
        let (d, errs) = p.update(1, vec![entry("u-1", 1.0)], t0);
        assert!(errs.is_empty() && d.len() == 1);
        let (d, errs) = p.update(1, vec![entry("u-1", 2.0)], t0 + ms(150));
        assert!(d.is_empty());
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, ErrorCode::RateLimited);
        assert_eq!(errs[0].rider.as_deref(), Some("u-1"));
        // The rejected entry did not move `last`: 200 ms after t0 is accepted.
        let (d, errs) = p.update(1, vec![entry("u-1", 3.0)], t0 + ms(200));
        assert!(errs.is_empty());
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn update_right_after_join_is_accepted() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, P).unwrap();
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        let (d, errs) = p.update(1, vec![entry("u-1", 1.0)], t0);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn timeout_after_rider_timeout_s() {
        let mut p = presence();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.subscribe(2, V, P).unwrap();
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        assert!(p.expire(t0 + ms(9900)).is_empty());
        let d = p.expire(t0 + ms(10_000));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].to, 2);
        assert!(matches!(&d[0].msg, ServerMsg::Left { id, reason: LeftReason::Timeout, .. } if id == "r1"));
        // Gone: it cannot expire twice and its rider value is free again.
        assert!(p.expire(t0 + ms(20_000)).is_empty());
        p.join(1, join_msg("u-1", "A"), t0 + ms(20_000)).unwrap();

        // An update at 5 s pushes the timeout to 15 s.
        let t1 = t0 + ms(30_000);
        p.join(1, join_msg("u-2", "B"), t1).unwrap();
        let (_, errs) = p.update(1, vec![entry("u-2", 1.0)], t1 + ms(5000));
        assert!(errs.is_empty());
        assert!(p
            .expire(t1 + ms(14_900))
            .iter()
            .all(|x| !matches!(&x.msg, ServerMsg::Left { id, .. } if id == "r3")));
        let d = p.expire(t1 + ms(15_000));
        assert!(d
            .iter()
            .any(|x| matches!(&x.msg, ServerMsg::Left { id, reason: LeftReason::Timeout, .. } if id == "r3")));
    }

    #[test]
    fn optional_fields_only_when_sent() {
        let mut p = presence();
        let t0 = Instant::now();
        p.connect(1, app("app-one"));
        p.connect(2, app("app-two"));
        p.join(1, join_msg("u-1", "A"), t0).unwrap();
        p.subscribe(2, V, P).unwrap();
        let mut e = entry("u-1", 1.0);
        e.state.power = Some(200);
        let t1 = t0 + Duration::from_secs(1);
        let (d, _) = p.update(1, vec![e], t1);
        assert_eq!(updated(&d[0])[0].state.power, Some(200));
        let (d, _) = p.update(1, vec![entry("u-1", 2.0)], t1 + Duration::from_secs(1));
        let u = &updated(&d[0])[0];
        assert_eq!(u.state.power, None);
        assert_eq!((u.state.cadence, u.state.heart_rate), (None, None));
    }
}
