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
normalised checks shape; merge-request discussions become threads. The merge
request object carries no line totals, so `get_pr` sums the `/changes` diffs
once and reports the real additions and deletions (`line_counts_known` is
`true`). A line is counted by its first character inside a hunk, so content
that itself begins `+++` or `---` is an added or removed line rather than a
file header. When GitLab answers with `overflow: true` the changes list was cut
off, and a change marked `collapsed` (a shortened patch) or `too_large` (an
omitted one) is incomplete too; a partial sum would be a wrong number, so the
totals stay unknown and the UI and the agent handoff say so.

Gitea and Forgejo share one client. Forgejo is API-compatible for everything the
PR view needs. Two differences are handled explicitly:

- review comments have no resolved flag, so threads are reconstructed from a
  review's comment list and reported unresolved;
- there is no nested-reply endpoint, so a reply is posted as a general PR
  comment. `capabilities().nested_replies` is `false`, and the UI can say so.

Bitbucket Cloud uses API v2. It accepts an Atlassian API token as HTTP Basic with
the account's Atlassian email and the token as the password (the documented API
token form), or a workspace/project/repository access token as Bearer.
Atlassian retired app passwords in July 2026, so they are no longer offered; a
form with an email creates an API token, and an empty email creates an access
token. An API token needs the `account` (OAuth) / `read:user:bitbucket` (API
token) scope to call `/user`. Access tokens cannot call `/user` or
`/user/workspaces`; the daemon validates them against a repository instead,
using the `repository` hint the settings form fills from the workspace's
detected Bitbucket remote (`ForgeAddAccount.repository`). When no repository
can be detected the form says so and refuses the add rather than storing a
credential it cannot prove. The client lists open PRs (`state=OPEN`; the UI
has no lifecycle filter, so merged and declined PRs are not listed) and uses
the documented `q=` filter for author, reviewer and source-branch lists, so
pagination stays server-side and pages never repeat; reviewer lists also ask
for the documented partial-response addition `fields=+values.reviewers`. The
Mine filter matches `author.uuid` and Review requested matches
`reviewers.uuid`, both taken from `/user`, as Bitbucket documents. Display
names and nicknames are not used: two accounts can share a display name, and a
token may report no nickname at all. A workspace, project or
repository access token has no user identity, so Mine and Review requested
answer `unsupported` with that reason instead of returning an empty list that
looks like “no pull requests”. A `/user` answer without a `uuid` is an error
rather than a filter by name. The PR list carries the same-origin `next` URL
the server returned as the cursor for the next page instead of rebuilding
`page=N`, so an opaque cursor keeps its filters. Learned cursors are sparse
and bounded (the 64 most recent per client), and a page whose cursor was never
learned falls back to the documented `page` parameter, so asking for a distant
page never allocates memory in proportion to its number. It reads the PR's raw
unified diff from `/pullrequests/{id}/diff` (the API redirects that to the
repository diff), rebuilds threads from the comment `parent` chain, and maps
the commit's build statuses onto checks. A root comment's `resolution` object
marks a thread resolved when the workspace reports it; otherwise threads are
unresolved. Bitbucket does have a resolve-comment endpoint
(`POST .../comments/{id}/resolve`), but this client does not call it yet, so
`resolve_threads` is `false`. A line-anchored thread takes `outdated` from the
root comment's `inline.outdated`; a file-level comment (no line anchor) is
never outdated because it has no line that could have moved. The comments page
reports `has_more` only when the last page it read still linked a `next` page;
a list that happens to end at a page boundary is not “more”. Diffs are
paginated locally. Additions, deletions and the changed-file count come from
`/diffstat`; when its five-page cap is reached while a further page exists, or
the diffstat request fails, the counts are left at zero,
`line_counts_known` and `changed_files_known` are `false`, and the UI and the
agent handoff show them as unknown rather than “+0 −0” or “0 files”.
Bitbucket does not report a credential's scopes, so the account row leaves
them unknown instead of reading GitHub's `x-oauth-scopes` header.

Azure DevOps Services uses REST 7.1. `ForgeRepoRef.base_url` carries the
organization (`https://dev.azure.com/acme`) and `owner` carries the project.
Remote path segments are percent-decoded when detected (so `My%20Project`
becomes the project name) and encoded again as path segments in requests;
decoded separators and `..` are rejected, and a name that itself contains a
`%` (a repository literally called `release%20notes`) keeps it as part of the
name and is encoded as `%25`. The legacy SSH host
`vs-ssh.visualstudio.com` derives `https://<org>.visualstudio.com` from its
`v3/<org>/...` path. Pull requests map `active`/`completed`/`abandoned` onto
open/merged/closed, but listing only asks for active PRs
(`searchCriteria.status=active`); the UI has no lifecycle filter, so completed
and abandoned PRs are not listed. Azure returns changed files per iteration
rather than a patch. The client reads the latest iteration's
`commonRefCommit` (old) and `sourceRefCommit` (new), pages the change entries,
and downloads only the files on the requested diff page through
`items?includeContentMetadata=true`, using `contentMetadata.isBinary` to mark
binaries. A PR from a fork names the fork in `forkSource.repository`: the old
side is read from the target repository and the new side from the fork, where
its source commit exists. A 404 for a side the change says exists is an error,
not an absent file, because it means the wrong repository or commit was read.
A page holds at most eight files: the worst case is two content
reads per file plus the two metadata reads, eighteen requests, which fits the
UI's ten-second deadline inside the default 120-requests-per-minute account
budget even when the local token bucket is empty. Files whose change keeps
their bytes (a pure `rename`, `branch` or `merge`) download nothing, and at
most six content requests run at once. A per-request cache avoids downloading
a blob twice. The PR detail only counts changed files from the
iteration metadata; it never downloads contents. When the iteration or its
change entries cannot be read, or a 200 omits the `changeEntries` collection,
`changed_files_known` is `false` and the UI shows the count as unknown; an
empty `changeEntries: []` is a real zero and stays known.
`/pullrequests/{id}/threads` supplies both general
comments and inline threads; a 200 whose body lacks its `value` collection is
an error, not an empty discussion, while `value: []` is a valid empty list.
Thread statuses `fixed`, `closed`, `wontFix` and `byDesign` are resolved.
Replies parse the caller's thread id (which goes into the request URL) and
comment id (which goes into the JSON body) as positive 32-bit integers before
either is used, so a malformed id returns `InvalidRequest` and sends nothing.
Checks are PR statuses plus policy evaluations requested with
`api-version=7.1-preview.1`; the project id comes
from the PR's `repository.project.id`. Both endpoints always send a `value`
collection, so a 200 whose status or policy body lacks one is an error rather
than an empty result; a valid `value: []` really is empty. If the status, iteration or
policy-evaluation request
fails, the whole checks request fails instead of reporting the surviving green
rows as passing: a branch gate that could not be read is never a pass.
Statuses carry an optional `iterationId`: PR-level statuses (no iteration) and
those for the current iteration are kept, so a failure from an earlier push
cannot outlive the code it tested. Each policy evaluation is a separate row keyed by its evaluation
and configuration ids, so two configurations with the same display name do not
collapse into one; `configuration.isBlocking` marks required rows; and a
policy outcome wins only over a *status* with the same name. Change types are
matched as exact tokens (`add`, `edit`, `rename`, `delete`, `undelete`, ...),
so `undelete` restores a file instead of reading as a deletion. Commit pages
follow the server's `x-ms-continuationtoken` through `continuationToken`, not
`$skip`. A PR's browser URL comes from `_links.web`; a response without it
(an older API or a projection that omits links) falls back to
`<repository.webUrl>/pullrequest/<id>`, or to the base URL, project and
repository when the repository object names no web URL. That fallback encodes
the project and repository as path segments, so a repository literally named
`release%20notes` links to `release%2520notes` instead of a space. Additions
and deletions are unknown for Azure PRs (`line_counts_known` is `false`): the
API does not report line counts without downloading every file, and the detail
request deliberately does not do that, so the agent handoff says the counts are
unknown instead of “+0 −0”. A file diff's totals count every added or removed
line inside a hunk, including content that itself begins `+++` or `---`; only
the file header before the first hunk is a marker.

Bitbucket Server / Data Center and Azure DevOps Server are not supported: they
use different APIs and hosts, and this build only detects and speaks the cloud
services.

Diffs for Gitea/Forgejo are fetched as the raw unified patch (`pull/<n>.diff`)
and parsed locally, which works across versions and does not depend on a newer
JSON files endpoint. If that endpoint fails the client falls back to
`pull/<n>/files`. Bitbucket returns its patch from
`/pullrequests/{id}/diff`; Azure is described above. Azure's unified diff is
built from the two file versions rather than from line slices: lines keep
their terminators, so a CRLF-to-LF change is visible; a line without a final
newline is followed by `\ No newline at end of file`; and `/dev/null` is used
only when the path is absent on that side, so an existing empty file is not a
new or deleted file.

## Detecting a repository

`ForgeDetectRepo` reads the workspace's git remotes and returns the forge,
owner, repository and base URL. It understands:

```
git@github.com:owner/repo.git
ssh://git@gitlab.example.com/group/subgroup/repo.git
https://user:token@gitea.example.com/owner/repo.git
git://codeberg.org/owner/repo
git@bitbucket.org:workspace/repo.git
git@ssh.bitbucket.org:workspace/repo.git
ssh://git@ssh.bitbucket.org/workspace/repo.git
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
the `_git` segment. Azure DevOps path segments are percent-decoded
(`My%20Project`) and validated (a decoded separator, `..` or an empty segment
is rejected) because they are encoded again in requests; a literal `%` is part
of the name (`release%20notes` becomes `release%2520notes`). Every other forge
keeps its long-standing lenient path handling: a double slash such as
`https://gitlab.com/team//repo.git` resolves like `team/repo`.

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
Azure DevOps base URL may include one organization or `DefaultCollection`
segment (`https://dev.azure.com/acme`,
`https://acme.visualstudio.com/DefaultCollection`). A Bitbucket or Azure host
may only be declared
for the cloud services; Server/Data Center and Azure DevOps Server are rejected
when the host file is read because their APIs differ. The well-known hosts
`github.com`, `gitlab.com`, `codeberg.org`, `gitea.com`, `bitbucket.org`,
`ssh.bitbucket.org`, `dev.azure.com` and `ssh.dev.azure.com` are preloaded and
can be overridden, and every `<org>.visualstudio.com` host is recognised as
Azure DevOps.

The data directory is `$H4RN_DATA_DIR`, or the platform default
(`$XDG_DATA_HOME/h4rn`, `~/Library/Application Support/h4rn`,
`%APPDATA%\h4rn`).

## Accounts and tokens

`ForgeAddAccount` validates a token against the forge before storing it, records
the discovered login and reported scopes, and returns the account. It also
carries an optional `repository` hint (the workspace's detected remote); for
Bitbucket access tokens the daemon validates against that repository, because
`/user` and `/user/workspaces` are unavailable to them. Several
accounts per forge are supported (work/personal, or two GHES hosts); every
request may name an `account_id`, and the daemon picks the only account whose
forge and base URL match when it is omitted. For Azure DevOps the legacy
`https://<org>.visualstudio.com` and
`https://<org>.visualstudio.com/DefaultCollection` forms name the same
account, so a remote spelled either way resolves an account added with the
other. The UI's account lookup applies the same equivalence: trailing slashes,
host case and the `DefaultCollection` segment's letter case are ignored, so a
`DefaultCollection` checkout enables the PR view for an account added without
the segment and the other way round.

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
| Bitbucket Cloud (access token) | `read:repository:bitbucket`, `read:pullrequest:bitbucket`, `write:pullrequest:bitbucket` |
| Azure DevOps | Code: read & write (PAT) |

`repo` covers check runs and commit statuses. A read-only account cannot use
`ForgeReply`, `ForgeComment` or `ForgeOpenPr`; the forge will return 403 and the
daemon reports it as an auth error instead of retrying.

Bitbucket API tokens are sent as HTTP Basic with the Atlassian account email as
the username and the token as the password. A workspace, project or repository
access token is sent as Bearer and needs no username. Azure DevOps personal
access tokens use HTTP Basic with an empty username. Bitbucket accounts record
which scheme validated (`bitbucket_api_token` or `bitbucket_access_token`) so
a restart keeps the same credential type. App passwords were retired by
Atlassian in July 2026 and are not accepted by the form or the docs. A
Bitbucket Cloud base URL must be the plain `https://bitbucket.org` origin: no
credentials, path, query or fragment, and no Bitbucket Server/Data Center
host. An Azure DevOps base URL must be plain `https://` with one optional
path segment: the organization (`https://dev.azure.com/acme`) or the legacy
collection (`https://acme.visualstudio.com/DefaultCollection`), and no
credentials, query or fragment; Azure DevOps Server hosts are rejected. Both
forges are rejected before anything is stored when the host is not the cloud
service. Neither provider's base-URL errors repeat the input, because a path
or query can carry a secret; Atlassian's `ATATT…` and `ATBB…` token prefixes
are redacted from error messages like GitHub's and GitLab's, including when
they appear quoted inside a JSON error body.

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
no OAuth device flow in their public API. Bitbucket Cloud and Azure DevOps use API
tokens, workspace/project/repository access tokens or personal access tokens
instead.
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
| Conditional PR poll | yes | no | no | no | no | no |

The conditional PR poll is what the thread watch uses on GitHub: every read of
`GET /pulls/{n}` revalidates with `If-None-Match`, so an unchanged detail comes
back as a `304` and costs no second API request. A `304` is answered from the
body the account client's cache holds, which may be newer than an individual
watch's snapshot because the checks poll, the badge and the PR page share the
same client; the watch therefore takes its snapshot from that body and never
from its own earlier read. The other forges read the PR every time, which
spends more of their request budget for the same two-minute interval.

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
  active (Azure DevOps) PRs only, because the filter has no lifecycle selector.
  Bitbucket applies author, reviewer and source-branch filters with the
  documented server-side `q=` language, so a page maps one to one onto a
  server page and later pages never repeat earlier rows
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

## Thread links and watching

A thread can hold any number of pull requests from any of its forges. A link is
the account, repository and number; the thread journal stores it in
`daemon_pr_links`, so links survive a daemon restart. The PR's state is never
part of the link: `ListThreadPullRequests` reads each linked PR from the forge
and returns it with the link, and a failed read is returned as an `error` with
no summary rather than as stale state.

- `LinkPullRequest` validates the PR with the forge, stores the link and returns
  the current state. `UnlinkPullRequest` removes the link and ends any watch.
- `ListThreadPullRequests` fills the thread header badge and the linked-items
  panel. `ListPrThreads` is the reverse lookup on the Pull requests page.
- `ForgeOpenPr` links the PR it just opened automatically. A draft travels in
  the draft field on GitHub, Bitbucket and Azure DevOps; GitLab and Gitea have no
  such field on their create endpoints, so a draft is a `Draft:` or `WIP:` title
  prefix there, and a title that already carries one is not prefixed twice.
- `WatchPullRequest` / `UnwatchPullRequest` start and stop the watch for one
  linked PR.

Watches live in daemon memory (links do not). While the thread is not settled,
the daemon polls every two minutes and shares one read between every thread
watching the same PR. It wakes the agent by appending a user-visible message
that starts a turn, naming what changed and linking to the PR or comment:

- a check fails;
- every required check passes (skipped when the forge cannot say which checks
  are required — GitHub without branch-rule access, a Gitea token that is not
  an administrator);
- the remaining checks finish after a failure was reported;
- someone other than the linked account comments or reviews (including inline
  review comments);
- the branch starts to conflict;
- the PR merges or closes, which also ends that watch. A PR that is already merged or closed has nothing left to watch, so the panel offers no Watch for it and a watch request ends at once.

Each event is reported once per watch, and re-watching starts a fresh baseline
so old news is not repeated. Every watcher keeps its own snapshot and
seen-failure state: a second thread watching the same PR, or a thread watching
again, never replaces another watcher's baseline, so it can neither hide a
change that watcher has not heard nor repeat one it has. A new watch starts
from the current snapshot, and it never clears a rate-limit pause the poller
recorded for that PR. While that pause is in effect a watch request spends no
requests: the first poll after the pause takes the baseline. A re-watch is a
fresh watch: changes that happened during a rate-limit pause before it are not
replayed, and the watcher's seen-failure and comment-only state start again. A
re-watch read that a poll overtakes is discarded, so a body from before a
failure can never move the baseline back and cause the same failure to be
reported twice; a poll read that a re-watch overtakes is discarded too, so a
stale failing body cannot re-report a failure the re-watch already moved past.
Every watcher generation is drawn from one service-wide counter that never
reuses a value, so a read that captured the generation of a watcher the thread
has since unwatched and replaced can never apply its older body to the
replacement. That guard applies to a terminal re-watch as well: when a poll has
changed the watcher meanwhile, the re-watch does not delete it, so a watcher
waiting to retry an undelivered merge wake still gets to deliver it. A
delivered merge, close or tenth-comment wake likewise removes only the watcher
it was computed for: if the thread starts watching again while that message is
being delivered, the new watch stays. A failure wake fires only for a failing
check that was not already failing in that watcher's previous snapshot, so a
re-run that passed and left other checks failing is not reported as a new
failure. A watch ends after ten wakes in a row that brought only comments or
reviews, after eight failed reads in a row, or when the thread is settled or
archived. A tenth comment-only wake that cannot be delivered is retried like a
terminal one rather than dropped. A rate limit only pauses the watch until the
forge's `retry-after` (or two minutes when it does not say); it is not a failed
read.

The watch reads the forge through the service's read paths without asking the
30-second checks poller to take on the PR. That poller belongs to the screens
that show a PR; a watch has its own two-minute poll and leaves no entry behind
when the watch or the link goes away.

A wake the daemon cannot deliver — the thread is busy, the daemon is shutting
down — keeps that watcher's previous snapshot, and the next poll recomputes the
same events and tries again. A terminal watch stays until its wake is
delivered. Failed wakes are bounded like failed reads: after eight in a row the
watch ends.

Comments and reviews from the linked account itself never wake the agent. That
needs the account's login, which some credentials do not report (a Bitbucket
workspace or repository access token has no user identity). When the login is
unknown, the watch skips comment and review triggers for as long as it runs
instead of treating every author as someone else, and
`ThreadPullRequest.watch_notice` says so beside the watch state in the
linked-items panel. The other triggers (checks, conflicts, merge and close)
still apply.

Because the thread list is a client concern, watch changes do not add a
journaled event. The badge re-reads `ListThreadPullRequests` when its panel
opens and when a wake message is appended to its thread, so a watch that ended
(a merge, a close) stops offering "Stop watching" without waiting for a
reconnect. A reconnect replays the history the client missed. When that replay
includes a user message for this thread — the shape a wake takes — the badge
reads once for the whole burst, so a PR that merged while the client was
offline stops showing "Open" as soon as the client catches up. Replayed
history for another thread costs no forge read. A PR that changes with no wake
at all — for example a PR merged while no thread was watching it — updates when
the panel opens.

## Polling, rate limits and caching

The daemon polls PRs it is watching for checks changes. The base interval is
`H4RN_FORGE_POLL_SECS` (default 30, minimum 5); failures back off up to 300
seconds. A state change broadcasts a transient `forge_checks_updated` event
carrying the PR, aggregated state, counts and the connection's current durable
`last_seq`. The event is not journaled, so a client that reconnects re-fetches.
The PR page treats a pushed event as the freshest snapshot: it bumps a
generation counter, and a checks request that was already in flight keeps an
older generation, so its late answer (or failure) does not replace the pushed
one.

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

Error messages redact the account's own configured credential first, verbatim
and in every form an error body or URL can echo it: quoted inside a JSON
message, percent-encoded (the `%xx` escapes match whichever hex case the server
used), or Base64 inside a Basic header. Every copy in a body is replaced, so a
message that carries the plain value plus an uppercase- and a lowercase-escaped
copy loses all three. A JSON body is redacted as decoded strings and object
keys before a nested value is serialized into the message, so a credential
containing a quote cannot come back as a JSON-escaped `\"` value and a
credential used as a key cannot come back as a property name; the escaped
spelling is also matched as a second layer. A JSON body with no known message
key is serialized from the redacted decoded value, never from the raw body, so
a `\/` or `\u00xx` spelling does not survive. Matching normalizes the text once
and replaces every match offset in a single pass, so a body that echoes the
credential thousands of times stays linear. A pattern-based pass removes
anything that still looks like a GitHub, GitLab or Atlassian token. The
redacted message is what reaches a log or a protocol error; the raw response
body never does.

## Testing

`crates/h4rn-forge/tests/checks_and_commits.rs` replays fixtures from
`tests/fixtures/`. The Gitea commit list was recorded from Gitea 1.22
(`H4RN_RECORD_FIXTURES=<dir> ./scripts/test-forges.sh` re-records); the GitHub and
GitLab ones follow those providers' REST schemas. `./scripts/test-forges.sh --ui`
also checks the Commits tab and the disabled re-run reason against Docker Gitea.

The client tests are wiremock tests with realistic JSON for each API shape:
GitHub REST + GraphQL, ETag/304, 429 `retry-after`; GitLab MR, changes, diffs,
discussions and jobs; Gitea/Forgejo PRs, raw diff, comments, reviews, statuses;
Bitbucket Cloud PRs, diffstat (including the paging cap), the `/diff`
redirect, comment `parent` chains, build statuses, basic/bearer auth,
repository-token validation with both account endpoints rejecting, the
documented `q=` filters (including uuid-based personal filters for a viewer
without a nickname and a namesake account) and duplicate-free pages, an opaque
`next` cursor on the second PR page, a distant page served by the documented
`page` parameter, comments `has_more` at the page cap, file-level comments and
`inline.outdated` threads,
`ssh.bitbucket.org`, and same-origin `next` handling; GitLab overflow,
`collapsed` and `too_large` changes and content lines that begin `+++`/`---`;
Azure DevOps PRs, raw
iteration arrays, changed-file contents, the 100-file per-page budget, threads,
statuses across iterations, same-name policy evaluations, missing versus empty
`value` collections, a failed policy read, PRs without `_links`, unknown line
counts, fork PRs whose source side exists only in the fork, unexpected item
404s, `undelete` change types, PAT auth, commit `continuationToken` pages and
per-page diff contents. They also cover the
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
