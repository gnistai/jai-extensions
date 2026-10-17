//! With the `gui` feature (cargo sets CARGO_FEATURE_GUI and the matching cfg),
//! runs tauri-build with JAI's Windows manifest: Tauri's default manifest plus
//! consoleAllocationPolicy=detached for the console-subsystem exe (DESIGN.md
//! section 10, "Windows console"). The headless CLI and server need no
//! tauri-build.
//!
//! Always: embeds the third-party licence notices that
//! `studio/scripts/notices.sh` generates, named by `JAI_THIRD_PARTY_NOTICES`
//! (DESIGN.md section 14). Without it the build says it has none, and must
//! not be distributed.

use std::path::{Path, PathBuf};

/// The environment variable naming the notices file.
const NOTICES_ENV: &str = "JAI_THIRD_PARTY_NOTICES";

/// What a build without notices prints for `jai licenses`.
const NO_NOTICES: &str =
    "This build of JAI Studio was made without its third-party licence notices.\n\
Builds that are given to other people include them: studio/scripts/notices.sh generates them\n\
and the build embeds the file named by JAI_THIRD_PARTY_NOTICES.\n";

fn main() {
    // cargo-xwin's MSVC CRT libraries name PDBs that are not shipped, so the
    // linker warns LNK4099 for each of their objects, and rustc reports every
    // one as a linker message.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/IGNORE:4099");
        println!("cargo:rustc-link-arg-tests=/IGNORE:4099");
    }
    notices();
    #[cfg(feature = "gui")]
    gui();
}

/// Writes `$OUT_DIR/notices.txt`, which `main.rs` embeds.
fn notices() {
    println!("cargo:rerun-if-env-changed={NOTICES_ENV}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let text = match std::env::var_os(NOTICES_ENV).filter(|v| !v.is_empty()) {
        Some(file) => {
            let file = Path::new(&file);
            println!("cargo:rerun-if-changed={}", file.display());
            match std::fs::read_to_string(file) {
                Ok(text) if !text.trim().is_empty() => text,
                Ok(_) => fail(&format!("{NOTICES_ENV}: {} is empty", file.display())),
                Err(e) => fail(&format!(
                    "{NOTICES_ENV}: cannot read {}: {e}",
                    file.display()
                )),
            }
        }
        None => NO_NOTICES.to_string(),
    };
    let target = out.join("notices.txt");
    if let Err(e) = std::fs::write(&target, text) {
        fail(&format!("cannot write {}: {e}", target.display()));
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

#[cfg(feature = "gui")]
fn gui() {
    let windows = tauri_build::WindowsAttributes::new().app_manifest(include_str!("jai.manifest"));
    let attributes = tauri_build::Attributes::new().windows_attributes(windows);
    if let Err(e) = tauri_build::try_build(attributes) {
        eprintln!("error: tauri-build failed: {e:#}");
        std::process::exit(1);
    }
}
