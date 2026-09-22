//! Build script: bundle the frontend into `frontend/dist` so `include_dir!`
//! picks it up at compile time.
//!
//! The npm build is SKIPPED when `frontend/dist` is up to date (every dist
//! output is newer than every input — the `frontend/src` tree, `index.html`,
//! `package.json`, `package-lock.json` and `tsconfig.json`, which esbuild
//! reads). This keeps `cargo build` fast on incremental server-only changes.
//!
//! `WOD_SKIP_FRONTEND=1` force-skips the npm build entirely (fast server-only
//! dev loops) and embeds whatever `frontend/dist` already contains; it fails
//! loudly if dist is missing, since `include_dir!` would not.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

/// The files `npm run build` emits (esbuild bundle + copied SPA shell). All
/// must exist and be fresh for the skip to trigger.
const DIST_OUTPUTS: &[&str] = &["frontend/dist/main.js", "frontend/dist/index.html"];

/// Everything the build consumes besides the `frontend/src` tree (watched via
/// `cargo:rerun-if-changed` below as well): `index.html` is copied verbatim,
/// `package.json`/`package-lock.json` pin the esbuild toolchain, and
/// `tsconfig.json` is read by esbuild for compiler options.
const INPUT_FILES: &[&str] = &[
    "frontend/index.html",
    "frontend/package.json",
    "frontend/package-lock.json",
    "frontend/tsconfig.json",
];

fn main() {
    // Re-run if build.rs or any build input changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=frontend/src");
    for input in INPUT_FILES {
        println!("cargo:rerun-if-changed={input}");
    }

    // Force-skip for fast server-only dev loops (documented in README).
    if std::env::var("WOD_SKIP_FRONTEND").is_ok_and(|v| v == "1") {
        if !dist_outputs_exist() {
            eprintln!(
                "WOD_SKIP_FRONTEND=1 but frontend/dist outputs are missing \n\
                 ({}) — run `cd frontend && npm run build` once, or unset \n\
                 WOD_SKIP_FRONTEND",
                DIST_OUTPUTS.join(", ")
            );
            std::process::exit(1);
        }
        eprintln!("WOD_SKIP_FRONTEND=1: skipping npm build, embedding existing frontend/dist");
        return;
    }

    if dist_is_fresh() {
        eprintln!("frontend dist up to date, skipping npm build");
        return;
    }

    let status = Command::new("npm")
        .current_dir("frontend")
        .args(["run", "build"])
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("Frontend build failed with status: {}", s);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to run npm build: {}", e);
            std::process::exit(1);
        }
    }
}

/// True when every dist output exists on disk.
fn dist_outputs_exist() -> bool {
    DIST_OUTPUTS.iter().all(|path| Path::new(path).is_file())
}

/// True when every dist output is strictly newer than every input (the newest
/// file in `frontend/src` plus the individual input files): the OLDEST dist
/// output must be newer than the NEWEST input. Any missing file or unreadable
/// mtime counts as "not fresh" — rebuilding is always the safe side.
/// Strictly-newer (not `>=`) so a same-second input touch rebuilds.
fn dist_is_fresh() -> bool {
    // All dist outputs must exist (a missing one can never be fresh).
    if !dist_outputs_exist() {
        return false;
    }
    let output_oldest = DIST_OUTPUTS
        .iter()
        .map(|p| mtime(p))
        .min() // Option: None < Some, so a missing/unreadable mtime fails
        .flatten();
    let Some(output_oldest) = output_oldest else {
        return false;
    };
    // All input files must exist too.
    let input_times: Vec<Duration> = INPUT_FILES.iter().filter_map(|p| mtime(p)).collect();
    if input_times.len() != INPUT_FILES.len() {
        return false;
    }
    let Some(mut newest_input) = input_times.into_iter().max() else {
        return false;
    };
    if let Some(src_newest) = src_tree_newest_mtime() {
        if src_newest > newest_input {
            newest_input = src_newest;
        }
    } else {
        // `frontend/src` missing or empty → not fresh.
        return false;
    }
    output_oldest > newest_input
}

fn mtime(path: &str) -> Option<Duration> {
    std::fs::metadata(path).ok().and_then(mtime_of)
}

fn mtime_of(meta: std::fs::Metadata) -> Option<Duration> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
}

/// Newest mtime of any file under `frontend/src` (recursive). Errors or an
/// empty tree → `None` (treated as not fresh).
fn src_tree_newest_mtime() -> Option<Duration> {
    let mut newest = None;
    let mut stack = vec![Path::new("frontend/src").to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return None;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(t) = std::fs::metadata(&path).ok().and_then(mtime_of) {
                if newest.is_none_or(|n| t > n) {
                    newest = Some(t);
                }
            }
        }
    }
    newest
}
