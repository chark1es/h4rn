# Mobile

h4rn ships one Tauri v2 shell for Android and iOS. The shell is deliberately thin: it loads the
same React bundle as the desktop app, and every product behaviour still goes through the daemon's
WebSocket protocol (ADR-001). What the native side adds is the small set of things a WebView
cannot do on its own:

| Concern | Implementation |
|---|---|
| Pairing secret | Android Keystore (AES-256/GCM) behind `secret_get` / `secret_set` / `secret_delete` (ADR-016) |
| QR pairing | `tauri-plugin-barcode-scanner` (ML Kit on Android, Vision on iOS) |
| `h4rn://` links | `tauri-plugin-deep-link`, generated intent filter / `CFBundleURLTypes` from `tauri.conf.json` |
| Android back button | `MainActivity` maps back onto WebView history, which the React router already owns |
| Edge-to-edge | `enableEdgeToEdge()` + `windowLayoutInDisplayCutoutMode=shortEdges` + `viewport-fit=cover`; the UI can read `env(safe-area-inset-*)` (also exposed as `--h4rn-safe-area-*`) |
| Cleartext dev traffic | Noise end-to-end encryption with the daemon key pinned; the release network security config and iOS ATS allow ws:// to LAN, `.local`, MagicDNS and CGNAT/Tailscale candidates (see below) |
| Pairing screen | `apps/ui/src/lib/platform/mobile.ts`, the only mobile branch in the shared UI |

Notifications are not sent by the mobile build yet. Settings → Notifications says so instead of
showing switches that would do nothing; the desktop app and the browser send them through the
Tauri notification plugin and the Web Notification API. Wiring the same choices to Android and
iOS needs the notification plugin and its platform permission flow, which is a later pass.

The pairing screen is a small DOM overlay rather than a React route because the UI is a pure view
over the daemon and its first screen assumes it already knows where the daemon is. The adapter
runs before React and only activates inside a Tauri mobile WebView (`__TAURI_INTERNALS__` plus a
mobile user-agent), so desktop and browser builds are unchanged.

## Why the Android build runs in WSL

The desktop Windows build exists because Tauri's Linux window stack (WebKitGTK) is not installed
here; Android has no such constraint. The Linux side already has the Android SDK at
`~/Android/Sdk` (build-tools 36/37, platform `android-37.0`, NDK 27.1.12297006) and the Rust
Android targets, while a Windows build would have to mirror the worktree off `\\wsl$` first, as
`scripts/desktop-build-windows.sh` does, and would need a second Rust target installation. The
script therefore:

1. downloads Temurin JDK 21 into `~/.local/jdk` when no JDK is present (no `sudo` needed);
2. builds the APK in place with Gradle and the Linux SDK/NDK;
3. uses the **Windows** `adb.exe` only for device operations, because the emulator runs on
   Windows.

That keeps the slow path (Rust + Gradle) on a filesystem and toolchain that support it and keeps
the device path where the device actually is.

## Android: first run

Prerequisites (all present on the current development machine):

- Bun 1.4.4, Rust 1.99.1 with `aarch64-linux-android`, `armv7-linux-androideabi`,
  `i686-linux-android`, `x86_64-linux-android`.
- Android SDK with platform `android-37.0`, build-tools 36, NDK 27.1.12297006.
- A JDK 21. `scripts/mobile-android.sh` downloads Temurin 21 into `~/.local/jdk` if `JAVA_HOME`
  is unset.
- On Windows: Android Studio's emulator and `adb.exe`, plus an AVD (default
  `Medium_Phone_API_36.0`).

```sh
# Generate src-tauri/gen/android and apply the native overlay (rarely needed;
# `build` and `run` do it automatically when the project is missing).
scripts/mobile-android.sh init

# Build a debug APK per ABI: x86_64 for the emulator and aarch64 for phones.
scripts/mobile-android.sh build

# Restrict the build to one ABI when iterating.
H4RN_ANDROID_TARGETS=x86_64 scripts/mobile-android.sh build

# Start the emulator if needed, start a fake-harness daemon, wire adb reverse,
# install, launch with a pairing deep link, wait, screenshot and dump logcat.
scripts/mobile-android.sh run

scripts/mobile-android.sh screenshot inbox.png
scripts/mobile-android.sh logs
```

`gen/android` is generated and ignored by git. The parts h4rn owns live in
`apps/mobile/native/android/` and are copied over the generated project on every `init`, `build`
and `run`. This is the same pattern the repository already uses for generated protocol
bindings: the source is checked in, the generated tree is not. The overlay manifest carries the
`h4rn://` `<intent-filter>` inside the `DEEP LINK PLUGIN. AUTO-GENERATED` comment block:
`tauri-plugin-deep-link` rewrites that block from `plugins.deep-link.mobile` whenever its build
script runs, and the filter survives when cargo decides the plugin is already up to date.

## How the emulator reaches the daemon

```
Android app ──ws://127.0.0.1:47800/ws──▶ emulator loopback
        │
        ▼ adb reverse tcp:47800 tcp:47800
Windows adb server ──▶ Windows 127.0.0.1:47800
        │
        ▼ WSL2 localhost forwarding
WSL2 h4rnd (127.0.0.1:47800)
```

`adb reverse` is preferred over the emulator's `10.0.2.2` host alias because the resulting URL is
identical to the desktop and browser URL (`ws://127.0.0.1:47800/ws`), so a pairing token copied
from `h4rnd token` works everywhere without editing. Run `h4rnd serve --bind 0.0.0.0:47800` and
pair against `ws://10.0.2.2:47800/ws` only when `networkingMode=mirrored` or
`localhostForwarding` is disabled in `.wslconfig`.

`scripts/mobile-android.sh` verifies the whole path: it checks `/healthz` from WSL, then from
Windows through `curl.exe`, before launching the app.

## Cleartext candidates on mobile

The daemon has no TLS listener. Confidentiality, integrity and authentication
come from the Noise session, whose daemon static key is pinned from the pairing
payload ([ADR-010](decisions.md#adr-010-noise-e2e-between-device-and-daemon-the-relay-sees-metadata-only),
[ADR-031](decisions.md#adr-031-one-noise-ik-handshake-per-path-pinned-daemon-key-and-device-allowlist)).
A `ws://` candidate therefore adds no exposure: an on-path observer sees opaque
Noise packets, and an impostor cannot finish the handshake without the pinned
key. Only the relay candidate uses `wss://`, and the relay forwards Noise packets
too.

Both mobile OSes ship a policy that would otherwise veto the plaintext
candidates a phone needs — the daemon on the LAN, a `.local` name, a MagicDNS
name or a Tailscale/CGNAT address — and silently leave only the relay:

- **Android** (`apps/mobile/native/android/app/src/main/res/xml/network_security_config.xml`)
  permits cleartext in the release base config. `<domain-config>` can name exact
  hosts and IP literals but not IP ranges, so RFC 1918, `100.64.0.0/10` and
  `.local`/MagicDNS cannot all be expressed there. The debug source set keeps
  its own permissive config.
- **iOS** (`apps/mobile/src-tauri/Info.plist`, merged by Tauri) sets
  `NSAllowsLocalNetworking` for unqualified names and `.local`, and lists
  `.local` and `ts.net` as `NSExceptionDomains` so MagicDNS works. ATS matches
  DNS names, not address ranges, so private and CGNAT IP literals cannot be
  excepted with CIDR keys. Whether iOS 17 admits a raw `ws://` connection to
  such an address is unverified: iOS apps are built only by the macOS CI job,
  which does not run them. This is an accepted limitation; a phone that cannot
  reach an IP candidate uses the relay.

`apps/ui/src/lib/client/secure.test.ts` checks both checked-in configs and the
candidate-URL builder, so a future "tighten the release policy" change fails a
test instead of quietly breaking direct mobile access.

## Pairing

The app needs two values: a WebSocket URL and the daemon's 256-bit access token
(`h4rnd token`). They can arrive through any of these paths:

```text
h4rn://connect?daemon=ws%3A%2F%2F127.0.0.1%3A47800%2Fws&token=<token>
h4rn://pair?daemon=...&token=...
```

- Cold start: `am start -a android.intent.action.VIEW -d 'h4rn://connect?…' dev.h4rn.app` (what
  the script does).
- Warm start: the deep-link plugin delivers the URL; the shell forwards it to the WebView as a
  `h4rn-deep-link` DOM event.
- Typed or scanned in the pairing screen.
- Reused from the Android Keystore on later launches.

The token is not kept in the URL: the adapter writes it to the platform secret store (Android
Keystore / iOS Keychain behind `secret_*`) and rewrites the URL with only the non-secret
`?daemon=`. A `?token=` bootstrap value is consumed on boot and stripped from the address bar;
legacy `localStorage` copies are migrated and deleted (ADR-041). OIDC callbacks are registered
under the same scheme (`h4rn://oidc/callback`, `h4rn://callback`) and forwarded to the remote
store. Sign-in opens the provider in the system browser through the opener plugin, so the
webview keeps the app bundle and the deep link returns the authorization code.

### QR format

A pairing QR is just the deep link, for example:

```text
h4rn://connect?daemon=ws%3A%2F%2F10.0.2.2%3A47800%2Fws&token=<token>
```

The scanner accepts QR codes only and repopulates the same form, so a scanned link is never
trusted differently from a typed one.

## Native notes

- **Keystore.** `SecretStorePlugin` generates a non-exportable AES-256 key in the Android
  Keystore, encrypts each value with a fresh GCM IV, binds the logical key as AAD and stores the
  ciphertext in private SharedPreferences. A key invalidated by a lock-screen change is dropped
  so the app can pair again. There is no biometric prompt yet; the key is Keystore-backed, which
  is the Phase 3 requirement, and a `BiometricPrompt` gate can wrap reads later without changing
  the Tauri command surface.
- **Back button.** `MainActivity` installs an `OnBackPressedCallback` that calls
  `WebView.goBack()` while history remains. The React app listens for `popstate`, so
  thread → inbox works and the app exits only at the root.
- **Network security.** The Noise session, not the socket, is the security boundary: the
  daemon key is pinned from the pairing payload and an on-path observer sees only opaque
  packets, so a `ws://` candidate adds no exposure ([ADR-010](decisions.md#adr-010-noise-e2e-between-device-and-daemon-the-relay-sees-metadata-only),
  [ADR-031](decisions.md#adr-031-one-noise-ik-handshake-per-path-pinned-daemon-key-and-device-allowlist)).
  Release builds therefore permit cleartext in the Android base config so LAN, `.local`,
  MagicDNS and CGNAT/Tailscale candidates work; see
  [Cleartext candidates on mobile](#cleartext-candidates-on-mobile) above. `wss://` remains
  the relay's transport and adds defence in depth there, not a requirement for direct paths.
- **Icons and id.** `dev.h4rn.app`, product name `h4rn`, scaffold icons, min SDK 26, target SDK
  37.

## iOS

iOS is configured in `apps/mobile/src-tauri/tauri.conf.json` (bundle id, minimum iOS 16,
`h4rn` URL scheme via `plugins.deep-link.mobile`) but **cannot be initialised or built on
Linux**: every `tauri ios` command is macOS-only. The CI job `mobile-ios` runs on
`macos-latest` and does:

```sh
bun run --cwd apps/mobile tauri ios init --ci
bun run --cwd apps/mobile tauri ios build --debug --ci --target aarch64-sim
```

The simulator build is unsigned, which is all CI can do without the owner's Apple credentials.
The first `ios init` generates `src-tauri/gen/apple`; it is ignored like `gen/android`.

### What TestFlight needs from the owner

Nothing in this repository can complete an App Store upload without account credentials:

1. **Apple Developer Program** membership for the account that owns `dev.h4rn.app` (or a new
   bundle id registered in Certificates, Identifiers & Profiles).
2. An **App Store Connect app record** for `dev.h4rn.app` and the display name `h4rn`.
3. A **distribution certificate** (`.p12` + password) and a **provisioning profile** for that
   bundle id, or an App Store Connect API key (`.p8`, key id, issuer id) with App Manager access.
4. The **team id**, and in CI: `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
   `APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, plus `APPLE_ID` and `APPLE_PASSWORD` (app-specific
   password) or the `APPLE_API_*` triple. These belong in repository secrets, never in the tree.
5. A monotonically increasing build number; the release workflow passes the GitHub run number.

### TestFlight upload

The release workflow reuses the same `macos-latest` job: when the Apple signing secrets are
present it builds with `--export-method release-testing --build-number <run>`, copies the IPA
to `dist/ios/` and runs `packaging/fastlane/ios` lane `beta` (`upload_to_testflight`). The lane
accepts either `APPLE_API_KEY_PATH` (+ `APPLE_API_KEY_ID`/`APPLE_API_ISSUER`) or
`APPLE_ID`/`APPLE_PASSWORD`; `H4RN_TESTFLIGHT_GROUPS` and `H4RN_TESTFLIGHT_CHANGELOG` are
optional. Without Apple secrets the job still compiles the simulator build and uploads no IPA.
Exact secret names are in [release.md](release.md).

### iOS secret storage

The same `secret_get` / `secret_set` / `secret_delete` commands are implemented for iOS by a
local Swift package at `apps/mobile/native/ios` (`H4rnSecretStorePlugin`), linked from
`apps/mobile/src-tauri/build.rs`. It stores values as `kSecClassGenericPassword` items scoped to
the app's service and access group, with
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, and replaces any existing item rather than
creating duplicates. Key material never leaves the Keychain, and the Rust command surface is
identical to Android, so the UI does not branch on platform. The CI simulator build compiles the Swift
package, which is the only verification possible without a Mac and an Apple account.
