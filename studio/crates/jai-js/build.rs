//! Checks that the QuickJS bundle exists before `src/lib.rs` embeds it.

use std::path::PathBuf;

fn main() {
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        eprintln!("error: CARGO_MANIFEST_DIR is not set");
        std::process::exit(1);
    };
    // studio/crates/jai-js -> jai-tools
    let tools = PathBuf::from(manifest_dir).join("../../..");
    let tools = tools.canonicalize().unwrap_or(tools);
    let host = tools.join("dist/studio/host.mjs");
    println!("cargo:rerun-if-changed={}", host.display());
    if !host.is_file() {
        eprintln!(
            "error: {} is missing: run npm run build in jai-tools first",
            host.display()
        );
        std::process::exit(1);
    }
}
