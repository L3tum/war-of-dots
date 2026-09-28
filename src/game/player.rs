//! Player display state.
//!
//! Player resources (funds, units) live in ECS components and the `Game`
//! internals; this struct carries only the display name the server reads
//! (e.g. the GameOver winner lookup in `server/room/tick.rs`).

#[derive(Debug, Clone)]
pub struct Player {
    pub name: String,
}

impl Player {
    pub fn new(name: String) -> Self {
        Player { name }
    }
}
