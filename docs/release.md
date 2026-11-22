# Releasing h4rn

A release is one semver tag. Pushing `v*` runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
every target, attaches artifacts and checksums to a GitHub release, and pushes
the multi-arch relay image. Package-manager manifests are generated but **not**
published; a maintainer submits them by hand.

The workflow and the scripts under [`packaging/`](../packaging/README.md) are the
only release machinery. Nothing here needs a local toolchain except for the
verification builds in §8.

## 1. What a tag produces

| Target | Artifacts |
|---|---|
| Windows (`x86_64-pc-windows-msvc`) | `h4rn_<version>_x64_en-US.msi`, `h4rn_<version>_x64-setup.exe` (NSIS), updater `.sig`/`latest.json` when signing is configured |
| macOS (`universal-apple-darwin`) | `h4rn_<version>_universal.dmg` and the `.app` inside it, updater artifacts |
| Linux (`x86_64-unknown-linux-gnu`) | `h4rn_<version>_amd64.AppImage`, `h4rn_<version>_amd64.deb`, updater artifacts |
| Daemon Linux glibc | `h4rnd-v<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Daemon Linux arm64 | `h4rnd-v<version>-aarch64-unknown-linux-gnu.tar.gz` |
| Daemon Linux musl | `h4rnd-v<version>-x86_64-unknown-linux-musl.tar.gz` (static) |
| Daemon macOS | `h4rnd-v<version>-universal-apple-darwin.tar.gz` |
| Daemon Windows | `h4rnd-v<version>-x86_64-pc-windows-msvc.zip` |
| Android | `app-release.aab` (Play) and `app-release.apk` (GitHub release) |
| iOS | `h4rn-<run>.ipa` when Apple signing secrets are present; the simulator compile always runs |
| Relay image | `ghcr.io/<owner>/h4rn-connect:<version>`, `:latest` and `:sha-<commit>` for `linux/amd64` and `linux/arm64` |
| Manifests | `packaging-manifests.tar.gz` with winget, Homebrew cask and AUR files |
| Checksums | `SHA256SUMS` covering every other asset, plus `SHA256SUMS.sig` when the updater key is set |

Each `h4rnd` archive also contains `README.txt` and the platform service files
(`contrib/h4rnd.service` for systemd, `contrib/dev.h4rn.h4rnd.plist` for
launchd, or the two Windows scheduled-task scripts). Every archive has a
sibling `.sha256`.

When `TAURI_SIGNING_PRIVATE_KEY` is set, the publish job signs `SHA256SUMS`
with that minisign key and uploads `SHA256SUMS.sig`. `h4rnd update` requires
that signature; a release built without the key can still be installed by hand
but refuses to self-update.

Draft releases are created first, so a failed matrix leaves a draft rather than
a half-public release. `publish` only flips the release to public after desktop,
daemon, Android, iOS and relay-image jobs succeed.

## 2. Versioning and tagging

1. Bump `version` in `Cargo.toml` (workspace), `apps/desktop/src-tauri/Cargo.toml`,
   `apps/desktop/src-tauri/tauri.conf.json`, `apps/mobile/src-tauri/tauri.conf.json`
   and the chart `version`/`appVersion` in `packaging/helm/h4rn-connect/Chart.yaml`.
2. Add a `CHANGELOG.md` section for the version and update `deploy/.env.example`,
   `deploy/docker-compose.yml` and the Helm `image.tag` default only if the
   default image tag should move.
3. Merge to `dev`, then tag the merge commit:

   ```sh
   git tag -a v1.0.0 -m "h4rn 1.0.0"
   git push origin v1.0.0
   ```

The workflow rejects tags that are not `v<semver>` (pre-release suffixes such as
`-rc.1` are allowed). `H4RN_VERSION` for daemon archives comes from the tag, not
from `Cargo.toml`, so the two must still agree for the desktop bundles.

## 3. Update signing (Tauri updater)

The updater key is separate from platform code signing. Generate it once on a
trusted machine:

```sh
bun run --cwd apps/desktop tauri signer generate -w ~/.tauri/h4rn.key
```

This writes a minisign private key and `~/.tauri/h4rn.key.pub`.

- `TAURI_SIGNING_PRIVATE_KEY` — contents of `~/.tauri/h4rn.key` (the release
  workflow also accepts the base64 form `tauri signer generate` prints).
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — its password, if one was set.
- `H4RN_UPDATER_PUBKEY` — contents of `~/.tauri/h4rn.key.pub`.

Back the private key up offline. If it is lost, no installed copy can verify a
future update and users must reinstall manually.

The committed `tauri.conf.json` contains the placeholder
`__H4RN_UPDATER_PUBKEY__` so development builds work without secrets. At release
time `packaging/scripts/generate-tauri-config.sh` writes the gitignored
`apps/desktop/src-tauri/tauri.release.conf.json`, which the workflow passes with
`--config`. It sets:

- `plugins.updater.pubkey` from `H4RN_UPDATER_PUBKEY`;
- `plugins.updater.endpoints` from `H4RN_UPDATER_ENDPOINT` (repository variable),
  defaulting to
  `https://github.com/chark1es/h4rn/releases/latest/download/latest.json`;
- `bundle.createUpdaterArtifacts: true` when the private key is present, so
  `tauri-action` emits `.sig` files and `latest.json`.

Updater artifacts are omitted when either the private key or the public key is
missing. `includeUpdaterJson` in the workflow follows the private key; the
config script refuses a private key without a public key.

### Self-hosted updates

Point `H4RN_UPDATER_ENDPOINT` at any HTTPS URL that serves the
`latest.json` format `tauri-action` produces plus the signed bundles it
references. A plain static file server or object store is enough; there is no
update server to run. Keep the endpoint versioned or swap `latest.json`
atomically. The endpoint is baked into each build, so changing it requires a
release (or the app-specific `plugins.updater.endpoints` override).

### What the app does with it

The desktop shell calls the endpoint 20 seconds after launch and then every 6
hours (Settings → General → About has a toggle and a **Check for updates**
button). When `latest.json` names a newer version the app shows "h4rn X.Y.Z is
available" with **Restart to update** and **Later**; installing downloads the
signed bundle, verifies it against `plugins.updater.pubkey`, and relaunches.

- **No placeholder key in releases.** The committed `__H4RN_UPDATER_PUBKEY__` is
  deliberately invalid: a development build fails its check quietly (it is
  logged, never shown). A release must be built through
  `generate-tauri-config.sh` with `H4RN_UPDATER_PUBKEY` set, or it ships
  without a working updater.
- **Never commit the private key.** `TAURI_SIGNING_PRIVATE_KEY` is a repository
  secret only; `tauri signer generate -w` writes it outside the repository.
- **Endpoint and workflow agree.** `tauri.conf.json` and the release workflow both
  point at
  `https://github.com/chark1es/h4rn/releases/latest/download/latest.json`, which
  `tauri-action` uploads when `includeUpdaterJson` is on.
- **Web and mobile** show no notice. Mobile updates come from the App Store and
  Google Play.

### Daemon self-update

`h4rnd update` installs the daemon from the same GitHub release, and uses the
same `H4RN_UPDATER_PUBKEY`. The daemon jobs compile the key in and bake in the
release repository (`H4RN_REPOSITORY`, defaulting to the workspace
`repository`), so the binary knows where to look without a mock or local
endpoint.

```sh
h4rnd update --check          # print current and latest
h4rnd update                  # install the latest release
h4rnd update --version 1.2.3  # pin a version
h4rnd update --rollback       # restore h4rnd.previous
```

The update downloads the archive for the build's target triple (macOS uses the
universal archive), checks its SHA-256 against `SHA256SUMS`, and verifies the
minisign signature over `SHA256SUMS` (or over the archive) against the compiled
key. It then writes the new binary beside the running one, fsyncs it, renames it
into place, and keeps the old binary as `h4rnd.previous`. On Windows the running
binary is renamed to `h4rnd.old` first; an interrupted rename is completed on
the next start.

If the service is installed the update restarts it; otherwise the command asks
you to restart `h4rnd`. A running daemon that reports an active turn stops the
update with an explanation unless `--force` is passed. A build with no
`H4RN_UPDATER_PUBKEY` refuses to replace the binary and prints the manual
download, verify and restart steps instead. A release needs the signed
`SHA256SUMS` for its daemon to update itself.

## 4. Repository secrets and variables

Set these under **Settings → Secrets and variables → Actions**. Names are used
verbatim by the workflow.

### Required for every release

| Name | Kind | Purpose |
|---|---|---|
| `GITHUB_TOKEN` | automatic | Draft release, asset upload, GHCR push. No setup. |
| `H4RN_UPDATER_PUBKEY` | secret | Minisign public key for the desktop and daemon updaters. Without it neither build is updater-capable. |

### Optional, per platform

Leave a group unset to skip signing/upload for that platform. CI still builds
unsigned desktop bundles and compiles the iOS simulator app.

| Name | Platform | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | updater | Minisign private key. Enables `.sig` + `latest.json`. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | updater | Password for the private key. |
| `H4RN_UPDATER_ENDPOINT` | updater | **Repository variable**, not a secret. Update manifest URL. |
| `WINDOWS_CERTIFICATE_BASE64` | Windows | Base64 of an Authenticode `.pfx`. |
| `WINDOWS_CERTIFICATE_PASSWORD` | Windows | `.pfx` password. |
| `APPLE_CERTIFICATE` | macOS/iOS | Base64 of the distribution `.p12`. |
| `APPLE_CERTIFICATE_PASSWORD` | macOS/iOS | `.p12` password. |
| `APPLE_SIGNING_IDENTITY` | macOS/iOS | e.g. `Developer ID Application: …` / `Apple Distribution: …`. |
| `APPLE_TEAM_ID` | macOS/iOS | Apple team id. |
| `APPLE_ID` | macOS/iOS | Apple account for notarisation and TestFlight upload. |
| `APPLE_PASSWORD` | macOS/iOS | App-specific password for that account. |
| `APPLE_API_KEY` | App Store Connect | `.p8` contents; preferred over `APPLE_ID`/`APPLE_PASSWORD` for upload. |
| `APPLE_API_KEY_ID` | App Store Connect | Key id; CI writes the key as `AuthKey_<id>.p8`. |
| `APPLE_API_ISSUER` | App Store Connect | Issuer UUID. |
| `ANDROID_KEYSTORE_BASE64` | Android | Base64 of the upload keystore. |
| `ANDROID_KEYSTORE_PASSWORD` | Android | Keystore password. |
| `ANDROID_KEY_ALIAS` | Android | Upload key alias. |
| `ANDROID_KEY_PASSWORD` | Android | Key password. |
| `PLAY_STORE_JSON_KEY` | Android | Play service-account JSON, used by the fastlane internal-track lane. |

Optional non-secret release environment values, set inline in the workflow when
needed: `H4RN_PLAY_TRACK` (default `internal`), `H4RN_PLAY_RELEASE_STATUS`
(default `completed`), `H4RN_TESTFLIGHT_GROUPS` (comma-separated beta groups) and
`H4RN_TESTFLIGHT_CHANGELOG`.

`scripts/mobile-android.sh build-release` reads `ANDROID_KEYSTORE_PATH` (not
`..._BASE64`), so local builds export the decoded file path. CI decodes the
base64 secret into `$RUNNER_TEMP` first.

### Updating the updater public key

`H4RN_UPDATER_PUBKEY` is read as a secret because the workflow interpolates it
directly into the release config. If you prefer a variable, change the workflow
reference in the same commit; do not set both.

## 5. Platform signing details

### Windows

`packaging/windows/sign.ps1` is wired through
`H4RN_WINDOWS_SIGN_COMMAND`, which the workflow puts into `tauri.release.conf.json`
as `bundle.windows.signCommand`. It signs MSI and NSIS artifacts with `signtool`
and a timestamp, and exits successfully without a certificate so PR and local
builds stay unsigned. `WINDOWS_TIMESTAMP_URL` overrides the timestamp server.

### macOS

`tauri-action` uses `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_ID`/`APPLE_PASSWORD` or the
`APPLE_API_*` triple to sign and notarise the universal bundle. Without them the
DMG is ad-hoc/unsigned and Gatekeeper warns.

### Android

`apps/mobile/native/android/app/build.gradle.kts` overlays the generated project
(see `H4RN_ANDROID_CONFIG` in `scripts/mobile-android.sh`). It reads
`ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS` and
`ANDROID_KEY_PASSWORD`; when the keystore path is absent it falls back to the
debug key so `build-release` still produces an installable artifact. The
`versionCode` is the GitHub run number, supplied by a temporary config overlay.

The Play upload runs `packaging/fastlane/android` lane `internal` after the
artifact is collected. It is skipped when `PLAY_STORE_JSON_KEY` is empty.

### iOS

`packaging/fastlane/ios` lane `beta` uploads `H4RN_IPA_PATH` to TestFlight and
is skipped unless the Apple signing secrets exist. The job always builds the
simulator target, which compiles `apps/mobile/native/ios` (the Swift Keychain
plugin) through `apps/mobile/src-tauri/build.rs`.

## 6. Relay image and chart

`connect-image` uses Buildx/QEMU to build `deploy/Dockerfile` for
`linux/amd64,linux/arm64`, logs in to GHCR with `GITHUB_TOKEN`, and pushes the
version, `latest` and `sha-<commit>` tags. The workflow needs
`packages: write` (already set) and the package must allow the repository to
publish it on first push.

`deploy/docker-compose.yml` is pinned to
`ghcr.io/chark1es/h4rn-connect:<version>`; a local build adds
`deploy/docker-compose.build.yml`:

```sh
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.build.yml up -d --build
```

The Helm chart lives at `packaging/helm/h4rn-connect` and is versioned with the
application. It is not pushed to a chart repository by CI; install it from a
checkout:

```sh
helm install h4rn packaging/helm/h4rn-connect \
  --namespace h4rn --create-namespace \
  --set config.publicBaseUrl=https://connect.example.com \
  --set config.allowInsecure=false \
  --set ingress.enabled=true --set ingress.hosts[0].host=connect.example.com
```

See `packaging/helm/h4rn-connect/values.yaml` for Postgres-in-chart versus
`externalDatabase.url`, chart-managed versus `secrets.existingSecret`, and the
required `database-url`, `kek` and `signing-key` keys. The chart reuses generated
secrets across upgrades via `lookup`; back up the Secret alongside Postgres.

## 7. Package-manager manifests

`packaging/scripts/update-manifests.sh` fills the checked-in templates from the
release artifacts and writes `packaging-out/`:

- `winget/manifests/h/H4rn/h4rn/<version>/` — three-file manifest for a
  `microsoft/winget-pkgs` PR. It points at the MSI URL and hash.
- `homebrew/h4rn.rb` — cask for a tap or `homebrew-cask` PR. It points at the
  DMG and hash.
- `aur/PKGBUILD` and `.SRCINFO` — `h4rn-bin`, which repackages the x86_64
  `.deb` (desktop app plus bundled `h4rnd`). x86_64 only.

CI uploads the rendered directory and `packaging-manifests.tar.gz`. It never
pushes to winget, Homebrew or AUR. To publish, copy the rendered files into a
fork/branch and open the PR (or `git push` the AUR repo).

## 8. Local verification

These reproduce CI steps without publishing anything:

```sh
# Formatting, lints and tests (required before a release PR)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Standalone daemon release binary + archive
cargo build --release -p h4rn-daemon
packaging/scripts/build-daemon-archives.sh x86_64-unknown-linux-gnu dist/daemon

# Android release AAB/APK (needs JDK 21, Android SDK/NDK, Rust Android targets)
export JAVA_HOME="$HOME/.local/jdk/jdk-21.0.13+9"
scripts/mobile-android.sh build-release

# Relay image, single platform (Docker Desktop's docker.exe works from WSL)
docker.exe buildx build --platform linux/amd64 -f deploy/Dockerfile -t h4rn/connect:dev .

# Chart and workflow syntax
helm lint packaging/helm/h4rn-connect
helm template h4rn packaging/helm/h4rn-connect \
  --set secrets.kek="$(openssl rand -base64 32)" \
  --set secrets.signingKey="$(openssl rand -base64 32)" \
  --set postgres.auth.password=test >/dev/null
actionlint
bash -n packaging/scripts/*.sh scripts/mobile-android.sh

# Compose rendering (requires the mandatory env values)
docker compose -f deploy/docker-compose.yml config >/dev/null
```

Windows bundles are built from WSL with `scripts/desktop-build-windows.sh build`;
`all` additionally launches the app, checks the sidecar on `127.0.0.1:47800`,
screenshots it and confirms the sidecar exits. macOS bundles require a Mac, and
iOS requires macOS plus Xcode.

## 8a. Suites that run only through scripts

CI runs the Postgres-only `#[ignore]`d suites (`h4rn-connect` `postgres` and
`skills`) in the `connect-postgres` job against a `postgres:16` service. No
daemon test needs Postgres alone. These suites need a real OrbitID, which CI
does not have, so they run only through scripts before a release:

| Suite | Command |
|---|---|
| `h4rn-connect` `orbitid` (OIDC, device grant, issuer checks) | `./scripts/test-connect.sh --orbitid` |
| `h4rn-daemon` `remote` (device-grant sign-in, pairing, relay) | `./scripts/test-remote.sh` |

Both need Docker and an OrbitID image (`H4RN_ORBITID_SOURCE`, default `$HOME/.t3/projects/orbitid`).

## 9. Release checklist

1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`
   and `cargo test --workspace` pass.
2. Versions agree everywhere listed in §2; `CHANGELOG.md` has the section.
3. `actionlint` and `helm lint` pass locally, or the PR's `packaging` CI job is
   green.
4. Required secrets from §4 are present; the updater key is backed up.
5. Push the tag. Watch the draft release until every matrix job is green.
6. Download `SHA256SUMS` and one artifact per platform; verify with
   `sha256sum -c` / `shasum -a 256 -c`, and verify `SHA256SUMS.sig` when the
   updater key is configured.
7. Confirm the relay image manifest lists both architectures:
   `docker buildx imagetools inspect ghcr.io/<owner>/h4rn-connect:<version>`.
8. Confirm `latest.json` points at this version if the updater is enabled, and
   that a previous install offers the update.
9. Submit the generated winget, Homebrew and AUR manifests from
   `packaging-out/` (or the release asset) as separate PRs.

## 10. Failure and rollback

- A failed job leaves the GitHub release as a draft. Fix, delete the tag locally
  and remotely, and re-tag the corrected commit; never edit assets into a
  published release by hand.
- To withdraw a bad release, mark it pre-release or delete the release and move
  the `latest` image tag back to the previous version. Desktop installs keep the
  version they have until they see a newer `latest.json`.
- Rotating the updater key invalidates update checks for existing installs.
  Ship one release signed with the old key that points users at a manual
  download, or keep the old key available for verification.
