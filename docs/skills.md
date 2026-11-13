# Skills

A skill is a directory with a `SKILL.md` entry point plus any references,
scripts and assets the model should read. h4rn keeps one registry per user,
installs each skill into the harnesses the user chooses, and syncs the same
content-addressed versions between machines through `h4rn-connect`. The design
is recorded in [ADR-034](decisions.md#adr-034-skills-are-content-addressed-bundles-installed-as-marked-copies)
and [ADR-035](decisions.md#adr-035-connect-is-a-sync-mailbox-the-registry-decides-causality).

## The bundle model

`SKILL.md` must exist. Its flat YAML frontmatter supplies `name` and
`description`; when `name` is absent the directory name is used. Everything in
the directory is part of the bundle except:

- `.h4rn-managed`, the ownership marker;
- `.git` and `.DS_Store`;
- symlinks, which are skipped with a warning because they can point outside the
  selected directory.

The canonical manifest lists every remaining file by relative path, size,
BLAKE3 digest and executable bit, sorted by path. The version id is the BLAKE3
digest of a canonical JSON encoding of that manifest. Nothing machine-specific
(path, mtime, owner) enters the hash, so the same directory imported on two
machines has one version id. Changing one byte changes the version.

Limits are deliberately small because skills are documents and scripts, not
media: 16 MiB per bundle, 4 MiB per file and 512 files. A bundle is stored once
under `<data_dir>/skills/bundles/<version>`; SQLite metadata lives in
`<data_dir>/skills/registry.db`.

## Harness layouts

All four supported harnesses read a native `SKILL.md` directory. The table is
the contract the installer implements; it is also returned to the UI as
`SkillHarnessAvailability`.

| Harness | User scope | Project scope | Verified |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/skills/<name>/SKILL.md` | `.claude/skills` | Yes, 2.1.323: a managed copy was installed and `/skill-name` resolved in a real session |
| Codex | `$CODEX_HOME/skills` (default `~/.codex/skills`) | `.agents/skills` | Layout and CLI (`codex-cli 0.168.0`) inspected; not exercised by an automated pickup test |
| OpenCode | `$XDG_CONFIG_HOME/opencode/skills` (default `~/.config/opencode/skills`) | `.opencode/skills` | Layout and `opencode v2.2.7` inspected; it also imports `.claude/skills` and `.agents/skills`. Not exercised by an automated pickup test |
| Antigravity | `~/.gemini/config/skills` | `.agents/skills` | Layout and `agy 1.5.8` inspected; `.gemini/skills` is checked before `.agents/skills`. Not exercised by an automated pickup test |

`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME` and `$OPENCODE_CONFIG_DIR` override the user
roots. Project roots are reported but not installed into: user scope is the only
path every harness reads without a workspace trust decision.

A user could point one harness at a directory this table does not know about.
That is not guessed at. The trait-shaped installer and the availability payload
leave room for an instructions-file fallback later, but none is needed today.

## Installing and ownership

An install copies the bundle to `<user-root>/<skill-dir-name>` and writes
`.h4rn-managed` beside it:

```json
{
  "managed_by": "h4rn",
  "skill_id": "sk_…",
  "skill_name": "release-notes",
  "version": "…",
  "harnesses": ["codex", "antigravity"],
  "installed_at": "2026-01-01T00:00:00Z"
}
```

Copies, not symlinks, are used because the marker must travel with the copy and
because Codex and Antigravity can share `.agents/skills`: one copy records both
owners and removing one harness keeps the copy for the other.

Ownership rules:

- A directory without a marker is **user-owned**. h4rn never overwrites,
  updates or removes it; enabling the skill reports a conflict-style error
  naming the path.
- Updating a skill replaces only managed copies and keeps the marker's harness
  list.
- Removing a skill removes only matching managed copies. Unresolvable files are
  logged and the registry row is tombstoned.
- Enable/disable is per harness **and per machine**. Disabling uninstalls the
  managed copy but keeps the registry row; enabling reinstalls it.

The registry can add a skill without installing it (`harnesses: []`), which is
useful when the user only wants the version available on another machine.

## Sources

`SkillsAdd` and `SkillsUpdate` accept one of:

- `local_folder`: a directory on the daemon host; the absolute path is recorded.
- `git`: a URL, a commit and an optional subdirectory. The daemon clones with
  `--no-checkout`, checks out the exact commit, resolves the full id and refuses
  a moving branch. The resolved commit is recorded with the version.
- `harness_dir`: a directory owned by a harness, recorded together with that
  harness.
- `connect`: applied by sync, not accepted as a manual source.

`name` on `SkillsAdd` rewrites the `name:` line and the manifest before hashing,
so the renamed bundle has its own version.

## Import

`SkillsImportPreview` scans every configured user and project root, reads each
`SKILL.md`, hashes the directory and compares it with the registry:

- a new name is importable;
- the same name at the same version is a **duplicate** and becomes a no-op;
- the same name at a different version is a **conflict**, with the reason shown
  in the preview.

`SkillsImportCommit` takes the candidate ids from a preview but rescans by id
before writing. A directory that changed after the preview cannot be imported
under its old hash. Duplicates are skipped, conflicts become registry conflict
rows, and everything else is inserted. Import only reads harness directories; it
does not silently reinstall over them.

## Conflicts

A conflict stores the existing version, the rejected candidate, its source and a
reason. `SkillsConflicts` lists them with the ids the resolve request expects.
`SkillsResolveConflict` supports:

- `keep_local`: discard the candidate;
- `keep_remote`: replace the registry version and reinstall enabled copies;
- `keep_both`: rename the candidate with a short version suffix and insert it.

Import conflicts and sync conflicts use the same table and the same screen.

## Sync through connect

The daemon is the authority; `h4rn-connect` is a per-user mailbox. A push sends
every local entry (including tombstones) with a version vector keyed by the
registry's stable machine id. The vector's clock is the registry's monotonic
revision, so two machines cannot reuse the same counter.

The relay validates that every payload reconstructs its manifest and hashes to
the advertised version. It then stores the bundle envelope-encrypted with the
connect KEK and an AAD binding `user_id`, `skill_id` and `version`. Name,
description, version, vector and ETag stay plaintext so causality can be
compared without decrypting content.

Causality rules:

- an incoming vector that dominates the stored vector is applied;
- a vector dominated by the stored one is rejected and recorded as a conflict;
- concurrent vectors use `updated_at` as last-writer-wins and record a conflict
  on the relay; the losing daemon also records a local, resolvable conflict
  from its rejected bundle;
- the same version is acknowledged without a write even when the other machine
  has a newer local revision.

A pull takes a cursor derived from the server's receive time and returns every
entry after it, including deletions. A tombstone removes the registry row but
still leaves managed copies removable by the normal remove path on each machine.
The cursor is persisted in `skill_state`, so an incremental sync survives a
daemon restart.

### Secret scanning

Before the first byte leaves the machine the daemon scans the exact bytes that
would be pushed. It matches PEM private keys, AWS access key ids, GitHub, Slack,
Stripe and provider API keys, JWTs and generic credential assignments. A finding
blocks the sync with `Blocked` state and a hint naming the file and line. This is
a visible refusal, not a network error: fix the skill or remove it from the
registry and sync again.

### Offline and signed-out

`SkillsSyncNow` always returns a status. When the daemon has no connect session
the state is `Offline` with a hint to run `h4rnd login --connect`. Registry and
installed copies still work locally.

## Wire protocol

Requests are the `skills_*` family in `h4rn-protocol`:

- `SkillsList` returns the registry, per-harness install state and availability.
- `SkillsAdd`, `SkillsUpdate`, `SkillsRemove` mutate the registry and managed
  copies.
- `SkillsSetEnabled` toggles one harness on this machine.
- `SkillsImportPreview`, `SkillsImportCommit`, `SkillsConflicts` and
  `SkillsResolveConflict` implement import and conflict handling.
- `SkillsDiff` compares any two stored versions by path.
- `SkillsSyncNow` runs one push/pull cycle.

Responses are `Skills`, `SkillAdded`, `SkillDetail`, `SkillsImportPreview`,
`SkillConflicts`, `SkillDiff` and `SkillSyncStatus`. `SkillsChanged` is a
transient event carrying the daemon's host id; `SyncStatus` is the transient
result of a background sync. Neither is written to the thread journal: a client
re-lists skills after `SkillsChanged`, and the registry itself is the durable
state.

The TypeScript bindings are generated from the Rust types by
`cargo test -p h4rn-protocol --test bindings`; do not hand-edit
`apps/ui/src/gen`.

## Testing

Hermetic unit tests cover frontmatter parsing, hash stability across directory
layouts, marker exclusion, path traversal, all four installers in temporary
homes, user-owned refusal, shared-root uninstall, import duplicates and
conflicts, diffing, the secret scanner and sync vector behaviour.

Two integration tests are ignored by default:

```text
cargo test -p h4rn-skills --test claude_pickup -- --ignored --nocapture
./scripts/test-connect.sh
```

The first installs a uniquely named skill and proves real Claude Code resolves
it in a temporary config directory. The second starts Postgres in Docker and
runs two registries through the real connect router, including a concurrent
edit and a rejected mismatched hash. `scripts/test-connect.sh` always tears the
compose project down, including on failure.

## Unverified or intentionally deferred

- Automated pickup tests exist only for Claude Code. Codex, OpenCode and
  Antigravity layouts come from inspecting the installed CLIs and their
  discovery rules; run a real pickup check before claiming parity on a new
  version.
- Project-scope installs are recorded but not performed.
- Git sources require a local `git` binary and network access; there is no
  bundled git implementation.
- Conflict resolution is local to one machine. The next sync publishes the
  resolved version through the normal vector rules.
