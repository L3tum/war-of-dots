//! Binary to generate JSON Schema from Rust types

mod server {
    pub mod protocol;
}

use schemars::schema_for;
use std::fs;

fn main() {
    let schema = schema_for!(server::protocol::ServerMessage);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    fs::write("frontend/schema/protocol.schema.json", &json).expect("Failed to write schema");
    println!("Generated protocol.schema.json");
}
