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

/// List all registered map names
pub fn list_maps() -> Vec<String> {
    let registry = MAP_REGISTRY.lock().unwrap();
    registry
        .as_ref()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Initialize the registry with the default map and any handcrafted maps
pub fn init() {
    use crate::map::map_config::{load_default_map, load_map};

    register("default", load_default_map);

    // Load any handcrafted maps from the maps/ directory
    if let Ok(entries) = std::fs::read_dir("maps") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let name = path.file_stem().map(|s| s.to_string_lossy().to_string());
                if let Some(name) = name {
                    let path_str = path.clone();
                    register(&name, move || {
                        load_map(&path_str.to_string_lossy()).unwrap_or_else(|_| load_default_map())
                    });
                }
            }
        }
    }
}
