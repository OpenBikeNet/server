//! Prometheus text export for `/internal/metrics`: live gauges per app and a few counters.
//!
//! Not to be confused with [`crate::metrics`], which holds the SQLite usage statistics.
//! Counters are plain atomics, so counting never blocks the connection tasks.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::presence::AppCounts;
use crate::protocol::ErrorCode;

/// The `Content-Type` of the Prometheus text format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Counters since the server started.
#[derive(Default)]
pub struct Counters {
    connections: AtomicU64,
    rider_updates: AtomicU64,
    /// Indexed by `ErrorCode as usize`.
    errors: [AtomicU64; ErrorCode::ALL.len()],
    queue_full_drops: AtomicU64,
    rejected_per_ip: AtomicU64,
}

/// A point-in-time copy of [`Counters`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub connections: u64,
    pub rider_updates: u64,
    /// Every error code, in declaration order, including those never sent.
    pub errors: Vec<(&'static str, u64)>,
    pub queue_full_drops: u64,
    pub rejected_per_ip: u64,
}

impl Counters {
    pub fn connection_accepted(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn updates_accepted(&self, entries: usize) {
        self.rider_updates.fetch_add(entries as u64, Ordering::Relaxed);
    }

    pub fn error_sent(&self, code: ErrorCode) {
        self.errors[code as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn queue_full_drop(&self) {
        self.queue_full_drops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn rejected_per_ip(&self) {
        self.rejected_per_ip.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            connections: self.connections.load(Ordering::Relaxed),
            rider_updates: self.rider_updates.load(Ordering::Relaxed),
            errors: ErrorCode::ALL
                .iter()
                .map(|&code| (code.as_str(), self.errors[code as usize].load(Ordering::Relaxed)))
                .collect(),
            queue_full_drops: self.queue_full_drops.load(Ordering::Relaxed),
            rejected_per_ip: self.rejected_per_ip.load(Ordering::Relaxed),
        }
    }
}

/// Escapes a label value for the text format.
fn label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// The whole `/internal/metrics` body. Only live apps appear in the gauges, so label cardinality
/// stays bounded by the apps connected right now.
pub fn render(live: &BTreeMap<String, AppCounts>, counters: &Snapshot, version: &str, sha: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP obn_connections Current WebSocket connections per app.\n# TYPE obn_connections gauge"
    );
    for (app, c) in live {
        let _ = writeln!(out, "obn_connections{{app=\"{}\"}} {}", label(app), c.connections);
    }
    let _ = writeln!(
        out,
        "# HELP obn_riders Current riders per app.\n# TYPE obn_riders gauge"
    );
    for (app, c) in live {
        let _ = writeln!(out, "obn_riders{{app=\"{}\"}} {}", label(app), c.riders);
    }
    let _ = writeln!(
        out,
        "# HELP obn_connections_total WebSocket connections accepted since the server started.\n\
         # TYPE obn_connections_total counter\nobn_connections_total {}",
        counters.connections
    );
    let _ = writeln!(
        out,
        "# HELP obn_rider_updates_total Rider update entries accepted since the server started.\n\
         # TYPE obn_rider_updates_total counter\nobn_rider_updates_total {}",
        counters.rider_updates
    );
    let _ = writeln!(
        out,
        "# HELP obn_errors_total Error messages sent to apps, by error code.\n# TYPE obn_errors_total counter"
    );
    for (code, n) in &counters.errors {
        let _ = writeln!(out, "obn_errors_total{{code=\"{code}\"}} {n}");
    }
    let _ = writeln!(
        out,
        "# HELP obn_queue_full_drops_total Connections dropped because their outgoing queue was full.\n\
         # TYPE obn_queue_full_drops_total counter\nobn_queue_full_drops_total {}",
        counters.queue_full_drops
    );
    let _ = writeln!(
        out,
        "# HELP obn_connections_rejected_total Connections refused before the upgrade.\n\
         # TYPE obn_connections_rejected_total counter\nobn_connections_rejected_total{{reason=\"per_ip_limit\"}} {}",
        counters.rejected_per_ip
    );
    let _ = writeln!(
        out,
        "# HELP obn_build_info Which build is running.\n# TYPE obn_build_info gauge\n\
         obn_build_info{{version=\"{}\",sha=\"{}\"}} 1",
        label(version),
        label(sha)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_index_matches_declaration_order() {
        for (i, code) in ErrorCode::ALL.iter().enumerate() {
            assert_eq!(*code as usize, i);
        }
    }

    #[test]
    fn counters_count_and_prefill_every_error_code() {
        let c = Counters::default();
        let snap = c.snapshot();
        assert_eq!(snap.errors.len(), 14);
        assert!(snap.errors.iter().all(|(_, n)| *n == 0));
        c.connection_accepted();
        c.connection_accepted();
        c.updates_accepted(3);
        c.error_sent(ErrorCode::RateLimited);
        c.queue_full_drop();
        c.rejected_per_ip();
        let snap = c.snapshot();
        assert_eq!((snap.connections, snap.rider_updates), (2, 3));
        assert_eq!((snap.queue_full_drops, snap.rejected_per_ip), (1, 1));
        assert!(snap.errors.contains(&("rate_limited", 1)));
    }

    #[test]
    fn renders_gauges_and_counters() {
        let mut live = BTreeMap::new();
        live.insert(
            "openradlr".to_string(),
            AppCounts {
                connections: 2,
                riders: 5,
            },
        );
        let c = Counters::default();
        c.connection_accepted();
        c.error_sent(ErrorCode::UnknownRoute);
        let out = render(&live, &c.snapshot(), "0.1.0", "abc123");
        assert!(out.contains("obn_connections{app=\"openradlr\"} 2\n"));
        assert!(out.contains("obn_riders{app=\"openradlr\"} 5\n"));
        assert!(out.contains("obn_connections_total 1\n"));
        assert!(out.contains("obn_rider_updates_total 0\n"));
        assert!(out.contains("obn_errors_total{code=\"unknown_route\"} 1\n"));
        assert!(out.contains("obn_errors_total{code=\"hello_required\"} 0\n"));
        assert_eq!(out.matches("obn_errors_total{").count(), 14);
        assert!(out.contains("obn_queue_full_drops_total 0\n"));
        assert!(out.contains("obn_connections_rejected_total{reason=\"per_ip_limit\"} 0\n"));
        assert!(out.contains("obn_build_info{version=\"0.1.0\",sha=\"abc123\"} 1\n"));
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            assert!(line.starts_with("obn_"), "{line}");
        }
    }

    #[test]
    fn offline_apps_have_no_series() {
        let out = render(&BTreeMap::new(), &Counters::default().snapshot(), "0.1.0", "dev");
        assert!(!out.contains("obn_riders{"));
        assert!(!out.contains("obn_connections{"));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
