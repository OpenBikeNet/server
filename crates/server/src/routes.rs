//! Shared routes (`protocol/spec/v0.md` section 4): loading and validating the route files that the
//! protocol repository publishes under `routes/`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::protocol::is_valid_route_id;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub length: f64,
    #[serde(rename = "loop")]
    pub looped: bool,
    pub profile: Vec<[f64; 2]>,
}

/// The validated routes of one server, keyed by route ID.
#[derive(Debug, Clone)]
pub struct Routes {
    by_id: BTreeMap<String, Route>,
}

impl Routes {
    /// Loads every `*.json` file in `dir`, in sorted order. Each file must hold a valid route whose
    /// name is `<id>.json`.
    pub fn load_dir(dir: &Path) -> Result<Routes, String> {
        let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut routes = Vec::new();
        for path in &paths {
            let name = path.display();
            let text = fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
            let route: Route = serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
            check(&route).map_err(|e| format!("{name}: {e}"))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if stem != route.id {
                return Err(format!("{name}: file name must be {}.json to match its id", route.id));
            }
            routes.push(route);
        }
        Routes::from_routes(routes)
    }

    /// Builds the set from routes already in memory. Each route is checked, and IDs must be unique.
    pub fn from_routes(routes: Vec<Route>) -> Result<Routes, String> {
        let mut by_id = BTreeMap::new();
        for route in routes {
            check(&route).map_err(|e| format!("route {}: {e}", route.id))?;
            if by_id.contains_key(&route.id) {
                return Err(format!("duplicate route ID {}", route.id));
            }
            by_id.insert(route.id.clone(), route);
        }
        Ok(Routes { by_id })
    }

    pub fn get(&self, id: &str) -> Option<&Route> {
        self.by_id.get(id)
    }

    /// The loaded route IDs, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.by_id.keys().map(String::as_str)
    }
}

/// Checks one route against the profile rules of spec 4.1.
pub fn check(route: &Route) -> Result<(), String> {
    if !is_valid_route_id(&route.id) {
        return Err(format!("id {:?} is not a valid route ID", route.id));
    }
    if !route.length.is_finite() || route.length <= 0.0 {
        return Err("length must be greater than 0".to_string());
    }
    let (first, last) = match route.profile.as_slice() {
        [first, .., last] => (first, last),
        _ => return Err("profile needs at least two points".to_string()),
    };
    if route.profile.iter().any(|p| !p[0].is_finite() || !p[1].is_finite()) {
        return Err("profile values must be finite".to_string());
    }
    if first[0] != 0.0 {
        return Err("the first profile point must have distance 0".to_string());
    }
    if last[0] != route.length {
        return Err("the last profile point must have distance equal to length".to_string());
    }
    if route.profile.windows(2).any(|pair| pair[1][0] <= pair[0][0]) {
        return Err("profile distances must strictly increase".to_string());
    }
    if route.looped && first[1] != last[1] {
        return Err("on a loop, the first and last elevation must be equal".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../protocol/routes");
    const INVALID_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/tools/fixtures/invalid-routes"
    );
    const MISNAMED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/tools/fixtures/misnamed-route"
    );

    fn read_route(path: &Path) -> Route {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn velodrome() -> Route {
        read_route(&Path::new(ROUTES_DIR).join("velodrome-250.json"))
    }

    #[test]
    fn loads_velodrome_from_protocol_repo() {
        let routes = Routes::load_dir(Path::new(ROUTES_DIR)).unwrap();
        let route = routes.get("velodrome-250").expect("velodrome-250 is loaded");
        assert_eq!(route.length, 250.0);
        assert!(route.looped);
    }

    #[test]
    fn rejects_bad_profiles() {
        let mut paths: Vec<PathBuf> = fs::read_dir(INVALID_DIR).unwrap().map(|e| e.unwrap().path()).collect();
        paths.sort();
        assert_eq!(paths.len(), 5, "expected the five invalid route fixtures");
        for path in &paths {
            let route = read_route(path);
            assert!(check(&route).is_err(), "{} should be rejected", path.display());
        }
    }

    #[test]
    fn rejects_file_name_not_matching_id() {
        let err = Routes::load_dir(Path::new(MISNAMED_DIR)).unwrap_err();
        assert!(err.contains("velodrome.json"), "error should name the file: {err}");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let err = Routes::from_routes(vec![velodrome(), velodrome()]).unwrap_err();
        assert!(err.contains("duplicate"), "unexpected error: {err}");
    }
}
