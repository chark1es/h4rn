# Forges

`h4rn-forge` is the provider-agnostic pull-request layer behind the PR view. It
talks to GitHub, GitLab, Gitea, Forgejo, Bitbucket Cloud and Azure DevOps
Services, normalises their answers into the types in `h4rn-protocol::forge`, and
keeps credentials and writes inside the daemon. The UI never sees a token and
never speaks a forge API directly.

## Supported forges

| Forge | Public host | Self-hosted | API root |
| --- | --- | --- | --- |
| GitHub | `github.com` | GitHub Enterprise Server | `https://api.github.com`, `https://<host>/api/v3` |
| GitLab | `gitlab.com` | GitLab CE/EE | `https://<host>/api/v4` |
| Gitea | `gitea.com` | any Gitea 1.18+ | `https://<host>/api/v1` |
| Forgejo | `codeberg.org` | any Forgejo | `https://<host>/api/v1` |
| Bitbucket Cloud | `bitbucket.org` | no (Server/Data Center unsupported) | `https://api.bitbucket.org/2.0` |
| Azure DevOps Services | `dev.azure.com`, `<org>.visualstudio.com` | no (Azure DevOps Server unsupported) | `https://dev.azure.com/<org>/<project>/_apis/git/repositories/<repo>` |

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

Bitbucket Cloud uses API v2. It accepts an Atlassian API token as HTTP Basic with
the account's Atlassian email and the token as the password (the documented API
token form), a legacy app password as Basic with the account username, or a
workspace/project/repository access token as Bearer. An API token or app
password needs the `account` (OAuth) / `read:user:bitbucket` (API token) scope
to call `/user`, which is how the daemon validates it. Access tokens that
cannot read `/user` are validated against their repository (passing the
repository the PR view asks about) or `/user/workspaces`; the workspace slug
becomes the account login so the Mine filter can match authors. The client
lists open PRs (`state=OPEN`; the UI has no lifecycle filter, so merged and
declined PRs are not listed), reads the PR's raw unified diff from
`/pullrequests/{id}/diff` (the API redirects that to the repository diff),
rebuilds threads from the comment `parent` chain, and maps the commit's build
statuses onto checks. A root comment's `resolution` object marks a thread
resolved when the workspace reports it; otherwise threads are unresolved.
Bitbucket does have a resolve-comment endpoint
(`POST .../comments/{id}/resolve`), but this client does not call it yet, so
`resolve_threads` is `false`. Diffs are paginated locally.

Azure DevOps Services uses REST 7.1. `ForgeRepoRef.base_url` carries the
organization (`https://dev.azure.com/acme`) and `owner` carries the project.
Remote path segments are percent-decoded when detected (so `My%20Project`
becomes the project name) and encoded again as path segments in requests;
decoded separators and `..` are rejected. The legacy SSH host
`vs-ssh.visualstudio.com` derives `https://<org>.visualstudio.com` from its
`v3/<org>/...` path. Pull requests map `active`/`completed`/`abandoned` onto
open/merged/closed, but listing only asks for active PRs
(`searchCriteria.status=active`); the UI has no lifecycle filter, so completed
and abandoned PRs are not listed. Azure returns changed files per iteration
rather than a patch. The client reads the latest iteration's
`commonRefCommit` (old) and `sourceRefCommit` (new), pages the change entries,
and downloads only the files on the requested diff page through
`items?includeContentMetadata=true`, using `contentMetadata.isBinary` to mark
binaries. At most six content requests run at once and a per-request cache
avoids downloading a blob twice. The PR detail only counts changed files from
the iteration metadata; it never downloads contents. `/pullrequests/{id}/threads`
supplies both general comments and inline threads; thread statuses `fixed`,
`closed`, `wontFix` and `byDesign` are resolved. Checks are PR statuses plus
policy evaluations requested with `api-version=7.1-preview.1`; the project id
comes from the PR's `repository.project.id`, `configuration.isBlocking` marks
required rows, and a policy outcome wins over a status with the same name.
Commit pages follow the server's `x-ms-continuationtoken` through
`continuationToken`, not `$skip`. Additions/deletions are not shown for Azure
PRs: the API does not report line counts without downloading every file, and
the detail request deliberately does not do that.

Bitbucket Server / Data Center and Azure DevOps Server are not supported: they
use different APIs and hosts, and this build only detects and speaks the cloud
services.

Diffs for Gitea/Forgejo are fetched as the raw unified patch (`pull/<n>.diff`)
and parsed locally, which works across versions and does not depend on a newer
JSON files endpoint. If that endpoint fails the client falls back to
`pull/<n>/files`. Bitbucket returns its patch from
`/pullrequests/{id}/diff`; Azure is described above.

## Detecting a repository

`ForgeDetectRepo` reads the workspace's git remotes and returns the forge,
owner, repository and base URL. It understands:

```
git@github.com:owner/repo.git
ssh://git@gitlab.example.com/group/subgroup/repo.git
https://user:token@gitea.example.com/owner/repo.git
git://codeberg.org/owner/repo
git@bitbucket.org:workspace/repo.git
https://user@bitbucket.org/workspace/repo.git
git@ssh.dev.azure.com:v3/org/project/repo
git@vs-ssh.visualstudio.com:v3/org/project/repo
https://org@dev.azure.com/org/project/_git/repo
https://org.visualstudio.com/project/_git/repo
https://org.visualstudio.com/DefaultCollection/project/_git/repo
```

Credentials in an HTTPS remote are stripped before anything is stored or logged.
Local paths and file remotes are rejected. Azure DevOps remotes keep the
organization in `base_url` and the project in `owner`; the browser URL inserts
the `_git` segment.

Unknown hosts are never guessed, because an ssh alias such as `work` could send a
token to the wrong server. Declare custom hosts in
`<data-dir>/forge-hosts.json`:

```json
{
  "hosts": [
    { "host": "ghe.example.com", "forge": "github", "base_url": "https://ghe.example.com" },
    { "host": "gitlab.example.com", "forge": "gitlab", "base_url": "https://gitlab.example.com" },
    { "host": "work", "forge": "gitlab", "base_url": "https://gitlab.example.com" },
    { "host": "code.example.com", "forge": "forgejo", "base_url": "https://code.example.com" },
    { "host": "ado", "forge": "azure_devops", "base_url": "https://dev.azure.com" }
  ]
}
```

`host` is the name that appears in the remote (or the ssh alias), lowercased;
`base_url` is required and is the public web URL without a trailing slash. An
Azure DevOps base URL may include one organization segment
(`https://dev.azure.com/acme`). A Bitbucket or Azure host may only be declared
for the cloud services; Server/Data Center and Azure DevOps Server are rejected
when the host file is read because their APIs differ. The well-known hosts
`github.com`, `gitlab.com`, `codeberg.org`, `gitea.com`, `bitbucket.org`,
`dev.azure.com` and `ssh.dev.azure.com` are preloaded and can be overridden, and
every `<org>.visualstudio.com` host is recognised as Azure DevOps.

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
| Bitbucket Cloud (API token) | Account: read, Repositories: read, Pull requests: read/write |
| Bitbucket Cloud (legacy app password) | Account: read, Repositories: read, Pull requests: read/write |
| Bitbucket Cloud (access token) | `read:user:bitbucket` if it can read the account, plus `read:repository:bitbucket`, `read:pullrequest:bitbucket`, `write:pullrequest:bitbucket` |
| Azure DevOps | Code: read & write (PAT) |

`repo` covers check runs and commit statuses. A read-only account cannot use
`ForgeReply`, `ForgeComment` or `ForgeOpenPr`; the forge will return 403 and the
daemon reports it as an auth error instead of retrying.

Bitbucket API tokens are sent as HTTP Basic with the Atlassian account email as
the username and the token as the password; legacy app passwords use the
Bitbucket username instead. A workspace, project or repository access token is
sent as Bearer and needs no username. Azure DevOps personal access tokens use
HTTP Basic with an empty username. Bitbucket accounts record which scheme
validated (`bitbucket_app_password` or `bitbucket_access_token`) so a restart
keeps the same credential type. Bitbucket Cloud and Azure DevOps base URLs must
be plain `https://` origins without credentials, a query or a fragment, and
Bitbucket Cloud and Azure DevOps Server/Data Center hosts are rejected before
anything is stored.

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
no OAuth device flow in their public API. Bitbucket Cloud and Azure DevOps use
app passwords, workspace tokens or personal access tokens instead.
`capabilities().device_flow` is `false` for all of them.

### Importing from gh or glab

`import_gh_token` and `import_glab_token` read an existing token from
`gh auth token` / `glab auth status --show-token`. They are never called at
startup. The daemon only imports after the user explicitly chooses "import from
CLI"; the result is recorded with source `gh_cli` or `glab_cli`. If the CLI is
missing or has no token, the error is reported and nothing is stored.

## Capability matrix

`ForgeListAccounts` returns one `ForgeCapabilityEntry` per forge. Clients should
read the flags instead of assuming all forges behave alike.

| Capability | GitHub | GitLab | Gitea | Forgejo | Bitbucket Cloud | Azure DevOps |
| --- | --- | --- | --- | --- | --- | --- |
| OAuth device flow | yes | no | no | no | no | no |
| Review threads | yes | yes | yes | yes | yes | yes |
| Resolve threads | yes | yes | no | no | no (read only) | no (read only) |
| Nested replies | yes | yes | no | no | yes | yes |
| Inline review comments | yes | yes | yes | yes | yes | yes |
| Checks / statuses | yes | yes (pipelines/jobs) | yes | yes | yes (build statuses) | yes (statuses + policies) |
| Pipeline view | no | yes | no | no | no | no |
| Server-side diff pagination | yes | yes | no | no | no | no |
| GraphQL used | yes | no | no | no | no | no |
| Re-run failed checks | yes (check suites) | yes (pipeline retry) | no | no | no | no |

Diff pagination on Gitea/Forgejo, Bitbucket Cloud and Azure DevOps is applied
locally: the provider's diff (or changed-file list) is read and sliced by
`page`/`per_page`, and `has_more` reports whether more files exist.

## Checks, logs, re-run and commits

Each `ForgeCheck` carries `details_url` (the page that shows the log; the UI's
"View log" is a link, never a fetch) and `required`:

- **GitHub:** a check is `required` when branch protection
  (`.../protection/required_status_checks`) or a ruleset
  (`/rules/branches/{branch}`) names it. If neither can be read with the token
  the field is `null`, not `false`.
- **GitLab:** a job is required unless `allow_failure` is set.
- **Gitea/Forgejo:** from the branch protection rule's status-check contexts
  (wildcards allowed); reading it needs repository admin, otherwise `null`.
- **Bitbucket Cloud:** commit statuses have no required marker, so `required`
  is always `null` (unknown) rather than a guess.
- **Azure DevOps:** PR statuses are merged with policy evaluations; a blocking
  policy (`configuration.isBlocking`) is `required: true` and a non-blocking one
  `required: false`. When a status and a policy evaluate the same name, the
  policy result wins because it is the branch gate.

`ForgeRerunChecks` re-runs the failed checks and returns the refreshed checks.
GitHub re-requests each check suite that has a failed run
(`POST /check-suites/{id}/rerequest`); GitLab retries the head pipeline
(`POST /pipelines/{id}/retry`, which re-queues only failed jobs). Gitea and
Forgejo (1.22 / 9) report CI as commit statuses and expose no re-run API, and
Bitbucket Cloud exposes no build-status re-run either, so `ForgeChecks.rerun`
says `supported: false` with the reason, and the UI keeps the button disabled
with that reason as its tooltip. Azure DevOps could re-queue a pipeline through
the Builds API, but that needs extra scopes; this surface marks it unsupported
and says so.

`ForgeListPrCommits` lists a PR's commits oldest first (title, body, author,
date, link) for the Commits tab; forges that answer newest first are reversed.

## Requests and events

All forge requests live in `ClientRequest` and every credential stays in the
daemon:

- `ForgeListAccounts`, `ForgeAddAccount`, `ForgeRemoveAccount`
- `ForgeDetectRepo { workspace }`
- `ForgeListPrs { repo, filter, page }` — `open`, `mine`,
  `review_requested` or `by_branch`; every forge lists open (Bitbucket) or
  active (Azure DevOps) PRs only, because the filter has no lifecycle selector
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

`crates/h4rn-forge/tests/checks_and_commits.rs` replays fixtures from
`tests/fixtures/`. The Gitea commit list was recorded from Gitea 1.22
(`H4RN_RECORD_FIXTURES=<dir> ./scripts/test-forges.sh` re-records); the GitHub and
GitLab ones follow those providers' REST schemas. `./scripts/test-forges.sh --ui`
also checks the Commits tab and the disabled re-run reason against Docker Gitea.

The client tests are wiremock tests with realistic JSON for each API shape:
GitHub REST + GraphQL, ETag/304, 429 `retry-after`; GitLab MR, changes, diffs,
discussions and jobs; Gitea/Forgejo PRs, raw diff, comments, reviews, statuses;
Bitbucket Cloud PRs, diffstat, the `/diff` redirect, comment `parent` chains,
build statuses, basic/bearer auth, workspace and repository token validation,
and same-origin `next` handling; Azure DevOps PRs, raw iteration arrays,
changed-file contents, threads, statuses, policy evaluations, PAT auth, commit
`continuationToken` pages and per-page diff contents. They also cover the
remote-URL parsing matrix (including percent-decoded segments and the legacy
`vs-ssh.visualstudio.com` host), diff parsing and pagination, the local
unified-diff builder, and the agent-context renderer.

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
