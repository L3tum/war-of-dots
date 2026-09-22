//! Handcrafted map loader

use crate::game::unit::Terrain;
use crate::map::map::{Map, MapConfig};

pub fn load_map(path: &str) -> Result<Map, crate::errors::MapError> {
    let content = std::fs::read_to_string(path).map_err(|_| crate::errors::MapError::NotFound {
        path: path.to_string(),
    })?;
    let config: MapConfig =
        serde_json::from_str(&content).map_err(|e| crate::errors::MapError::InvalidFormat {
            detail: e.to_string(),
        })?;
    Ok(Map::from_config(&config))
}

/// Load the map JSON at `path`, falling back to the built-in default map
/// (with a warning) when the file is missing or corrupt. This is the loader
/// shape the registry uses for handcrafted `maps/*.json` files: a bad map
/// file must degrade to the default map, not crash game start.
pub fn load_map_or_default(path: &str) -> Map {
    match load_map(path) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(%e, path, "map JSON failed to load — falling back to the default map");
            load_default_map()
        }
    }
}

pub fn load_default_map() -> Map {
    // Default 200x200 map with a central lake, corner mountain clusters,
    // hill/forest patches, and cities at the four corners + one central.
    // cells are stored as cells[x][y] (matches Map::terrain(x, y)).
    const W: usize = 200;
    const H: usize = 200;
    let mut cells = vec![vec![Terrain::Plains; H]; W];

    // Central lake (rounded), centered at (100, 100), radius 8. Kept modest so the
    // default centered view isn't entirely water.
    let (lcx, lcy, lr): (i32, i32, i32) = (100, 100, 8);
    let mountains: [(i32, i32); 4] = [(30, 30), (170, 30), (30, 170), (170, 170)];
    let hills: [(i32, i32); 5] = [(100, 55), (55, 100), (145, 100), (100, 145), (170, 170)];
    let forests: [(i32, i32); 6] = [
        (100, 30),
        (30, 100),
        (170, 100),
        (100, 170),
        (72, 72),
        (128, 128),
    ];

    for (xi, row) in cells.iter_mut().enumerate() {
        for (yi, cell) in row.iter_mut().enumerate() {
            // Lake
            if (xi as i32 - lcx).pow(2) + (yi as i32 - lcy).pow(2) <= lr.pow(2) {
                *cell = Terrain::Water;
            }
        }
    }

    // Mountain clusters (higher priority than plains/lake-overlap)
    for (xi, row) in cells.iter_mut().enumerate() {
        for (yi, cell) in row.iter_mut().enumerate() {
            if *cell != Terrain::Plains {
                continue;
            }
            for (mx, my) in mountains {
                if (xi as i32 - mx).pow(2) + (yi as i32 - my).pow(2) <= 100 {
                    *cell = Terrain::Mountain;
                }
            }
        }
    }

    // Hills
    for (xi, row) in cells.iter_mut().enumerate() {
        for (yi, cell) in row.iter_mut().enumerate() {
            if *cell != Terrain::Plains {
                continue;
            }
            for (hx, hy) in hills {
                if (xi as i32 - hx).pow(2) + (yi as i32 - hy).pow(2) <= 49 {
                    *cell = Terrain::Hills;
                }
            }
        }
    }

    // Forests
    for (xi, row) in cells.iter_mut().enumerate() {
        for (yi, cell) in row.iter_mut().enumerate() {
            if *cell != Terrain::Plains {
                continue;
            }
            for (fx, fy) in forests {
                if (xi as i32 - fx).pow(2) + (yi as i32 - fy).pow(2) <= 64 {
                    *cell = Terrain::Forest;
                }
            }
        }
    }

    Map {
        width: W as u32,
        height: H as u32,
        cells,
        cities: vec![
            (10.0, 10.0),
            (190.0, 10.0),
            (10.0, 190.0),
            (190.0, 190.0),
            (100.0, 100.0),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::MapError;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wod-map-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_map_missing_file_is_not_found() {
        let err =
            load_map("/nonexistent/wod-test-map.json").expect_err("missing file must not load");
        match err {
            MapError::NotFound { path } => assert_eq!(path, "/nonexistent/wod-test-map.json"),
            other => panic!("expected NotFound, got: {other}"),
        }
    }

    #[test]
    fn load_map_corrupt_json_is_invalid_format() {
        let dir = temp_dir();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"this is not json {").unwrap();

        let err = load_map(path.to_str().unwrap()).expect_err("corrupt JSON must not load");
        match err {
            MapError::InvalidFormat { detail } => {
                assert!(!detail.is_empty(), "error should carry the parse reason")
            }
            other => panic!("expected InvalidFormat, got: {other}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_map_parses_valid_json() {
        let dir = temp_dir();
        let path = dir.join("valid.json");
        std::fs::write(
            &path,
            r#"{"width":20,"height":20,"cities":[[3.0,3.0],[16.0,16.0]],"terrain_data":null}"#,
        )
        .unwrap();

        let map = load_map(path.to_str().unwrap()).expect("valid JSON must load");
        assert_eq!(map.width, 20);
        assert_eq!(map.height, 20);
        assert_eq!(map.cities.len(), 2);
        assert_eq!(map.cities[0], (3.0, 3.0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_map_or_default_falls_back_on_corrupt_file() {
        let dir = temp_dir();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"not json").unwrap();

        let map = load_map_or_default(path.to_str().unwrap());
        // The built-in default is 200x200 with five cities.
        assert_eq!(map.width, 200);
        assert_eq!(map.height, 200);
        assert_eq!(map.cities.len(), 5);
        // The central lake is part of the default map — proves the fallback
        // returned the real default, not an empty/half-built map.
        assert_eq!(map.terrain(100, 100), Terrain::Water);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_map_or_default_falls_back_on_missing_file() {
        let map = load_map_or_default("/nonexistent/wod-test-map.json");
        assert_eq!(map.width, 200);
        assert_eq!(map.cities.len(), 5);
    }

    #[test]
    fn load_map_or_default_returns_loaded_map_when_valid() {
        let dir = temp_dir();
        let path = dir.join("valid.json");
        std::fs::write(
            &path,
            r#"{"width":12,"height":9,"cities":[[1.0,1.0]],"terrain_data":null}"#,
        )
        .unwrap();

        let map = load_map_or_default(path.to_str().unwrap());
        assert_eq!(map.width, 12);
        assert_eq!(map.height, 9);
        assert_eq!(map.cities.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
