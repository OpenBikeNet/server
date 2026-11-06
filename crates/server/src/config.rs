//! Server configuration, read from the environment.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::protocol::Limits;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub routes_dir: PathBuf,
    pub db_path: PathBuf,
    /// Take the client IP from the `X-Forwarded-For` header (set by a trusted reverse proxy).
    pub trust_proxy: bool,
    pub hello_timeout: Duration,
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
    pub max_connections_per_ip: u32,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: SocketAddr::from(([0, 0, 0, 0], 8080)),
            routes_dir: PathBuf::from("protocol/routes"),
            db_path: PathBuf::from("obn.sqlite"),
            trust_proxy: false,
            hello_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            pong_timeout: Duration::from_secs(30),
            max_connections_per_ip: 10,
            limits: Limits::default(),
        }
    }
}

impl Config {
    /// `OBN_BIND`, `OBN_ROUTES_DIR`, `OBN_DB` and `OBN_TRUST_PROXY=1` (or `true`); everything else is fixed.
    pub fn from_env() -> Result<Config, String> {
        let mut c = Config::default();
        if let Ok(v) = std::env::var("OBN_BIND") {
            c.bind = v.parse().map_err(|e| format!("OBN_BIND {v:?}: {e}"))?;
        }
        if let Ok(v) = std::env::var("OBN_ROUTES_DIR") {
            c.routes_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("OBN_DB") {
            c.db_path = PathBuf::from(v);
        }
        c.trust_proxy = std::env::var("OBN_TRUST_PROXY").is_ok_and(|v| is_on(&v));
        Ok(c)
    }
}

/// `1` or `true` switch a flag on; anything else, including an empty value, leaves it off.
fn is_on(value: &str) -> bool {
    matches!(value.trim(), "1" | "true")
}

#[cfg(test)]
mod tests {
    use super::is_on;

    #[test]
    fn flags_accept_one_and_true() {
        assert!(is_on("1"));
        assert!(is_on("true"));
        assert!(!is_on("0"));
        assert!(!is_on("false"));
        assert!(!is_on(""));
        assert!(!is_on("yes"));
    }
}
