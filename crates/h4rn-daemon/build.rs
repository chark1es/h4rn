//! Build metadata the daemon needs at compile time.

fn main() {
    // Rust does not expose the target triple to library code, and a daemon
    // updating itself must pick the archive built for the exact target
    // (glibc, musl and the Apple universal build differ).
    let target = std::env::var("TARGET").expect("Cargo always sets TARGET");
    println!("cargo:rustc-env=H4RN_TARGET={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
    // `option_env!` reads these in the library; without this a rebuild in the
    // same target directory could keep a stale key or repository.
    println!("cargo:rerun-if-env-changed=H4RN_UPDATER_PUBKEY");
    println!("cargo:rerun-if-env-changed=H4RN_REPOSITORY");
}
