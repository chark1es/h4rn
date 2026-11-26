// swift-tools-version:5.9
//
// The words of h4rn's iOS phone alerts (ADR-055).
//
// `H4rnAlertWords` is plain Foundation: no UIKit, no UserNotifications and no
// Tauri, so `swift test` runs it on a Mac (or Linux) without a simulator. The
// Notification Service Extension compiles the same sources and the same
// `.lproj` folders into its own bundle; `scripts/mobile-ios-notifications.sh`
// adds that target to the generated Xcode project. It is a separate package
// from `native/ios`, which needs Tauri's staged iOS API and UIKit to build.

import PackageDescription

let package = Package(
  name: "h4rn-alert-words",
  defaultLocalization: "en",
  platforms: [
    .iOS(.v16),
    .macOS(.v13),
  ],
  products: [
    .library(name: "H4rnAlertWords", targets: ["H4rnAlertWords"])
  ],
  targets: [
    .target(
      name: "H4rnAlertWords",
      path: "Sources/H4rnAlertWords",
      resources: [.process("Resources")]
    ),
    .testTarget(
      name: "H4rnAlertWordsTests",
      dependencies: ["H4rnAlertWords"],
      path: "Tests/H4rnAlertWordsTests"
    ),
  ]
)
