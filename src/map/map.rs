//! Map representation

use crate::game::unit::Terrain;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Map {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<Vec<Terrain>>,
    pub cities: Vec<(f32, f32)>,
}

impl Map {
    pub fn terrain(&self, x: u32, y: u32) -> Terrain {
        self.cells[x as usize][y as usize]
    }

    pub fn from_config(config: &MapConfig) -> Self {
        let mut cells = vec![vec![Terrain::Plains; config.height as usize]; config.width as usize];
        // Parse terrain string if present
        if let Some(terrain) = &config.terrain_data {
            let lines: Vec<&str> = terrain.split('\n').collect();
            for (y, line) in lines.iter().enumerate() {
                for (x, ch) in line.chars().enumerate() {
                    if x < config.width as usize && y < config.height as usize {
                        cells[x][y] = match ch {
                            '.' => Terrain::Plains,
                            '~' => Terrain::Water,
                            '^' => Terrain::Mountain,
                            '#' => Terrain::Hills,
                            '*' => Terrain::Forest,
                            _ => Terrain::Plains,
                        };
                    }
                }
            }
        }
        Map {
            width: config.width,
            height: config.height,
            cells,
            cities: config.cities.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapConfig {
    pub width: u32,
    pub height: u32,
    pub cities: Vec<(f32, f32)>,
    pub terrain_data: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // Map terrain
    // ---------------------------------------------------------------------------

    #[test]
    fn terrain_at_valid_position() {
        let m = Map {
            width: 5,
            height: 5,
            cells: vec![vec![Terrain::Plains; 5]; 5],
            cities: vec![],
        };
        assert_eq!(m.terrain(0, 0), Terrain::Plains);
        assert_eq!(m.terrain(4, 4), Terrain::Plains);
    }

    #[test]
    fn terrain_mixed_cells() {
        let mut m = Map {
            width: 3,
            height: 3,
            cells: vec![vec![Terrain::Plains; 3]; 3],
            cities: vec![],
        };
        m.cells[0][0] = Terrain::Water;
        m.cells[1][1] = Terrain::Hills;
        m.cells[2][2] = Terrain::Mountain;

        assert_eq!(m.terrain(0, 0), Terrain::Water);
        assert_eq!(m.terrain(1, 1), Terrain::Hills);
        assert_eq!(m.terrain(2, 2), Terrain::Mountain);
        assert_eq!(m.terrain(0, 2), Terrain::Plains);
    }

    // ---------------------------------------------------------------------------
    // Map::from_config
    // ---------------------------------------------------------------------------

    #[test]
    fn from_config_no_terrain_data_all_plains() {
        let config = MapConfig {
            width: 3,
            height: 3,
            cities: vec![(1.0, 1.0)],
            terrain_data: None,
        };
        let map = Map::from_config(&config);

        assert_eq!(map.width, 3);
        assert_eq!(map.height, 3);
        assert_eq!(map.cities, vec![(1.0, 1.0)]);
        for x in 0..3 {
            for y in 0..3 {
                assert_eq!(map.terrain(x, y), Terrain::Plains);
            }
        }
    }

    #[test]
    fn from_config_parses_terrain_string() {
        let config = MapConfig {
            width: 3,
            height: 3,
            cities: vec![],
            terrain_data: Some(".~^\n#*\n...".to_string()),
        };
        let map = Map::from_config(&config);

        // Row 0: .~^
        assert_eq!(map.terrain(0, 0), Terrain::Plains);
        assert_eq!(map.terrain(1, 0), Terrain::Water);
        assert_eq!(map.terrain(2, 0), Terrain::Mountain);
        // Row 1: #*
        assert_eq!(map.terrain(0, 1), Terrain::Hills);
        assert_eq!(map.terrain(1, 1), Terrain::Forest);
        // Row 2: ...
        assert_eq!(map.terrain(0, 2), Terrain::Plains);
        assert_eq!(map.terrain(1, 2), Terrain::Plains);
        assert_eq!(map.terrain(2, 2), Terrain::Plains);
    }

    #[test]
    fn from_config_ignores_unknown_chars_as_plains() {
        let config = MapConfig {
            width: 3,
            height: 1,
            cities: vec![],
            terrain_data: Some("X!@".to_string()),
        };
        let map = Map::from_config(&config);

        assert_eq!(map.terrain(0, 0), Terrain::Plains);
        assert_eq!(map.terrain(1, 0), Terrain::Plains);
        assert_eq!(map.terrain(2, 0), Terrain::Plains);
    }

    #[test]
    fn from_config_clips_to_width_height() {
        let config = MapConfig {
            width: 2,
            height: 1,
            cities: vec![],
            terrain_data: Some(".~^\n#*".to_string()),
        };
        let map = Map::from_config(&config);

        assert_eq!(map.width, 2);
        assert_eq!(map.height, 1);
        assert_eq!(map.terrain(0, 0), Terrain::Plains);
        assert_eq!(map.terrain(1, 0), Terrain::Water);
        // Third char (^) should be clipped
        assert_eq!(map.cells.len(), 2);
    }

    #[test]
    fn from_config_cities_preserved() {
        let config = MapConfig {
            width: 10,
            height: 10,
            cities: vec![(2.0, 2.0), (7.0, 5.0), (9.0, 9.0)],
            terrain_data: None,
        };
        let map = Map::from_config(&config);

        assert_eq!(map.cities.len(), 3);
        assert_eq!(map.cities[0], (2.0, 2.0));
        assert_eq!(map.cities[1], (7.0, 5.0));
        assert_eq!(map.cities[2], (9.0, 9.0));
    }
}
