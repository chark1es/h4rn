// swift-tools-version:5.9
//
// Local Swift package for h4rn's iOS Keychain secret store.
//
// `src-tauri/build.rs` copies Tauri's iOS API package into `.tauri/tauri-api`
// and calls `link_apple_library("h4rn-secret-store", "native/ios")`, so this
// package is compiled into the app's Rust static library exactly like a Tauri
// plugin's native code.

import PackageDescription

let package = Package(
  name: "h4rn-secret-store",
  platforms: [
    .iOS(.v16)
  ],
  products: [
    .library(
      name: "h4rn-secret-store",
      type: .static,
      targets: ["H4rnSecretStore"]
    )
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "H4rnSecretStore",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources"
    )
  ]
)
