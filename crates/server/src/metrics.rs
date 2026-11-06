//! Per-app usage counters in SQLite. This is the only module that writes to disk. It stores app
//! IDs, versions and dates, and rider counts per minute. It never stores names, rider values or
//! IP addresses (spec 10).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::presence::AppCounts;

/// The statistics served at `GET /v0/stats` (spec 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub apps: Vec<AppStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppStats {
    pub id: String,
    pub versions: Vec<String>,
    pub connections: u32,
    pub riders: u32,
    pub first_seen: String,
    pub last_seen: String,
    pub rider_minutes_30d: u64,
}

pub struct Metrics {
    conn: rusqlite::Connection,
}

impl Metrics {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS apps (
                 id TEXT PRIMARY KEY,
                 first_seen TEXT NOT NULL,
                 last_seen TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS versions (
                 app TEXT NOT NULL,
                 version TEXT NOT NULL,
                 last_day TEXT NOT NULL,
                 PRIMARY KEY (app, version)
             );
             CREATE TABLE IF NOT EXISTS minutes (
                 app TEXT NOT NULL,
                 day TEXT NOT NULL,
                 minutes INTEGER NOT NULL,
                 PRIMARY KEY (app, day)
             );",
        )?;
        Ok(Self { conn })
    }

    /// Records a `hello` from `app` running `version` on the UTC day `day` (`YYYY-MM-DD`).
    pub fn record_hello(&self, app: &str, version: &str, day: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO apps (id, first_seen, last_seen) VALUES (?1, ?2, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 first_seen = MIN(first_seen, excluded.first_seen),
                 last_seen = MAX(last_seen, excluded.last_seen)",
            rusqlite::params![app, day],
        )?;
        self.conn.execute(
            "INSERT INTO versions (app, version, last_day) VALUES (?1, ?2, ?3)
             ON CONFLICT(app, version) DO UPDATE SET last_day = MAX(last_day, excluded.last_day)",
            rusqlite::params![app, version, day],
        )?;
        Ok(())
    }

    /// Adds one minute of the current live state: each app's riders count as rider-minutes on
    /// `day`, and apps with open connections have `last_seen` set to `day`.
    pub fn add_minute(&self, live: &BTreeMap<String, AppCounts>, day: &str) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (app, counts) in live {
            if counts.connections > 0 {
                tx.execute(
                    "INSERT INTO apps (id, first_seen, last_seen) VALUES (?1, ?2, ?2)
                     ON CONFLICT(id) DO UPDATE SET last_seen = MAX(last_seen, excluded.last_seen)",
                    rusqlite::params![app, day],
                )?;
            }
            if counts.riders > 0 {
                tx.execute(
                    "INSERT INTO minutes (app, day, minutes) VALUES (?1, ?2, ?3)
                     ON CONFLICT(app, day) DO UPDATE SET minutes = minutes + excluded.minutes",
                    rusqlite::params![app, day, counts.riders],
                )?;
            }
        }
        tx.commit()
    }

    /// The statistics as of the UTC day `today`, with `live` as the current connections and riders.
    pub fn stats(&self, live: &BTreeMap<String, AppCounts>, today: &str) -> rusqlite::Result<Stats> {
        // Keyed by app ID, so the output is sorted by ID.
        let mut apps: BTreeMap<String, AppStats> = BTreeMap::new();
        let mut stmt = self.conn.prepare("SELECT id, first_seen, last_seen FROM apps")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)))?;
        for row in rows {
            let (id, first_seen, last_seen) = row?;
            apps.insert(id.clone(), AppStats::new(id, first_seen, last_seen));
        }

        // Live apps that have no row yet are new today.
        for id in live.keys() {
            apps.entry(id.clone())
                .or_insert_with(|| AppStats::new(id.clone(), today.to_string(), today.to_string()));
        }

        let mut stmt = self.conn.prepare(
            "SELECT app, version FROM versions
             WHERE last_day >= date(?1, '-29 days')
             ORDER BY app, version",
        )?;
        let rows = stmt.query_map([today], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (app, version) = row?;
            if let Some(entry) = apps.get_mut(&app) {
                entry.versions.push(version);
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT app, SUM(minutes) FROM minutes
             WHERE day >= date(?1, '-29 days')
             GROUP BY app",
        )?;
        let rows = stmt.query_map([today], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (app, minutes) = row?;
            if let Some(entry) = apps.get_mut(&app) {
                entry.rider_minutes_30d = u64::try_from(minutes).unwrap_or(0);
            }
        }

        for (id, counts) in live {
            if let Some(entry) = apps.get_mut(id) {
                entry.connections = counts.connections;
                entry.riders = counts.riders;
            }
        }

        Ok(Stats {
            apps: apps.into_values().collect(),
        })
    }
}

/// Today's UTC date as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    let d = time::OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

impl AppStats {
    fn new(id: String, first_seen: String, last_seen: String) -> Self {
        Self {
            id,
            versions: Vec::new(),
            connections: 0,
            riders: 0,
            first_seen,
            last_seen,
            rider_minutes_30d: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(app: &str, connections: u32, riders: u32) -> BTreeMap<String, AppCounts> {
        BTreeMap::from([(app.to_string(), AppCounts { connections, riders })])
    }

    #[test]
    fn stats_matches_spec_example() {
        let m = Metrics::open_in_memory().unwrap();
        m.record_hello("openradlr", "0.3.1", "2026-11-20").unwrap();
        m.add_minute(&live("openradlr", 1, 412), "2026-11-20").unwrap();
        m.add_minute(&live("openradlr", 1, 400), "2026-11-21").unwrap();
        let stats = m.stats(&live("openradlr", 1, 4), "2026-11-21").unwrap();

        let got = serde_json::to_value(&stats).unwrap();
        let want: serde_json::Value =
            serde_json::from_str(include_str!("../../../protocol/examples/stats.json")).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn minutes_older_than_30_days_drop_out() {
        let m = Metrics::open_in_memory().unwrap();
        m.record_hello("a", "1.0.0", "2026-10-01").unwrap();
        m.add_minute(&live("a", 0, 10), "2026-10-01").unwrap();
        // The window for 2026-11-21 is 2026-10-23 through 2026-11-21 (30 days).
        m.add_minute(&live("a", 0, 100), "2026-10-22").unwrap();
        m.add_minute(&live("a", 0, 7), "2026-10-23").unwrap();
        m.add_minute(&live("a", 0, 5), "2026-11-20").unwrap();

        let stats = m.stats(&live("a", 0, 0), "2026-11-21").unwrap();
        assert_eq!(stats.apps.len(), 1);
        assert_eq!(stats.apps[0].rider_minutes_30d, 12);
        assert!(
            stats.apps[0].versions.is_empty(),
            "a version last seen 2026-10-01 is outside the window"
        );
    }

    #[test]
    fn apps_without_live_connection_show_zero() {
        let m = Metrics::open_in_memory().unwrap();
        m.record_hello("a", "1.0.0", "2026-11-20").unwrap();
        m.add_minute(&live("a", 1, 3), "2026-11-20").unwrap();
        m.record_hello("b", "2.0.0", "2026-11-20").unwrap();
        // "c" is live but was never recorded by a hello.
        let now = BTreeMap::from([
            (
                "a".to_string(),
                AppCounts {
                    connections: 1,
                    riders: 3,
                },
            ),
            (
                "c".to_string(),
                AppCounts {
                    connections: 2,
                    riders: 7,
                },
            ),
        ]);

        let stats = m.stats(&now, "2026-11-21").unwrap();
        let ids: Vec<&str> = stats.apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);

        let b = &stats.apps[1];
        assert_eq!((b.connections, b.riders), (0, 0));
        assert_eq!(b.versions, ["2.0.0"]);
        assert_eq!(b.rider_minutes_30d, 0);

        let c = &stats.apps[2];
        assert_eq!((c.connections, c.riders), (2, 7));
        assert!(c.versions.is_empty());
    }

    #[test]
    fn reopen_keeps_data() {
        let path = std::env::temp_dir().join(format!("obn-metrics-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let now = live("a", 1, 2);

        let before = {
            let m = Metrics::open(&path).unwrap();
            m.record_hello("a", "1.0.0", "2026-11-20").unwrap();
            m.add_minute(&now, "2026-11-20").unwrap();
            m.stats(&now, "2026-11-21").unwrap()
        };
        let after = Metrics::open(&path).unwrap().stats(&now, "2026-11-21").unwrap();

        let _ = std::fs::remove_file(&path);
        assert_eq!(before, after);
        assert_eq!(after.apps[0].rider_minutes_30d, 2);
    }
}
