//! Player state: funds, units, vision

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub is_human: bool,
    pub funds: u32,
    pub units: Vec<String>, // unit ids
    pub cities_owned: Vec<String>,
}

impl Player {
    pub fn new(id: String, name: String, is_human: bool, starting_funds: u32) -> Self {
        Player {
            id,
            name,
            is_human,
            funds: starting_funds,
            units: Vec::new(),
            cities_owned: Vec::new(),
        }
    }
}
