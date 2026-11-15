//! Build script for the h4rn mobile shell.
//!
//! Android native code is a source overlay applied by
//! `scripts/mobile-android.sh`. iOS native code is a local Swift package under
//! `native/ios`, which must be linked into the Rust static library so the
//! generated Xcode project picks it up — this mirrors what a Tauri plugin's
//! `ios_path` does, without adding a separate crate.

#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

fn main() {
    link_ios_secret_store();
    tauri_build::build()
}

/// Links `native/ios` on an iOS build.
///
/// The build script runs on the host. `tauri_utils::build::link_apple_library`
/// only exists on macOS, so the whole function is compiled out elsewhere; the
/// `CARGO_CFG_TARGET_OS` check keeps a macOS desktop build from linking it too.
fn link_ios_secret_store() {
    println!("cargo:rerun-if-changed=native/ios");
    println!("cargo:rerun-if-changed=native/ios/Package.swift");

    #[cfg(target_os = "macos")]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("ios") {
            return;
        }

        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
        // The package lives beside `src-tauri`, at `apps/mobile/native/ios`.
        let source = manifest_dir
            .parent()
            .expect("src-tauri has a parent")
            .join("native")
            .join("ios");

        // The Swift package imports Tauri, so give it the same local copy of
        // the iOS API package that plugin build scripts receive.
        if let Ok(tauri_library) = std::env::var("DEP_TAURI_IOS_LIBRARY_PATH") {
            let destination = source
                .parent()
                .expect("native/ios has a parent")
                .join(".tauri")
                .join("tauri-api");
            if let Err(error) = copy_tree(Path::new(&tauri_library), &destination) {
                panic!(
                    "could not stage the Tauri iOS API package at {}: {error}",
                    destination.display()
                );
            }
        }

        tauri_utils::build::link_apple_library("h4rn-secret-store", &source);
    }
}

#[cfg(target_os = "macos")]
fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_dir_all(destination);
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
