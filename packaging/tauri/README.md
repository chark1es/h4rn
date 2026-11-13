# Tauri release config

`apps/desktop/src-tauri/tauri.conf.json` is the development configuration. It
has the updater plugin enabled with a **placeholder** public key, so a normal
`tauri build`/`tauri dev` works with no secrets and `check()` fails rather than
installing anything.

Release builds merge a generated overlay:

```sh
H4RN_UPDATER_ENDPOINT=https://updates.example.com/latest.json \
H4RN_UPDATER_PUBKEY="$(cat ~/.tauri/h4rn.key.pub)" \
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/h4rn.key)" \
packaging/scripts/generate-tauri-config.sh
```

That writes `apps/desktop/src-tauri/tauri.release.conf.json` (gitignored) with:

- `plugins.updater.endpoints` — the hosted default
  (`https://github.com/chark1es/h4rn/releases/latest/download/latest.json`) or
  a self-hosted manifest URL;
- `plugins.updater.pubkey` — the minisign public key;
- `bundle.createUpdaterArtifacts: true` when the private key is present, so the
  Tauri CLI emits `.sig` files and `tauri-action` uploads `latest.json`.

The private key is **never** committed. Generate it once with:

```sh
bun run --cwd apps/desktop tauri signer generate -w ~/.tauri/h4rn.key
```

Store the private key and its password as `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the public key as the
`H4RN_UPDATER_PUBKEY` repository secret. Losing the private key means no
existing install can accept a future update; back it up offline.

Self-hosting updates is a file server: publish `latest.json` plus the signed
artifacts at the URL in `H4RN_UPDATER_ENDPOINT`. The manifest format is the one
`tauri-action` produces (`includeUpdaterJson: true`).
