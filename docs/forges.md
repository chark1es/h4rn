# Forges

`h4rn-forge` is the provider-agnostic pull-request layer behind the PR view. It
talks to GitHub, GitLab, Gitea and Forgejo, normalises their answers into the
types in `h4rn-protocol::forge`, and keeps credentials and writes inside the
daemon. The UI never sees a token and never speaks a forge API directly.

## Supported forges

| Forge | Public host | Self-hosted | API root |
| --- | --- | --- | --- |
| GitHub | `github.com` | GitHub Enterprise Server | `https://api.github.com`, `https://<host>/api/v3` |
| GitLab | `gitlab.com` | GitLab CE/EE | `https://<host>/api/v4` |
| Gitea | `gitea.com` | any Gitea 1.18+ | `https://<host>/api/v1` |
| Forgejo | `codeberg.org` | any Forgejo | `https://<host>/api/v1` |

GitHub uses REST for lists, detail, diffs, comments and checks, and GraphQL only
for `reviewThreads` because the REST review-comment endpoint does not expose
resolution state. When GraphQL is unavailable (for example an old GHES without
it enabled) the client falls back to grouping REST review comments by their
reply chain and reports every thread as unresolved.

GitLab uses REST v4 throughout. Pipelines and their jobs are mapped onto the
normalised checks shape; merge-request discussions become threads.

Gitea and Forgejo share one client. Forgejo is API-compatible for everything the
PR view needs. Two differences are handled explicitly:

- review comments have no resolved flag, so threads are reconstructed from a
  review's comment list and reported unresolved;
- there is no nested-reply endpoint, so a reply is posted as a general PR
  comment. `capabilities().nested_replies` is `false`, and the UI can say so.

Diffs are fetched as the raw unified patch (`pull/<n>.diff`) and parsed locally,
which works across versions and does not depend on a newer JSON files endpoint.
If that endpoint fails the client falls back to `pull/<n>/files`.

## Detecting a repository

`ForgeDetectRepo` reads the workspace's git remotes and returns the forge,
owner, repository and base URL. It understands:

```
git@github.com:owner/repo.git
ssh://git@gitlab.example.com/group/subgroup/repo.git
https://user:token@gitea.example.com/owner/repo.git
git://codeberg.org/owner/repo
```

Credentials in an HTTPS remote are stripped before anything is stored or logged.
Local paths and file remotes are rejected.

Unknown hosts are never guessed, because an ssh alias such as `work` could send a
token to the wrong server. Declare custom hosts in
`<data-dir>/forge-hosts.json`:

```json
{
  "hosts": [
    { "host": "ghe.example.com", "forge": "github", "base_url": "https://ghe.example.com" },
    { "host": "gitlab.example.com", "forge": "gitlab", "base_url": "https://gitlab.example.com" },
    { "host": "work", "forge": "gitlab", "base_url": "https://gitlab.example.com" },
    { "host": "code.example.com", "forge": "forgejo", "base_url": "https://code.example.com" }
  ]
}
```

`host` is the name that appears in the remote (or the ssh alias), lowercased;
`base_url` is required and is the public web URL without a path or trailing
slash. The well-known hosts `github.com`, `gitlab.com`, `codeberg.org` and
`gitea.com` are preloaded and can be overridden.

The data directory is `$H4RN_DATA_DIR`, or the platform default
(`$XDG_DATA_HOME/h4rn`, `~/Library/Application Support/h4rn`,
`%APPDATA%\h4rn`).

## Accounts and tokens

`ForgeAddAccount` validates a token against the forge before storing it, records
the discovered login and reported scopes, and returns the account. Several
accounts per forge are supported (work/personal, or two GHES hosts); every
request may name an `account_id`, and the daemon picks the only account whose
forge and base URL match when it is omitted.

Tokens are stored with the OS keychain under service `h4rn-forge` and account
id when a keychain is available. Headless Linux and WSL fall back to an
encrypted file (`forge-token-<account_id>`, ChaCha20-Poly1305, `0600`). Tokens
are never written to `forge-accounts.json`, logs, events or audit entries.

### Least-privilege scopes

Use a fine-grained token where the forge offers one. The documented minimum for
the full PR view (list, detail, diff, threads, checks, comment, reply) is:

| Forge | Scopes |
| --- | --- |
| GitHub (private repos) | `repo`, `read:user` |
| GitHub (public repos only) | `public_repo`, `read:user` |
| GitLab | `api` (or `read_api` + `read_user` for a read-only account) |
| Gitea / Forgejo | `read:user`, `read:repository`, `write:repository`, `read:issue`, `write:issue` |

`repo` covers check runs and commit statuses. A read-only account cannot use
`ForgeReply`, `ForgeComment` or `ForgeOpenPr`; the forge will return 403 and the
daemon reports it as an auth error instead of retrying.

The daemon compares the scopes the forge reports with the least-privilege set
and surfaces extras in the account row (`ForgeAccount.scopes`), so an
over-broad token is visible rather than silent.

### GitHub OAuth device flow

GitHub is the only supported forge with a device authorization grant.
Register an OAuth app (no callback URL is needed for the device grant), then
start the daemon with:

```sh
export H4RN_GITHUB_CLIENT_ID=<client-id>
```

The daemon starts the grant, shows the verification URL and user code, and polls
with the interval GitHub returns (`authorization_pending`, `slow_down` handled).
The default scopes are `repo read:user`; public-only use can pass
`public_repo read:user`. Tokens obtained this way are recorded with source
`oauth_device`.

GitLab does not offer a device grant for this use case, and Gitea/Forgejo have
no OAuth device flow in their public API, so those accounts use a personal
access token. `capabilities().device_flow` is `false` for them.

### Importing from gh or glab

`import_gh_token` and `import_glab_token` read an existing token from
`gh auth token` / `glab auth status --show-token`. They are never called at
startup. The daemon only imports after the user explicitly chooses "import from
CLI"; the result is recorded with source `gh_cli` or `glab_cli`. If the CLI is
missing or has no token, the error is reported and nothing is stored.

## Capability matrix

`ForgeListAccounts` returns one `ForgeCapabilityEntry` per forge. Clients should
read the flags instead of assuming all forges behave alike.

| Capability | GitHub | GitLab | Gitea | Forgejo |
| --- | --- | --- | --- | --- |
| OAuth device flow | yes | no | no | no |
| Review threads | yes | yes | yes | yes |
| Resolve threads | yes | yes | no | no |
| Nested replies | yes | yes | no | no |
| Inline review comments | yes | yes | yes | yes |
| Checks / statuses | yes | yes (pipelines/jobs) | yes | yes |
| Pipeline view | no | yes | no | no |
| Server-side diff pagination | yes | yes | no | no |
| GraphQL used | yes | no | no | no |

Diff pagination on Gitea/Forgejo is applied locally: the whole patch is parsed
and sliced by `page`/`per_page`, and `has_more` reports whether more files
exist.

## Requests and events

All forge requests live in `ClientRequest` and every credential stays in the
daemon:

- `ForgeListAccounts`, `ForgeAddAccount`, `ForgeRemoveAccount`
- `ForgeDetectRepo { workspace }`
- `ForgeListPrs { repo, filter, page }` — `open`, `mine`,
  `review_requested` or `by_branch`
- `ForgeGetPr { repo, number }`
- `ForgeGetPrDiff { repo, number, page, per_page }`
- `ForgeListPrComments { repo, number }` — general comments, review threads
  (path/line/side/resolved) and reviews
- `ForgeListChecks { repo, number }`
- `ForgeReply { repo, number, thread_id, comment_id, body }`
- `ForgeComment { repo, number, body }`
- `ForgeOpenPr { thread_id, title, body, base }` — the daemon detects the
  workspace remote, pushes the thread branch when the remote does not have it,
  then opens the PR
- `ForgeHandPrToAgent { repo, number, thread_id?, target? }`

`ForgeHandPrToAgent` builds compact markdown with the PR metadata, unresolved
threads (path, line, side, author, a bounded body), failing checks and diff
stats, then either appends it to an existing thread and starts a turn, or
creates a new thread when `target` is supplied. Bodies and thread counts are
truncated so a huge PR cannot blow up the prompt.

Writes (`reply`, `comment`, `open_pr`, and follow-up messages from
`hand_pr_to_agent`) are appended to `<data-dir>/forge-audit.jsonl` with time,
action, account, forge, repository, PR number and non-secret metadata. The file
is local and append-only.

## Polling, rate limits and caching

The daemon polls PRs it is watching for checks changes. The base interval is
`H4RN_FORGE_POLL_SECS` (default 30, minimum 5); failures back off up to 300
seconds. A state change broadcasts a transient `forges_checks_updated` event
carrying the PR, aggregated state, counts and the connection's current durable
`last_seq`. The event is not journaled, so a client that reconnects re-fetches.

Each account gets a client with a local request budget,
`H4RN_FORGE_RPM` (default 120 requests/minute). Every response is also checked
against the forge's own limits:

- `retry-after` on 429/403 is honoured and surfaced as
  `ForgeError::RateLimited { retry_after_secs }`; the daemon does not spin.
- `x-ratelimit-reset` is used when `retry-after` is absent.
- GitHub responses with an `etag` are cached in memory and revalidated with
  `if-none-match`; a `304` serves the cached body without counting as new data.
- Pagination follows `link: rel="next"` when present and otherwise the
  page/`limit` contract, with a hard cap so a misbehaving server cannot loop.

Error messages redact `token`, `authorization`, `private-token` and common
credential query parameters before they reach a log or a protocol error.

## Testing

The client tests are wiremock tests with realistic JSON for each API shape:
GitHub REST + GraphQL, ETag/304, 429 `retry-after`; GitLab MR, changes, diffs,
discussions and jobs; Gitea/Forgejo PRs, raw diff, comments, reviews, statuses.
They also cover the remote-URL parsing matrix, diff parsing and pagination, and
the agent-context renderer.

Two live tests are ignored by default:

```sh
# Real GitHub read-only test (needs a token; no writes).
H4RN_GITHUB_TOKEN=... cargo test -p h4rn-forge --test wiremock_clients \
  -- --ignored live_github_read_only

# Real Gitea/Forgejo, started and torn down by the script.
./scripts/test-forges.sh            # Gitea
./scripts/test-forges.sh --forgejo  # Forgejo
./scripts/test-forges.sh --github   # also run the read-only GitHub test
```

`scripts/test-forges.sh` starts the forge on a free published port, creates an
admin and token through the forge's CLI, seeds a private repository, a feature
branch, a PR, a commit status and a comment through the HTTP API, runs
list/detail/diff/comments/comment/reply/checks, and removes the container on
exit (including failure). It uses the Docker CLI, falling back to `docker.exe`
under WSL.

GitLab has no live Docker test yet: its wiremock coverage is complete, and a
live run needs a token and a project. Set `H4RN_GITLAB_TOKEN` and a repository
if you add one.
