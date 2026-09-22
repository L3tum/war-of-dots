/// Map registry for loading maps by name
use crate::map::map::Map;
use std::collections::HashMap;
use std::sync::Mutex;

#[allow(clippy::type_complexity)]
static MAP_REGISTRY: Mutex<Option<HashMap<String, Box<dyn Fn() -> Map + Send + Sync>>>> =
    Mutex::new(None);

/// Register a map loader function under a name
pub fn register(name: &str, loader: impl Fn() -> Map + Send + Sync + 'static) {
    let mut registry = MAP_REGISTRY.lock().unwrap();
    registry
        .get_or_insert_with(HashMap::new)
        .insert(name.to_string(), Box::new(loader));
}

/// Get a map by name, or None if not found
pub fn get(name: &str) -> Option<Map> {
    let registry = MAP_REGISTRY.lock().unwrap();
    registry
        .as_ref()
        .and_then(|m| m.get(name))
        .map(|loader| loader())
}

/// List all registered map names (sorted alphabetically for deterministic ordering)
pub fn list_maps() -> Vec<String> {
    let registry = MAP_REGISTRY.lock().unwrap();
    let mut names: Vec<String> = registry
        .as_ref()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Initialize the registry with the default map and any handcrafted maps
pub fn init() {
    use crate::map::map_config::{load_default_map, load_map_or_default};

    register("default", load_default_map);

    // Load any handcrafted maps from the maps/ directory
    if let Ok(entries) = std::fs::read_dir("maps") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let name = path.file_stem().map(|s| s.to_string_lossy().to_string());
                if let Some(name) = name {
                    let path_str = path.clone();
                    // A missing/corrupt handcrafted map degrades to the
                    // built-in default (with a warning) instead of crashing
                    // game start — see load_map_or_default.
                    register(&name, move || {
                        load_map_or_default(&path_str.to_string_lossy())
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::unit::Terrain;
    use crate::map::map_config::load_map_or_default;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wod-map-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A corrupt handcrafted map file must resolve through the registry to
    /// the built-in default map (the fallback `init`'s loader closures
    /// rely on) — never None, never a panic.
    #[test]
    fn registry_corrupt_map_falls_back_to_default() {
        let dir = temp_dir();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"this is not json {").unwrap();

        let name = format!("wod-test-corrupt-{}", uuid::Uuid::new_v4());
        let owned_path = path.to_string_lossy().into_owned();
        register(&name, move || load_map_or_default(&owned_path));

        let map = get(&name).expect("registered name must resolve");
        assert_eq!(map.width, 200);
        assert_eq!(map.height, 200);
        assert_eq!(map.cities.len(), 5);
        // The central lake is part of the default map — proves we got the
        // fallback, not an empty or half-built map.
        assert_eq!(map.terrain(100, 100), Terrain::Water);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A valid handcrafted map file resolves through the registry to the
    /// loaded map (cities + dimensions preserved).
    #[test]
    fn registry_valid_map_loads() {
        let dir = temp_dir();
        let path = dir.join("valid.json");
        std::fs::write(
            &path,
            r#"{"width":12,"height":9,"cities":[[1.0,1.0],[5.0,4.0]],"terrain_data":null}"#,
        )
        .unwrap();

        let name = format!("wod-test-valid-{}", uuid::Uuid::new_v4());
        let owned_path = path.to_string_lossy().into_owned();
        register(&name, move || load_map_or_default(&owned_path));

        let map = get(&name).expect("registered name must resolve");
        assert_eq!(map.width, 12);
        assert_eq!(map.height, 9);
        assert_eq!(map.cities.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `get` on an unregistered name is None (documented contract).
    #[test]
    fn registry_unknown_name_is_none() {
        assert!(get(&format!(
            "wod-test-never-registered-{}",
            uuid::Uuid::new_v4()
        ))
        .is_none());
    }
}
