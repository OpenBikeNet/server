//! Who is where: connections, their subscriptions and riders. A plain struct without async code or
//! a clock of its own (time is passed in). Every method returns the messages addressed to each
//! connection; the `ws` hub sends them.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::protocol::{AppInfo, ErrorCode, Join, LeftReason, Limits, ProtoError, RiderState, RiderView, ServerMsg};
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
    /// When the last state was accepted.
    #[allow(dead_code)]
    last: Instant,
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
        if join.state.distance >= length {
            return Err(ProtoError::new(
                ErrorCode::InvalidValue,
                format!("distance must be less than the route length {length}"),
            )
            .with_rider(rider_name()));
        }
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
}
