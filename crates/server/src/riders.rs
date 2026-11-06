//! `obn-server riders` — who is riding right now, for the Makefile's guard before `update`,
//! `restart` and `down`. Probes `/v0/stats` like `healthcheck` does, so the runtime image still
//! needs no curl.

use serde_json::Value;

/// Reads the rider counts from a `/v0/stats` body.
///
/// Output: the total on the first line, then one `app_id<TAB>riders` line per app with riders on
/// it. `None` if the body is not a stats reply.
pub fn summary(stats: &str) -> Option<String> {
    let stats: Value = serde_json::from_str(stats).ok()?;
    let apps = stats.get("apps")?.as_array()?;
    let mut total = 0;
    let mut lines = String::new();
    for app in apps {
        let riders = app.get("riders").and_then(Value::as_u64).unwrap_or(0);
        let id = app.get("id").and_then(Value::as_str).unwrap_or("?");
        total += riders;
        if riders > 0 {
            lines.push_str(&format!("{id}\t{riders}\n"));
        }
    }
    Some(format!("{total}\n{lines}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_riders_per_app() {
        let stats = r#"{"apps":[{"id":"openradlr","riders":2,"connections":1},
            {"id":"idle","riders":0,"connections":1},{"id":"incyclist","riders":1,"connections":1}]}"#;
        assert_eq!(summary(stats).unwrap(), "3\nopenradlr\t2\nincyclist\t1\n");
    }

    #[test]
    fn empty_server_is_just_zero() {
        assert_eq!(summary(r#"{"apps":[]}"#).unwrap(), "0\n");
    }

    #[test]
    fn rejects_other_replies() {
        assert!(summary("not json").is_none());
        assert!(summary(r#"{"name":"x"}"#).is_none());
        assert!(summary(r#"{"apps":3}"#).is_none());
    }
}
