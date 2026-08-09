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
            (100.0, 45.0),
        ],
    }
}
