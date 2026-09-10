//! Regenerate the frontend JSON schema (`frontend/schema/protocol.schema.json`)
//! from the current wire protocol types, so `npm run generate-types` stays in
//! lockstep with `src/server/protocol.rs`.
//!
//! Usage: `cargo run --bin generate_schema` (run from the repo root; the
//! frontend `npm run generate-types` script wraps this call).

use std::fs;
use schemars::schema_for;
use war_of_dots::server::protocol::ServerMessage;

fn main() {
    let schema = schema_for!(ServerMessage);
    let json = serde_json::to_string_pretty(&schema).expect("failed to serialize schema");
    let path = "frontend/schema/protocol.schema.json";
    fs::write(path, json + "\n").expect("failed to write frontend/schema/protocol.schema.json");
    println!("wrote {path}");
}
