# h4rn packaging

Everything needed to turn a tagged commit into installable artifacts. The
release workflow is `.github/workflows/release.yml`; the operational guide is
[docs/release.md](../docs/release.md).

| Directory | Purpose |
|---|---|
| `tauri/` | release-only Tauri config overlay notes (generated file is gitignored) |
| `scripts/` | sidecar staging, daemon archives, manifest updates, Tauri config generation |
| `winget/` | winget three-file manifest templates (`__VERSION__`, SHA placeholders) |
| `homebrew/` | Homebrew cask template (`h4rn.rb`) |
| `aur/` | `h4rn-bin` PKGBUILD and `.SRCINFO` templates |
| `daemon/` | systemd user unit, launchd plist, Windows scheduled-task scripts |
| `helm/h4rn-connect/` | Helm chart for the relay (Deployment, Service, Ingress, Postgres) |
| `fastlane/` | Google Play internal-track and TestFlight upload lanes |
| `windows/` | `signtool` wrapper used by the Windows Tauri build |
| `docker/` | notes for the multi-arch relay image |

Templates are filled by `scripts/update-manifests.sh`; nothing is published
automatically. Secrets are documented with exact names in `docs/release.md`.
