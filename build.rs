use std::process::Command;

fn main() {
    // Build the frontend so include_dir! picks up the latest dist/
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

    // Re-run if build.rs or any frontend source changes
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/package.json");
}
