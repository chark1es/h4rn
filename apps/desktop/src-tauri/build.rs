//! Build script for the h4rn desktop shell.
//!
//! Tauri bundles the daemon as an external binary ("sidecar") and requires the
//! target-suffixed file to exist when this build script runs. Real builds get a
//! freshly built `h4rnd` from `scripts/desktop-build-windows.sh`; a plain
//! `cargo check` (what CI runs on Linux) only compiles the shell, so a zero-byte
//! placeholder is enough there. A `tauri build` without the sidecar fails loudly
//! instead of silently shipping a bundle that cannot start its daemon.

use std::path::{Path, PathBuf};

fn main() {
    ensure_sidecar();
    println!("cargo:rerun-if-changed=binaries");
    tauri_build::build()
}

fn ensure_sidecar() {
    let target = match std::env::var("TARGET") {
        Ok(target) => target,
        Err(_) => return,
    };
    let extension = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let path = PathBuf::from("binaries").join(format!("h4rnd-{target}{extension}"));
    if is_real_binary(&path) {
        return;
    }

    // The Tauri CLI sets TAURI_ENV_PLATFORM for both `tauri dev` and
    // `tauri build`, which means the bundle will look for this executable at
    // runtime. Never paper over that with a placeholder.
    let bundling = std::env::var_os("TAURI_ENV_PLATFORM").is_some();
    if bundling {
        panic!(
            "the h4rnd sidecar is missing or empty at {}.\n\
             Build it first:\n  \
             cargo build --release --manifest-path ../../../crates/h4rn-daemon/Cargo.toml\n  \
             and copy target/release/h4rnd to apps/desktop/src-tauri/binaries/h4rnd-{target}{extension}\n\
             or run scripts/desktop-build-windows.sh, which does both.",
            path.display()
        );
    }

    if let Err(error) = std::fs::create_dir_all("binaries") {
        println!("cargo:warning=could not create the sidecar directory: {error}");
        return;
    }
    if let Err(error) = std::fs::write(&path, []) {
        println!("cargo:warning=could not write the sidecar placeholder: {error}");
        return;
    }
    println!(
        "cargo:warning=wrote an empty sidecar placeholder at {}; \
         this is fine for `cargo check`, but bundles need a real h4rnd",
        path.display()
    );
}

fn is_real_binary(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}
