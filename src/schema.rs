//! Binary to generate JSON Schema from Rust types

use schemars::schema_for;
use std::fs;
use war_of_dots::server::protocol::ServerMessage;

fn main() {
    let schema = schema_for!(ServerMessage);
    let json = serde_json::to_string_pretty(&schema).unwrap();
    fs::write("frontend/schema/protocol.schema.json", &json).expect("Failed to write schema");
    println!("Generated protocol.schema.json");
}
