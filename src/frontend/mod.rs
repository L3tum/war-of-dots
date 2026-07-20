//! Frontend asset embedding

use include_dir::{include_dir, Dir};

#[allow(dead_code)]
static FRONTEND: Dir = include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

#[allow(dead_code)]
pub fn get_frontend() -> &'static Dir<'static> {
    &FRONTEND
}
