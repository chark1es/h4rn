# Changelog

All notable changes to h4rn are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses semantic versioning.

## Unreleased

### Added

- **Agents can link pull requests from inside a thread, and outside agents can work with h4rn
  over MCP.** Every harness session now gets h4rn's own tools through its existing per-session
  token: `link_pull_request`, `unlink_pull_request`, `list_thread_pull_requests`,
  `watch_pull_request`, `unwatch_pull_request`, `thread_status` and
  `list_forge_pull_requests`. They act on the calling thread and accept no thread id, so a token
  cannot reach another thread; Settings → Sharing & access turns them off per harness. Agents
  h4rn did not start — Claude Code or Codex in your terminal, and other local MCP clients — can
  connect to `/mcp` with OAuth (authorization code + PKCE, dynamic client registration). Outside
  agents are in **Labs** and off by default: Settings → Labs → **Outside agents (OAuth)** turns
  them on. While it is off, the OAuth and discovery routes answer 404, an outside-agent token is
  refused on `/mcp`, no sign-in can be created, and Settings → Sharing & access hides the
  outside-agent section behind one plain line saying the feature is in Labs. Turning the switch
  off revokes every grant and drops every waiting sign-in in one durable write, so turning it
  back on does not revive them. Only you can grant one: the client's browser shows who is asking
  and where it would return, and the request is
  approved in h4rn → Settings → Sharing & access, where you pick **Read only**, **Supervised**
  (threads run at approval-required) or **Full access** with no preselected default. Tokens are
  scoped, expiring, revocable and stored only as digests; a revocation is durable and a replayed
  code or refresh token kills the grants it produced. A supervised message carries its ceiling, so
  raising the thread's mode later cannot lift it. The OAuth and MCP routes exist only on the
  loopback listener and refuse another site's page. The same page shows the MCP URL with a Copy
  button and lists connected agents with Revoke. Redirects are admitted only to `https` or
  localhost, and the OAuth endpoints are rate-limited in aggregate and per validated client.
  A turn already delivered may still finish after its grant is revoked or the switch is turned off,
  and a Full-access grant has no ceiling; the other limits are listed in
  [docs/outside-agents.md](docs/outside-agents.md#known-limitations). See
  [docs/outside-agents.md](docs/outside-agents.md) and [ADR-046](docs/decisions.md).
- **Share one thread with a teammate through an expiring guest link.** A link is
  bound to one thread and one browser, lasts 24 hours, and works through the
  Connect relay when the machine is linked; without Connect it works only in a
  browser on this machine. The guest sees that thread's real events — messages,
  replies, plans and tool output — and can attach up to four files (10 MB and
  32 KB of text per message; at most 8 messages and 20 MB waiting); they never
  see diffs, other threads, files, terminals or settings, and they cannot answer
  the agent or change the model. Every message they write waits as a **Request**
  above your composer and in the sidebar; **Send** runs it as a normal turn with
  the thread's current permissions, **Dismiss** deletes it and its files.
  Settling the thread, Stop sharing, a new link, turning Settings → Labs → Guest
  links off, or the 24-hour expiry ends the link and deletes waiting files; an
  expired link is swept at startup and periodically. The journal stores only
  hashes of the link token and the browser cookie, only an admin device may copy
  the URL, and the page through Connect never runs on Connect's origin with
  daemon HTML.
- **Issues from every forge and Linear sit beside your projects, and can be linked to a
  thread.** The new Issues page reads each visible project's tracker, grouped per account, with
  open/closed, assigned/opened/mentioning filters, a per-project visibility toggle and search that
  runs on the tracker. Selecting an issue opens it beside the list with its body, state, labels,
  assignees, comments and linked pull requests. Where the account allows it you can comment, edit
  the title and body, replace labels and assignees, and close or reopen (with a reason where the
  tracker records one); a repository no local project owns is read-only, and only the project's
  designated remote is writable, so a fork never writes to its parent; ownership is read from the
  checkouts on every write, so removing a project or repointing `origin` takes effect at once
  rather than after a cache window. Choosing a filter or search that is already applied leaves the
  list alone, and two accounts on the same host let the person choose which one reads each project
  in the Projects menu. GitHub, GitLab, Gitea/Forgejo and Azure Boards are supported; Azure reads
  the work-item types the project actually defines, keyed by type, so a process without `Issue` or
  without `Bug` still lists, and each type closes on its own finished state. Bitbucket Cloud
  removed its issue API in August 2026, so each such project shows a note and no request is sent.
  What each tracker can do comes from its capability row (Linear's arrives in
  the response's own optional field, so an older client's forge list keeps its
  shape), so the page hides a filter, the labels
  editor or the close-reason picker where it would fail, and a request that names a comment page
  carries the provider's continuation (Gitea returns a discussion whole and
  reads the whole label catalogue, even when a server caps its pages smaller
  than the 50 rows asked for). A GitHub duplicate close resolves the
  picked number to that issue's own
  id, and Linear's `duplicate` workflow state shows as closed. A Linear API key connected under Settings → Source control is
  stored like a forge token, never returned and never echoed in an error; pick a team per project
  and its issues appear beside the forge ones, with comments. Issues link to threads through the
  journal, survive a restart, are always read live, and appear with pull requests in the thread's
  linked items. New requests: `ForgeListIssues`, `ForgeGetIssue`, `ForgeListIssueComments`,
  `ForgeCommentIssue`, `ForgeUpdateIssue`, the `Linear*` connection/team/issue requests, and
  `LinkIssue`/`UnlinkIssue`/`ListThreadIssues`. See [docs/forges.md](docs/forges.md).
- **Claude Code and Codex conversations can be imported and continued.** Add
  project → Import finds the conversations those harnesses already saved on this
  machine, grouped by git checkout with GitHub remotes named `owner/name` and
  other folders under "Other folders". Git repositories with three or more
  conversations active in the last 30 days are checked by default; Review all
  shows everything with Select all and Select none. Importing registers new
  projects and creates one thread per conversation with its title, harness,
  model, working folder and kept messages (the first request plus the newest
  user and assistant messages, 200 in total, each with its timestamp; tool
  activity, attachments and setup context are left out — for Codex a turn's
  canonical user event wins, so the generated environment block and the
  project's private AGENTS.md instructions never reach the import, while a turn
  with no canonical event keeps its context). A canonical prompt is stored
  exactly as written — its indentation and trailing newline included — and only
  the comparison key is trimmed. A rewound Codex rollout streams the byte prefix
  it inherits from the rollout it replaced, so its original request and reply
  stay in the imported conversation. The transcript is kept as it was
  written — a long first prompt is stored whole and the handoff applies its own
  ceiling later — and a record is streamed past attachments and tool output
  instead of being dropped for its size; discovery uses the same streaming
  reader, so a short prompt beside a 5 MiB attachment is still offered. A single
  record over 48 MiB — text, a
  tool result or an attachment — skips the conversation with a warning, and
  retention stops as soon as a limit is crossed, so one oversized conversation
  cannot grow memory toward its file size. Each source session id is
  recorded, so running the import again only adds what is new; a transcript
  without one is identified by its path, size and content. The claim and the
  new thread are written in one transaction, so overlapping runs and a crash
  cannot import a conversation twice, and the claim uses the identity the full
  parse found, so two copies whose session id sits past the scan's head still
  import once. Progress is visible and counts the
  conversations finished, with the limits a scan hit, and the import keeps
  running if you leave the view; reopening it shows the current counts.
  Discovery is read-only, skips
  `~/Downloads` (including through a symlink), offers at most 100
  not-yet-imported conversations and 4 GiB per project per attempt (once a
  project's quota is full its other Claude Code transcripts are not read at all,
  while one already imported is still recognised; Codex rollouts are still
  peeked to learn their project), and skips
  transcripts over 4 GiB, conversations over 32 MiB of kept text or 100,000
  records; the list shows what a scan could not read. Imported threads land in
  Done with their own message times and stay there after a restart — including
  their finished agent and their place in the sidebar, which sorts by the newest
  imported message rather than the moment h4rn wrote the thread; their replies
  render as finished history — never as a live or paused stream — and the thread
  says **Imported from** with the conversation's own time instead of a duration
  and token count it never had. An imported thread starts a session when you send
  its first message: it prefers the harness's own resume — spent once that session
  starts — and falls back to a bounded handoff of the saved history (16,000 bytes
  by default, `H4RN_HANDOFF_BYTES` clamped to 1,024–64,000) before your exact,
  unshortened request, including when the harness exits after starting a resume it
  cannot honour, and a Stop during that retry ends the turn interrupted. The
  request is sent once: the fallback never includes the current turn's own prompt
  in the history block, so a large request cannot push the recent messages out of
  the handoff. The handoff reads the history from the thread's journaled messages,
  so no second copy of the transcript is stored. A long prompt still crosses a
  paired connection: a replay arrives in bounded pages, and an oversized frame
  is split into chunks the client joins, so no message is shortened to fit.
  Message text is never logged and
  token shapes are redacted before storage. See [docs/import.md](docs/import.md).
- **A prompt can run on a schedule or when a webhook fires.** Settings →
  Scheduled tasks creates a task with a workspace, harness, model, permissions
  and prompt, then runs it once, every N minutes or hours, daily or weekly at a
  local time, or on a five-field cron expression in a chosen time zone. A run
  can start a new thread, continue one thread, or keep one thread with a fresh
  session each time. The daemon owns the timer and stores tasks and the newest
  100 runs in the journal, so they survive a restart; a slot missed while the
  daemon was down runs once, and a slot whose previous run is still working is
  skipped and recorded. **Run now** starts a task by hand, even while it is
  paused, and History links to the threads it created. A webhook task gets a random path with Copy and Rotate;
  the path is a secret, so lists and change events carry only a "has a URL" flag
  and an admin request reveals it. It accepts POST only, checks the path before
  reading the body, caps the body at 64 KiB, allows 30 deliveries a minute and
  answers 202 before the run starts. Prompts can use `{{body.path}}`,
  `{{headers.name}}`, `{{query.name}}`, `{{body}}` and `{{request}}`; each value
  is wrapped as untrusted request data, and the secret path and credential
  headers are redacted from everything a prompt or transcript sees. Admission is
  atomic, so one slot starts one run even across a pause or two simultaneous
  deliveries; run history and the advanced clock commit together; and a failed
  journal write retries only that run while the real next slot is kept, so a
  retry never becomes a second run and a slot waiting for its retry is never
  read as due again. A once slot is taken when it is admitted, so a pause
  during the run cannot arm it again, and a spent Once task stays spent across
  edits and resumes; only a different date, time or time zone names a new slot,
  and a run is refused until that marker is durable, so a crash cannot re-arm
  it; a spent marker written before time zones joined that identity is migrated
  when the daemon opens it. A schedule that cannot name its next slot is
  disarmed and paused in the same write — it records the reason, offers
  **Resume** instead of **Pause**, and releases its thread's checkout — instead
  of starting a thread on every backoff; a failed write retries the pause with
  its failure, and the pause checks the task's current settings under the same
  write on the first attempt and on the retry, so fixing the schedule meanwhile
  keeps the task enabled and lets that slot run once while the original failure
  stays in the history. On open, a task is paused only when its newest
  clock-scheduled run is that disarm failure — a **Run now** pressed after it
  changes nothing; a row whose timestamp cannot be read and
  that never failed to run is left as stored, keeping its checkout protected.
  Daily and weekly times
  run only at the first occurrence in a fall-back hour, cron returns the
  earliest UTC instant among that hour's jobs, and the editor's default zone
  comes from the daemon. The editor also says why when it cannot reveal a
  webhook URL.
  Webhooks follow the configured `--bind` and any extra bind, never the tailnet
  or LAN listeners.
  A thread with an enabled task keeps its worktree through a clear. See
  [docs/scheduled-tasks.md](docs/scheduled-tasks.md).
- **Threads can change permission mode, model and reasoning effort mid-thread.**
  The model chip in the composer now opens a thread-controls panel: permission
  mode, model, reasoning effort and (on Codex) fast mode. A change applies to the
  next turn — in place where the harness can switch (Claude Code's `set_model`
  and `set_permission_mode`, Codex's per-turn model, effort and sandbox, ACP
  `session/set_model`/`session/set_mode`/`set_config_option`, OpenCode's session
  model and permissions), otherwise by closing an idle session so the next turn
  resumes the conversation on the new settings. The thread summary reports the
  live values and a thread that differs from this device's defaults shows them
  quietly beside its model. Plan mode is offered for Claude Code and for ACP
  agents that advertise it; reasoning effort for Claude Code and Codex; fast mode
  for Codex. A value the harness cannot honour is refused with a clear reason.
  New requests: `SetThreadRuntimeMode`, `SetThreadModel`,
  `SetThreadReasoningEffort` and `SetThreadFast`; `CreateThread` accepts
  `reasoning_effort` and `fast`, and older daemons/threads without the new fields
  still deserialize. `⌘⇧A`/`Ctrl+Shift+A` opens the permission picker
  (Acknowledge moves to `⌘⌥A`/`Ctrl+Alt+A`), and Settings → Threads → New
  threads sets the default permission mode on this device.
- **Threads can commit, push and open a pull request.** The thread menu's **Commit, push and
  PR…** panel shows the workspace's real branch, upstream, ahead/behind counts and its staged,
  unstaged and untracked paths, and whether a rebase, merge or cherry-pick is in progress. Commit
  records the index, or stages tracked and new files first when **Include unstaged changes** is on;
  it refuses a detached `HEAD`, an in-progress operation or an empty index, runs hooks (never
  `--no-verify`) and reports a hook's output. Push creates an upstream on `origin` when asked and
  is never forced: a non-fast-forward rejection or a bad credential is reported as exactly that.
  **Create pull request…** offers title, body, base branch (the default branch is preselected) and
  a draft flag; the daemon checks the remote without running any hook (or submodule recursion) and
  then pushes first when the branch needs it and opens the request. When the real push does run,
  its hooks run as usual and a hook failure stops the open, exactly as a push from the panel.
  **Generate** can fill the commit message or the pull request text from a model chosen under
  Settings → Source control (Off by default, coding agent, or text generator, plus custom
  instructions); it fills the field only and never commits, pushes or opens anything. Only the
  staged diff (capped, binary files named), recent subjects, or the branch's commits and diff stat
  are sent, wrapped as untrusted input. See [docs/git-actions.md](docs/git-actions.md).
- **Threads remember the pull requests that belong to them, and can watch one for you.** Link a
  PR by pasting its URL or picking it from the repository's open PRs in the command palette
  (`Link pull request…`), or let opening a PR from h4rn link it automatically. Links are stored in
  the thread journal, so they survive a restart, and the thread header shows a quiet badge with the
  PR number, its state and `+N` for more; the badge opens a linked-items panel with open, watch and
  unlink. The PR page lists the threads that link to it. A linked PR can be watched: while the
  thread is active the daemon polls every two minutes, sharing one poll across threads watching the
  same PR, and wakes the agent with a plain-language message when a check fails, the required checks
  pass, the remaining checks finish after a failure, someone else comments or reviews, the branch
  starts to conflict, or the PR merges or closes. GitHub is polled conditionally, so an unchanged PR
  costs no full read. Each event is reported once per watch; a watch ends after ten comment-only
  wakes in a row, eight failed reads in a row, or when the thread is settled, and a rate limit only
  pauses it. The state beside every link is read from the forge, and a failed read is shown as
  unavailable rather than guessed. After a reconnect the badge reads the linked PRs once for the
  wakes the client missed, so a PR that merged offline is not left showing as open; a PR that
  changed without a wake updates when the panel opens.
- **Thread titles are written by a model.** A new thread still appears immediately with the first
  line of your first prompt, then a title of at most 60 characters replaces it through the normal
  thread update. Settings → Threads → Title model picks Off, Claude Code (run once in an empty
  folder with no tools) or a text generator you configure under Settings → Providers → Text
  generators: an OpenAI-compatible chat API (OpenAI, OpenRouter, Ollama, LM Studio) or Anthropic.
  Codex is not offered as a title agent: its flags remove nothing model-visible, so a Codex thread
  keeps its first-line title, or uses a text generator. Generators have a Test button that sends a
  tiny prompt and shows the provider's own error; keys are stored on the host like forge tokens and
  are never returned to a client. Plain `http` base URLs are accepted only on loopback; everything
  else must be `https`. Only the first prompt text and the workspace folder name are sent. A
  failed, timed-out or empty answer keeps the first-line title and logs once. Renaming a thread
  yourself saves the name in the thread menu, and no generated title ever replaces it. Subagent
  threads keep the title their task gave them. With no agent picked, the default mode uses Claude
  Code for a Claude Code thread and keeps the first line for every other harness, so a prompt is
  never handed to another provider unasked. See [docs/text-generation.md](docs/text-generation.md).
- **Title generation keeps keys and prompts where they belong.** No redirect is followed, so a
  `3xx` from a generator cannot forward the key or the prompt to another host. Claude Code runs
  with no tools, no MCP servers and no user configuration in an empty folder under the system temp
  directory — never in the data directory beside `keys/` — and one 30-second deadline covers
  version discovery, the run and cleanup. It gets exactly `--tools ""`, `--strict-mcp-config`,
  `--setting-sources ""`, `--disable-slash-commands`, `--permission-mode dontAsk` and the title
  instruction as its system prompt, so it has no shell, no MCP servers, no hooks and no plugins.
  When it fails, the daemon quotes the last line of stderr (the error) instead of the banner before
  it. Any spelling of `</untrusted_request` in the prompt or folder name is escaped, so untrusted
  text cannot close the delimited block early, and the untrusted prompt is never repeated or
  mistaken for instructions.
  A stored key is scrubbed from provider text, Test results, logs and errors before truncation, and
  a reply that echoes the key is discarded. A generator's endpoint, kind and key are read as one
  snapshot so a title job can never pair an old key with a new URL, and a new configuration is
  published only after it is on disk. Key presence is cached, so listing generators never calls the
  keychain, and keychain reads and writes needed by a save or a job run off the async worker. Both
  files are written atomically under one lock, so concurrent saves lose nothing. The Coding-agent
  picker defaults to **Follow the thread's agent** (no agent picked); choosing Claude Code hands
  every thread's title to it. Turning titles Off starts no job and logs nothing per thread. Removing
  the selected generator turns titles Off rather than borrowing a coding agent, a generated title
  that beats the create reply is kept, the rename dialog keeps your draft, the add-generator form
  forgets the key and its fields on Cancel and after saving, a Test spinner always clears (including
  on an unexpected response) and a Test no longer blocks other requests on the same connection, and
  OpenAI-compatible requests send no `max_tokens` (OpenAI's own API gets
  `max_completion_tokens`).
- **Review a branch or one commit, and send line comments to the agent.** The IDE diff pane's
  source picker now offers Uncommitted (HEAD), the turn, Branch and Commits. Branch review compares
  the working tree with the merge base of a base branch, so commits made on the base after the fork
  stay out of the review; the base is the upstream's default branch, then `main`, then `master`,
  and can be changed from the repository's branches. A remote `HEAD` that no longer resolves is
  skipped rather than becoming a dead base. The file list carries git status, added and removed
  line counts, renames and untracked files, and stops at 2,000 entries with a visible truncation
  note; the daemon stops reading `git ls-files` at the first entry inside the workspace past that
  cap and ends git, so a tree full of build output is not walked to the end (a workspace below the
  repository root skips paths outside it, so it may read further down the listing). Untracked line
  counts stop at a byte budget and say `no counts`, and an unreadable or binary new file is marked
  instead of reading as an empty addition. Commits lists the newest 100 commits in the branch range
  and shows one commit's files against its first parent (a root commit against the empty tree),
  following a rename to the parent's old path and reading only the committed blobs, so a large
  working copy does not hide a small commit. A shallow clone's boundary commit says its parent is
  missing instead of pretending every file was added. Commit ids are validated before git sees
  them; a tag or branch name is refused, while an annotated tag's object id is peeled to its
  commit. Both wider lists re-read when entered and on Refresh, and the base-branch list loads for
  the repository rather than the open file, so a slow one still fills the picker after the file
  list and the diff have appeared. The daemon resolves the default base to a branch name and the
  pane keeps it, so the default and that branch picked by name are one comparison. With no base,
  the pane says why and **Compare with HEAD** switches to Uncommitted. Every file still shows the
  existing binary and too-large states, with the file list and comment actions left usable.
- **The diff pane follows the editor tab and stays on the file you pick.** Entering the diff shows
  the active tab's file, and switching tabs follows the new one, but a file chosen inside the pane
  — the phone Turn picker or a Branch or Commits row — stays until you choose another, switch
  editor tabs, or switch the diff source. A reply that arrives after a newer pick is dropped, so a
  slow Branch file list or an older diff answer never replaces what you chose; the base-branch
  picker's own list belongs to the repository, so it still arrives after the diff. The picker shows
  the new source while it loads. The path bar always names the file on screen, including a diff
  whose tab was closed since the diff outlives its tab. Replies for a previous workspace are
  dropped too, so switching threads never shows one project's diff in another.
- **Line review comments go to the thread's composer as plain context.** Select a line or range in
  any diff mode (click, Shift-click, or the keyboard) and write a comment. Each comment remembers
  the exact diff it was written on — the turn's id, the commit's sha, the resolved base's name —
  and the label printed above the diff is display text only. The daemon prints a short one (a
  turn's trailing id characters, a commit's short sha, the base's name) for people to read; it may
  repeat or change without moving a comment, because the full request is what matches. A branch
  comparison is keyed on the base the daemon resolved, so the default base and that branch picked
  by name keep the same comment inline. Pending comments sit under their lines while that scope is
  open and are listed with their source when it is not, survive switching files and modes for the
  thread (in page memory; a reload clears them), and can be edited or deleted. **Send to agent**
  appends one quoted block per comment (path, side, line range, the quoted lines, the comment and
  the diff source) to the composer draft; nothing is sent until the person presses Send. There is
  no mock data behind any of it: every list and diff comes from git in the thread's workspace.
- **The daemon can run as a per-user service.** `h4rnd service install` writes the
  shipped systemd user unit (or launch agent, or Windows scheduled task) with the
  running binary's absolute path and current data directory, then enables and
  starts it. `status` reports installed/active/enabled, `restart` and `logs` work
  on every platform, and `uninstall` stops and removes the service without touching
  the data directory. Installation is idempotent, refuses a binary under
  `target/debug` or a temp directory unless `--force`, and mentions
  `loginctl enable-linger` on Linux when lingering is off. Reinstalling an
  unchanged launch agent that is already running leaves it alone; on Linux a
  changed unit that is active is enabled and restarted after `daemon-reload`,
  so its new `ExecStart=` and environment take effect immediately and it still
  comes back at login, while an unchanged or stopped unit goes through
  `enable --now`. On Windows the
  task is named per account (`h4rn-<user>`) and uses an inline launcher that
  waits up to 30 seconds for the updater's installation lock and repairs a
  missing `h4rnd.exe` itself before starting the daemon, so it starts after an
  interrupted update even when the binary there is an older image without
  `--recover-only`. A task that an older release registered under
  the machine-wide `h4rn` name is removed by the next install or uninstall. The
  systemd unit keeps a `$` in the executable path literal (systemd does not
  expand variables there) while still escaping `$$` in arguments.
- **`h4rnd update` installs signed daemon releases.** It finds the latest GitHub
  release for the repository baked in at build time, downloads the archive for
  this target triple and `SHA256SUMS`, verifies the hash and a minisign signature
  over `SHA256SUMS` against the compiled `H4RN_UPDATER_PUBKEY`, and replaces the
  binary atomically. The previous binary is kept for `h4rnd update --rollback`; a
  running service is restarted, and an active turn stops the update unless
  `--force`. Builds without a public key refuse to self-update and print manual
  steps instead. Release builds with the signing key now publish
  `SHA256SUMS.sig`; an archive signature alone is not accepted.
- **The updater secrets are a pair.** `H4RN_UPDATER_PUBKEY` gives a build the key
  it verifies with, and `TAURI_SIGNING_PRIVATE_KEY` produces `SHA256SUMS.sig`. A
  release with the public key but no private key now fails in the release
  workflow instead of publishing archives every daemon refuses to install.
- **Updates are one-at-a-time and interruption-safe.** An advisory lock beside the
  binary serializes updates, rollbacks and startup recovery, and staging files are
  created exclusively with unique names. On Unix the replace and the rollback are
  each a single `rename` over the install path; on Windows the moved-aside binary
  is restored by the next start (or by the scheduled task's recovery step).
- **The backup rename is its only commit point.** The rollback copy no longer
  goes straight to `h4rnd.previous`: it is copied into a unique temp file beside
  it and its checksum sidecar is staged. The old sidecar is removed before the
  backup is renamed, and the new one is published afterwards as an optional
  extra check. A crash or failure before the rename leaves the old backup in
  place, with its sidecar when that is still present; one after it leaves a
  backup that `--rollback` still accepts after its size and executable checks.
  The new backup can therefore never sit next to the old backup's checksum, so
  the last good release stays rollbackable. `--rollback` and startup recovery
  refuse a backup that is missing, empty, not executable
  on Unix or, when a record is present, does not match it; recovery leaves the
  files in place and says what to do. On Windows `h4rnd update` rewrites the
  registered task's launcher before it renames anything. If that task exists but
  its action cannot be read (missing PowerShell, a failed or non-zero query, or
  unparsable output), the update now refuses before any rename unless `--force`,
  and prints the manual recovery steps. A legacy task that passed no
  `--data-dir` is migrated with the daemon's default data directory, which is
  what it was already using. The launcher waits for the installation lock and
  repairs a missing binary itself, so an interrupted update or rollback comes
  back up even from a task registered by an older release. The update releases
  the lock before it dispatches the restart, so the launcher never waits for the
  update that started it.
- **A legacy Windows migration cannot leave the daemon stopped.** Migrating the
  machine-wide `h4rn` task stops it, so the idle guard probes the service once
  more immediately before the migration. If that migration ran and the binary
  replace then fails, the update starts the service again, or explains that the
  task was stopped and that `h4rnd service restart` brings it back. A refusal
  before the migration, or a task that belongs to another installation, leaves
  the running daemon alone.
- **A turn that starts during the final step is not cut off.** The idle check is
  repeated after the new binary is in place and immediately before the restart.
  If a turn started, the binary stays installed, the restart is skipped, and the
  update says to run `h4rnd service restart` when the turn finishes. Release
  metadata requests keep a short total timeout; asset downloads use a 30 s
  idle timeout and no total limit, so a large archive on a slow link can finish.
  On Windows a failed `/End` when the task is not running is harmless and only
  the start step must succeed.
- **`h4rnd update` probes the daemon it will actually restart.** The guard
  resolves the registered unit, plist or task first and uses that service's own
  endpoint, in the daemon's order: a `--bind` in the registered command line,
  then `H4RN_BIND` from its environment (systemd `Environment=`,
  `EnvironmentFile=` and the user manager; launchd `EnvironmentVariables`; or
  the account-wide Windows environment), then the compiled default. The
  updater's own `--bind` is never used, so an idle second daemon on the CLI's
  port cannot make a busy service look safe. A source that cannot be read (the
  systemd user manager environment, an `EnvironmentFile` the manager names, or
  the account-wide Windows environment)
  leaves the service unverified, and the update and `--rollback` refuse unless
  `--force` is passed. The guard is checked again
  right before the binary is replaced and again just before the restart, and it
  fails closed: a daemon that is active or listening but does not answer
  `/healthz` blocks the update, as does a health response without `active_turns`.
  The update only restarts the service when that unit/plist/task runs the binary
  being replaced; a failed restart is a warning, not a failed update. The Linux
  command line comes from `systemctl --user show -p ExecStart --value` and the
  environment from `systemctl --user show -p Environment -p EnvironmentFiles
  --value`, so a runtime drop-in that sets `H4RN_BIND` is seen. The unit files
  (with reset-and-override drop-ins and `%h`) are read only when systemctl
  cannot answer.
- **Windows service reporting has two limits.** `h4rnd service status` reports
  `enabled: unknown` there, because Task Scheduler's states do not map cleanly to
  "starts at logon", and `h4rnd service logs` prints guidance instead of following
  a file because the task runs without a console and the daemon writes no log
  file. Task names are per account, and a task installed by an older release
  (which started `h4rnd.exe` directly) keeps working and is migrated the next
  time `h4rnd service install` runs. macOS service status reports
  `enabled: unknown` unless `launchctl print-disabled` could be read, instead of
  assuming the agent is enabled.
- **Preview HTML files in the right panel.** A report, chart or mockup the agent wrote can be
  opened from the Files tab's new action on `.html`/`.htm` rows, or from a relative HTML link in
  an agent message. The Preview tab reads the file through the daemon, inlines its relative CSS,
  JS, images and fonts from inside the workspace, and renders it in a `sandbox="allow-scripts"`
  iframe with a `default-src 'none'` CSP, so page scripts run without gaining access to h4rn, its
  cookies or its storage. The desktop and mobile shells serve the prepared page from their own
  `h4rn-preview` origin (a `srcdoc` frame would inherit the window's `script-src 'self'`); the
  browser build keeps `srcdoc`. The prepared document starts with a trusted doctype and the policy,
  so no malformed comment in the workspace file can put its markup ahead of the CSP. Limits are a
  400 KiB text read for the page and its stylesheets and scripts (one secure-transport frame,
  escaping included, so a smaller file that needs more than the frame once serialized is refused
  as too large instead of closing the session), 360 KiB per binary asset and 10 MB for all embedded
  copies. That frame bound applies to the secure transport, which closes a session on an oversized
  frame; the plain loopback connection has no frame bound, so large listings and the editor's 2 MiB
  reads are delivered whole there. The budget is charged for each copy as it is inserted, including
  a file reached again through an imported stylesheet, and an insertion that would not fit is left
  out and reported, so the page is never built past the cap; fetching stops once the budget is
  spent. References cover styles, scripts, images, every `srcset` candidate (workspace candidates
  inlined or removed, `data:` and `https:` ones kept), `<video>/<audio>/<track>`, `<source>`,
  `<iframe>`, `<object>`, `<embed>`, SVG `href`/`xlink:href` and CSS `@import` of a workspace
  stylesheet, which is inlined as CSS text. Links in the page ask in an inline bar before opening
  in the system browser, and that bar freezes its URL until the user answers. It reloads when the
  file or an asset changes on disk (the workspace watch is shared with the IDE and re-sent after a
  reconnect), expands to full
  width in the main area, and opens in the phone panel sheet. Files outside the workspace, over a
  size limit, or not UTF-8 show a plain message instead, and the selected file is still watched so
  recreating it reloads the preview. On an Android WebView without document-start script support,
  where Wry's fallback would add script hashes to the CSP and silently block every page script, the
  preview turns itself off with a plain message asking for an updated System WebView instead of
  showing a page without its scripts. MCP Apps, a headless browser and dev-server proxying are not
  part of this change.
- **Bitbucket Cloud and Azure DevOps pull requests.** The forge layer now understands two more
  providers. Bitbucket Cloud takes an Atlassian API token (account email plus token, HTTP Basic) or
  a workspace, project or repository access token (Bearer); app passwords were retired in July 2026
  and are no longer offered. Access tokens cannot read `/user` or `/user/workspaces`, so the add
  form sends the workspace's detected Bitbucket repository and the daemon validates the token
  against it. The client lists open pull requests, applies the documented `q=` author, reviewer and
  source-branch filters so pagination stays server-side and pages never repeat, reads the raw PR
  diff from `/pullrequests/{id}/diff`, rebuilds comment threads from `parent` chains, and maps build
  statuses onto checks. Azure DevOps Services takes a personal access token (Basic with an empty
  username), lists active pull requests, reads commits and iteration changes, builds a unified diff
  page by page from the iteration's `commonRefCommit` and `sourceRefCommit`, maps thread statuses
  (`fixed`, `closed`, `wontFix`, `byDesign`) to resolved, and combines PR statuses with blocking
  policy evaluations as checks. Remote detection understands `bitbucket.org`, `ssh.bitbucket.org`,
  `dev.azure.com`, `ssh.dev.azure.com`, the legacy `vs-ssh.visualstudio.com` SSH host and legacy
  `*.visualstudio.com` URLs. Bitbucket Server/Data Center and Azure DevOps Server are explicitly
  unsupported. Neither provider can re-run checks from this surface, and the UI shows that reason
  the same way it does for Gitea.

- **Notifications, Open at login and the tray are real settings.** Settings → Notifications now
  controls OS notifications on the desktop app (the Tauri notification plugin) and in the browser
  (the Web Notification API, asked for only when a switch is turned on; there is no separate
  Allow button). Approvals and waiting questions, finished and failed turns, and a linked machine
  going offline notify only when the window is not focused or a different thread is open.
  Replay follows the journal-head boundary named by the subscribe answer. On a first connect
  nothing was replayed, so every frame held during the handshake is live even when its sequence
  sits at or below the head the answer names; a resumed subscribe treats events at or below its
  boundary as history. A subscribe that outlives its own 6 s request
  keeps holding the buffer until its answer closes the replay at that boundary (or the socket
  drops it for the next subscribe to replay); an older daemon that accepts a live-edge subscribe
  without naming a boundary delivers those frames untagged, because nothing was replayed. A Done
  follows the daemon's thread summary and needs a live completion for the current turn: a thread
  moved to Done by another client, or a replayed completion followed by a live tick, says nothing.
  A failure is
  the turn's own outcome, not the tab's reputation, so it alerts even after a manual move to Done;
  a failure whose thread no summary has named yet waits for that summary, and a subagent child's
  failure is dropped once the summary names its parent. A live completion is spent
  on the one daemon transition it caused: a manual tab move by the person discards it once the
  daemon accepts the move, and a reconnect clears every remembered completion. The notifier keeps
  the last streamed summary per thread separately from list snapshots: a routine refresh only adds
  threads the stream has not named, so a list that answers Done before the completion and its
  upsert cannot swallow the finish. A
  turn that hands off to a queued turn says nothing, an interrupted turn that lands in Done reads
  “Stopped: the turn was interrupted.”, and a token or turn limit reads as “Stopped”, never
  “Finished”. A burst in one thread
  waits for a quiet period (1 s of no new event, capped at 3 s) and announces the latest message.
  Clicking a notification focuses h4rn and opens the thread. Subagent child threads stay quiet (the
  parent's mirrored approval covers them and their own failures are dropped); an approval or
  failure that arrives before the summary names the child is withdrawn when that summary arrives,
  and an answered approval cancels that
  thread's queued notification unless a newer approval replaced it. The choices persist per device
  (`h4rn.notifications`): approvals and finished turns on, host offline off. In the browser a
  denied OS permission is shown as “Blocked in system settings”, and a still-undecided permission
  renders the switches off without forgetting the stored choice; the desktop notification plugin
  cannot detect a blocked permission, so Settings says nothing about it there rather than claiming
  a state it does not know. Host offline covers only the machines this device is linked to through
  the relay; the machine the live transport reaches is always online. That machine comes from the
  peer, not the account's device rows: a relay link names the machine pairing recorded from
  `machine_devices`, and a direct connection uses the connect machine the daemon reports in `hello`
  from its own signed-in session. Only when neither exists does `hello`'s local host name stand in,
  narrowed to the one host it names; a shared name can no longer say which machine the transport
  reached and matches neither. A verified machine keeps all of its hosts online, including WSL
  siblings. A
  due alert fetches linked presence once more before delivery and is
  dropped if the host came back, the fetch failed or the client disconnected. A direct-only session
  hides the row and says why. On Linux the desktop notification plugin passes `replaces_id = 0`, so
  a later message for the same key stacks instead of replacing; coalescing still limits it to one
  message per quiet period.
- **Open at login** registers the desktop app as a login item through the autostart plugin.
- **Keep running in tray** adds an Open h4rn / Quit tray icon and makes the window's close button
  hide h4rn instead of quitting while agents keep working; the shell stores the choice so the
  close handler knows it before the UI loads. Launching h4rn a second time shows and focuses the
  hidden window, and a macOS dock click does the same, so a tray icon the Linux desktop never shows
  cannot trap the app. Hide-on-close is only enabled when the tray icon was actually created: if
  the shell cannot add one, closing quits as before and Settings disables the switch with an
  explanation. Both startup rows are hidden in the browser and on mobile.
- The mobile build does not send notifications yet, and Settings says so instead of offering
  switches that do nothing.
- **The composer picks files with `@` and runs commands with `/`.** Typing `@` at a word boundary,
  or pressing the `@` button, opens a caret-anchored list of the thread's workspace files; typing
  filters it with a path-aware fuzzy match (`src/comp` finds `apps/ui/src/components/…`). The index
  walks the daemon's existing directory listings, skips dependency and build folders, and reuses
  one cached walk per host and workspace. `↑`/`↓` move, `↵` or `Tab` inserts, `Esc` closes and
  leaves the typed text, and a click inserts too. The `@query` token becomes the workspace-relative
  path plus a space, quoted when the path contains spaces (`@"src/with space.ts"`). Typing `/` as
  the first character opens the actions h4rn already has for the thread — stop the turn, changes,
  subagents, the terminal, the IDE and a new thread — and choosing one runs it and clears the
  slash text. While a menu is open its arrows and accept keys do not fire prompt history or send;
  IME composition never opens or drives it. On a phone the same list is a sheet above the keyboard.
  Harness-native slash commands and skills are not listed: no harness publishes commands through
  h4rn and h4rn defines no skill invocation of its own.
- **Unsent composer text survives a reload, an app restart or Android killing the webview.** Every
  draft is stored under its own key for a host and thread, so one tab never erases another
  thread's draft; for the same thread the most recent edit wins, including when both tabs type
  inside one debounce window — the later edit is saved and the tab whose queued edit lost updates
  to the text that won. Writes happen after a short debounce and are flushed when the page hides.
  A sent message clears only the
  text that was sent: a newer draft written in another tab while the message was in flight stays,
  and a failed send keeps the text so it can be retried. Sending a prompt recalled with `↑` does
  not clear the unsent draft recall preserved; it comes back when the send finishes. A draft for a
  thread the daemon is still starting stays in memory under its temporary id and moves to the real
  thread id once it exists, and is never written under the temporary id. Every write carries its own revision (a
  timestamp plus a per-tab id), so two tabs writing in the same moment cannot be confused, and a
  send clears only the revision the composer was showing rather than whatever storage holds when
  the reply finishes; a removal that failed is retried only while storage still holds that
  revision. Drafts nobody has touched in 30 days are dropped when the app loads, and the store
  keeps at most 200, dropping the oldest; a draft whose age cannot be read is never treated as the
  oldest, and a corrupt value is dropped from storage. Attachment bytes are never persisted: a
  restored draft is its text, and files must be attached again. A draft changed in another tab
  appears in the open composer only when that composer has no unsaved edit of its own.
- **The new-thread box follows the host and folder you see.** Its prompt is saved per host and
  folder, and the last folder used on a host is remembered for the next time the dialog opens. The
  box stays disabled with a quiet “Connecting…” until a host is known, so no text is typed before
  there is a slot to keep it in. The folder's draft switches only when you commit the folder (leave
  the field, press Enter, or pick a project), not on every keystroke; an empty folder means the
  default one it will use. Switching folders, or the daemon's own host changing before you pick
  one, shows that slot's text and moves nothing. Start sends exactly the text on screen to the
  folder on screen, even when you typed the folder but did not commit it, and clears only the slot
  that text came from — only while it still holds what was sent.
- **The composer recalls earlier prompts.** With the caret on the first line and nothing selected,
  `↑` walks back through the prompts already sent in this thread and `↓` walks forward, finally
  restoring the text that was in progress. Recalling a prompt does not touch the stored draft, so a
  reload still brings back what you were typing; editing the recalled text replaces it. Multi-line
  prompts keep stepping with the caret at their first line. The history is read from the thread's
  own user messages, so it needs no separate copy and works on any device that can see the thread.
  Typing leaves history mode, and an open menu or IME composition leaves the arrow keys alone.
- **Harnesses: run any ACP agent as a local command.** Settings → Harnesses can now store any
  number of local ACP agents (display name, unique instance id, executable, literal arguments,
  environment overrides and a thread-workspace or fixed working directory). A new `acp` harness
  kind reuses the shared ACP session runtime, so prompts, tool calls, plans, permission requests,
  cancel and `session/load` resume behave exactly as they do for Antigravity; the Antigravity
  fallback transport and limit parsing stay in its own adapter. The new-thread picker lists
  configured agents and reads the models each one advertises. Writes need the `admin` scope and are
  persisted in `acp-agents.json`. Installed state comes from resolving the executable, and a version
  is shown only when the agent reports one during `initialize`; h4rn does not install or update it.
  See [docs/acp.md](docs/acp.md).

### Changed

- **The `thread.acknowledge` shortcut moved off `Mod+Shift+A`.** It is now
  `Mod+Alt+A` (`⌘⌥A` / `Ctrl+Alt+A`); `Mod+Shift+A` opens the new permission
  picker. Shortcuts help and the command palette show the new binding.

### Fixed

- **An outside agent's grant can no longer be widened, replayed or read from the
  store.** A supervised ceiling now holds for the whole turn it was attached to:
  raising the thread's mode partway through that turn no longer switches the live
  session to full access (the person's next, uncapped turn gets the new mode),
  and a turn stopped by a provider limit resumes under the same ceiling, including
  after a daemon restart. `list_threads` and `start_thread` redact the thread
  title as `get_thread` already did, so a credential pasted into a first prompt
  never comes back from a read-only grant. A revocation cannot lose a race with a
  token's last-used write, and revoking an agent frees its registration slot so
  the "revoke one in h4rn" advice works at the cap; a revocation that could not
  be saved now reports an error instead of success. The daemon accepts an
  `Origin` only when it is its own exact `scheme://host:port` (a page on another
  loopback port is refused) and a `Sec-Fetch-Site` only when it is `same-origin`
  or `none`; the check runs before the rate limiter, so a refused page cannot
  spend the aggregate bucket. `/oauth/authorize` is counted in that bucket, and
  waiting sign-ins are capped per client and in total. The token endpoint checks
  RFC 8707 `resource` against the code's binding, and an approval keeps its
  one-time code in memory only, so the store file never holds a live code.
- **A supervised ceiling now travels with the work it belongs to, and the OAuth
  routes follow the real socket.** A supervised `start_thread` caps its first
  prompt, not only the thread's mode, and the resume schedule stores the
  reporting turn's ceiling in the same ledger write that arms it, so an exit
  report that arrives after the capped turn settled and a daemon that stops
  right after arming both resume at the ceiling. A PR watch created in a
  supervised turn keeps the grant and its ceiling: a revoked grant starts
  nothing, and a live one starts at most at the ceiling. A live mode switch and
  the selection of a session for a prompt share one per-thread gate, so an
  incoming supervised message cannot reuse a session the switch has already made
  permissive. A grant approved without an RFC 8707 `resource` is bound to the
  daemon's own `/mcp` audience; exchange, refresh and use check the same
  binding, and a token with no audience is refused. OAuth and `/mcp` are served
  only by a socket whose bound address is loopback, so a `0.0.0.0` or LAN bind
  no longer exposes them, and the guard also refuses a peer that is not this
  machine. A failed revoke keeps the grant so a retry works and reports
  `Internal` rather than `NotFound`, a spent code is kept while its token family
  is live so a late replay still revokes it, a failed replay revocation is
  reported, a malformed or duplicated `Origin`/`Sec-Fetch-Site` is refused
  instead of treated as absent, `set_harness` commits a switch only after its
  write succeeds, and both browser sign-in screens lost their uppercase eyebrow.
- **A grant is now the exact sign-in that asked for the work, and every later
  turn is checked against it.** The daemon carries a `(client, token family,
  ceiling)` triple through everything that can start a turn later: a queued
  message, a prompt parked by a usage limit, the resume schedule, a PR watch, the
  wake that watch causes, and a delegated child. Delivery re-checks that exact
  family and refuses — settling the turn with a plain explanation and starting no
  session — once it is revoked, including across a restart. Any live token in the
  family keeps it alive, so a watch no longer dies when the one-hour access token
  expires while the refresh token is live, and revoking one family stops its work
  even when the same client holds another live sign-in. A wake turn carries the
  triple, so a watch it re-creates keeps the grant instead of clearing it, and a
  delegated child starts at the calling turn's ceiling rather than the thread's
  newer mode. An auto-resume selects its session through the same per-thread gate
  as a live mode switch, so it cannot reuse a session the switch already made
  permissive. A second approval no longer prunes the spent code of a live family,
  so a late replay still revokes it. Thread titles are redacted before the
  80-character cut, the redactor removes a partial owner-token prefix and
  daemon-issued guest links (`/guest/<token>`), and the MCP URL follows the socket
  that really serves it (`[::1]` for an IPv6 loopback bind, the loopback extra
  bind when the primary is a wildcard or LAN address, `127.0.0.1` for a wildcard
  bind, and unavailable when no loopback socket exists); `/mcp` is mounted on a
  non-loopback bind behind the loopback guard so harness tools keep working,
  while OAuth stays loopback-only.
- **A guest exchange never stalls the relay, and a guest request is one decision with its card.**
  Guest work now runs in its own tasks behind a small concurrency cap, so a chunked 10 MB answer, a
  snapshot or a file write can never block the loop that drains the relay socket or starve other
  paired devices. A submission reserves the pending budget before writing any file and becomes
  visible only once its files are written; ending a link, a new link, settling the thread and the
  Labs switch all take the same lock, so a racing submission leaves no request and no directory. A
  sweep and a Send or Dismiss raise `GuestRequestResolved` before deleting a row, and the event names
  its request, so a crash between the claim and the event cannot leave a card nobody can answer and
  a repeated event cannot clear another guest's hand. A settled thread now ends its guest link from
  the lifecycle tick and from the sweep (including after a restart), and refuses a new link, a
  submission, a Send or a Dismiss; its header hides **Share with guest…** like the row menu and
  palette. The guest page reuses one node per streamed reply and one plan list per turn, so a live
  stream, a reload replay and a plan update each render once, and its "Waiting for the connection"
  notice stays up while it retries. Pairing lost the unused `guest` scope. Connect now sends
  `Content-Security-Policy: sandbox; default-src 'none'` on every relayed answer, downloads PDFs and
  opaque files, and forgets a guest waiter on every failure. Reading a guest link's URL from a paired
  device is audited; reading it on the machine itself is not a remote action. The custom-journal
  fallback uses one in-memory SQLite connection, so its tables do not vanish.
- **Guest links bind the first browser through Connect, and each request and its card stay one
  decision.** The relay now carries the daemon's page-load cookie back to Connect, which checks its
  name, path, `HttpOnly`, `Secure` and `SameSite=Strict` before rebuilding it, so a fresh link opens
  bound instead of showing "opened in another browser". A request is written in a `publishing` step
  before its card and finished as `pending`, and startup either raises the same card or discards the
  row and its files, so a crash never leaves a row that uses quota without a card. Publication
  re-checks the link, the Labs switch and the thread's placement, so an expired, revoked or settled
  guest gets a refusal and leaves nothing behind. Cleanup claims each row with a conditional update
  like Send and Dismiss, so a sweep cannot delete an attachment a Send already claimed or release a
  reservation its writer still owns. Turning the Labs switch off saves the setting and ends every
  link in one transaction, so re-enabling it cannot revive a link. A guest's page also sees only its
  own link's queued messages: a new link never replays the previous guest's dismissed text or file
  names. The LAN listener serves no guest route at all, direct links stay loopback-only, and the
  guest page shows real plan text (not "Step 1"), keys plans by turn, and draws its focus ring on
  the visible attach control. `?thumbnail=0` now serves the original image, not a thumbnail.
- **A guest link follows the address the daemon really bound, and a dropped upload still
  finishes.** The direct guest routes are chosen from each listener's actual bound address: a
  loopback bind, or a wildcard bind whose loopback connection reaches the same port. The guest
  handlers repeat the peer check `/ws` makes and answer 404 to any other address, so a plaintext
  network request never reaches a token. The direct link names the primary listener's port instead
  of the Network-access port, and a full pending queue is refused from the request headers before
  an upload is buffered. A submission or a Send now runs to completion on the daemon when the
  guest's browser or the owner's app disconnects, so a half-written request, its files and its
  quota are never stranded until the next restart. The guest page opens on the state the daemon
  served — expired, revoked, disabled or unbound shows its notice with no composer — and its attach
  focus ring appears only for keyboard focus.
- **A queued message runs at most once, even when the agent hits a limit.** A prompt the agent has
  accepted is never sent again automatically, whether or not it reported work: the turn settles as
  limited with its trace, and the person can resend. Only a write that failed before acceptance is
  handed back to the pause. While a promoted turn's write is in flight, its output is held and
  released in order; a flood of tool updates can no longer push its completion out of the hold, and
  an acceptance that lands between two events still releases the earlier output first, so the turn
  can never stay Working forever. The hold also counts stale diagnostics, so an agent that floods
  the stream with garbage during the write cannot grow it without limit while the completion is
  still kept. The ACP reader now bounds every event it queues, not only session updates: a
  notification or stdout-garbage flood drops the excess like text does, a permission backlog has
  its own small bound, and a permission flood can never delay the exit report. A permission request
  the backlog cannot take is answered at once with the cancelled outcome instead of being dropped,
  so the agent never waits on a request no one will answer and a turn never hangs on it. Below
  that pressure the lanes keep their arrival order, so a permission request that arrives before an
  update is not overtaken. A turn stopped while a closed session's write fails ends Interrupted with
  no transport row, and a prompt is saved for a restart only once a schedule really owns it.
- **Issues follow through on the tracker's answers.** An Azure team project whose name has a
  space (`Platform Team`) now reads through percent-encoded paths and links instead of a 404, and
  Gitea/Forgejo finds a label that belongs to the organization as well as one that belongs to the
  repository, without needing an organization-scoped token for a label already on the issue. GitHub's
  "Opened by me" and "Mentioning me" searches stay on open issues, its timeline is read across up
  to ten pages so a pull request cross-referenced on a later page still appears, and a timeline
  link to another origin is never followed with the account's credentials. A Linear team attached
  to two projects stays writable after one project is removed, Settings shows why a Linear account
  or team list failed to load, and a project whose repository has no matching account is named on
  the Issues page (or the failed account load is named instead). On the page itself, a failed
  "Load more" keeps its button, a page that arrives after its project was hidden or its account
  changed is dropped, saving an edit keeps the comment draft, the linked-items panel tells two
  `#12` issues from different repositories apart and names a Linear team by its key, and the filter
  strip wraps on a phone screen with 44 px targets.
- **The diff pane follows the editor tab in every review mode.** Branch and Commits used to keep
  the file picked from their list while a tab switch moved the pane only in Uncommitted and Turn.
  All four sources now show the active tab's file against the source on screen, and the wider
  file lists stay one click away.
- **Commit mode names the base it reuses.** The commit range has always followed the base chosen
  for Branch (or the repository default), but the pane never said so. The diff header now reads
  `since main` — or whichever base — beside the commit, so a review against a non-default base is
  never implicit.
- **A superseded diff never speaks for the newer choice.** Starting a newer diff or list request
  drops the abandoned one's body as well as its spinner, so changing the base, the commit or the
  lists while a file is still loading never shows the previous comparison's lines under the new
  one. A Branch or Commits tab switch keeps the scope and re-reads its file list when that list is
  still out, so the list and the empty commit range survive the switch. A stale branch-list reply
  no longer stops the newer read's spinner. The revisited-workspace spinner reset and the branch
  generation's move with the workspace already shipped on main; tests now cover them, so a reply
  from the workspace you left is still ignored.
- **Commit, push and pull request follow what git and the forge really say.** A
  branch deleted on the forge no longer skips the push a pull request needs just
  because the local remote-tracking ref is stale: the daemon asks the remote with
  a dry run of the exact push the real push will run, so a separate `pushurl` or
  an `insteadOf` rewrite cannot make it check a different repository. Every
  `pushurl` must report the branch up to date before the push is skipped. That
  dry run skips the pre-push hook, so a failing, slow or side-effecting hook
  cannot block or delay opening the request; the real push still runs it. A
  failing lookup is classified and redacted like a push, so a 403 that echoes a
  credential shows "sign in to the git remote" without the credential, whatever
  the URL's shape (IPv4, IPv6, hostname or port, userinfo or query): the URL is
  parsed and its userinfo, query and fragment removed, with a conservative
  fallback when it cannot be parsed. A URL git prints with raw whitespace inside
  its quoted field is replaced whole — no real URL contains whitespace, and
  splitting at it would leave the rest of the token in the message. A rejected
  non-fast-forward is still named as one when git's advice is turned off,
  because the push's porcelain record is read. A path-limited commit message
  describes the working-tree content `--only` will record, including an edit
  that was never staged, and works in a repository with no commits yet. A push
  result names the remote branch it wrote (still sent as `upstream`, so older
  clients read it) and no longer claims the branch's tracking upstream changed
  when `set_upstream` was false. Opening a pull request gives the lookup, the
  push and the forge call one 220-second budget, inside the dialog's 240-second
  wait, and the forge call fails fast with "rate limited; try again in Ns"
  rather than sleeping past it. An ordinary request keeps its own timing: a
  documented rate-limit reset is waited out before the transfer's timeout
  starts, so a wait longer than one transfer no longer fails a viewer or a
  lookup. See [docs/git-actions.md](docs/git-actions.md).
- **A terminal's reply owner still answers after a resync.** With an old client attached, the first
  viewer to answer owns every reply. But another client that held the query's bytes raw could make
  the owner's snapshot-only claim give way, and the old client's copy was dropped too, so the shell
  received no answer at all. A claim now gives way only to a viewer that could actually be written
  under the ownership rule: any viewer when all of them name positions, otherwise only the owner.
  A positioned viewer that the owner rule blocks can no longer swallow the answer either.
- **Closing a terminal never signals a pid that another process may own, and still ends a shell that
  ignores the hangup.** On Unix a pid is freed the moment the shell is reaped, so a close that ran
  between the reap and the exit report could SIGHUP or SIGKILL whatever process reused it. The
  reader thread now waits for the shell with `waitid(WNOWAIT)`, which reports the exit but leaves
  the shell a zombie, and every real reap publishes the exit under a lock the close holds around
  each pid-targeted signal: the opening hangup, the primary kill (a SIGHUP by pid on Unix) and the
  pid fallback. While that lock shows no published exit, the pid is still this shell's live process
  or its unreaped zombie, never a reused pid. A shell that survives the hangup and has no usable
  process group is still ended, and the session's internal `alive` flag is never taken for a reap
  confirmation. The group's TERM and KILL still go out on purpose, to end the shell's jobs, and
  Windows keeps its handle kill, which holds a process handle rather than a bare pid.
- **Thread settings always describe the session that will run the next turn.** The model list a
  Codex session reports survives the session ending, so an idle thread still offers the models its
  CLI really has (and a new Codex thread starts on the first of them, not on a static default the
  installed CLI may have dropped). A thread created on a model the CLI no longer lists keeps the
  live list, and a switch is still checked against it. Two changes to the same setting are
  serialised, so the summary, the journal and the session record always name the value the session
  ended on. A prompt claimed just as its session died is handed back and delivered on a fresh
  session instead of being written into a dead one. A resumed ACP session now reports the model its
  agent restored, and the summary shows that model instead of the `default` alias. The ACP start
  records the model it selects in place, so the thread controls never name the model the session
  left. A model chosen while a session is still starting is not reconciled away by that session's
  older snapshot, and the New thread dialog adopts the host's live model list whenever it arrives —
  reselecting the same host, or a catalogue that arrives after the dialog opened, never falls back
  to a static model the installed CLI may have dropped, and a model typed by hand is never
  overwritten.
- **Thread settings apply to the turn they promise, and never to one already running.** A
  settings change that lands while a session is still starting no longer detaches the session that
  startup is about to write the turn to (which left the thread Working forever), and only a turn
  whose prompt has not reached the harness yet may be moved onto a fresh session; a change made
  during a running turn applies from the turn after it. Two settings changed at the same time each
  keep their own field instead of one request writing back the whole snapshot it read before the
  other landed. Choosing an ACP agent's `default` model on a live session is deferred to the next
  session instead of being refused. A Claude Code process that exits while a control request is
  outstanding now fails that request at once instead of after a 10-second timeout. The composer's
  model list uses the session's own catalogue (Codex's `model/list`, a configured agent's advertised
  models) once a session has run, falling back to the static list only before the first session, and
  Codex offers `minimal` reasoning effort. An ACP agent that advertises no mode catalogue shows
  "Mode is managed by the agent" instead of all four permission modes, and a start refuses a
  non-default mode it cannot map.
- **The PR watch keeps its promises under races and failures.** A re-watch read
  that a poll overtakes is discarded instead of moving the baseline backwards,
  and a poll read that a re-watch overtakes is discarded the same way, so the
  same failed check is not reported twice in either order. Watch reads no
  longer add the PR to the 30-second checks poller that the PR screens use, so
  ending a watch or a link leaves no poll behind. A watch request during a
  rate-limit pause spends no requests and takes its baseline from the first
  poll after the pause. A wake the daemon cannot deliver keeps the watcher's
  previous snapshot so the next poll retries the same events (bounded like
  failed reads), and both a terminal watch and one at the ten-comment cap wait
  for their wake instead of ending unheard. The thread header badge re-reads
  when the daemon's watch state can have changed — when a wake message is
  appended and when its panel opens — so a merged PR stops offering "Stop
  watching", and messages replayed on reconnect do not each cost a forge read.
- **The Tailscale CLI tests no longer fail for a reason the code cannot fix.** The prompt-CLI test
  wrote a fresh executable script and spawned it immediately; when another test thread forked while
  that write handle was open, the child inherited it and the spawn failed with `Text file busy`
  (os error 26), which `run_until` reports as "off" - the exact CI failure. The tests now run
  `/bin/sh`, which is never freshly written, and each test that expects "off" first proves the shell
  really runs, so a spawn failure cannot pass for the command's result. The shell preflight has its
  own budget, measured separately from the command under test, and the hung-CLI test writes a marker
  as its first act and asserts it: a spent deadline that never started the hung command now fails
  instead of passing for a timeout nobody exercised. The sibling harness fixtures no longer leave a
  write handle to chance: a short-lived `/bin/sh` writes the fake CLI and exits, and the finished
  file is renamed into place, so the test process itself never holds a descriptor another thread's
  fork can inherit. A stress run of the same sequence (writers plus `/bin/sh` churn) produced no
  `Text file busy` failures in 20,000 install-and-spawn rounds; the same sequence failed 254 of
  20,000 before the change (264 in a second run).
- **A reply that reached the pipe before the agent exited is kept, and a queued message survives a
  usage limit even when the queue was already promoted.** A real ACP peer ends its turn with an
  ordinary newline-terminated frame and can exit at once, so the process may be reaped before the
  stdout reader has drained the reply already in the pipe. The supervisor now waits, bounded, for
  the reader to reach EOF before it fails anything still pending, so an answered turn settles
  Completed instead of Failed with the exit report. Reply resolution no longer depends on the event
  consumer keeping up either: the reader publishes into its own queue that one forwarder hands to
  the session in order, so a full 16-slot queue can no longer park the reader before it reads the
  reply behind it (a long `session/load` replay still drains its own history before the next turn).
  That queue is bounded at four session queues: when it is full, the newest ordinary update is
  dropped and counted, so a chatty agent cannot grow memory. A permission request, a transport close,
  stdout's close and the exit report take a reserved path instead: they are never counted or dropped,
  and once more than one session queue of updates is waiting they get the next slot, so a flood of
  text cannot hide the event that decides what a request means or that the agent is gone. While the
  consumer keeps up they stay in the reader's order, so a close never arrives before the words read
  ahead of it. A turn's settlement also stays in order behind the updates the reader had already
  taken. The adapter also tags every update with the prompt generation it follows and
  publishes a turn's settlement behind the updates the reader had already taken. The daemon holds a
  promoted turn's text, tools, plan and usage from the moment its prompt goes on the wire until that
  write returns `Ok`: an adapter that has not accepted the prompt yet can still be draining the
  finished turn's output, so those events are neither published nor journaled, and a write that
  fails drops them instead of keeping them under a turn that never ran. The hold is released in the
  adapter's order when the write is accepted, so the turn's own first words are not lost. Text a
  peer writes after it has read the next prompt still cannot be attributed - ACP carries no turn id -
  so the hold covers exactly everything up to the acceptance. When the queued message was already
  promoted onto a transport that then closed, it is handed back to the exit report's pause instead of
  being failed: its write failed, so nothing it collected can be its own, and the parked prompt is
  delivered after the reset. A turn that already did work is never run a second time: a completed
  tool call (which leaves the running-tool map empty) counts as proof the prompt reached the
  provider, so that turn settles as limited with its own tool card and output instead of being
  replayed. A death observed before the promotion is still held back by a bounded wait for the exit
  report; in the ordering where the completion is published first, the requeue is what guarantees
  the outcome. A pause that binds to a promoted turn saves that prompt on the thread row only once
  the ledger has really armed it, and a claim that finds the turn parked refuses to write it, so a limit landing between
  promotion and claim survives a restart and is delivered once after the reset; a ledger that refuses
  the schedule leaves no prompt on the row for a restart to deliver again. If no schedule can be
  armed (no usage meter, or a ledger that refuses it), the requeued turn fails honestly instead of
  staying pending with no delivery owner, and the thread reads as rate-limited, never as
  `Scheduled` with nothing to fire. A requeue also keeps the dead session's exit status and stderr
  row instead of clearing it.
- **A re-watched PR and a reconnected badge keep their promises.** Every watcher
  generation now comes from one service-wide counter that never reuses a value,
  so a poll that started before an unwatch and re-watch cannot hand its older
  body to the watcher that replaced it. A delivered merge, close or
  tenth-comment wake removes only the watcher it was computed for: if the
  thread started watching again while that message was being delivered, the new
  watch stays. After a reconnect the linked-PR badge re-reads once per replayed
  burst, so a PR that merged while the client was offline stops showing "Open"
  without waiting for the panel to open.
- **Git actions only ever move the branch you asked for, and drafts are real on every forge.**
  Push names one refspec (`refs/heads/<branch>` to the same name on the forge remote) instead of
  relying on `push.default` or `remote.<name>.push`, so a `matching` configuration cannot push other
  branches and a `+` refspec in config cannot force an update; "behind or ahead" is measured against
  `origin/<branch>`, not whatever upstream a branch happens to track, and a fork layout pushes to the
  fork. Opening a draft on Gitea or Forgejo now prefixes the title with `WIP:` because those APIs
  have no draft field, and no forge gets a doubled `Draft:` or `WIP:` prefix. The pull request dialog
  waits as long as the push may take and cannot submit twice. A commit that names paths records only
  those paths and leaves other staged work staged. An older app that saves a title model no longer
  turns the Commit & PR mode off. A rebase, merge or cherry-pick is named in the error before the
  detached `HEAD` it causes, and a credential-shaped string in push output is redacted. The panel
  keeps a typed message when generation is off, shows the live branch (a detached `HEAD` disables
  push and the PR dialog with the reason), never guesses `main` as a base, and shows its busy state on
  **Commit & push** for the whole commit-and-push. Settings → Source control describes the message
  model in its footer and says plainly that non-Claude threads need a configured text generator.
- **A PR watch hears every change since it started, exactly once.** The GitHub
  change probe is gone: the watch reads the PR detail conditionally, so a `304`
  is built from the body the account's shared cache returns rather than from the
  watch's own earlier snapshot, and a merge, close or conflict another reader
  already saw is no longer reported late or missed. Each thread keeps its own
  baseline and seen-failure state, so a second watch or a re-watch neither hides
  a change another watcher has not seen nor repeats one it has, a new watch
  starts from the current state, and it never clears a rate-limit pause the
  poller recorded. A failure wake now fires only for a failing check that was not
  already failing, so a re-run that passed is not reported as a new failure. When
  the forge does not report the account's login, the watch skips comment and
  review triggers rather than treating every author as someone else, and the
  linked-items panel says so. A damaged `linked_at` in the link journal is an
  error instead of being replaced with the current time. The badge popover is
  keyboard reachable, and its rows say what they do instead of showing an icon
  for the opposite action.

- **Composer drafts survive recall, two tabs and a full quota.** Sending a recalled prompt puts the
  draft that recall preserved back in the box instead of clearing it. Two tabs editing the same
  thread inside one 300 ms window no longer lose the later edit, and a tab whose queued write was
  superseded shows the text that actually won. A storage event carrying a corrupt value re-reads
  the key first, so a value repaired in the meantime is kept, and a write that failed on a full
  quota no longer counts against the 200-draft cap and evicts stored drafts. Drafts for a
  `local-thread-*` id stay in memory and move to the real thread id once the daemon returns it;
  temporary ids are never persisted. The empty-inbox Start a thread box now keeps its draft per
  host and folder like the New thread dialog. The file walk checks its ignore list before the
  depth cap, so a deep `node_modules` no longer marks the index truncated; `@` stays closed after
  a decomposed accent (`e` + U+0301); switching workspaces clears the picker instead of flashing
  the previous workspace's files or claiming the new one is empty before its index lands; and the
  flip-below fallback uses the caret line's measured height. The dead single-blob draft migration
  was removed: no released build wrote that format.
- **The empty-inbox Start box starts from what is on screen.** Pressing Start — clicked or
  reached with Tab — blurs the folder field on the way to the button, and that blur used to swap
  the task box to the typed folder's draft before the send; Start now carries the visible task to
  the folder as typed. The box also restores the folder and draft last used on the host, instead
  of always opening on `~/code` and ignoring them. The automatic first-project choice is not
  remembered as the last folder; it shows that folder's draft, and text typed while it is on
  screen is saved with that folder, so a reload brings back the same folder and text. It is
  skipped while the box holds unsent text, and a typed folder that already has its own saved
  draft keeps it: the keystroke stays with the slot it was typed in rather than replacing text
  nobody is looking at. When Start loses focus without being pressed and focus leaves the
  composer, the folder field's skipped commit runs then, so the box shows that folder's draft
  before the next keystroke can land anywhere. The box is blanked only when the draft it sent was
  actually cleared and it is still on the slot it sent, so a Start that finishes after the person
  switched folders leaves that folder's draft in the box and in storage; it re-reads the slot when
  the New thread dialog closes or another tab writes it, so the dialog's text appears in the inbox.
  In the composer, `↵` in the `/` menu runs the command ranked for the query on screen
  (and keeps the text when nothing matches), never a row left from a menu whose trigger the same
  keystroke replaced, and a recall made while a recalled prompt is sending
  stays on screen instead of the draft recall had preserved. A thread whose create fails no longer
  leaves the text typed into its optimistic box behind under the temporary id.
- **Preview follow-ups from the HTML preview review.** A descriptorless `srcset` candidate followed
  by a comma (`data:image/png;base64,AAAA, local.png 2x`) no longer swallows the candidate after
  it: the trailing comma ends the candidate, so the workspace image is inlined or removed and
  reported like any other. The 10 MB budget now covers the whole serialized document — the trusted
  doctype and CSP prefix, the bridge script and the markup generated for each insertion are
  reserved — so a near-cap page renders with its excess asset skipped instead of failing as too
  large. The "update Android System WebView" notice appears only when the shell reports an Android
  WebView without document-start script support; on desktop, or on Android where that feature
  exists, a missing bridge message (for example an unterminated comment that swallowed the bridge)
  shows a plain "preview scripts didn’t start" note with Reload. The ready-message deadline scales
  with the prepared document instead of a fixed 800 ms, so a big page is not mistaken for blocked
  scripts. Every held workspace watch is re-sent on a replacement connection by workspace, so
  retaining a second workspace before the new client is ready no longer leaves the first
  unwatched. The frame-limit choice is covered by end-to-end reads over a plain and a secure
  connection, and the shell now returns only the frame URL so one preview token — the one already
  written into the document — is used everywhere.
- **Preview: the budget, Reload and the Android capability check are honest.** The 10 MB budget
  now charges the bytes the builder itself adds: the `target`/`rel` an anchor rewrite appends, the
  quotes and escaping that a rewritten `img`, `source`, `video`, `svg` or style-bearing tag grows
  by, and the backslash that escaping `</script` or `</style` adds to inlined text. A near-cap page
  with many rewritten links renders with its excess image skipped instead of failing as too large;
  an anchor that cannot pay for its rewrite keeps its original tag. Reload re-asks the shell for the
  preview WebView's capabilities — including from the Android notice, which now has its own Reload
  — so updating System WebView clears the notice without closing the tab. The capability command
  runs off the main thread, and a failed or unparseable WebView version is reported as unknown and
  treated as unsupported, showing the generic "preview scripts didn’t start" note with Reload
  instead of defaulting to supported. See [docs/ux.md](docs/ux.md).
- **A first connect no longer calls its own handshake history.** `since_seq: null` makes the daemon
  send no replay, so every frame held between `hello` and the subscribe answer is live news, even
  when its sequence already sits at or below the journal head named by the answer. Only a resumed
  subscribe tags frames at or below its boundary as replay. This is what lets an approval that
  arrives during a first handshake notify.
- **A pinned or hand-moved thread still alerts when its turn fails.** `placed_by_user` is also set
  for the Working pin and a manual move to Working. Those are per-thread overrides, not a finished
  turn, so only a Done transition is attributed to the placement; a failure that raises its hand
  while the thread is pinned now notifies as it should.
- **Refreshing the thread list keeps live completions.** The notifier keeps the last streamed
  summary per thread apart from list snapshots. A routine refresh only adds threads the stream has
  not named yet; it never replaces a baseline or forgets a `turn_completed` that arrived live, so a
  list that answers Done before the completion and its upsert cannot swallow the finish.
  Remembered completions and baselines are re-seeded only when the stream recovers (a reconnect),
  because that is when the list may be healing a real gap.
- **An older relay's ignored `?linked=true` no longer becomes this device's linked presence.** The
  relay now marks a filtered machine list with `x-h4rn-machine-filter: linked` (and exposes it to
  browsers). A client that sees no marker treats the list as unsupported, keeps its presence rows
  off and says the relay is too old, instead of announcing hosts on the whole account.
- **Presence identifies the connected machine by the peer the transport reached.** A relay link
  names the machine pairing recorded from `machine_devices`; a direct connection uses the connect
  machine the daemon reports in `hello` from its own signed-in session. Neither comes from the
  account's device rows, where a paired client's `machine_id` is empty and a daemon device's row
  names a different thing. A verified machine keeps all of its hosts online, including WSL
  siblings, because the transport reached the machine rather than one row of it. The local host's
  name is only the fallback when there is no id; it matches one host, so working in one WSL
  distribution does not mark the Windows host or its siblings online. A name shared by two
  machines matches neither, so a new same-named machine cannot announce the one in use offline.
- **A failed tab move leaves no placement note.** The notifier hears about a manual move into Done
  or Settled only after the daemon accepts it, so a rejected move cannot spend a remembered live
  completion.
- **Open at login shows the real OS state.** The row draws no switch while the plugin is being read
  (instead of a make-believe off), and stays hidden when the shell has no autostart plugin rather
  than offering a switch that cannot work.
- **A browser `Notification` constructor that throws is reported once.** Permission revoked between
  the check and the constructor, or a context that forbids notifications, no longer throws again
  for every message: the channel logs the first failure and then degrades quietly for the page.
  Settings reads the same page state, so it can say notifications stopped working on this page
  (reload to try again) without confusing that with an OS permission.
- **The Host offline row waits for the first account answer.** Settings no longer flashes the "no
  linked host" explanation before the relay has had a chance to say what is linked.
- **A failed linked-presence fetch no longer reads as an old relay.** Whether the relay honours
  `?linked=true` is tri-state: unknown until a successful answer arrives, `false` only when that
  answer lacks the filter marker. A failed first fetch keeps it unknown, so Settings hides the row
  instead of telling the person to update a relay that may be current, and a cached sign-in that is
  `signed_in` before `refresh()` answers waits the same way.
- **A machine this client is connected to is never announced offline.** The connected machine is
  the peer the live transport actually reached: the relay link's machine, or the connect machine
  the daemon reports in `hello` from its own signed-in session on a direct connection. The
  account's device rows are not consulted, because a paired client has no machine id there and a
  daemon device's row names a different thing. The local host's name is only the fallback, and a
  name shared by two machines now leaves those hosts out of presence entirely instead of feeding
  them the relay's answer. Adding a second same-named machine can no longer raise an offline alert
  for the first, and every host of a verified machine — including WSL siblings — stays online
  while the transport is up.
- **A live frame queued just behind the subscribe reply still notifies.** The journal head named by
  the reply is kept as an acknowledged cursor for the next reconnect, separate from the position of
  frames actually delivered. A first-connect approval that reaches the socket after the reply, with
  a sequence the reply's head already covers, is therefore delivered as live news instead of being
  deduplicated away.
- **A failure alerts even when the thread was already moved to Done.** The turn's own live
  completion is the signal, independently of where the tab sits: a person who moves a running
  thread to Done still hears that its turn failed, while a manual move with no new failure stays
  quiet. One failure is announced once per turn, whether it arrives from the outcome or from the
  summary's raised hand. Token and turn limits stay with the summary, since the daemon may resume
  from them.
- **A list refresh that answers Done first no longer swallows a live completion.** The last
  streamed summary is kept per thread independently of list snapshots. Whether the list replies
  before or after the completion, its Done placement is never adopted as the baseline for a thread
  the stream has spoken about, so the live upsert that follows is still the endpoint the finish
  announces.
- **A failure waits for its thread to be known.** A live failed turn can arrive before any summary
  names the thread. The alert now waits for that summary, keeping the failed turn's outcome by turn
  id: a queued turn that starts in the meantime does not erase it. An ordinary thread still gets
  its failure, while a subagent child's pending failure is cancelled because the parent mirrors
  the moment. A first-connect child failure therefore stays quiet instead of alerting on a thread
  the notifier cannot yet place.
- **A browser notification failure is one page state.** Settings and delivery share the fact that
  the `Notification` constructor stopped working on this page. Settings says notifications stopped
  working there and need a reload, separately from the OS permission, which is still reported as it
  is; before, only the notifier knew, and Settings could only offer the OS-blocked sentence.
- **Open at login keeps real read failures visible.** The toggle is hidden only for the confirmed
  missing-plugin or missing-command answer from the shell. Any other failure — including a
  capability denial from a plugin that is installed — keeps the row and shows an error line
  instead of pretending the setting does not exist. The reason belongs to that row, so changing
  the tray setting or anything else on the page does not erase it.
- **An oversized filesystem response no longer closes the paired session.** JSON escaping of
  quotes, backslashes, newlines and control characters can push a 400 KiB text read past the
  secure transport's 512 KiB frame, and the transport answers an oversized frame with
  `FrameTooLarge` by closing the session. The daemon now measures the serialized response with
  headroom and answers a text or base64 read with a typed `content_omitted: too_large` /
  `bytes_omitted: too_large` and an honest hint, and the response path keeps a last guard so no
  filesystem response can exceed one frame. Copy and docs say 400 KiB is a read bound rather than
  a promise that every file under it opens, and the too-large message uses one unit (KiB) for the
  file size and the limit.
- **Workspace watches survive a connection restart.** Signing in, pairing, switching machines or
  recovering from offline replaces the transport client, and the file watcher's held watches were
  tied to the old client instance, so live reload stopped until a view was remounted. The counts
  and owners now live outside the client, and every held watch is re-sent on the connection that
  replaces it, after a transport reconnect and on a new client's first ready notice.
- **The mobile shell's Rust tests are compiled on the host.** The Android and iOS CI jobs build
  for their own targets, so the mobile crate's unit tests (the preview store and its origin) were
  never compiled. The Linux job that already installs Tauri's WebKitGTK packages now runs
  `cargo check --tests` for `apps/mobile/src-tauri`.
- **A parked prompt survives a shutdown while the ledger cannot be read.** If the startup ledger
  read failed after a parked prompt was restored, a shutdown used to settle that turn and erase the
  saved prompt while the resume schedule stayed armed; the next launch then ran the policy's
  "continue" instead of the person's message. The parked turn is now left alone until the schedule
  is resolved.
- **A resume retry no longer undoes a newer schedule.** A retry that reads the ledger can race a
  newer limit that arms another schedule, and it used to write the older reset and schedule id back
  onto the thread. The retry now keeps the newer pause (and the prompt it holds) and only fills in
  details for the schedule the thread already names.
- **A ledger that stays unreadable no longer rewrites the inbox every tick.** Retries now wait from
  one second up to a minute, and only threads whose restored state actually changed are
  re-published, instead of clearing every thread's cached summary on every tick.
- **A prompt is no longer left waiting behind a resume the person cancelled.** If the startup
  ledger read failed and its retry had already read a resume schedule when the person cancelled it,
  the retry used to attach that cancelled schedule to the restored prompt; nothing could ever fire
  it. The retry now re-reads that one schedule before attaching it, and releases the prompt to run
  instead when it is gone.
- **A system clock change no longer delays a resume retry.** Retry waits are measured on a
  monotonic clock, so moving the clock backwards cannot stretch the wait past its cap.
- **A resume check that cannot re-read one schedule is retried.** The startup pass could read the
  schedule list and the limit history, fail on the per-schedule re-check that confirms a restored
  row, and then never retry: the thread's pause stayed unnamed and a restored prompt could wait
  forever. Any partial failure now leaves the resync unresolved, so the ticker runs the pass again
  and the due schedule fires.
- **A cancelled pause no longer releases a prompt it never held.** Cancelling an idle thread's
  schedule after a retry had snapshotted it — or sending while an older pause is armed that does not
  hold the pending turn — used to record a release and flip the thread to Thinking even though
  nothing was parked. Now only a prompt that is really parked (a pending turn whose prompt is saved
  on the thread row) is released; a stale pause is dropped without a release or a phase change, and
  a cancel that already released the prompt is not released twice.
- **Forge errors never repeat the account's own credential.** Every client now
  redacts the configured token verbatim before an error leaves it or reaches a
  log, including a token quoted in a JSON error body, a percent-encoded copy
  from a URL and the Base64 Basic credential Azure DevOps can echo back. Every
  copy goes, whichever hex case and whichever spelling: the plain value plus an
  uppercase-escaped and a lowercase-escaped copy in one body all disappear.
  Nested JSON error details are redacted as decoded strings before they are
  serialized into the message, so a credential containing a quote cannot
  survive as a JSON-escaped value, and the escaped spelling is matched as a
  second layer. Object keys are redacted as well as values (a credential used
  as a key cannot slip through a serialized `error` object), and a JSON body
  with no `message`, `error`, `detail` or `errors` key is serialized from the
  redacted decoded value instead of echoing the raw body, so no `\/` or
  `\u00xx` spelling survives. Matching normalizes the text once and reads every
  match offset from that normalised copy, so a body that echoes a credential
  thousands of times is redacted in one linear pass. The configured secret is
  removed before the pattern-based token pass, which stays as a second layer.
- **Azure DevOps reply ids are parsed before they are used.** A reply's thread
  id goes into the request URL and its comment id into the JSON body; both must
  be positive 32-bit integers, and an invalid one returns `InvalidRequest`
  without sending a request.
- **A `DefaultCollection` Azure DevOps checkout finds its account in the UI.**
  Account matching applies the daemon's alias rule
  (`https://acme.visualstudio.com` equals
  `https://acme.visualstudio.com/DefaultCollection`, host case, the segment's
  letter case and trailing slashes ignored), so Open PR works whichever
  spelling the remote uses.
- **GitLab merge requests report their real line totals.** `get_pr` already
  summed the `/changes` diffs; it now counts added and removed lines by their
  first character inside a hunk (content that itself begins `+++` or `---`
  counts) and marks the totals known so the UI and the agent handoff show
  them. A truncated answer (`overflow: true`, or a change marked `collapsed` or
  `too_large`) leaves them unknown instead of reporting a partial sum.
- **Every pull-request tab recovers on its own.** The detail, comments, checks,
  diff and commits load independently, and each tab has its own loading state
  and Retry: a quiet “Loading…” fills the tab while its request is in flight
  instead of a “did not load” or empty state, Retry re-runs only that tab's
  request while the PR header, checks and the rest of the detail stay on
  screen, and a live checks update clears a failed Checks load. A live update
  also wins over a checks request that was already in flight: its late answer,
  success or failure, describes an older snapshot and no longer replaces what
  the daemon pushed. An error still stays inside its own tab.
- **Azure DevOps missing collections are errors, not empty answers.** A 200
  without `threads` or `changeEntries` is a parse error, while an explicit
  empty list stays empty; a missing `changeEntries` leaves
  `changed_files_known` false rather than reporting zero files.
- **Azure DevOps links and diff totals are correct for unusual names and
  content.** The browser-URL fallback percent-encodes the project and
  repository segments (a repository named `release%20notes` no longer links to
  `release notes`), and a file's totals count added or removed lines whose
  content starts `+++` or `---` instead of mistaking them for file headers.
- **A literal `%` in an Azure DevOps project or repository name works.** Azure
  returns decoded names, so `release%20notes` is a name, not an escaped space.
  Shared validation accepts it and every segment is percent-encoded before it
  enters a request or browser URL (`%` becomes `%25`), while the checks that
  keep a segment a single safe path step still reject separators and `.`/`..`.
- **The composer's phone sheet stays inside the keyboard, and the panel sheet takes focus.** The
  `@` sheet now ends at whichever comes first, the composer or the visible viewport's bottom, so no
  row hides under an open keyboard when the composer itself sits below it. When less than the header
  plus one row fits above, it covers the composer inside the visible viewport instead of collapsing
  to nothing. Opening Changes or Subagents with `/` moves focus into the panel sheet, so typing no
  longer edits the draft behind it. `@` no longer opens after a Unicode letter (`café@src`), and a
  workspace with exactly 3,000 files is a complete index, not a truncated one.
- **The composer's menus trust the caret and the query in front of them.** `↵` and `Tab` accept
  only rows ranked for the text on screen, so pressing Enter inside the old 120 ms debounce cannot
  insert the row for the previous query. The `@` button separates a new mention from the words on
  both sides of the caret (`Review this part` → `Review @README.md this part`) and completion never
  glues a path to the following word. A quoted mention no longer pairs its opening quote with one
  on another line, so a finished `@"…"` on the next line stays inert. The popover anchors to the
  actual selection (not the end of the token) and copies the textarea's computed styles under CSS
  property names, so its mirror measures the caret correctly in Chromium. The file walk spends its
  250-listing budget on failed folders too, limits each batch to what remains, and stops adding
  files the moment the 3,000 cap is hit; the partial-index note sits beside the footer instead of
  scrolling away with the list. On a phone the sheet is sized to the visible space above the
  composer (visual viewport and keyboard included), keeping its header and Done on screen with an
  expanded composer, and a `/` command that opens a dialog or sheet keeps focus there instead of
  pulling it back into the textarea.
- **The composer's `@` picker and `/` menu hold up on a phone and at the edges.** The phone file
  sheet no longer inherits the shared bottom-sheet placement and covers the composer: it sits above
  the input it belongs to. On a phone, `/` → Changes and Subagents open the phone panel sheet on the
  matching segment instead of calling the desktop panel action, like the header button does. `@`
  now opens after any non-identifier character (`(`, `[`, `{`, a quote, a comma), not only after
  whitespace, and the `@` button inserts a separating space on either side of the caret, so the
  menu no longer dies on the next keystroke. The caret popover measures itself and
  clamps to the viewport, moving left and opening below the caret when there is no room above.
  The file index now reports the caps it hit — a truncated listing, 3,000 files, 250 folders or
  8 levels — and the menu says “Showing the first 3,000 files.” instead of a bare “No files match”.
  Every option has an id, `↑`/`↓` scroll the highlighted row into view and the textarea names it
  with `aria-activedescendant`.
- **The composer's file list tells the truth when the workspace cannot be fully read.** A folder
  that fails to list makes the index partial, so the quiet “Showing the first … files.” note beside
  the footer appears instead of claiming no other file exists; if the workspace root itself fails,
  the picker shows that error rather than “No files in …”. The phone `/` menu no longer offers
  “Open the IDE” (the phone shell has no IDE view to open), the phone sheet's Done button dismisses
  it exactly like `Esc`, and `⇧↵` inserts a newline even while the `@` list is open. A quoted
  mention (`@"path with space"`) is scanned as one token on one line: the picker keeps filtering
  while its closing quote is missing, does nothing inside a finished mention, and completes it
  without doubling or dropping the quotes. The caret popover also follows the caret line when the
  textarea has scrolled.
- **Large Azure DevOps pull requests load within the page deadline.** Azure needs file contents to
  build a diff, so a 30-file page could not finish inside the UI's ten-second budget once the
  account's local request allowance ran out. A diff page now holds at most eight files (eighteen
  requests in the worst case), a file whose change keeps its bytes (a pure rename, branch or merge)
  downloads nothing, and the existing per-request cache still avoids a duplicate blob.
- **Azure DevOps checks no longer flip to passing.** Statuses are filtered to PR-level rows and the
  current iteration only, so a failure from an earlier push cannot outlive the code it tested.
  Policy evaluations stay distinct by evaluation and configuration id: an approved optional policy
  and a rejected blocking policy that share a display name are two rows, and the rejection keeps the
  PR failing. A policy outcome still wins over a status of the same name.
- **Azure DevOps no longer reads `undelete` as a deletion.** Change types are matched as exact
  comma-separated tokens (`rename, edit`, `undelete`, `sourceRename`) instead of by substring.
- **Azure DevOps diffs keep line endings and empty files.** The local patch builder keeps `\r\n`
  terminators, emits `\ No newline at end of file` where one side lacks a final newline, and uses
  `/dev/null` only when the path is absent on that side, so an existing empty file is not shown as
  new or deleted.
- **Bitbucket Cloud lists no longer repeat rows.** Author, reviewer and source-branch filters use
  the documented server-side `q=` language (reviewer rows also request `fields=+values.reviewers`),
  so page two resumes where page one ended. The GitHub-style `x-oauth-scopes` header is no
  longer read as a Bitbucket scope list, and a diffstat whose five-page cap is reached reports the size as
  unknown instead of as a partial total.
- **Remote detection keeps other forges' URL handling.** Azure DevOps keeps the strict
  percent-decoded path rules; every other forge still resolves a path with an empty segment the way
  it always has (`https://gitlab.com/team//repo.git`). Azure base-URL errors never echo the input,
  which can carry a secret in its path.
- **Bitbucket Cloud recognises its SSH host.** `git@ssh.bitbucket.org:workspace/repo.git` and
  `ssh://git@ssh.bitbucket.org/workspace/repo.git` resolve to the `https://bitbucket.org` account,
  like the web and `bitbucket.org` SSH forms.
- **Azure DevOps checks fail closed.** If the iteration, status or policy-evaluation request fails,
  the checks request now fails instead of reporting the green rows as passing; a branch gate that
  could not be read is never a pass.
- **Azure DevOps fork pull requests read the source side from the fork.** When a PR names a
  `forkSource.repository`, the old side comes from the target repository and the new side from the
  fork, where the source commit lives. A 404 for a side the change says exists is an error rather
  than a silently invented addition or deletion.
- **Azure DevOps checks need both collections.** A 200 whose status or policy body lacks its `value`
  collection is now an error instead of zero rows; a valid `value: []` stays empty. Half an answer
  can no longer read as a pass.
- **Bitbucket Cloud follows the cursor it is given.** The PR list carries the same-origin `next` URL
  forward as the next page instead of rebuilding `page=N`, so an opaque cursor keeps its filters and
  pages do not repeat. Learned cursors are sparse and bounded (the 64 most recent), and a page with
  no learned cursor falls back to the documented `page` parameter, so a distant page never
  allocates memory in proportion to its number.
- **Bitbucket Cloud Mine and Review requested match the account uuid.** The filters use
  `author.uuid` and `reviewers.uuid` from `/user`, as Bitbucket documents, instead of a nickname or
  display name that two people can share (or that a token may not report). Workspace, project and
  repository access tokens have no user identity, so those two filters say “unsupported” with that
  reason instead of showing an empty list.
- **Bitbucket Cloud reads `inline.outdated`.** A line-anchored thread is outdated when its root
  comment says so; a file-level comment stays not outdated.
- **Redaction finds tokens wherever they appear.** Atlassian and GitHub/GitLab token values are
  removed next to quotes, colons, commas and brackets, including inside a quoted JSON error body,
  instead of only at the start of a whitespace-delimited word.
- **Azure DevOps pull requests keep a browser URL.** A response without `_links.web` falls back to
  `<repository.webUrl>/pullrequest/<id>`, or to the base URL, project and repository, instead of an
  empty link.
- **Line counts are only shown when the forge reports them.** Azure DevOps (and GitLab) report no
  PR-wide totals, so `line_counts_known` is `false` and the agent handoff says the counts are
  unknown instead of “+0 −0”. Bitbucket sets it once the diffstat was read in full.
- **A legacy `DefaultCollection` remote finds its Azure DevOps account.** An account added as
  `https://<org>.visualstudio.com` matches a remote that spells
  `https://<org>.visualstudio.com/DefaultCollection`, and the other way round.
- **Bitbucket Cloud base-URL errors never repeat the input.** A non-root path is rejected with the
  same fixed wording Azure DevOps uses, and `ATATT…` Atlassian API tokens are redacted from error
  messages like `ATBB…` already was.
- **Honest Bitbucket Cloud counts and anchors.** A failed or capped diffstat leaves the changed-file
  count and line counts unknown (`changed_files_known` and `line_counts_known` false) so the PR
  header shows “—” instead of “0”; the comments page reports `has_more` only when the last page it
  read linked a `next` page.
- **The Bitbucket token form says Pull requests: read and write**, matching the documented scopes,
  and with several detected Bitbucket checkouts it lets the person choose which repository proves
  an access token (the first is used by default).

- **A second launch hands its deep link to the running app.** Launching h4rn while it is already
  running (including from a hidden tray) used to focus the window and drop the `h4rn://` link that
  started the second process. The single-instance plugin now has its `deep-link` feature, so it
  passes the second launch's arguments to the deep-link plugin before the focus callback, and the
  running window opens what the link names.
- **The machine this client is connected to is always online.** The relay mints its own machine and
  host ids when a machine registers, so its `MachineHost.id` never equals the id `hello` reports
  for the local host. The notifier compared those ids, decided its own working host was offline,
  and could announce a host the person was actively using. It now names the connected machine from
  the peer the transport actually reached — the relay link's machine, or the connect machine the
  daemon reports in `hello` from its own signed-in session on a direct connection — and falls back
  to `hello`'s local host name through the daemon's host list (the same pairing the sidebar uses,
  case ignored) only when there is no such id and exactly one linked machine owns a host of that
  name. A verified machine keeps all of its hosts online, including WSL siblings; two machines
  sharing a name match neither, so neither can mask the other or be announced offline while the
  transport is talking to it.
- **A first-connect subscribe is replay-free whatever its success answer looks like.** An older
  daemon may answer a live-edge subscribe with the protocol's plain `{type:"ack"}`, or with
  `subscribed` and no boundary. Both are successful acceptances of `since_seq: null`, so the
  approvals that arrived while the handshake was open are delivered as live news instead of being
  tagged as history; a resumed subscribe that gets the same answer still treats its held frames as
  replay.
- **Another client's manual move to Done is not reported as "Finished".** The daemon's thread
  summary now says whether the person placed the thread in its tab (`placed_by_user`). A notifier
  that still remembers a live completion for the thread stays quiet when the Done summary it sees
  came from a manual move on another client, and a local pin toggle no longer counts as a
  placement.
- **Approvals and finished turns no longer wait for the host-presence request.** A due approval or
  finished-turn message is delivered as soon as the OS permission allows; the relay request that
  re-checks a due host-offline alert runs beside it. A queued approval is taken out of the queue
  only at delivery, so an answer that arrives while the message is waiting cancels it. If the
  permission query throws, nothing is dropped: the same drafts are re-queued and retried after a
  short delay instead of a tight timer loop.
- **The linked-machine presence request is signed over its pathname.** Connect verifies the DPoP
  proof against `parts.uri.path()`, which excludes the query string, so signing
  `/v1/machines?linked=true` was rejected with 401 and took the whole account and machine refresh
  down with it. The client now signs `/v1/machines` and still fetches with `?linked=true`; a
  presence failure falls back to an empty linked list instead of aborting the refresh.
- **Local ACP agents: credentials stay off view-scoped devices.** `ListAcpAgents` now masks every
  environment value and omits the literal argument list, which can also carry a secret; the edit form
  reads the real configuration through a new admin-scoped `GetAcpAgent`. The last successful probe's
  models, modes and version are stored and returned to every device, so the model picker no longer
  needs to start the agent.
- **Local ACP agents: absolute, same-file resolution and fresh restarts.** A configured command now
  resolves to an absolute executable before the child starts (relative `PATH` entries are ignored;
  Windows `.bat`/`.cmd` are refused), so detection, `Check` and launch name the same file. A stored
  session id is replaced by a fresh session when the agent does not advertise `session/load`, and a
  thread's stored id is cleared when its agent's command line or effective working directory changes
  — in the thread itself, so a start that fails before the agent opens a session cannot leave the old
  id to be replayed by the next attempt. Plans are forwarded and shown in the turn as a quiet
  checklist, ACP v1 `configOptions` supply model and mode choices, and a view-only model picker shows
  the last probe or its error instead of spinning.
- **Local ACP agents are not a startable harness by themselves.** Host summaries and the new-thread,
  quick-start and delegate pickers no longer offer the bare `acp` kind, so no request can create an
  ACP thread or subagent without an instance; the daemon rejects both paths explicitly. Delegating
  from an ACP thread starts the child on the fallback harness's own default model instead of sending
  it a model id only the ACP agent knew.
- **Antigravity is unchanged by the shared ACP runtime.** Its non-`auto` default-model aliases are
  case-sensitive again, its "Antigravity stopped the turn" / "Antigravity is requesting permission"
  wording is back, its unknown-model error still says "the account offers", and the UTF-16 attachment
  test covers the shared prompt builder.
- **ACP agent form saves and checks line up with the daemon.** Instance ids are capped at 64
  characters, the form accepts either platform's absolute path form (`/dir`, `C:\dir`, UNC) because
  the daemon's platform decides, and the display-name cap counts characters rather than bytes on both
  sides. Explicitly added empty arguments are kept, and `Check` stays available and refreshes the
  list after the binary is installed. Any edit clears the cached version, a probe whose configuration
  changed while it ran stores nothing, a failed probe leaves the last good models in place, and an
  invalid entry loaded from `acp-agents.json` is dropped with one log line instead of failing at
  spawn.
- **Local ACP agents: a probe is one conversation, and editing one never loses work.** A probe whose
  configuration changed while it ran now reports a conflict instead of success, its answer is
  discarded by the client even if it arrives after the edit, and the picker never starts a second
  probe for an agent already handshaking. A probe also answers on its own task, so an agent that is
  slow to start no longer holds the connection: other requests on the same socket keep answering
  while the handshake runs. A probe answer that carries the agent's own error falls back to the
  daemon's cached models instead of emptying the picker. The delegate dialog's erased task, summary
  and budget survive a parent summary refresh while the dialog is open, and the environment editor
  stacks its name, value and Remove controls with 44 px targets on a phone. A plan step that is in
  progress now carries a neutral mark rather than the accent: blue still means the person is needed.
- **Local ACP agents: the method the agent actually advertises.** `session/resume` is no longer read
  as `session/load` support: an agent that advertises only resume is continued with
  `session/resume`, one that advertises load replays with `session/load`, and one that advertises
  neither starts a fresh session. A generic agent that advertises both a legacy `models`/`modes`
  catalogue and v1 `configOptions` now uses `configOptions` and selects with
  `session/set_config_option`; Antigravity keeps its own account catalogue. A command-line
  fingerprint is structured (JSON), so an edit from no arguments to one empty argument starts a new
  session instead of replaying the old id. Two entries with the same instance id in
  `acp-agents.json` keep the first and drop the second with a warning.
- **Local ACP agents: one answer for the catalogue, the resume and the process.** `Check` now reads
  the same v1 `configOptions` catalogue a thread start selects from, so the picker no longer offers
  legacy ids the agent would reject, and the model stays `default` until you choose one. The client
  waits 45 s for a probe, covering the daemon's 30 s exchange plus its shutdown grace, instead of
  abandoning it at the default 10 s. A `session/resume` or `session/load` reply that names no catalogue keeps the thread's saved
  model and mode and still sends the prompt. A load's replayed history is consumed before the new
  turn, so a conversation longer than the event queue no longer stalls the load and the old messages
  are not shown twice. A probe edited while it ran reports the same conflict whether it succeeded or
  failed, and the Check control stays busy until the original probe settles, so an edit cannot start
  a second one. The resume fingerprint and the launch now come from one resolved configuration, so
  an edit in between cannot hand the new command the old session id. The new-thread mode picker
  offers only modes h4rn can map onto the agent's advertised ids and otherwise says
  **Mode is managed by the agent**; a fixed working directory is shown before Start and becomes the
  thread's workspace for Files and Changes.
  presence failure keeps the last known linked list (empty before the first successful fetch)
  instead of aborting the refresh.
- **Closing a terminal ends its background jobs.** With job control, `sleep 1000 &` runs in its own
  process group, so the old group-only signal left it running after the shell was gone. Closing a
  terminal now sends SIGHUP to the shell's own process id, waits for it to forward the hangup, keeps
  the grace period after SIGTERM so cleanup handlers can run, and only then sends SIGKILL to the
  shell's process group after `child.kill()`. The hangup uses the spawned child's pid, not the
  foreground group cached from `tcgetpgrp`, which a job started during shell startup can own. A
  `nohup` job and one that moved itself into another session with `setsid` are left alone. The wait
  after the hangup ends as soon as the shell exits (bounded at two seconds), so a loaded machine no
  longer needs a fixed pause long enough for the shell to forward it; the two-second wait now runs
  only when a hangup was actually delivered, so closing a terminal on Windows does not pause. If the
  close wait reaped the shell, it captures the status under the child lock and the reader thread
  reads it under the same lock order, so the exit report keeps that status instead of reporting
  `-1` from a second failed wait. Closing no longer waits on the child handle: the hangup wait only
  tries its lock, so a shell that hit PTY EOF first (and is being reaped by the reader thread) can
  no longer hold the close past its two-second bound; TERM and KILL still go out on schedule, and
  the session keeps a cloned child handle that terminates the shell even while the reader thread
  holds the child in `wait()`, which is the only way a Windows console ends when its handle is
  busy. A handle kill that fails falls back to the same cloned handle, so a console that refuses
  the first termination still ends without the child lock. Once the close wait has reaped the shell,
  the pid-based fallbacks are skipped: a failed handle kill no longer sends SIGHUP or SIGKILL to a
  pid that may already belong to another process. The Windows handle kill still runs, because it
  owns a duplicated process handle rather than a bare pid. The Windows close test now also waits for
  the exit report, so a leaked blocked reader fails it instead of passing on the child handle alone.
  The close path drops both PTY ends: a killed Windows ConPTY client leaves the pseudoconsole open,
  which would keep the reader thread blocked and the session alive, so closing the master ends the
  console, closing the writer releases its input pipe, and together they release the reader and let
  the exit report run.
- **Only one attached viewer answers terminal queries.** Two xterm instances each answered device,
  cursor and colour queries, so a shell with a desktop and a phone attached received duplicate
  replies. The UI now marks xterm's own answers to just-asked queries with the output chunk that
  carried the query's first byte and the query's index among the queries that start in that chunk
  (one chunk can ask several questions and xterm answers each separately), and the daemon writes
  each distinct answer once from any viewer. Numbering by the starting chunk makes the identity a
  property of the output stream: two viewers name the same answer the same way even when their
  chunks arrive in a different order, and a viewer whose attach snapshot already contained the
  query's first byte simply never saw the query. The scanner that reads a sequence across a chunk
  boundary now follows xterm's own parser, so a parameterless DA1, DA2 or DECRQSS query keeps the
  chunk its ESC came in instead of falling back to the chunk that finished it: an empty parameter
  list is `[0]`, an empty sub-parameter is `-1`, values clamp the way xterm clamps them, ESC starts
  a new escape in every state (abandoning a CSI, ending an OSC, DCS or string first), and CAN or
  SUB cancels to ground, so a sequence xterm threw away is never matched against a later hook. The
  C0 controls xterm executes inside escape, CSI and DCS entry/parameter states (0x00–0x17, 0x19 and
  0x1c–0x1f) execute in place rather than becoming parameters, and DEL is ignored there. The scan
  runs on decoded code points with one streaming UTF-8 decoder, so the C1 controls work as they do
  in xterm (U+009B CSI, U+0090 DCS, U+009D OSC, U+0098/9E/9F strings, U+009C ST, and the rest back to
  ground) and a 0x9c byte inside a multi-byte character is never mistaken for ST, even when the
  character spans two chunks. A code point split inside its UTF-8 bytes is named by the chunk that
  carried its lead byte, so a C1 opener sent as `C2` | `9B` belongs to the chunk with `C2` even
  though xterm only decodes it when `9B` arrives, and invalid bytes are discarded exactly as xterm
  discards them (a wrong continuation drops the sequence; no replacement character is invented), so
  a malformed payload cannot stop a carried query from matching its hook. An OSC identifier counts
  only when every character before the first `;` is an ASCII digit, as xterm's `OscParser` aborts
  `10x`, ` 10` and `+10` at the first non-digit. `term.reset()` keeps xterm's escape parser and
  UTF-8 decoder, so a resync snapshot continues a sequence or character that was open before it: a
  chunk that ends inside `OSC 10;?` or `DCS $ q m` is completed by the snapshot's leading bytes and
  keeps the chunk that carried its first byte, the same identity the viewer that saw that chunk raw
  gives it. Only the record of queries from before the snapshot is cleared, and only when the
  snapshot chunk really starts parsing: xterm's write buffer parses later, so an output chunk applied
  just before the snapshot still names its answers by its own chunk, and ordinals for a chunk this
  viewer already parsed survive so a query completed by the snapshot keeps its place. A viewer that
  stalled before a
  chunk and caught up
  later therefore cannot write a second answer for a query another viewer answered in the meantime.
  Only exact duplicates and positions at or below the evicted high-water mark are dropped; a reply
  for an older position that has not been answered yet is accepted, because it can only rescue a
  pending query. A reply naming output the daemon never sent to that viewer is ignored, so a bogus
  position cannot suppress a real answer, and acceptance and the PTY write happen under one lock so
  accepted replies cannot reach the shell out of order. A viewer may claim any position it was sent
  as raw output, including a chunk it parsed raw before a later resync replaced it, and a position
  only inside its snapshot when no other attached viewer received that position raw: the daemon
  drops a claim only when it is snapshot-only for this viewer **and** another viewer has that
  position's bytes, so that viewer's answer is the one written. When every viewer saw the query only
  through a snapshot the claim still stands, because xterm completes it with the snapshot's bytes
  and the answer record keeps the reply count at one; the shell is never left without an answer. The
  byte-split fuzz also resyncs a lagging viewer mid-stream through a snapshot and checks that every
  answer after it keeps the identity the viewer that kept up gave it. The old per-viewer
  attach floor is gone, because there is no chunk-local numbering left for it to protect: a sole or
  resynced viewer answers its first raw chunk, and so does a positioned viewer in mixed-version mode
  after a resync. The sent bound advances only when a frame is really handed to the outbound
  channel, so a blocked send cannot let a reply name unseen output, and an ack past that bound is
  clamped, so a stale or bogus acknowledgement cannot mark later chunks free. Clients declare
  reply-position
  support when they attach with `reply_positions_v2`; the new field exists because the numbering
  changed from the ending chunk to the starting chunk, so a client that only sends the old flag is
  treated as legacy rather than having its numbers trusted. While any attached viewer does not
  declare it, every reply keeps the legacy
  single-owner rule, whichever client's reply arrives first. The owner role and the
  answered-position record are only committed once the reply's bytes reach the shell, so a failed
  write leaves neither behind, the answer can still be written by another viewer, and a bogus
  positioned claim cannot suppress the old client's next valid answer. Another legacy viewer
  attaching or detaching no longer resets the owner while a query is in flight. A stalled or
  backgrounded viewer no longer holds an owner role that blocks the others. The per-answer record
  keeps 64 recent chunks with 1024 answers each by design, and older chunks collapse to a high-water
  mark, so memory stays constant; a late reply for a chunk older than the whole window is the entry
  that ages out, so it cannot push a newer chunk out.
- **A batched colour query marks every answer it earns.** The query tracker kept at most 32 pending
  answers, so an `OSC 4` palette query with more entries left its tail looking like typed input. It
  now tracks the whole batch.
- **The client keeps reconnecting.** After the fast backoff it used to stop after twelve attempts
  and stay offline until the user pressed Reconnect. It now keeps retrying at the capped interval
  and retries immediately when the browser comes back online, the window regains focus or the
  document becomes visible. A wake in the first second of a connect attempt is ignored, so a burst
  of online/focus/visibility events does not tear down a socket that is still opening. Auth failures
  still stop, and the banner's Reconnect button stays available while it retries.
- **A Send during an idle agent's death starts a fresh agent.** When a stdout read failed or EOF
  arrived between turns, the session stayed in the slot for the bounded wait for the exit report, and
  a message sent in that window claimed it and failed on a dead write. The session now leaves the
  slot the moment its reader is known to be gone, so the message starts a new agent at once, while
  the pump still reads the old stream: the exit report publishes the death's one diagnostic row and
  arms the reset it carries.
- **A stopped turn whose transport dies ends interrupted, not with an error row.** After a cancel,
  the ACP adapter closes the transport and then reports the turn failed from its prompt task. The
  completion arm settled that as a failure with an error row even though the person had already
  stopped the turn. A failed completion on a closed transport now follows the same rule as a stream
  end on a stopped turn: interrupted, with the death's report dropped instead of published. Only a
  `Failed` report is rewritten: a turn the adapter already reported completed stays completed when a
  Stop arrives while its exit report is still on its way.
- **Each dying agent keeps its own diagnostic row and reset.** Two sessions can die between turns
  before the first one's exit report arrives — a message sent in the window replaces the dead
  session, and the replacement can die too. The pending deaths were kept in one slot, so the second
  overwrote the first and the first death's row and reset were lost. They are kept per session now,
  and each pump finalizes only its own. A reset a death already applied is also kept with it, so an
  exit report that repeats a reset after the person cancelled the resume no longer arms it again.
- **Auto-resume stays cancelled, waits for the newest reset, and covers retried writes.** A
  `LimitHit` followed by an `Exit` with the same reset, with no EOF in between, used to re-arm the
  pause after the person cancelled it; the live session now remembers the reset it applied and hands
  it to the death. When two dead sessions are waiting for their exit reports, an older one's later,
  earlier reset no longer replaces the newer one's schedule: both deaths keep their row, but the
  thread waits for the newest reset. Finally, a failed write whose session was lost still honors the
  adapter's typed limit: the retry is held behind the schedule and delivered when it fires, instead
  of running straight into the paused provider.
- **A paused requeued prompt is never stranded, and the pause belongs to the session that reported
  it.** A failed write that put its prompt back behind a typed limit left the turn Pending when the
  person then cancelled the resume or sent a message: clearing the schedule removed the only thing
  that would deliver it. Both actions now release that turn. A Send queues its message behind it,
  so the parked prompt runs first. An older session generation's delayed exit report no longer
  marks the live replacement's reset as applied, and a requeued write's pause names the generation
  that claimed the write instead of `None`. A newer generation that confirms the same reset takes
  ownership of the pause without re-arming it, so an older generation's later, earlier reset can no
  longer replace it. A firing schedule re-checks under the record lock that the pause is still its
  own before it clears anything or hands off delivery. Finally, a typed limit without a reset time
  on a requeued write waits the meter's unknown-reset backoff instead of retrying immediately.
- **A parked resume survives storage errors, restarts and unrelated pauses.** Releasing a parked
  prompt never waits on a storage write now: the fire's ledger outcome and the Send/cancel state
  write are logged if they fail, and the prompt is delivered either way, so a failed write can no
  longer leave a Pending turn with no delivery owner. A released turn is projected as running
  rather than left Rate limited: the thread leaves the paused phase and drops its limit without
  counting a second turn, so a cancelled or superseded pause shows Working and no resume time. A
  fire only acts when the record still names the schedule it claimed; a missing or different
  identity is not a match, and the failover model change now happens after that check. A pause parks
  only the prompt it was armed for, so an old agent's exit report no longer holds a replacement turn
  a Send started. Applied resets are recorded per session generation, including when a live session
  adopts an equal reset, so a cancelled resume stays cancelled through that generation's exit-only
  repeat. Finally, the parked prompt is stored with the thread row: a daemon restart restores the
  dispatch and its schedule delivers the person's message instead of the policy's "continue".
- **One provider limit arms one schedule.** The bridge can report one pause twice: the prompt's
  typed limit before the completion and the same reset on the exit report after it. The
  closed-completion wait now collects those limits and applies one after the wait, preferring the
  exit report's reset, and does not replace a schedule the turn already armed for that reset. The
  attempt counter keeps its first value and the jitter is no longer silently changed.
- **A paused resume is only fired, held and released by the record that owns it.** A fire now checks
  that the thread's record names the schedule before it claims the attempt in the ledger, and the
  daemon resyncs that record before it starts the ticker, so a restarted daemon cannot consume a due
  attempt it never knew about. A delayed report from an older session
  generation no longer overwrites a newer turn's applied-reset marker (the marker is only written by
  the generation that owns the turn), so that turn's own completion cannot re-arm a pause the person
  cancelled. The failed-write retry decides whether a pause holds it with the same
  `pause_parks_turn` predicate `advance_queue` uses: a pause armed by another generation does not
  hold a dispatch it does not name. A newer limit that replaces a parked pause keeps carrying the
  prompt the pause was holding, so a pump-end advance cannot deliver it into the paused provider.
  Finally, a parked prompt whose ledger schedule did not survive a restart is released exactly as a
  cancel releases it — recorded as `LimitResumeReleased` with the limit cleared — instead of running
  while the thread still claims to be rate limited.
- **Shutdown, restart and firing keep the parked prompt and its own turn.** A
  graceful daemon shutdown now leaves a prompt parked behind an armed schedule exactly as it is, so
  reopening restores the dispatch and the schedule delivers the person's message instead of the
  policy's "continue". The restart resync restores schedule ids from the ledger even when the limit
  history cannot be read, and the ticker retries a failed ledger read until it succeeds, so a
  restored pause never stays unnamed. A fire only releases the pending turn its own pause parks: a
  Send's startup that merely happens to be pending is left alone and spends no attempt. If a claimed
  fire then finds the record no longer names its schedule, it marks the attempt `Cancelled` with
  "superseded before delivery" instead of leaving it looking `Fired`, and a newer row that replaced
  the attempt is untouched. A release whose journal append fails is no longer hidden by a cached
  summary: the next tick publishes the running state.
- **A request refused after an agent death names what actually closed.** The client keeps the close
  it observed (stdout EOF, a failed read, a writer failure or the process exit) and uses it when a
  later request is rejected, so a stdout death no longer looks like a dead writer in the failure row.
- **A second Stop retries a cancel the harness did not take.** Stop used to record the intent
  before sending the cancel and then treat every later Stop on that turn as already stopped. If the
  first cancel failed while the session stayed open (a transient interrupt error), the turn kept
  running while the UI showed it stopped. Every Stop on a still-claimed turn now tries the cancel
  again; only a cancel that had nowhere to go counts as done.
- **A failed write and a Stop can no longer leave a turn Working.** Whether a prompt whose session
  went away goes back to the queue or the turn ends interrupted is now one decision under the
  turn's dispatch lock. A Stop that landed between the old check and the requeue made the requeue
  fail quietly, and the turn stayed Working with nobody delivering it.
- **A closed transport's exit report always beats the wait's deadline.** The completion wait after
  a dead transport now polls its stream before the deadline, matching the idle fallback, so an exit
  report that is ready at the same moment is the row the turn keeps instead of the generic close
  notice.
- **A proxy restart no longer drops a session a new message just claimed.** A restart that
  inspected a thread as idle now re-checks under the same lock as its detach that no turn became
  active in between. A Send that claimed the session keeps it, and the new proxy routing applies on
  the thread's next session start instead of shutting down an agent that is already writing.
- **A Stop the person saw acknowledged never replays.** Stop is recorded on a turn even when its
  prompt is already on the wire. If the session detaches before the cancel reaches it (a restart, a
  shutdown, a model change), the write's later failure ends the turn as interrupted instead of
  writing the stopped prompt again on a fresh session.
- **An approval buffered behind a closed completion expires with the turn.** The turn's settlement
  detaches the transport, so no session could receive an answer; the request was never published,
  so it is dropped instead of becoming a cue nothing can resolve. Tool and text updates buffered in
  the same window still reach the turn.
- **A limit notification without a reset no longer ends the wait for the exit report.** Only the
  process-exit report is terminal for a completion on a closed transport, and that report now
  carries the provider reset itself. An ordinary limit that arrives first no longer detaches the
  session and promotes queued work before the exit's reset has armed auto-resume.
- **A duplicate exit reset no longer undoes a cancelled auto-resume.** The reset a turn already
  applied is remembered on the turn itself, not only in the live schedule, so the exit report's
  repeat of that same reset cannot arm a new schedule after the person cancelled the resume while
  the report was still arriving. A different, newer reset still arms.
- **A Stop followed by the session's death ends interrupted, not with an error row.** The death the
  transport reports behind the Stop is incidental to the cancel, so its held report is dropped
  instead of taking the stopped turn's one row; a limit it carried was already applied on its own.
- **A closed stdout is reported as an EOF, not a write failure.** The ACP client carries the part
  of the transport it observed closing on `Closed`, and the Antigravity prompt path uses it, so a
  stdout death no longer looks like a dead writer in the turn's row. The generic stdout-close
  notice carries the same kind, so a failed read is not reported as an EOF either.
- **A prompt whose session was detached mid-write is delivered again.** When a write fails after a
  restart, a shutdown or a model change detached its session, the turn is still active and no other
  path owns it. The prompt now goes back to the queue and is written on a live session or a fresh
  startup instead of leaving the turn Working with nobody delivering it.
- **An ACP request registered while the reader drains can no longer be lost.** The closed check and
  the pending-map insert happen under one lock, so teardown either drains the request or rejects its
  registration with `Closed`; a request inserted after the final drain no longer waits for a reply
  no reader can deliver.
- **An idle agent that only the process exit announces is detached immediately.** An exit whose
  stderr carries a rate limit used to publish only the limit, so the pump never learned the session
  was dead: the dead agent stayed attached and the next message failed on it. The exit report now
  always follows its limit, so the session is detached and the next message starts a fresh agent.
- **A stdout read error is no longer mistaken for the process-exit report.** Transport deaths now
  carry their kind (EOF, read, write or exit). Only the exit kind ends the wait for a closed
  transport's final report, so a read error that precedes the exit cannot detach the session and
  promote queued work before the exit's rate-limit reset has armed auto-resume.
- **Tool updates buffered behind a closed transport's completion reach their turn.** Events that
  arrive inside the exit-report window are applied to the completing turn before it settles, so a
  tool card that completed there no longer stays Running. The idle fallback also prefers a queued
  report over its deadline, so the exit report names the row whenever both are ready.
- **The ACP fixture models a death only the exit can report.** `agy-fixture` can answer a turn and
  then exit while a keeper process holds stdout open, so the reader never sees EOF; the reset it
  prints on stderr makes the exit report a typed limit, which is the ordering that used to leave
  the dead session attached.
- **Characters split across streamed chunks arrive intact.** OpenCode's event stream and the MCP
  hub's SSE client now carry an incomplete trailing UTF-8 sequence into the next chunk instead of
  decoding each chunk alone, so an emoji or CJK character split between two chunks is no longer
  replaced by `�`. A chunk of invalid bytes is skipped with one replacement character per bad
  sequence instead of quadratic copying. A character cut off by the end of the stream is dropped
  with the unterminated last line or event it was part of, which neither reader ever dispatches.
- **An ACP cancel can no longer overtake the prompt it cancels.** `send` reports the turn as
  running only after `session/prompt` has been flushed to the agent, so an immediate interrupt
  sends `session/cancel` after the prompt. Waiting for the writer queue and for the flush share one
  ten-second deadline; when it expires the writer is stopped and the agent killed, so a blocked
  frame can never reach it later. A timed-out write ends the turn with a single failure and detaches
  the dead session, so the next message starts a fresh one. A Stop that arrives before the prompt is
  dispatched, including while the session is still starting, ends the turn as interrupted without
  writing anything.
- **A Stop belongs to one turn, and a thread starts one agent at a time.** Each turn now carries its
  own dispatch state, so Stop decides atomically whether the prompt can still be dropped or needs a
  cancel: a Stop during startup drops that prompt even when a follow-up is already queued, and the
  follow-up is then delivered on the same newly started session instead of starting a second agent.
  A Stop that arrives for a turn that already ended no longer cancels the turn that replaced it, and
  a Stop with a full writer queue waits at most ten seconds for it before the writer is stopped and
  the agent killed, instead of waiting for the queue to drain.
- **Every ended turn advances the queue, exactly once.** A completed turn, a stopped one, a failed
  prompt write, a failed startup and a dead session all end through one settlement: it checks the
  turn is still the active one, publishes exactly one `TurnCompleted` (and at most one error row, so
  a Stop racing a failure or a bridge error racing a write failure cannot double-report), and
  promotes the next queued message under the same lock, so a Send that arrives during the failure
  queues behind that message instead of jumping ahead of it. A failed startup now reports a failed
  turn instead of leaving the thread Working. Each session startup carries a generation: one that
  returns after a restart or a shutdown shuts its session down instead of attaching it, and a
  detach only clears the generation it inspected, so a late path never drops a session it never
  saw. A delivery whose session was detached starts a fresh one instead of leaving its turn
  Working. A transport that died mid-turn (a writer I/O error, a cancel that timed out, an agent
  that exited) is detached before the next turn is promoted, so it starts a fresh agent rather than
  being written to a closed client, and a Stop that races that death is treated as delivered. A
  writer I/O error also marks the ACP client closed, and an agent the client killed itself after a
  write deadline or a shutdown no longer adds a second exit-error row. Replies the ACP reader sends
  while answering the agent queue under the same deadline as every other frame, so a full writer
  queue cannot park the reader.
- **A failed startup no longer strands the turn waiting behind it.** When a startup fails after its
  turn was stopped (or after the thread moved on), the message queued behind that turn, or sent
  after the stop, is still promoted and delivered on a fresh agent instead of staying Working with
  no session. A startup that has no turn to carry (a Stop settled it before the startup reached the
  thread) starts no session at all and leaves the next message its own startup; a startup failure
  only ever ends the turn it captured, never one that arrived afterwards.
- **A session's event pump speaks only for its own session.** Every pump carries the generation of
  the session it serves and drops its events once the thread has detached that session; every
  detach stops the pump. A late `TurnCompleted` from a dead transport can therefore no longer fail
  the turn that replaced it, and a stream that ends while a promoted turn's prompt was never
  written leaves that turn for a fresh session instead of failing it. The pump reads the session's
  transport health from a lock-free flag rather than the session lock, so a write that is still in
  flight (waiting for its own reply) never stops the pump from draining the events that unblock it.
- **A transport failure is tagged where it happens, so only it shares a row.** The ACP writer-error
  path, a closed or exited agent, and a prompt that failed because the transport died report a
  distinct transport-death event; the daemon collapses exactly those reports and the failed-write
  row for the same turn into one failure, which keeps the agent's stderr text. Any other harness
  diagnostic is always its own row, so one buffered before a death can no longer take the failure's
  place and hide it.
- **A writer I/O error closes the ACP client and kills the agent before waking a caller.** An agent
  that closed its stdin but stayed alive can no longer survive under a client the daemon still
  believes is open, so the prompt that observes the error always finds a closed, dead transport.
  The error also keeps the writer's own I/O detail ("Broken pipe"), and the daemon's one failure row
  for that write includes the agent's stderr tail too, so a diagnostic printed just before stdin
  closed is not lost.
- **Detaching a dead session and failing its turn are one step.** The pump no longer clears the
  session slot and then re-locks to settle the turn it left behind: both happen under one record
  lock, so a delivery cannot see the empty slot in between and start a replacement agent for a turn
  that is about to be marked failed. When the pump instead leaves a still-pending turn for a fresh
  startup, the dead session's held transport report is dropped with it, so the retry does not
  inherit the old session's death as an error row on an otherwise successful turn. A failed
  startup clears its own slot and settles the turn it carried under that same lock, so a second
  startup scheduled for the same turn (a Send, a delayed queue advance) can no longer take the
  empty slot, start an agent and write the prompt just before the turn is recorded as failed.
- **A dead agent's exit report still reaches the turn's one row.** When stdout closes before the
  process is reaped, the prompt that wakes on the EOF now waits briefly (up to 500 ms) for the
  supervisor's exit report and the stderr drain, and merges the exit status and the agent's last
  stderr line into the same failure row. A rate-limit reset that only the exit report carries still
  reaches the limit and auto-resume handling, and the writer that sees stdin close waits for the
  stderr reader to drain the pipe before it wakes a caller, so a diagnostic printed in the same step
  as the close is not lost.
- **An agent that closes between turns gets one failure row, not two.** The adapter reports that
  death twice (the generic stdout close, then the exit status with the stderr it drained). The
  reports are now merged per session generation and published as one row from the exit report
  itself, so the generic notice can no longer take the row and hide the exit status and stderr.
  The row never waits for the session's stream to end: a live adapter keeps its event sender alive
  for as long as the daemon holds the session, so that stream may not end at all. A generic close
  with no exit report behind it publishes after a bounded 500 ms wait, and the dead session is
  detached with the row, so the next message starts a fresh agent.
- **A rate limit that arrives after `end_turn` still pauses the thread.** ACP can answer a turn and
  only then exit, leaving the provider's reset on the exit report rather than on the turn. The
  completion now sees the closed transport and waits (up to 500 ms) for that report before it
  detaches the session and promotes the queue, so the schedule is armed and the message queued
  behind the turn waits instead of starting at once on a fresh agent. A death between turns still
  publishes its one diagnostic row, but without flipping a thread whose auto-resume just armed from
  Scheduled to Error, and a stderr-less exit report now names that row instead of the longer
  generic stdout-close notice (reports of one kind are still compared by detail).
- **The ACP test peer models a limit that only the exit report carries.** `agy-fixture` can answer
  a turn, close stdout and exit with the reset on stderr, both right after `end_turn` and as an
  idle death, and it can exit mid-turn with no stderr at all, so all three orderings are covered
  end-to-end.
- **A pump ends with the session it detached.** When a turn settles on a closed transport, or an
  idle death is published, the pump now detaches and shuts its session down, schedules any
  successor and returns instead of waiting for a stream end that the session it still holds keeps
  from happening. Every other detach (a Stop, a restart, a model change, a failed prompt write, a
  shutdown) already aborts the session's pump, so one path ends each pump.
- **The ACP test peer models a death between turns and a stalled reader.** `agy-fixture` can now
  answer a turn and then exit, so an idle death can be tested end-to-end without another Send, and
  it can answer, print a rate-limit reset on stderr and stop reading stdin, so a prompt larger than
  the pipe buffer exercises the write deadline, the typed limit, the persisted schedule and the
  held queue through the real adapter.
- **A prompt that never reached the agent still reports its rate limit, but only when the adapter
  says so.** When the agent closed stdin before the next prompt could be flushed, the failure row
  still carries the agent's stderr and exit detail, and a reset time printed there reaches the
  usage-limit path. A write that times out because the agent stopped reading stdin collects the same
  stderr tail and exit detail, so a reset time printed before the stall still arms the schedule and
  the queued message waits instead of running into the limit. The ACP adapter parses that detail
  itself and returns a typed limit signal, and the daemon arms the resume only for that signal, so
  another harness's transport error that merely says "payload size limit exceeded" stays an
  ordinary failed turn. A typed limit reads as `Rate limited: <message>` instead of a rejected
  write. The limit is recorded and its schedule armed only while the failed write still owns the
  turn and its session: a shutdown that interrupted the turn while the write was pending drops the
  late limit instead of leaving a schedule that would fire after restart.
- **A model change during a startup discards that startup.** Failing over while a session is still
  booting the limited model supersedes the startup, so the resume runs on a session started with
  the new model instead of the one that was already on its way.
- **A missing `stopReason` is not a completed turn.** A prompt result without a string stop reason
  now ends the turn as a failure ("the agent ended the turn without a stop reason") instead of
  being treated as a normal `end_turn`.
- **A turn stopped by `max_tokens` or `max_turn_requests` is no longer shown as completed.** The turn footer says "Stopped: the agent hit its token limit" (or its turn-request limit) even when
  the agent produced no output, and its Reply button puts the cursor back in the composer instead of
  resending the prompt (a message sent during the turn is already queued, so a resend would run it
  twice), as does a failed or interrupted turn, whose button no longer says "Retry" while only
  focusing the composer; `end_turn` remains the only normal completion.
- **A Stop after Antigravity's turn ended no longer interrupts the next turn.** The interrupt flag
  is cleared when a prompt is sent and only set while a prompt is running, so a late Stop cannot
  colour the following turn.
- **An agent that exits at startup no longer replaces itself forever.** A turn whose own startup's
  session loses its event stream before the prompt is claimed now ends as failed ("the agent exited
  before it accepted the prompt", with the agent's stderr when there is any) and the queue advances.
  A promoted message may still move off the previous turn's agent once, but a session started for a
  turn is that turn's only attempt, so a harness whose sessions all die at start fails the turns it
  was given instead of starting replacements in a loop.
- **A transport death's one row keeps the agent's stderr.** The bridge usually reports a dead
  transport first with its generic stdout-close notice and only then with the prompt failure that
  carries the agent's stderr. The first report is now held for the turn's settlement, later reports
  sharpen it, and the settlement publishes the most specific detail as the single row, so the
  generic notice can no longer take that row and leave the stderr out.
- **A refused or dropped ACP frame tears the client down.** When the ACP writer task exits without
  writing or acknowledging a queued frame, the client is now closed and the agent killed before the
  caller sees the error, so a later send can never find an open client behind a dead writer.
- **A titleless `tool_call_update` keeps the tool's name, summary and input.** Updates are merged
  into the call's last title, kind, input and locations, so a later title reaches the row while a
  status-only update can neither rename a kind-only call to "other" nor empty the expanded card's
  input.
- **Antigravity inlines UTF-16 text attachments with a BOM.** The shared attachment reader decodes
  UTF-8, UTF-16LE and UTF-16BE byte-order marks, so the text is embedded instead of falling back to
  a link. UTF-32 files and text containing NUL bytes stay links, and so do all text attachments on
  Claude Code, Codex and OpenCode, which pass a path reference.
- **The ACP test peer is no longer a daemon binary.** `agy-fixture` lives in its own `publish =
  false` workspace crate that the Antigravity test builds on demand and locates from Cargo's JSON
  output, so `cargo run -p h4rn-daemon` is unambiguous again and `cargo build`/`install` ship only
  `h4rnd`.

## 1.0.1 — 2026-11-19

A patch release for the clients and the relay: the post-1.0 UI fixes, project folders in the
sidebar, skill adoption, and the new relay defaults. The daemon protocol and the transport are
unchanged apart from additive project requests and the skill candidate's `managed` field.

### Fixed

- **Appearance settings take effect and persist.** The color scheme now applies at start and on
  change, and the terminal's "match app" choice is stored (`h4rn.terminalMatchesApp`) and applied
  to the terminal palette; turning it off pins the terminal to its dark palette.
- **Dialogs and the command palette stay inside the window and centred.** The entrance animation
  no longer stacks a second `-50%` offset on the placement translate, which threw dialogs up and
  left while they opened; tall dialogs now scroll instead of overflowing short windows.
- **Settings rows line up.** Controls sit at the trailing edge, and a lone switch stays on the
  label row on phones.
- **Every shortcut is a `⌘`/`Ctrl` chord from one rebindable registry.** The single-key and
  `G`-sequence shortcuts are gone, so typing never runs a command; a command that does not apply
  now leaves the key alone, and a new binding must include `⌘`/`Ctrl` and may not collide with
  another command. `⌘/` (Ctrl+/) opens the shortcut sheet.
- **The sidebar footer no longer duplicates Settings.** The MCP & skills button is gone, and so is
  the hardcoded PR badge.
- **Attachment chips are cards** that show the file type and size, and the start-a-thread box is
  wider.
- **The model picker follows the selected harness,** hides harnesses that are not detected and
  shows the thinking modes each harness supports.
- **The Coolify compose file does not trust forwarded-for by default.**

### Added

- **Projects in the sidebar.** A project is a folder on a host that the sidebar lists before it
  has a thread. Add one by opening an existing folder, creating a new folder (optionally running
  `git init`) or cloning a repository; projects sort and reorder, and New thread can start in a
  project's folder. The protocol gains `Project`, `ProjectSource` and directory listing frames.
- **Skills already installed in harness folders are detected and can be adopted.** The scan
  reports whether a copy is h4rn-managed, and a matching folder can be taken over without
  touching its files (h4rn writes its marker) or synced through the registry.
- **Settings → Hosts and Devices are one page.**

### Changed

- **The default relay is `h4rn.spwnd.dev`.**

## 1.0.0 — 2026-11-15

The first stable release. h4rn is a single Rust daemon (`h4rnd`) behind one typed WebSocket
protocol, with the desktop, mobile and browser clients as pure views over it. v1 ships:

- **Daemon and protocol:** a typed protocol with generated TypeScript bindings, an
  event-sourced thread lifecycle (SQLite), and Claude Code, Codex, OpenCode and Antigravity
  harnesses with approvals, terminals, blobs and metering.
- **Transport and pairing:** the Noise `IK` / `IKpsk2` secure transport (native and wasm), direct
  and relay paths with resume, scoped pairing with a typed short code, and the PSK-possession fix
  (a wrong one-time secret no longer admits a device). The pre-attach handshake is bounded
  (`handshake_timeout` 10 s, `restart_timeout` 5 s, enforced inside `SecureService::accept`), the
  `Open` record is validated before authorisation, and every failed `Open` gets the same
  `NotPaired` reject.
  A legitimate pairing no longer fails when the daemon's durable pairing write is slow: the
  client waits up to `establish_timeout_ms` (10 s, the daemon's own handshake deadline) for the
  first reply on a fresh channel, and only an established path uses the 1 s liveness limit.
- **Connect and sharing:** the OrbitID-backed relay and registry, MCP sharing and the tunnel,
  content-addressed skills, subagents as child threads with daemon-enforced delegation policy, and
  forge integrations.
- **Clients and updates:** the quiet native UI, the desktop updater notice, Settings → About,
  and Settings → Harnesses with CLI versions and updates (ADR-043).
- **Packaging:** installers, daemon archives, winget/Homebrew/AUR templates, the Helm chart and
  the Docker image.

### Known limitations

- macOS and iOS builds and flows are unverified: they were not exercised on the Linux
  development host.
- The Android UI was built but not driven end to end on a device or emulator.
- A new thread cannot request a worktree (`CreateThread` with `worktree` returns
  `NotImplemented`); per-thread worktrees are not implemented. Subagent worktree isolation is.
- Remote-host subagents are not implemented: a subagent runs only on its parent's host.
- Gitea and Forgejo have no workflow re-run API, so re-run is unavailable for them.
- The desktop updater needs a signing key: the repository carries a development placeholder, and
  release builds must supply the real updater key and `latest.json` endpoint.
- The OrbitID-backed suite was not run for the final release commit, and the Docker-based suites
  (relay, connect, remote, forges) were skipped on the release host.

## Development history (pre-1.0)

### Council round 6: a wrong PSK no longer pairs a device (P0, N1–N4)

- **A wrong one-time secret no longer admits a device or burns the real one (P0).** `IKpsk2`
  mixes the PSK only into the responder's message 2, so the daemon could not verify possession at
  message 1, yet it consumed the secret, allowlisted the key and ran the pairing hook there. Anyone
  who knew the daemon's public key could pair with a random secret during a `pair --direct`
  window, lock the real phone out and later connect over plain `IK`. A PSK-authorised new device is
  now provisional: possession is proven by the first transport record decrypting under the
  PSK-derived keys, and only then is the secret consumed, the device recorded and allowed, before
  any `Welcome` or ticket. A record that does not decrypt is refused as `NotPaired` and nothing is
  touched.
- **A paired client is no longer wedged by a stale secret (N1).** A device admitted just before
  its socket dropped kept retrying `IKpsk2` with a consumed secret and got no reject. The
  initiator (native and wasm) now always opens with plain `IK` and restarts once with `IKpsk2`
  only after a `NotPaired` reject.
- **Revocation is read afresh during the handshake (N2).** The policy was cloned once before
  message 1; it is now re-read after the handshake, so a mid-handshake `revoke()` refuses the
  session.
- **The unpaired-device refusal test is exact (N3).** It now requires `device is not paired`.
- **Docs (N4).** `docs/transport.md`, ADR-042 and the round-5 entry describe possession being
  proven by the first transport record and the plain-first fallback; the round-4 K2 backoff
  wording is corrected.
- **An unknown key can no longer hold a daemon task open (Q1).** After a `NotPaired` reject the
  server waited for the restart with no deadline, and `/secure` needs no credentials. `accept` now
  bounds the whole pre-attach phase (`handshake_timeout`, 10 s) and the restart wait
  (`restart_timeout`, 5 s), so every caller (`/secure`, the relay connector, `/secure-mcp`) is
  covered.
- **A PSK with a bogus resume token no longer pairs (Q2).** The `Open` record is decoded before
  authorisation, and a PSK-authorised device that also sends a token is refused with `NotPaired`
  before it is allowlisted or the secret is consumed.
- **A failed `Open` rejects alike for every key (Q3).** The `NotPaired` reject no longer depends
  on whether a secret is armed, so it cannot tell an allowed key from an unknown one.
- **Paired devices leave the secret armed, now pinned (Q4).** Documented and tested.
- **Test gaps closed (Q5).** The native `retry_with_psk` path and the daemon's forged-first-record
  branch over a real WebSocket are covered.

### Desktop app updates and harness CLI updates (v1)

- **The desktop app tells you when a newer h4rn exists.** The Tauri shell checks the configured
  updater endpoint (`latest.json` on GitHub Releases) 20 seconds after launch and every 6 hours.
  A missing endpoint, the development placeholder key or no network is logged and otherwise silent.
  A newer release shows a quiet notice, "h4rn X.Y.Z is available", with **Restart to update** and
  **Later**; Later hides it until the next check. Download progress is shown, and the install
  relaunches the app. Web and mobile never show it (mobile updates arrive through the stores).
- **Settings → General → About** shows the app version, **Check for updates**, the last-checked
  time and an automatic-check toggle. The shell exposes `app_update_status`, `app_update_check`,
  `app_update_set_auto` and `app_update_install`, plus the `h4rn://update-checked` and
  `h4rn://update-progress` events.
- **Settings → Harnesses** lists Claude Code, Codex, OpenCode and Antigravity with the installed
  version, the latest npm version and an **Update** button that streams the command's output. The
  Settings nav shows how many harnesses have updates. `ListHarnessVersions` (view scope) and
  `UpdateHarness` (admin scope) are new requests; output arrives as `HarnessEvent` frames. The
  update runs without a shell, arguments as an array, and is refused while that harness has an
  active turn. Homebrew or system-package installs show the command to run instead; Antigravity
  has no registry source and reports "unknown". See ADR-043.

### Council round 5: per-device handshake patterns, self-driving peer retries and honest tunnel capabilities (M1–M3)

- **An armed one-time PSK no longer locks out paired devices (M1).** The daemon passed the live
  secret into every responder, so an allowlisted device that correctly initiated plain `IK` failed
  with `Handshake("decrypt error")` for the whole `h4rnd pair --direct` window (and permanently
  with `H4RN_PAIRING_PSK`). The pattern is now chosen per device: while a secret is armed the
  daemon reads message 1 with a no-PSK responder first and replays it through an `IKpsk2`
  responder only when that cannot use it. A paired device connects and leaves the secret armed; a
  new device pairs and consumes it; a device without the secret is refused. A client holding a
  resume ticket never sends the PSK. (Round 6 corrected where possession is proven: see below.)
- **Peer admission retries on its own timer (M2).** A failed `register_host` used to wait for the
  next topology report, which may never arrive while the peer set is stable. The directory keeps
  the last merged set and a background driver re-applies it on the bounded backoff deadline; the
  timer is only armed while a retry is pending. A peer is admitted after storage recovers without
  a second `replace_source`.
- **The tunnel no longer advertises `resources/subscribe` (M3).** The tunnel has no subscribe
  method (`METHOD_NOT_FOUND`), yet its `initialize` omitted `subscribe: false` that the local
  server already sends. It now advertises `resources.subscribe: false`, and the docs no longer
  claim that operation is guarded.

### Council round 4: MCP tunnel authorization, peer admission, direct pairing and upload ownership (K1–K4)

- **Every protected MCP tunnel operation revalidates its grant (K1).** Resources and prompts used
  the scope fixed when the tunnel opened, so a device excluded by `allowed_devices` or a server
  the owner had unshared was still readable through an open tunnel. One guard now re-checks the
  authenticated peer, the current share and the device restrictions before `tools/list`,
  `tools/call`, `resources/list`/`read` and `prompts/list`/`get`; tool policy and
  approvals stay in the shared call path. Covered by a persistent-tunnel test that unshares,
  narrows the policy and excludes the device.
- **A failed peer admission is retried instead of leaving the peer unadmitted (K2).** The
  directory skipped any peer already in its in-memory set, so a `register_host` write failure
  stuck for the rest of the run. It now decides from the registry: a peer is skipped only when a
  `host` entry exists for its key and the transport already allows it; otherwise it retries with
  an immediate first retry, then exponential backoff (500 ms before the third attempt, doubling, capped at 30 s). A
  `client` key is still never re-roled.
- **A phantom direct pairing is refused, and an undeletable QR cannot be re-armed (K3).** The
  transport pairing hook is fallible: if the new device's registry entry cannot be written, the
  handshake gets `NotPaired` and the device is not added to the allowlist. If invalidating the
  consumed `direct-pairing.json` fails, the daemon records the BLAKE3 hash of the PSK in
  `direct-pairing.consumed` and the watcher refuses to arm that file after a restart (best
  effort, no secret stored).
- **An in-flight upload belongs to the device that opened it (K4).** `UploadSession` records an
  owner — the authenticated Noise key for a remote session, `Local` for loopback — and
  `begin_upload`, `append_chunk`, `commit_upload`, `abort_upload` and `query_upload` return
  `UploadNotFound` to anyone else. Binding by device, not connection, keeps resume-after-reconnect
  working while another paired device can no longer append to, query, commit or abort the stream.

### Council round 3: pairing, registry, approvals, resume claims and deleted-file diffs (H1–H4, I1–I4)

- **QR pairing now completes the SAS comparison and waits for the host (H1).** The phone derives
  the string in Rust from its own key and the daemon key pinned by the QR, shows the comparison
  screen, and pins nothing until "They match". It then waits in a pending state until the host
  approves the device; a timeout explains what to do and never reports success early. Both QR and
  typed-code flows are covered in vitest and the remote e2e now pairs the QR path through host
  confirmation as well.
- **A restart no longer re-admits devices revoked while connect was unreachable (H2).**
  Connect-provenance clients and peer hosts load unverified: off the transport allowlist and
  without effective scopes until the first connect snapshot or peer report of this run. Direct
  pairings stay admitted. Trade-off documented in ADR-042 and `docs/transport.md`: a
  connect-paired phone cannot use a direct or tailnet path until the daemon has reached connect
  once.
- **A failed direct-pairing invalidation hook refuses the pairing (H3).** `consume_psk` takes a
  fallible hook; if the one-time file cannot be deleted, the handshake gets `not paired` and the
  PSK stays consumed, so the QR cannot be re-armed after a restart.
- **Registry write failures are reported instead of swallowed (H4).** `persist` returns an error:
  confirmations that cannot be written roll back and surface through `ConfirmPairing`, while
  revocations stay revoked in memory and are recorded in a `devices.revoked` sidecar that the next
  start applies and clears. Peer-host updates log and skip admission rather than pretending.
- **An approval cannot outlive its grant (I1).** `confirm_tools` / `confirm_all` approvals were
  raised from a grant snapshot read before the wait, so unsharing the server, narrowing its policy
  or removing it during the wait still ran the call after the owner approved. Grant changes now
  withdraw the matching cards (`mcp_share_approval_resolved` with `cancel`), and the daemon
  re-reads the grant and re-checks the authenticated peer after the answer and again immediately
  before dispatch. A changed grant fails the call with `GrantChanged`.
- **A dropped caller withdraws its approvals (I2).** The owner's `/secure-mcp` loop awaited each
  call, so a pending approval blocked every later call on the tunnel and a card outlived the
  caller. It now keeps reading frames, runs at most eight calls at once, and — when the transport
  reports that the session has no live path left — withdraws that `server → machine` grant's cards
  and drops the in-flight calls.
- **A cancelled auto-resume cannot fire (I3).** `fire_schedule` re-checked only `closed`/`active`
  after the proxy and session I/O, and `finish_schedule` then overwrote a `Cancelled` row with
  `Fired`. Firing now claims the exact attempt (`schedule_id` + `attempt`, `Scheduled`/`Waiting`
  → `Fired`) under the thread lock that `cancel_resume` also takes; cancellation and the claim are
  conditional ledger updates, so exactly one wins. `finish_schedule` refuses a cancelled or
  superseded row, and a cancelled resume starts no turn and does not switch models.
- **A deleted file diffs again (I4).** `diff` resolved the target with `Existing`, so a file the
  IDE was asked to show after deletion failed with `NotFound`. It now resolves with
  `ResolveMode::Create`: the nearest existing ancestor is canonicalised, dangling symlinks and
  escapes are refused, and a missing target is `after: None` for both `HEAD` and agent-turn diffs.

### Council round 2: pairing, direct pairing and registry lifecycle

- **`h4rnd pair --direct` can pair again (E1).** A live one-time secret now opens PSK pairing;
  clearing or consuming it closes the window. The watcher no longer needs a test-only `with_psk`.
- **A used direct QR cannot be replayed after a restart (E2).** The consumed hook deletes the
  matching `direct-pairing.json` before the device is authorised.
- **Direct-pairing files have one owner (E3).** Expired files are deleted, the watcher clears only
  the secret it installed, an operator `H4RN_PAIRING_PSK` is never replaced or cleared, and the
  armed scopes are dropped with the secret.
- **Relay-provisioned devices are pending until the host confirms (E4).** `DevicePaired` and
  snapshots register the key with no effective scopes and off the transport allowlist; Settings →
  Devices shows the SAS with Confirm/Reject, backed by `ConfirmPairing` (`admin`). Snapshots never
  promote a pending key. QR pairings follow the same rule.
- **Revoked peer daemons do not survive a restart (E5).** The first authoritative peer report
  reconciles persisted connect-provenance host entries, removing and revoking keys that are gone.
- **A client pairing never overwrites a peer daemon (E6).** `insert_scoped` keeps the existing
  `host` entry and logs a warning instead of re-roling it.
- **A direct pairing without `--scope` grants `view` and `approve` (E7).** Terminal, files and admin
  need an explicit `--scope`; the same default applies to old files and an unarmed operator PSK.
- **Terminal ownership uses the verified device (E8).** A remote client's `Hello` device id is a
  claim; terminal ownership and the remote audit trail use the registry id or the handshake key.

### Council round 2: MCP sharing over the relay, real approvals, device ids (F1–F3)

- **MCP tunnels can use the relay (F1).** Relay frames carry an explicit `service` (`secure` for
  clients, `secure_mcp` for peer daemons). A daemon multiplexes peer sessions over its existing
  relay socket, targeting another machine of the same account with a live daemon; user devices
  cannot open `secure_mcp`. Direct and tailnet paths still rank ahead of relay.
- **Confirmation policies raise a real approval (F2).** `confirm_all` / `confirm_tools` produce a
  pending card in the owner's UI with the tool name and a bounded argument preview. It needs the
  `approve` scope, expires closed after two minutes, is cancelled when the tunnel drops, and is
  re-read with `PendingApprovals` after a reconnect.
- **`allowed_devices` matches a real id (F3).** The peer daemon's connect device id is carried
  through discovery into `register_host`; the owner compares that recorded id, never a claim, and
  docs/mcp.md now states exactly what the list holds.

### Council round 2: files, mobile reachability, sync ordering and thread locks

- **Diffs require the `files` scope (G1).** `FsDiff` and `FsTurnChanges` were classified as
  `View`, so a view-only device could read file contents. `diff` and snapshot turn changes now
  also resolve through `resolve_inside(…, Existing)`, so a symlink pointing outside the workspace
  is refused instead of followed.
- **Release mobile builds reach the LAN and a tailnet again (G2).** Android's release network
  security config `base-config` and iOS ATS (`NSAllowsLocalNetworking`, `.local`, `ts.net`
  exception domains) now permit the cleartext candidates the daemon actually serves. Noise
  end-to-end encryption with the pinned daemon key is the security boundary, so ws:// adds no
  exposure; the rationale is in `docs/mobile.md` and `docs/transport.md`, and a unit test checks
  both native configs and the candidate-URL builder.
- **Skill-sync cursors are commit-ordered (G3).** `server_updated_at` is the transaction start
  time, so a slow push that committed after a fast one could be skipped forever. Each push now
  takes a per-user `BIGINT` sequence under a clock-row lock that is released at commit; the pull
  cursor is that sequence (opaque string on the wire). A legacy RFC 3339 cursor replays the full
  set instead of being misread.
- **A due auto-resume no longer freezes its thread (G4).** `fire_schedule` held the thread record
  lock across the proxy's failover HTTP calls and `session.shutdown()`, so `GetThread` and
  `SendMessage` on that thread waited tens of seconds. It now reads what it needs, releases the
  lock for the I/O, and re-checks `closed`/`active` before mutating.
- **Docs and UI stop implying two unimplemented paths (G5).** README no longer claims a Phase 1a
  scaffold, the stale protocol comments are corrected, ADR-022 records that only subagent
  worktrees ship, and the New-thread dialog, the inbox composer and the Delegate dialog disable
  other hosts with the reason that a new thread and a subagent both run on the daemon's host.

### Council round 1: authorization, transport and pairing

- **Binary PTY input is authorised (A1).** It needs `Hello`, the `terminal` scope and a viewer
  this connection attached; blob chunks need `Hello` and `approve`.
- **`/secure-mcp` authenticates the peer (A2).** The caller must be a registered peer daemon key;
  `machine` and device id come from the registry, not the frame; `allowed_devices: None` means the
  target machine's daemon only.
- **Authorization fails closed (A3).** A persisted device registry (`devices.json`) records scopes
  and pairing provenance per key. No entry means denied; the identity is the handshake key, never
  the relay's device id. Pairings made before scopes were recorded must be paired again.
- **Reconnect reconciles revocations (A4).** A relay snapshot removes connect-paired devices it no
  longer lists and ends their sessions; direct pairings are kept.
- **`h4rnd pair --direct --scope` is honoured (A5).** Without `--scope` a direct pairing grants every
  scope, now stated in the help. The pairing watcher also runs for daemons not signed in to connect.
- **Short authentication string for typed codes (A6).** The phone shows a 12-digit code derived in
  Rust and confirms it against the host's before pinning the daemon key. New request
  `PairingConfirmations`; wasm export `pairing_sas` (committed wasm rebuilt).
- **One QR pairs one device (A7).** The one-time PSK is consumed atomically at authorisation.
- **`SetThreadTab` and `Snooze` need `approve` (A8).**
- **Scopes are per machine in h4rn-connect (A9).** Admin on one machine no longer mints or revokes
  pairings for another.

### Cross-machine MCP sharing, target side

- **Shares now reach the target machine.** Each daemon keeps a peer directory (from connect, or
  added directly), dials a peer's `/secure-mcp` with its own daemon key, receives that peer's
  grants as pushed snapshots (share, unshare, policy change, server removal, peer revoked), builds
  the tunnel itself and registers the server with its hub. Unshare, revocation and a dropped
  session unregister it. B's MCP list shows the server as running on the owner.
- At that point the tunnel needed a LAN or tailnet path; council round 2 added the relay fallback.

### Council round 1: correctness and hygiene

- **IDE filesystem.** Creating or writing through a dangling symlink is refused instead of
  following it outside the workspace. Saving a file that was deleted mid-edit now returns a
  conflict with `current_etag: null` (the IDE conflict dialog offers "Recreate with mine" or
  "Leave deleted") instead of silently recreating it.
- **Journal.** `Journal::append` writes the thread row, the domain log and the wire log in one
  SQLite transaction.
- **Auto-resume.** A due schedule on an active thread waits silently instead of emitting an
  Error every second, and a message appended to an active turn cancels the stale schedule so it
  cannot fire a duplicate prompt.
- **h4rn-connect.** The skill-sync pull cursor is derived from the rows the pull returned, so a
  concurrent push cannot be skipped. The OIDC token exchange repeats `redirect_uri`.
- **Browser wasm freshness.** `scripts/wasm.sh check` rebuilds `h4rn-transport-wasm` with the
  pinned `wasm-bindgen` CLI and a path-remapped build and fails on any difference from
  `apps/ui/src/gen/transport-wasm`; CI runs it (a new `wasm` job). The committed wasm was
  rebuilt so it no longer embeds machine paths.
- **Lint.** `bun run lint` runs `biome check --error-on-warnings`, so any warning fails instead
  of scrolling past; the stale registry comment in `h4rn-harness` is fixed.

### Council round 1 (part D)

- **Usage numbers come from Rust.** `Usage` gains `generation_ms` and `tps` (tokens per second of
  generation, tool time excluded), computed by the meter; `ThreadSummary` gains
  `context_fill_percent`. The UI formats them and no longer divides output by wall time. Threads
  recorded before this change show `—` until their next turn.
- **Removed** the never-implemented `ProxyProbe` and `ProxyConfigure` requests.
- **Mock daemon is dev-only.** `?mock=1` works under `bun run dev` and in tests; a production
  build excludes the ~5.6k lines of mock code unless built with `VITE_H4RN_MOCK=1`.
- **CI** runs the Postgres-only `h4rn-connect` suites (`postgres`, `skills`) against a
  `postgres:16` service; `docs/release.md` lists the suites that still need OrbitID.
- **Tests.** `resume_now_acknowledges_before_a_slow_first_send` and its sibling no longer race a
  5 s event budget against a 5 s harness delay.

### UI fidelity round 4

- **Apply and restart (Providers).** `ApplyProxyOverrides` interrupts the live turn of every
  routed harness, drops its session, and the next message starts one with the new routing.
  Each per-harness override now reports its `config_path` and a redacted `preview` (secrets are
  `null`, never sent).
- **Fail over…** options carry an `account_label` (the credential name without directory or
  extension) and plain-text reasons; the card and the menu name the same account.
- **Test runs** show `48 passed` / `46 passed · 2 failed` in the tool row's result column
  (`ToolStats::Tests`, parsed in Rust from the shell output).
- The phone auto-resume card has its own padded layout; opening the right panel at a narrow
  width closes the sidebar to make room; every native `<select>` is a `ChipSelect` and a lint
  rule keeps it that way; the palette gained Deny, Changes, Subagents, IDE, Right panel,
  Terminal, Sidebar and Pair a device; `bun run lint` reports 0 warnings.

### Gaps left by UI fidelity round 3

- **Pairing scopes and a real short code (ADR-042).** The Devices checkboxes now choose what the
  paired device may do (`view`, `approve`, `terminal`, `files`, `admin`); connect records and
  signs them, and the daemon refuses anything outside them. The 8-character code is Crockford
  base32, single use, expires with the QR and is rate limited; the phone pairing screen accepts
  it (`POST /v1/pairings/claim-code`). `h4rnd pair --scope` sets scopes from the terminal.
- **Fail over…** on a limit-hit thread resumes now on another model or account: the daemon names
  its default and the eligible alternatives (`ListFailoverOptions`, `FailOver`).
- **PR detail:** a Commits tab, `required` marks on checks, View log and Re-run failed (GitHub
  check suites, GitLab pipelines; disabled with the reason on Gitea/Forgejo).
- **MCP import conflicts** use one "Resolve…" popover (keep mine, use theirs, rename, skip).
- **Tool rows** show `+N −M` on edits and match counts on searches, derived in Rust for all four
  adapters (`ToolCall.stats`).
- **Sidebar footer** names the host on a fresh install and counts real hosts otherwise.

### Credentials and remote sign-in

- **Browser storage never holds a credential.** The daemon token, pairing URI, Noise
  identity/pin/session and h4rn-connect access/refresh tokens now go through one platform secret
  store: Android Keystore / iOS Keychain behind `secret_*`, the OS keychain behind `keychain_*`
  on desktop, and AES-GCM envelopes in IndexedDB under a non-extractable `CryptoKey` on the web
  (ADR-041). Legacy `localStorage`/`sessionStorage` values are migrated on first boot and
  deleted; `?token=` and `?pair=` bootstrap secrets are consumed and stripped from the address
  bar, and `sessionStorage` keeps only short-lived, non-secret OIDC state.
- **OIDC sign-in completes on every shell.** The web returns through `/auth/callback` and
  reloads the route the person left, desktop waits on the loopback callback, and mobile opens the
  provider in the system browser and returns through `h4rn://callback`. The device-code grant
  remains as "Sign in on another device"; the remote e2e now drives both the browser redirect
  and the device-code fallback (from a fresh profile, since the relay refuses a revoked device's key) against Docker OrbitID.
- **Resume now acknowledges immediately.** `ResumeNow` finishes the schedule and returns before
  the harness produces its first token, and the resumed turn reports progress through
  `ScheduleChanged` and thread events, so a realistic slow harness no longer times out the
  request. A normal `SendMessage` likewise returns once the turn is recorded, and the thread
  record is no longer locked across the harness write, so a slow adapter cannot stall thread
  listings, snapshots or settings requests with it.

### UI: layout v2

- **One left sidebar, one right panel.** The nav rail and inbox column are merged into a single
  resizable sidebar grouped by project (folder + host), with Working / Done / Settled, Snoozed,
  search, and a hosts footer that opens presence, pairing and management. Settings, including
  MCP servers and Skills, temporarily replaces the thread list with its own nav and a back button.
  The sidebar collapses with ⌘B and keeps a needs-you dot on the reveal button.
- **Thread | IDE is one segmented toggle** (⌘E). The IDE follows the selected thread's project,
  keeps its tabs per project, and says "Restored N tabs" when it returns. Its file tree moved into
  the right panel's Files tab; the header carries a thread chip back to the conversation.
- **The right panel** is scoped to the selected thread: Changes (vs agent / vs HEAD, by turn),
  Files, and Subagents. It remembers open state, width and tab, opens with ⌘⌥B, and folds into one
  Changes | Subagents sheet on a phone, where Files and the IDE are desktop-only.
- **Pull requests** no longer claims a list column: the main area shows the list and the detail.
- **Shortcuts** add ⌘B, ⌘E, ⌘⌥B, D, ⌘J, ⌘P and T, and Esc now leaves Settings and MCP & skills.
- `apps/ui/scripts/screenshots.mjs` captures the new `panel-changes`, `ide-switch`,
  `sidebar-collapsed`, `p-changes` and `p-subagents` screens alongside the existing set, and the
  Playwright e2e drives the new view toggle and right panel against the real daemon.

### Phase 7 — packaging

- **Desktop bundles** ship on Windows (MSI and NSIS), macOS (universal DMG) and Linux
  (AppImage and `.deb`) with the `h4rnd` sidecar staged per target. Tauri's updater plugin is
  configured through a generated release overlay: `H4RN_UPDATER_ENDPOINT` selects the hosted or
  self-hosted manifest, `H4RN_UPDATER_PUBKEY` pins the minisign key, and signed `.sig`/`latest.json`
  artifacts are emitted only when the signing key is present.
- **Standalone `h4rnd` archives** for Linux x86_64 glibc/musl, Linux aarch64, macOS universal and
  Windows, each with a checksum, `README.txt` and contrib service files (systemd user unit,
  launchd plist, Windows scheduled-task scripts).
- **Android release signing** reads the upload keystore from
  `ANDROID_KEYSTORE_PATH`/password/alias/key variables (CI decodes `ANDROID_KEYSTORE_BASE64`) and
  falls back to debug signing locally; the Play internal track uploads through a fastlane lane.
- **iOS Keychain secret storage** implements `secret_get`/`secret_set`/`secret_delete` in Swift
  with `kSecClassGenericPassword` and `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, matching
  the Android Keystore command surface; the CI macOS job compiles the Swift package for the
  simulator and uploads a signed TestFlight build when Apple credentials exist.
- **Relay image and chart**: `ghcr.io/<owner>/h4rn-connect` is built for `linux/amd64` and
  `linux/arm64` with Buildx on tags, compose is pinned to the published tag (with a local build
  override), and a Helm chart provides Deployment, Service, Ingress, PodDisruptionBudget and
  optional in-chart Postgres or an external database URL.
- **Tag-driven release workflow** (`.github/workflows/release.yml`) builds all targets into a
  draft release, writes `SHA256SUMS`, and generates review-only winget, Homebrew cask and AUR
  manifests. `docs/release.md` lists every secret and the release/update flow.

### UI: Quiet native redesign

- **Direction A ("Quiet native") replaces the Prompt Book look** across `apps/ui`: graphite
  neutrals with one 258° blue accent reserved for "you", Geist and Geist Mono self-hosted via
  `@fontsource-variable`, hairline panes, 4 px spacing, and the 4/6/8/12/14 radii scale.
- **Shell, inbox and thread** are rebuilt around the prototype: 212 px sidebar with hosts and
  connection line, 328 px inbox with Working / Done / Settled segments and state glyphs, a
  detail pane with provenance, an approval card above the composer, and a usage footer.
- **Subagents inspector, terminal, attachment tray/lightbox, command palette, new-thread and
  shortcuts dialogs, toasts** are restyled to the same grammar; the terminal follows the theme.
- **IDE** ports the prototype's file tree, agent-changes group, tabs, `Edit | vs HEAD | vs agent`
  control, change banner with Revert file / Accept, full-hunk diff bands and 26 px status bar.
- **Phone** gains the large-title inbox, 66 px rows, pushed thread header, compact approval card
  and review sheet, full-screen terminal with key bar, pairing consent flow, and a 14-day Usage
  chart.
- **Offline, loading, empty and partial-failure states** now use the prototype's copy and
  layout-shaped skeletons, including "Offline · threads as of HH:MM", "Showing 3 of 4 hosts",
  the paused-stream line and disabled approvals with a reason.
- **New mock-mode surfaces** for Pull requests, MCP & skills, Usage, Settings (hosts, devices,
  providers) and pairing are routed now and clearly marked as Phase 4–6 shells until their wire
  protocol lands.
- `apps/ui/scripts/screenshots.mjs` now captures the 22 prototype screens in light and dark at
  1440×900 or 390×844 into `apps/ui/screenshots/`, and `?scenario=` adds `loading`, `offline`
  and `error` states for review.

### Phase 4 — skills

- **Skill bundles** (`h4rn-skills`): directories with `SKILL.md` are
  canonicalised into a BLAKE3-addressed manifest (relative paths, sizes,
  digests, executable bits) and stored once under `skills/bundles/<version>`,
  with SQLite metadata in `skills/registry.db`. Frontmatter parsing, git
  sources pinned to a resolved commit, size/file caps and path-traversal
  validation are included.
- **Per-harness installers**: native `SKILL.md` copies for Claude Code, Codex,
  OpenCode and Antigravity, with user and project roots detected, per-machine
  enable/disable, upgrade-in-place and a `.h4rn-managed` JSON marker. A
  user-owned directory is never overwritten or removed; Codex and Antigravity
  share `.agents/skills` through the marker's harness list.
- **Import**: preview and commit scans of harness directories with
  content-hash dedupe, duplicate detection and conflict rows for a different
  version of the same name.
- **Cross-machine sync**: `h4rn-connect` endpoints `POST /v1/skills/sync/push`
  and `/pull`, Postgres migrations `0003_skills.sql`, envelope encryption at
  rest, version vectors with last-writer-wins and conflict surfacing, pull
  cursors and tombstones. The daemon refuses to upload a bundle the secret
  scanner flags and reports `Blocked` with a file/line hint.
- **Protocol**: `SkillsList/Add/Update/Remove/SetEnabled/ImportPreview/
  ImportCommit/Conflicts/ResolveConflict/Diff/SyncNow`, response bodies
  `Skills`, `SkillAdded`, `SkillDetail`, `SkillsImportPreview`,
  `SkillConflicts`, `SkillDiff`, `SkillSyncStatus`, plus transient
  `SkillsChanged` and `SyncStatus` events. TypeScript bindings regenerated.
- **Daemon glue**: `AppState.skills`, all request handlers, transient event
  forwarding, and a `ConnectClient` implementation of `SkillSyncApi`.
- **Docs and tests**: `docs/skills.md`, ADR-034/035, hermetic tests for
  frontmatter, hashing, installers, import, conflicts and secrets, a real
  Claude Code skill-pickup test behind `--ignored`, and two-registry sync tests
  run against real Postgres by `scripts/test-connect.sh`.

### Phase 4 — MCP hub

- **One hub per daemon** (`h4rn-mcp`): a loopback streamable-HTTP endpoint at `/mcp` speaking
  revisions `2025-06-18`, `2025-03-26` and `2024-11-05`, with per-thread bearer tokens stored as
  BLAKE3 digests, scopes for server and tool narrowing, `Mcp-Session-Id` handling and `DELETE`
  revocation.
- **Namespacing and providers**: `server__tool` names with deterministic `_2`/`_3` collisions, a
  reserved `h4rn` namespace and the `ToolProvider` extension point behind the built-in
  `h4rn__echo`.
- **Upstreams**: stdio, streamable HTTP and legacy SSE with lazy start, health ticks, restart
  backoff, capability caching, list-changed invalidation, timeouts, UTF-8-safe output caps and
  cancellation.
- **Durable configuration**: `mcp.db` registry for transports, enable matrices, imports and
  shares; environment variables and headers stored only as references, with values in the OS
  keychain or an encrypted-file fallback.
- **Native import**: Claude Code (`~/.claude.json`, `.mcp.json`), Codex (`config.toml`),
  OpenCode (`opencode.json`) and Antigravity (`mcp_config.json`), with a conflict preview and
  `KeepMine`/`UseTheirs`/`Rename`/`Skip` resolution.
- **Cross-machine sharing**: owner-enforced policies (allowed tools, confirmation, allowed
  devices), a second secure `/secure-mcp` tunnel carrying JSON-RPC, local audit of every remote
  call, and no credential forwarding.
- **Harness injection**: Claude, Codex, OpenCode and Antigravity receive the endpoint and bearer
  token per session; the token never appears in argv.
- **Protocol and events**: `ClientRequest::Mcp` with the full MCP operation set, legacy request
  mapping, `McpServers`/`McpServer`/`McpImportPreview`/`McpTools`/`McpCall` responses and
  transient `McpServerStatus`/`McpToolCalled` events on `Topic::Mcp`.
- **Docs and tests**: `docs/mcp.md`, ADR-036/037, real stdio-fixture and two-daemon tunnel tests,
  secret non-leak tests, and ignored real Claude/Codex injection tests (`h4rn__echo`).
### Phase 4 — cross-harness subagents

- **Child threads** (`h4rn-daemon`): `spawn_subagent` creates a normal thread
  with `parent_thread_id` and a pre-allocated `SubagentId`, so delegation reuses
  the journal, resume, usage and approval machinery. `Delegated` items and
  `SubagentSpawned`/`SubagentUpdated`/`SubagentFinished` events project the
  parent's view; child rows stay out of the top-level list and inbox.
- **Supervisor**: one daemon-wide watcher enforces turn, wall-clock and token
  budgets, maps terminal turns to `completed`/`failed`/`cancelled`/
  `budget_exceeded`, folds the last assistant text, usage and changed files into
  a result, and cleans worktrees on settle. Nesting is capped at depth 2 by
  default and harness availability is checked against the installed catalog.
- **Isolation and context**: shared workspace or a git worktree under
  `<data_dir>/subagents/<id>/worktree` on `h4rn/sa-<short id>`; context handoff
  is none, a caller summary, or selected workspace files (128 KiB cap, path
  confinement).
- **Approvals**: a child inherits the parent runtime mode; its approvals are
  mirrored to the parent as `sa:<id>` with `[Subagent]` attribution and either
  side answers both. Settling a child clears stale mirrored hands.
- **MCP tools**: `h4rn__spawn_subagent`, `get_subagent_status`,
  `get_subagent_result` (bounded wait), `cancel_subagent` and `list_subagents`,
  scoped by the parent session token. The registry now supports multiple
  providers per namespace so these coexist with `h4rn__echo`.
- **Protocol**: `ClientRequest::Subagent { SubagentRequest }`,
  `SubagentView`/budget/context/result types, `ThreadSummary.parent_thread_id`,
  `ThreadFilter.include_children`, `ResponseBody::Subagents/Subagent`,
  `ApprovalRequested.subagent_id`, and TypeScript bindings.
- **Docs and tests**: `docs/subagents.md`, ADR-038/039, unit tests for budgets
  and depth, an MCP-HTTP integration test with `FakeHarness` parent and child,
  and an ignored real Claude Code → Codex delegation test.
- **Metering, resume and durability**: child turns are metered with
  `parent_thread_id`, and `GetUsage`/reports accept `include_children` to roll a
  delegation tree into the parent's totals. Token budgets enforce the ledger's
  metered totals and wall-clock excludes time paused for a resume. A limit-hit
  child becomes `scheduled`, arms auto-resume and is mirrored to the parent
  instead of failing. Supervisor records persist in `subagents.db`, so the
  delegation card, status and result survive a daemon restart.

### Phase 5 — metering and auto-resume

- **Ledger** (`h4rn-meter`): durable, append-only SQLite rows per turn with
  input/output/cached read/cached creation tokens, cost and provenance, TTFT,
  TPS excluding tool time, generation and wall time, timestamps, workspace,
  machine, account, harness, model, provider and raw payload. Prices are a
  deterministic estimate when the harness reports no cost; unknown models stay
  visibly unpriced.
- **Metering**: per-turn observation of harness usage, first-token TTFT, tool
  call intervals, `TurnCompleted` accounting and a live `UsageTick` throttled to
  about one per second. Reports aggregate per thread, workspace, machine or
  globally, with optional hour/day buckets and thread/workspace/machine/model/
  harness grouping.
- **Proxy usage**: optional ingestion of CLIProxyAPI `/v8/management/performance`
  rows with stable ids, deduplicated against self-metered turns by
  `(thread_id, run_id)`, correlation id or same provider/model within two
  minutes. Documented in [`docs/metering.md`](docs/metering.md).
- **Limits and resets**: `LimitHit` records harness, provider/account, model,
  reset time, consumption and message; the UI can read limits, countdowns and
  reset history.
- **Auto-resume**: schedules at the provider reset plus deterministic jitter,
  capped exponential backoff with an explicit reason when the reset is unknown,
  persisted schedules across restarts, `CancelResume` and `ResumeNow`, thread
  stays Working with `AgentState::Scheduled`, queued input held, resumed turn
  uses a configurable prompt, and optional proxy failover recorded visibly on
  the schedule.
- **Protocol and daemon**: `GetUsage`, `GetLimits`, `ListResets`, `CancelResume`,
  `ResumeNow`, `SetAutoResumePolicy`; ephemeral `UsageTick`, `LimitStatus` and
  `ScheduleChanged` frames; daemon handlers, meter assembly and background
  proxy-ingest/usage-sync loops.
- **Sync**: idempotent batched ledger sync to h4rn-connect (`row_id` unique per
  account), migration `0004_usage_sync.sql`, `POST /v1/usage` and
  `GET /v1/usage/aggregate` with real Postgres aggregation.
- **Tests**: ledger math, price provenance, dedupe, scheduler restart/cancel/
  resume-now/failover, lifecycle while scheduled, a FakeHarness
  limit-hit→scheduled→resumed end-to-end test, Postgres sync idempotency and
  aggregate buckets.
- **Subagent rollup**: ledger rows carry `parent_thread_id`; `GetUsage`'s
  `include_children` folds a parent's child turns into its totals, and subagent
  token budgets enforce the same metered counts.
- **Docs**: [`docs/metering.md`](docs/metering.md) defines every metric, its
  source and the dedupe rule.

### Phase 5 — CLIProxyAPI

- **Proxy crate** (`h4rn-proxy`): profiles, capability probes, three-shape model catalogue,
  per-harness env/config recipes, credential health and failover selection, with secrets behind a
  narrow `SecretStore` trait.
- **Profiles**: multiple profiles with one default, per-profile/per-harness toggles and an optional
  per-toggle default model. The default profile wins when its toggle is on; otherwise profiles are
  tried in creation order. Metadata is a `0600` JSON file that never contains a key.
- **Feature detection**: probes only, never version strings. `HEAD /healthz`, a keyless management
  probe for `X-CPA-*` headers, authenticated route probes (usage queue, legacy usage, v8 config with
  ETag, fork performance, credentials verify, auth files, quota providers) and header-selected model
  shapes. Capabilities and catalogues are cached for five minutes and invalidated by writes or a
  failed read.
- **Models and aliases**: `/v1/models` is read once in the OpenAI shape and once with
  `Anthropic-Version` for the Claude shape, plus `/v1beta/models`; entries merge by id with
  provenance. Management-config aliases (`oauth.model-alias`, `openai-compatibility`,
  `api-keys.<provider>`) are merged when a management key is available, without copying secrets.
- **Per-harness routing**: Claude Code (root URL, token, model discovery), Codex
  (`model_providers` with `env_key` and `wire_api`), OpenCode (generated `OPENCODE_CONFIG_CONTENT`)
  and Antigravity (marked unverified). Overrides are applied through
  `SessionSpec.env_overrides`, so keys never enter `argv`.
- **Health and failover**: credential cooldowns from `auth-files` exposed as `ListProxyLimits`;
  `PickProxyFailover` implements `same_family_first`, `account_first` and `any_model`, honouring
  exclusions and active cooldowns.
- **Protocol and daemon**: new `h4rn-protocol::proxy` wire types, nine client requests, response
  bodies, ephemeral `ProxyEvent` server frames, a keychain-or-encrypted-file `DaemonSecretStore`,
  daemon wiring and session-start override application.
- **Tests**: wiremock coverage for upstream/fork probes, 404/401/403 management and client outcomes,
  header-selected shapes, ETag, aliases and secret redaction; golden environment/config tests for
  every harness; an ignored live test that runs a real Claude Code turn through a proxy and asserts
  the reply.
- **Docs**: [`docs/cliproxyapi.md`](docs/cliproxyapi.md) covers setup, the upstream-vs-fork
  capability matrix and the per-harness override table.

### Phase 6 — forges

- **Provider-agnostic forge layer** (`h4rn-forge`): one `Forge` trait with
  GitHub (REST, plus GraphQL review threads and a REST fallback), GitLab REST
  v4, and a shared Gitea/Forgejo API v1 client. Normalised PRs, diffs, review
  threads (path/line/side/resolved), general comments, checks/pipelines and
  review state live in `h4rn-protocol::forge`.
- **Repository detection** from ssh, scp-like and https git remotes, with
  credential redaction, nested GitLab namespaces, and explicit
  `forge-hosts.json` mappings for GitHub Enterprise, self-hosted GitLab,
  Gitea/Forgejo and ssh aliases; unknown hosts are never guessed.
- **Accounts and auth**: least-privilege PATs validated before storage, GitHub
  OAuth device flow (`H4RN_GITHUB_CLIENT_ID`), and explicit ask-then-import from
  `gh auth token` / `glab auth status --show-token`. Tokens live in the OS
  keychain or the encrypted-file fallback; they never reach the wire, logs or
  audit.
- **Daemon wiring** for `ForgeListAccounts`, `AddAccount`, `RemoveAccount`,
  `DetectRepo`, `ListPrs`, `GetPr`, `GetPrDiff`, `ListPrComments`, `ListChecks`,
  `Reply`, `Comment`, `OpenPr` and `HandPrToAgent`; PR writes are recorded in
  `forge-audit.jsonl`.
- **Checks polling** with backoff (`H4RN_FORGE_POLL_SECS`) and a transient
  `forges_checks_updated` event, per-account request budgets
  (`H4RN_FORGE_RPM`), `retry-after`/`x-ratelimit-reset` handling, GitHub ETag
  revalidation and bounded pagination.
- **Agent handoff** (`pr_context_for_agent`): unresolved threads, failing
  checks and diff stats rendered as compact markdown for a new or existing
  thread.
- **Docs and tests**: `docs/forges.md` (setup, scopes, self-hosted URLs,
  capability matrix), wiremock coverage for all three API shapes, a real Docker
  Gitea/Forgejo run via `scripts/test-forges.sh`, and an ignored read-only live
  GitHub test.

### Phase 3 — connect

- **Relay service** (`h4rn-connect`): axum + Postgres, self-hostable image and compose stack
  (`deploy/Dockerfile`, `deploy/docker-compose.yml`, `deploy/.env.example`), optional local
  OrbitID profile, `/healthz`, `/readyz`, JSON errors as `{code, message, hint}`, optional
  static web serving, CORS and per-IP rate limits.
- **OIDC login**: authorization code + S256 PKCE via `openidconnect` with a stored nonce,
  device authorization grant for daemons, `userinfo` fallback for thin ID tokens, refresh and
  revocation, and the OrbitID quirks documented in `docs/research.md` §9.
- **Sessions and devices**: h4rn-signed EdDSA access tokens plus rotating refresh tokens
  bound to the device's Ed25519 key through DPoP-style proofs with a per-request nonce; refresh
  rotation is single-flight per family with a 30-second idempotent successor window and
  family revocation on reuse outside it; device revocation ends its sessions.
- **Pairing**: `h4rn://pair?...` QR creation and same-user single-use claims, five-minute
  expiry, rate-limited attempts, daemon notification on claim and revoke, and machine host
  lists including WSL entries.
- **Relay WebSocket**: authenticated `/relay` for daemons and clients, opaque
  `RelayFrame { conn_id, kind, payload }` routing by connection id, pairing enforcement,
  bounded per-peer queues, payload-blind metadata logs and presence broadcasts.
- **Data and audit**: Postgres migrations for users, identities, auth flows, sessions,
  devices, machines/hosts, pairings, presence, envelope-encrypted secrets, audit log, device
  authorizations and the usage ledger; `/v1/me`, devices, machines, pairings, audit, usage and
  auth routes.
- **Shared wire types** (`h4rn-protocol::connect`) and regenerated TypeScript bindings.
- **Docs and tests**: `docs/self-hosting.md`, ADR-028–030, unit tests for pairing hashes,
  proof/envelope/crypto, refresh classification and relay routing/backpressure, plus real
  Postgres integration tests run by `scripts/test-connect.sh` and an optional live OrbitID
  test behind `--orbitid`.

### Phase 3 — remote access glue

- **Daemon login** (`h4rnd login --connect`, `h4rnd logout`): OIDC device
  authorization grant with a printed verification URL and user code, device
  registration with the daemon's transport keys, host and address publishing,
  and credentials in the OS keychain with an encrypted-file fallback
  (ChaCha20-Poly1305, `0600` sibling key) for headless Linux and WSL.
- **Authenticated relay connector**: a persistent, DPoP-proofed `/relay`
  WebSocket with exponential backoff, JSON `RelayFrame`/binary transport
  adaptation, 30 s presence pings, `DevicePaired` allowlisting and
  `DeviceRevoked` session drops. Fixed `http(s)` to `ws(s)` relay URL
  conversion.
- **Pairing CLI** (`h4rnd pair`, `--direct`): connect pairing rendered as a
  terminal QR plus `h4rn://pair` URL, and a connect-free `IKpsk2` flow whose
  QR carries LAN/tailnet candidates, the daemon key and a single-use PSK
  picked up by the running daemon.
- **Tailscale**: best-effort detection through `tailscale status --json` /
  `tailscale ip -4`, `tailscale = auto|off`, optional secure-listener bind on
  the tailnet address, and address advertisement ordered LAN, tailnet,
  relay.
- **Remote-action audit**: `thread.created`, `terminal.opened` and
  `file.written` written locally to `remote-audit.jsonl` and mirrored
  best-effort to `POST /v1/audit`.
- **Connect additions**: machine address candidates (`PUT
  /v1/machines/{id}/addresses`, migration `0002_machine_addresses.sql`) and
  the audit-ingest route.
- **Transport fixes for multi-path remote sessions**: one application-frame
  and one command channel per logical session, resumed paths that attach
  without a second loop, authoritative session close on a peer `Close`
  control, persisted allowlists, and resume-ticket handling.
- **Tests and docs**: `crates/h4rn-daemon/tests/remote.rs` with
  `scripts/test-remote.sh` (Postgres, relay and OrbitID in Docker: device
  grant, pairing, relay-only thread creation, direct→relay failover under
  2 s, revocation and audit), plus updates to `docs/transport.md` and
  `docs/self-hosting.md`.

### Phase 3 — encrypted multipath transport

- **Noise E2E per path** (`h4rn-transport/src/noise.rs`):
  `Noise_IK_25519_ChaChaPoly_BLAKE2s`, with `IKpsk2` for one-time pairing,
  X25519 device identity plus Ed25519 signing key, daemon key pinning from the
  pairing payload, an explicit allowlist/revocation policy, rekey budgets and a
  session-bound prologue.
- **Path-agnostic reliable session** (`src/session.rs`, `src/wire.rs`): 20-byte
  clear packet header, 40-byte encrypted record header, per-direction 64-bit
  sequences, cumulative ack plus 64-bit bitmap, bounded retransmit buffer,
  fragmentation/reassembly, backpressure, 256-byte padding buckets, and an
  opaque resume token carried inside the encrypted `Open` record.
- **Client multipath engine** (`src/engine.rs`): parallel Noise probes for
  direct, Tailscale and relay candidates, health pings, smoothed RTT, immediate
  failover, hysteretic switch-back, and a transient `TransportStatus` event.
- **Daemon `/secure` listener** (`h4rn-daemon`): `SecureService` over
  WebSockets, a `SecureSocket` adapter that feeds the existing protocol loop,
  detached-session resume, fail-closed `404` when disabled, and a `403` for
  plaintext `/ws` on non-loopback binds. Enabled with `H4RN_SECURE=1` and
  optional `H4RN_PAIRING_PSK`.
- **wasm client** (`h4rn-transport-wasm`, `apps/ui/src/gen/transport-wasm`):
  the same core compiled to `wasm32-unknown-unknown` with a `wasm-bindgen`
  API, plus `apps/ui/src/lib/client/secure.ts` implementing the UI `Transport`
  interface. TypeScript owns only sockets, timers and persisted opaque tokens.
- **Protocol bindings**: `TransportStatus`, path/session enums and the
  `transport_status` event, with regenerated TypeScript bindings.
- **Tests**: handshake, revoked key and PSK pairing; loss/reorder/duplication;
  failover without loss or duplication in under 2 s; hysteretic switch-back;
  resume across reconnect; a real daemon Noise `CreateThread` with
  `FakeHarness`; and a wasm handshake smoke test.
- **Docs**: ADR-028/029/030 and [`docs/transport.md`](docs/transport.md)
  covering protocol, threat model and failover.

### Phase 3 — mobile

- **Android/iOS shell** (`apps/mobile`): Tauri v2 app `dev.h4rn.app` that loads the same React
  bundle as desktop, with min SDK 26, target SDK 37, iOS 16, scaffold icons and the `h4rn://`
  URL scheme. Native customisations live in `apps/mobile/native/android/` and are copied over
  the ignored generated project.
- **Pairing** (`apps/ui/src/lib/platform/mobile.ts`): a mobile-only adapter added with one import
  in `main.tsx`. It accepts `h4rn://connect?daemon=…&token=…` on cold and warm starts, scans
  pairing QR codes through `tauri-plugin-barcode-scanner`, stores the pairing in the Android
  Keystore and localStorage, and keeps the token out of the URL. Desktop and browser builds are
  unchanged.
- **Keystore secret store** (`SecretStorePlugin.kt`): AES-256/GCM key in the Android Keystore
  behind `secret_get` / `secret_set` / `secret_delete`; invalidated keys are dropped so the app
  can pair again. iOS returns an explicit unimplemented error rather than falling back to
  plaintext.
- **Native integration**: edge-to-edge window with `env(safe-area-inset-*)`, hardware back mapped
  onto WebView history, and a network security config for the cleartext candidates. It originally
  allowed loopback and emulator host aliases in release (LAN hosts only in debug); G2 below widened
  the release base config to the LAN and tailnet candidates the daemon actually serves.
- **Build pipeline** (`scripts/mobile-android.sh`): `init`, `build`, `install`, `run`,
  `screenshot`, `logs` and `daemon` commands. Builds one debug APK per ABI (x86_64 for the
  emulator, aarch64 for phones); `run` starts a fake-harness daemon, wires
  `adb reverse tcp:47800 tcp:47800`, launches through a pairing deep link, screenshots and dumps
  logcat.
- **CI**: `mobile-android` builds both APKs on Linux with Temurin 21, SDK 36 and NDK
  27.1.12297006; `mobile-ios` builds an unsigned simulator app on `macos-latest`. TestFlight
  signing requirements and the current Android/iOS limitations are documented in
  `docs/mobile.md`.
- **Checked on the Android emulator**: a debug APK was built, installed and launched against a
  fake-harness daemon. Screenshots captured between 11:50 and 11:57 show the connect screen, the
  online inbox and a fake-harness thread, and logcat was checked for WebView, Tauri and Keystore
  errors (`apps/mobile/screenshots/`). The QR-scan permission prompt, Keystore cold-start reuse,
  hardware back and notched safe-area layout were not exercised. CI builds both APKs but does not
  run the UI; iOS is built only as an unsigned simulator app.

### Phase 2 — more harnesses, terminal, attachments and IDE mode

- **Codex adapter** (`h4rn-harness/src/codex.rs`): long-lived `codex app-server` sessions over
  newline-delimited JSON-RPC with `initialize`, `thread/start`/`thread/resume`, `turn/start`,
  server-initiated approvals, MCP injection through the thread config, provider/base-URL
  overrides, interrupt and process-group shutdown.
- **OpenCode adapter** (`h4rn-harness/src/opencode.rs`): HTTP + SSE sessions against the local
  `opencode` server, model/provider config via `OPENCODE_CONFIG_CONTENT`, permission rules per
  runtime mode, approvals, MCP registration and prompt file parts.
- **Antigravity adapter + ACP client** (`h4rn-harness/src/antigravity.rs`, `acp/`): Agent Client
  Protocol over stdio for Google Antigravity, with capability negotiation, streaming
  updates, permission requests, image/resource prompt content, usage mapping and session
  resume.
- **PTY terminal** (`h4rn-vt`, `h4rn-daemon/src/terminal/`, `apps/ui/src/features/terminal`):
  daemon-owned PTYs with credit-based binary output framing, resize/ack flow control,
  ANSI screen snapshots on attach, multiple viewers per terminal, and an xterm.js pane that
  survives reconnects.
- **Attachments** (`h4rn-protocol/src/blob.rs`, `h4rn-daemon/src/blobs/`,
  `h4rn-harness/src/attachments.rs`, `apps/ui/src/features/attachments`): BLAKE3
  content-addressed blob store with chunked streaming upload, resume, range downloads,
  thumbnails, dedupe and refcount GC; composer tray, picker, lightbox and transcript
  rendering; a shared per-harness delivery matrix (Claude native image blocks, Codex
  `localImage` paths, OpenCode `file://` image parts, ACP image blocks when negotiated,
  `@path` mentions otherwise).
- **IDE mode** (`h4rn-protocol/src/fs.rs`, `h4rn-daemon/src/fs/`, `apps/ui/src/features/ide`):
  workspace-scoped file tree, editor tabs, search, file watching, create/rename/trash/restore
  and git diffs backed by a daemon-side fs/git service with etag write-conflict detection.
- The daemon now routes both binary stream types — PTY input and blob upload chunks — over
  one WebSocket, and the UI client dispatches both while keeping transient fs events out of
  the durable event cursor.

### Phase 1c — protocol gaps closed

- Added `ProtoErrorCode::NotImplemented` and replaced the daemon's
  `Unsupported`-plus-message-prefix workaround.
- `CreateThread` now returns `thread_created` (`thread_id`, `turn_id`, summary) and
  `SendMessage` returns `message_accepted` (`thread_id`, `turn_id`, `seq`), so the UI reconciles
  optimistic rows without guessing.
- Every thread-scoped event carries `thread_id` (and `turn_id` where applicable);
  `ItemCompleted` nests its `item`. The UI no longer infers ownership from the last event.
- `RespondApproval` carries the selected `ApprovalOption.id`, and the daemon broadcasts
  `approval_resolved` with the option and answering device; every UI resolves from the stream.
- Documented `Event.seq` as a durable, daemon-global cursor. `Thread.events` is now
  `SeqEvent[]`, `Subscribe` accepts `since_seq`, and the client resumes a gap-free global replay
  across reconnects (ADR-003 note).
- `CreateThread` accepts `runtime_mode` (`approval_required`, `auto_accept_edits`,
  `full_access`) and optional `worktree`; the Claude adapter maps runtime mode to
  `--permission-mode`. Worktree creation returns `not_implemented` for now.
- Added `AgentState::Queued`, `Scheduled` and `Unreachable`, with matching UI labels and glyphs.
- The daemon resolves harnesses through `h4rn_harness::registry::builtin_adapters()`, injects
  `FakeHarness` only under `H4RN_FAKE_HARNESS=1`, persists `session_id` for `--resume`, reports
  real `last_turn` usage, and surfaces `stderr_tail` on crashes.
- Added a real Claude Code daemon e2e (`crates/h4rn-daemon/tests/e2e_claude.rs`, ignored by
  default) and a Playwright UI e2e driving `h4rnd serve` with the fake harness.

### Phase 1d — desktop shell

- `apps/desktop`: Tauri v2 shell that starts the bundled `h4rnd` daemon as a sidecar when
  `127.0.0.1:47800/healthz` is not answering, reads its access token with `h4rnd token`, and
  loads the UI with `?daemon=<ws_url>&token=<token>`. The token never reaches a log.
- Windows WSL hosts: `daemon_endpoints()` lists `wsl.exe -l -q` distributions (UTF-16 output
  decoded) and, when one is activated, starts
  `wsl.exe -d <distro> -- h4rnd serve --bind 127.0.0.1:47801`, reachable through WSL2
  localhost forwarding.
- Narrow IPC surface (ADR-001): `daemon_endpoints`, `open_external` (http/https/mailto only),
  and keychain `get`/`set`/`delete` through the OS credential store (ADR-016). Capabilities
  grant only core APIs and scoped link opening.
- Window remembers size and position (`tauri-plugin-window-state`), opens at 1440x900 with a
  960x600 minimum, keeps the native title bar, follows the system light/dark theme, and stays
  hidden until a daemon endpoint is chosen.
- Bundles: MSI + NSIS on Windows, DMG/APP on macOS, AppImage/DEB on Linux; `h4rnd` ships as an
  `externalBin` sidecar. On exit the shell stops only the daemon it spawned.
- `scripts/desktop-build-windows.sh` mirrors the repo to a Windows-local path, builds
  `h4rnd.exe` and the Tauri sidecar, runs `cargo fmt --check` and `cargo clippy -D warnings`,
  produces the installers, and verifies the packaged app end-to-end (sidecar health check,
  window screenshot, clean teardown).

### Added

- Cargo workspace (edition 2024, resolver 3) with pinned dependency versions and
  `rust-toolchain.toml`, `rustfmt.toml` and `.editorconfig`.
- `h4rn-protocol`: string-newtype IDs (UUID v7), `ClientFrame`/`ServerFrame`, the full
  `ClientRequest` surface (including placeholder terminal, filesystem, MCP, skills, proxy, usage
  and forge requests), `Event`, the ten `AgentState` values, `ThreadTab`, `ThreadSummary`,
  `HarnessKind`, usage/approval/tool-call types, and binary PTY framing.
- Generated TypeScript bindings under `apps/ui/src/gen/` with a barrel and
  `scripts/check-bindings.sh` drift check.
- `h4rn-core`: the ADR-004 lifecycle (`apply(state, event)`, `tab_for(state, now, settings)`)
  with Working/Done/Settled placement, manual override and pins, snooze wake rules, and 24-hour
  auto-settle. Event store trait with SQLite and in-memory backends, embedded migrations.
- `h4rn-harness`: async `Harness`/`HarnessSession` trait, normalized `HarnessEvent` stream,
  `FakeHarness`/`FakeSession`, and a reusable test spec.
- Compiling skeletons with tests for `h4rn-daemon`, `h4rn-connect`, `h4rn-transport`, `h4rn-vt`,
  `h4rn-mcp`, `h4rn-skills`, `h4rn-proxy`, `h4rn-meter` and `h4rn-forge`. The relay serves
  `/healthz`.
- Tauri v2 desktop and mobile shells (workspace-excluded) with deep-link, opener, dialog and
  mobile barcode-scanner plugins, capabilities and placeholder icons.
- UI scaffold: React 19, Vite 8, TypeScript strict, Tailwind v4, design tokens and fonts from
  `DESIGN.md`, shadcn primitives (button, badge, input, textarea, separator, skeleton, tabs,
  dialog, command, sheet, tooltip, scroll-area, dropdown-menu), `motion`, `thinking-orbs` and
  `blobatar`, plus a placeholder shell.
- `apps/web` static publish step.
- GitHub Actions CI: Rust format/lint/test, bindings drift, UI typecheck/lint/test/build, Tauri
  desktop compile check, and a Docker image build.
- `deploy/Dockerfile` and `deploy/docker-compose.yml` for the relay.
