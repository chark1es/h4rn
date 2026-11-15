# Google Antigravity — headless / programmatic interface for h4rn

Question: can h4rn drive Google Antigravity programmatically, and if so, how?

**Verdict: yes.** Antigravity ships a first-party headless CLI, `agy`, and it is installed on
this machine. It exposes three usable programmatic surfaces, all verified running locally in
this session:

1. **Agent Client Protocol (ACP) over stdio** — `agy acp`, JSON-RPC 2.0, NDJSON framing,
   session create / resume / list / close, streaming updates, permission requests.
2. **Streaming NDJSON print mode** — `agy --input-format stream-json --output-format stream-json -p ""`,
   one turn per stdin line, token deltas on stdout.
3. **One-shot JSON** — `agy -p "<prompt>" --output-format json`, a single result object.

This is not a guess or a community wrapper: it is the same binary and the same protocol T3 Code
already ships an Antigravity provider for (§7).

Legend: **verified** = executed on this machine in this session; **read** = present in
`--help`/source but not exercised; **UNVERIFIED** = inferred, not confirmed.

---

## 1. What is installed locally

| Check | Result |
|---|---|
| `which agy` | `~/.local/bin/agy` **verified** |
| `agy --version` | `1.5.7` **verified** |
| Binary | 280,564,136 bytes (~268 MiB), single self-contained Go binary. It carries a full Go runtime and, judging by embedded strings (`[CDP Discovery]`, Playwright-style selectors), a browser-automation client — the 24 `browser_*` tools drive a Chromium instance rather than a bundled renderer. **UNVERIFIED** whether that Chromium is bundled or downloaded on demand. |
| `which antigravity` | not found |
| `which gemini` | not found — the standalone Gemini CLI is **not** installed |
| `~/.antigravity` | does not exist |
| `~/.gemini/antigravity-cli/` | exists — this is the actual `agy` profile directory |
| `~/.gemini/config/` | exists — global config, incl. `mcp_config.json` |
| Antigravity IDE on Windows | **not installed**: `/mnt/c/Users/chark1es/AppData/Local/Programs` contains `BetterC0de, Common, cursor, DeepSeek Harness, Grok Bot, Kiro, Legcord, Microsoft VS Code, MiKTeX, Notion, orca, @parsifydesktop, Paseo, PrismLauncher, pumble-desktop, Python, SparkDesktop, t3code, Zed` — no `Antigravity`/`Antigravity IDE` entry |

Profile layout (`~/.gemini/antigravity-cli/`) — **verified** by listing:

```
antigravity-oauth-token        # saved Google credential (present)
settings.json                  # {"colorScheme":"light","trustedWorkspaces":["~/Github/h4rn"]}
conversations/<uuid>.db        # one SQLite DB per conversation
conversation_summaries.db      # index of conversations
builtin/skills/…               # bundled skills (agy-customizations, antigravity_guide, native_swarm, …)
builtin/plugins/…
log/, crashes/, updater/, cache/
```

Global config (`~/.gemini/config/`) — **verified**:

```
config.json        # {"plugins":{"swarm":{"enabled":true}}}
mcp_config.json    # 0 bytes — no MCP servers configured
projects/          # project registry, incl. default-cli-project.json
.migrated
```

Note there is no separate Antigravity **IDE** here, only the CLI. T3 Code's editor integration
looks for `antigravity-ide` / `agy-ide` (`packages/contracts/src/editor.ts:61`), which are not
present on this machine; `agy` is the standalone CLI and is present.

---

## 2. CLI surface

`agy --help` (**read**, full output captured):

| Flag | Purpose |
|---|---|
| `-p`, `--print`, `--prompt` | Run one prompt non-interactively and print the response |
| `--output-format` | `text` (default) · `json` · `stream-json` |
| `--input-format` | `text` (default) · `stream-json` (one NDJSON message per line, one turn each; requires `--output-format stream-json`) |
| `--json-schema` | Inline JSON schema or path; constrains final structured output |
| `--conversation <id>` | Resume a previous conversation by id |
| `-c`, `--continue` | Continue the most recent conversation |
| `--model <id>` | Model for the session |
| `--effort <low\|medium\|high\|xhigh\|max>` | Reasoning effort |
| `--mode <accept-edits\|auto\|plan>` | Agent execution mode |
| `--agent <name>` | Agent for the session |
| `--project <id\|name>` | Project for the session |
| `--add-dir <path>` | Add a workspace directory (repeatable) |
| `--new-project` | Create a project for this session |
| `--sandbox` | Run with terminal restrictions |
| `--dangerously-skip-permissions` | Auto-approve every tool permission request |
| `--settings <json\|path>` | Per-session settings, never persisted |
| `--disable-slash-commands` | No slash-command/skill expansion in print mode |
| `--print-timeout <dur>` | Time limit for print mode; `0` waits for the turn |
| `--remote-agent`, `--remote-control` | Remote execution / remote-control daemon |
| `--log-file` | Override log path |

Subcommands: `acp`, `agent[s]`, `changelog`, `help`, `install`, `mcp`, `mic-serve`, `models`,
`plugin[s]`, `remote-control`, `update`.

Behavioural note from `agy changelog` for 1.5.7: **the default Agent Mode changed from Default
to Auto** — headless `-p` runs now start in Auto where available. `"agentMode": ""` in
`settings.json` restores Default, and `Tool Permission: strict` overrides Auto/accept-edits so
edits still ask. This matters for h4rn: a headless run is *auto-approving* by default, which is
a different risk posture than "it prompts and nobody answers".

---

## 3. Observed invocations

### 3.1 One-shot JSON — **verified**

```sh
agy -p "Reply with exactly: AGY_OK. Nothing else." --output-format json
```

```json
{"conversation_id":"ac806dc9-63d1-49f1-b08b-dfa5f9e89036","status":"SUCCESS",
 "response":"AGY_OK\n","duration_seconds":6.928131475,"num_turns":1,
 "usage":{"input_tokens":8942,"output_tokens":8,"thinking_tokens":0,
          "cache_read_tokens":0,"total_tokens":8950}}
```

`conversation_id` is the resume handle. Exit behaviour on failure is documented in the 1.5.7
changelog: a structured `AGY_ERROR: {...}` JSON line on **stderr** (canonical status, HTTP/gRPC
code, retryability, error id) and exit code `3` instead of `1`. That is good enough to drive
from Rust without scraping prose.

### 3.2 Session resume — **verified**

```sh
CID=$(agy -p "Reply with OK1" --output-format json | jq -r .conversation_id)
agy -p "What did I just say? Answer in 5 words." --conversation "$CID" --output-format json
# {"num_turns":2, "cache_read_tokens":6884, ...}

agy -p "And now?" --continue --output-format json
# {"conversation_id":"<same id>", "num_turns":3, "cache_read_tokens":13991, ...}
```

Context is genuinely reused: `num_turns` increments and `cache_read_tokens` grows across
invocations. `--continue` picks the most recent conversation — convenient, but racy if h4rn
runs more than one agent per profile. **h4rn should always use `--conversation <uuid>`**, never
`--continue`, unless it serializes runs per profile.

Conversations persist as `~/.gemini/antigravity-cli/conversations/<uuid>.db`, so resume works
across process restarts. `--conversation` takes the UUID, not a path.

### 3.3 Streaming NDJSON input + output — **verified**, with a flag-parsing gotcha

A line of stdin is one turn. The input envelope is **`{"event":"user","message":{…}}`**; other
event names are ignored with a warning, and a missing `event` is a hard error.

```sh
printf '%s\n' '{"event":"user","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly STREAM_OK"}]}}' \
  | agy --input-format stream-json --output-format stream-json -p ""
```

Observed stdout (init line trimmed):

```json
{"event":"step_update","step_update":{"conversation_id":"8c30…","step_index":0,"state":"DONE","step_type":"user_input"}}
{"event":"step_update","step_update":{"conversation_id":"8c30…","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"STREAM_OK"}}
{"event":"step_update","step_update":{"conversation_id":"8c30…","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"\n","duration_seconds":4.73,"usage":{…}}}
{"event":"result","result":{"conversation_id":"8c30…","status":"SUCCESS","response":"STREAM_OK\n","num_turns":1,"usage":{…}}}
```

The first line is always `{"event":"init", …}` and lists the full tool set — for 1.5.7:
`view_file, list_dir, find_by_name, grep_search, read_url_content, search_web, run_command,
command_status, send_command_input, write_to_file, replace_file_content, multi_replace_file_content,
sed_file, notebook_edit, notebook_execution, browser_* (24 tools), capture_browser_screenshot,
execute_browser_javascript, generate_image, schedule, manage_task, manage_subagents,
invoke_subagent, define_subagent, read_resource, call_mcp_tool, ask_question,
ask_permission, ask_custom_permission, finish, wait, wait_5_seconds, run_workflow,
search_marketplace, list_permissions, …`
plus `"permission_mode":"request-review"`.

**Gotcha, verified the hard way:** `-p` takes an optional *value*, so `-p` placed before
`--input-format` swallows the next token as its prompt
(`Error: -p took "--input-format" as its prompt`), and `-p` as the last argument with nothing
after it fails with `flag needs an argument: -p`. The working form is
`agy --input-format stream-json --output-format stream-json -p ""` — all flags first, then
`-p` with an explicitly empty string. This is a real integration trap worth hard-coding in
h4rn's spawn builder.

---

## 4. ACP — the primary integration surface

### 4.1 Transport and handshake — **verified**

```sh
agy acp            # JSON-RPC 2.0 over stdio, NDJSON (--stdio is the default)
```

A probe sent:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":2,
  "clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true},
  "clientInfo":{"name":"h4rn-probe","version":"0.0.1"}}}
```

and received:

```json
{"jsonrpc":"2.0","result":{
  "protocolVersion":1,
  "agentCapabilities":{
    "loadSession":true,"load_session":true,
    "promptCapabilities":{"image":true,"audio":false,"embeddedContext":true},
    "sessionCapabilities":{"resume":{},"close":{},"load":{},"list":{},"additionalDirectories":{}},
    "session_capabilities":{"resume":{},"close":{},"load":{},"list":{},"additionalDirectories":{}},
    "auth":{"logout":{}},
    "_meta":{"antigravity":{"tier":true,"systemPrompt":{"append":true}}}},
  "agentInfo":{"name":"Antigravity ACP Server","version":"1.5.7"},
  "authMethods":[
    {"id":"oauth-personal","name":"Log in with Google"},
    {"id":"oauth-business","name":"Log in with Gemini Enterprise"},
    {"id":"gemini-api-key","name":"Gemini API key"},
    {"id":"agent-platform","name":"Gemini Enterprise Agent Platform"}]},
 "id":1}
```

**Version negotiation caveat:** the server answers `protocolVersion: 1` even when asked for `2`.
Do not hard-code either number — read the reply. T3 Code's checked-in fixture
(`packages/effect-acp/test/fixtures/antigravity-initialize.json`) records `protocolVersion: 2`
with `agentInfo.version: "agy_acp_server_1.1.1"`, i.e. an older ACP server build with a different
revision scheme. The installed 1.5.7 reports `1`. Both advertise the same capability set, so the
practical difference looks like numbering rather than feature surface — **UNVERIFIED** whether
any v2-only method is actually missing in 1.5.7.

### 4.2 Session lifecycle — **verified end-to-end**

```
session/new  {cwd, mcpServers:[…]}
  → {sessionId:"f2f5b34f-…", models:{availableModels:[{modelId:"gemini-4-argon-medium",…}, …]}}
session/prompt {sessionId, prompt:[{type:"text",text:"…"}]}
  → notifications: session/update …
  → {stopReason:"end_turn", usage:{inputTokens,outputTokens,cachedReadTokens,cachedWriteTokens,thoughtTokens,totalTokens}}
```

Notifications observed for a plain text turn:

| `sessionUpdate` | Payload |
|---|---|
| `available_commands_update` | `availableCommands`: `goal`, `compact`, `schedule`, `browse`, … — the skill/slash-command surface |
| `user_message_chunk` | echoed user content |
| `session_info_update` | auto-generated conversation title |
| `agent_message_chunk` | streamed assistant text |
| (with tools) tool-call lifecycle updates, plus `session/request_permission` **requests** to the client |

`session/new` also returns the account's model list. `session/set_model` and `session/set_mode`
exist as client→agent methods (T3 wires both: `packages/effect-acp/src/rpc.ts:13-15`).
`session/load` (and `session/resume` where advertised) is how ACP resumes; `session/list` and
`session/close` are advertised and supported.

Client-side methods the server will call, which h4rn must implement or decline:

- `session/request_permission` — tool approval; in ACP the **client** decides.
- `fs/read_text_file`, `fs/write_text_file` — only if advertised.
- `terminal/*` — create/output/wait/kill/release, only if advertised.

Because the client owns permissions, h4rn can enforce its own approval UI and even auto-deny,
rather than relying on `--dangerously-skip-permissions`. That is the cleanest path to
"waiting for approval" as a first-class h4rn state.

### 4.3 ACP process flags — **read** from `agy help acp`

| Flag | Why it matters |
|---|---|
| `--stdio` | NDJSON over stdio (default) |
| `--process-per-session` | Isolate each session in a worker process — good for crash containment |
| `--cloud_code_endpoint <url>` | **Endpoint override**, default `https://daily-cloudcode-pa.googleapis.com` |
| `--override_user_agent_name <name>` | UA override for HTTP requests |
| `--login` | Interactive Google login, then exit (ACP terminal auth) |
| `--app-data-dir`, `--cli_dir_name`, `--gemini-dir` | Relocate profile/config; `--gemini-dir` default `.gemini` |
| `--ide-mcp-port <n>` | Host IDE tools MCP server (requires `nc` on PATH) |
| `--include-default-customizations` | Default `true`; include bundled skills/plugins |
| `--sandbox` | Terminal restrictions |
| `--dangerously-skip-permissions` | Auto-approve |
| `--disable-telemetry` | Opt out |
| `--worker` | Internal single-session worker for an ACP front process |

Multiple profiles are therefore possible via `--app-data-dir` / `--gemini-dir`, which is how
h4rn can keep two Google accounts isolated without a container.

---

## 5. MCP configuration

Three layers, all **verified** to exist or be documented by the CLI:

| Scope | Path / mechanism |
|---|---|
| Global config file | `~/.gemini/config/mcp_config.json` (exists, currently 0 bytes) |
| CLI mutation | `agy mcp add\|remove\|list\|enable\|disable` |
| Per-session (ACP) | `session/new` params include `mcpServers: []` |
| Project hooks | `<workspace>/.agents/hooks.json`; global `~/.gemini/config/hooks.json`, `~/.gemini/antigravity-cli/hooks.json` |
| IDE bridge | `--ide-mcp-port <n>` exposes host IDE tools over MCP |

`agy mcp add` syntax (**read**):

```sh
agy mcp add fs npx -y @modelcontextprotocol/server-filesystem /work         # stdio
agy mcp add --header "Authorization: Bearer TOKEN" api https://example.com/mcp  # http
agy mcp add --env GITHUB_TOKEN=xxx gh -- docker run -i ghcr.io/github/mcp
```

Flags must precede the server name; `--` is required before commands/args beginning with `-`.
`agy mcp list` on this machine returns `No MCP servers configured.` — consistent with the empty
file.

Because `mcp_config.json` is plain JSON and `agy` reads it at startup, h4rn can either shell out
to `agy mcp add` (safer, validates) or write the file atomically before spawning. Prefer the
former for user-visible settings and the latter for ephemeral per-session servers, which are
better passed through ACP `session/new` than written to disk.

---

## 6. Auth and endpoint/model overrides

**Auth methods** (from the ACP handshake, **verified**): `oauth-personal` (Google account),
`oauth-business` (Gemini Enterprise, needs GCP project + location), `gemini-api-key`
(Gemini Developer API key), `agent-platform` (Vertex/Agent Platform via ADC or key).
`--login` performs the browser flow and exits; the credential lands in
`~/.gemini/antigravity-cli/antigravity-oauth-token`. A saved token is already present here, so
the probes ran without prompting.

**Endpoint / model overrides** — mixed confidence, flagged accordingly:

| Knob | Status | Notes |
|---|---|---|
| `--cloud_code_endpoint` (ACP) | **read** | Documented default `https://daily-cloudcode-pa.googleapis.com`. The one first-party, documented endpoint override. |
| `GOOGLE_GEMINI_BASE_URL` | **string present in binary** | Almost certainly the Gemini-API base URL override, but the exact precedence/format is **UNVERIFIED**. |
| `CLOUD_CODE_URL` | **string present in binary** | Likely an internal endpoint variable; **UNVERIFIED**. |
| `AGY_LLM_GATEWAY_MODELS` | **string present in binary + changelog** | Enterprise model gateway catalog override, referenced by the 1.5.6 changelog ("every model in the gateway's catalog, including models listed in `AGY_LLM_GATEWAY_MODELS`"). **UNVERIFIED** format. |
| `AGY_ADC_AUTH` | **string present + changelog** | Switches pay-as-you-go Google Cloud accounts that sign in with ADC. |
| `GEMINI_API_KEY` | **changelog + binary** | Enables API-key sessions; 1.5.7 changelog mentions improved behavior and quota-failure handling for it. |
| `--settings '{"agentMode":"plan"}'` | **read**, key **UNVERIFIED** | Per-session settings; the changelog confirms an `agentMode` key in `settings.json`. |

There is **no** documented public HTTP API for Antigravity itself. The two supported programmatic
surfaces are the CLI (json / stream-json) and ACP. Anything that looks like "a local server API"
would be the remote-control daemon (`agy remote-control start|status|stop`) or the IDE sidecar,
neither of which is a documented integration contract.

Models observed for this account (`agy models`, **verified** — account-dependent):

```
gemini-4-argon-{low,medium,high}      gemini-4-hydrogen-{low,medium,high,xhigh}
gemini-pro-orchestrator-g1-pro        gemini-3.8-flash-{low,medium,high}
claude-opus-5-5-{low,medium,high}     claude-sonnet-5-5-{low,medium,high}
gpt-oss-120b-medium
```

So the model list is account-scoped and can include Claude and GPT models. Do not hard-code it;
call `agy models` or read `session/new`'s `availableModels`.

---

## 7. How T3 Code integrates Antigravity

Source read: `pingdotgg/t3code`, commit `8a1811e269e81ba0ebc59db34d4dc4e24986df2c`
(2026-11-12), cloned read-only to `/tmp/h4rn-research/t3code-ag`. This is the strongest available
reference implementation because it is a shipping product doing exactly what h4rn wants.

| Aspect | T3 Code | Where |
|---|---|---|
| Driver kind | `ProviderDriverKind.make("antigravity")`, default model sentinel `antigravity-default` | `packages/contracts/src/model.ts:138,158` |
| Transport | **ACP over stdio**, adapter flag `mcpOverHttp: true` | `packages/provider-acp/src/server/adapter.ts`, `AntigravityAdapterV2.ts:214` (also noted in `docs/research/t3code.md:47`) |
| Protocol client | Own Effect-based ACP client (`effect-acp`), not a vendor SDK | `packages/effect-acp/src/{client,rpc,protocol}.ts` |
| Methods used | `session/load`, `session/set_model`, `session/set_mode`, `session/prompt`, `session/cancel`, `session/request_permission` | `packages/effect-acp/src/rpc.ts:13-15`, `:115` |
| Runtime | Managed download, or a manual **Binary path to `agy`**, or `agy` on `PATH` | `docs/user/providers-antigravity.md` §"Use a manual installation" |
| Sign-in | Four methods (Google, Gemini Enterprise, Gemini API key, Agent Platform/Vertex); T3 stores the key outside settings; **ambient `GEMINI_API_KEY` does not override** the instance's chosen method | same doc, §"Other sign-in methods" |
| Models | Account list + per-model thinking level; resumed thread keeps its model | same doc, §"Models and threads" |
| Plan / goal | Native `/plan`; `/goal` runs until complete or cancel | same doc |
| Compaction | **Unavailable via ACP** even though the CLI has `/compact`; `/clear` instead | same doc |
| Rewind | **Not supported** — Antigravity cannot rewind its conversation | same doc |
| Skills | Project: `.gemini/skills`, `.agents/skills`, legacy `.agent/skills` (first wins in that order); global: `~/.gemini/config/skills`, `~/.gemini/antigravity-cli/skills` | same doc |
| Workspace side effect | ACP creates **empty** `.gemini/skills` and `.agents/skills` in the workspace at session start | same doc; `AntigravityTextGeneration.ts` allow-lists exactly these dirs |
| Subagents | Grouped into read-only batches; cannot message/control a child | same doc |
| Utility text generation | Reuses the same ACP runtime for structured JSON (titles, commit messages), gated on `~/.gemini/config/{hooks.json,mcp_config.json}` being empty | `apps/server/src/textGeneration/AntigravityTextGeneration.ts` |
| Limits | Google five-hourly/weekly Gemini limits plus separate shared Claude/GPT limits, visible in usage UI | `docs/user/providers-antigravity.md` |

Two lessons h4rn should take directly:

1. **The empty-directory side effect is a real bug source.** Antigravity creates
   `.gemini/skills` and `.agents/skills` in the working directory. They are empty so git ignores
   them, but a `git status --porcelain`-clean check or a workspace-diff feature will trip over
   them. T3 special-cases the exact allow-list; h4rn must too.
2. **Utility calls deserve a separate gate.** T3 only allows Antigravity for title/commit-message
   generation when global hooks and MCP config are empty, because global hooks and MCP servers can
   run before a helper can deny a tool request. That is a sensible safety rule to copy: an
   "invisible" JSON helper call should not inherit a user's global tooling.

T3's fixture also confirms the ACP shape is stable across builds: same `agentInfo.name`
(`antigravity-acp` in the fixture vs `Antigravity ACP Server` in 1.5.7 — the display name changed,
the method surface did not), same four `authMethods`, same capability block.

---

## 8. Recommendation for h4rn

**Primary: ACP stdio.** Spawn `agy acp` as a long-lived child per Antigravity provider instance,
speak JSON-RPC 2.0 NDJSON, and implement the client half (`session/request_permission`,
`fs/*`, `terminal/*`). This gives h4rn, in order of value:

- **approval as a first-class state** — the client decides, so "waiting for approval" is a real
  state h4rn owns rather than a TTY prompt nobody can answer;
- **streaming** — `session/update` notifications map to h4rn's event log without a second parser;
- **resume** — `session/load` / `session/resume` plus `session/list` for a thread picker;
- **model switching** — `session/set_model` / `session/set_mode`, with the account's model list
  coming back from `session/new`.

Practical spawn line:

```
agy acp --process-per-session --app-data-dir <h4rn-profile-dir> --gemini-dir <dir>
```

Add `--sandbox` and **omit** `--dangerously-skip-permissions` if h4rn wants to enforce its own
approvals; add `--disable-telemetry` if the product policy requires it. Use one
`--app-data-dir` per Google account to isolate credentials and conversations.

**Secondary: `stream-json` print mode** when h4rn wants a turn-shaped subprocess without an ACP
client — e.g. a one-off "explain this diff" button, or a harness that cannot hold a long-lived
child. Remember the `-p ""` ordering trap (§3.3).

**Tertiary: `-p --output-format json`** for fire-and-forget structured calls (titles, commit
messages, classification). Parse `conversation_id`, `status`, `response`, `usage`; on failure
expect `AGY_ERROR: {…}` on stderr and exit code `3`.

**What not to do:** do not scrape the TUI, and do not build on the remote-control daemon or the
IDE sidecar — neither is a documented contract, and both are more surface than an ACP child.

### Session resume, precisely

| Need | Mechanism |
|---|---|
| Resume one known conversation | CLI: `--conversation <uuid>`; ACP: `session/load` (or `session/resume`) |
| Resume "whatever ran last" | CLI: `--continue` — **avoid** in h4rn, it is per-profile and racy |
| Enumerate conversations | ACP: `session/list`; on disk: `~/.gemini/antigravity-cli/conversations/*.db` (do not parse the DBs) |
| Start fresh | Omit both; a new `conversation_id` is minted |
| Close cleanly | ACP: `session/close` (must cancel in-flight work) |

### Model endpoint override

For a gateway/proxy deployment, the only documented knob is `--cloud_code_endpoint <url>` on
`agy acp`. `GOOGLE_GEMINI_BASE_URL`, `CLOUD_CODE_URL` and `AGY_LLM_GATEWAY_MODELS` exist in the
binary and the changelog references a model gateway, but their semantics are **UNVERIFIED** —
test them against the account type in use before relying on them.

### Fallback if ACP proves unusable

`stream-json` mode is a complete fallback and needs no protocol client. The standalone **Gemini
CLI is not installed here** (`which gemini` empty) and is not needed: `agy` is a superset for the
surfaces h4rn wants, and installing a second Google agent would duplicate auth and quota for no
gain. Keep Gemini CLI as a last resort, not a plan.

### Cautions

- **Account-gated.** Everything is scoped to a Google account's plan and quota; a
  `SUBSCRIPTION_REQUIRED` or usage-limit error is a normal, expected response the UI must model.
- **Auto is the default mode** in 1.5.7 headless runs. For an unattended daemon, set the mode
  explicitly (`--mode plan` or `--mode accept-edits`) and/or enforce permissions client-side.
- **No rewind.** Reverting a thread or editing-and-resending an earlier turn is not available.
  h4rn's UI must not offer it for Antigravity sessions.
- **No manual compaction over ACP.** The CLI has `/compact`; the ACP path does not expose it.
  Plan for `/clear` + a new session, or budget context accordingly.
- **Workspace mutation on session start** (`.gemini/skills`, `.agents/skills`) — allow-list it in
  any diff/clean-tree check.
- **Binary size.** ~268 MiB, plus T3 reserves ≥1.25 GiB free disk for a managed install and
  update staging. That is a real installer decision for h4rn; a manual `agy` on `PATH` avoids it.
- **Telemetry and UA.** `--disable-telemetry` and `--override_user_agent_name` exist; set them
  deliberately rather than by default.

---

## 9. Sources

Local, executed in this session:

- `which antigravity agy gemini`; `agy --version`; `agy --help`; `agy help acp`;
  `agy mcp --help`; `agy mcp add --help`; `agy mcp list`; `agy models`; `agy changelog`.
- `agy -p … --output-format json` (one-shot, §3.1).
- `agy -p … --conversation <uuid>` and `--continue` (resume, §3.2).
- `agy --input-format stream-json --output-format stream-json -p ""` with the
  `{"event":"user","message":…}` envelope (§3.3).
- A Python JSON-RPC probe against `agy acp`: `initialize`, `session/new`, `session/prompt`,
  collecting `session/update` notifications (§4.1–4.2).
- Filesystem: `~/.gemini/antigravity-cli/**`, `~/.gemini/config/**`,
  `/mnt/c/Users/chark1es/AppData/Local/Programs`.

T3 Code (`pingdotgg/t3code` @ `8a1811e`):

- `docs/user/providers-antigravity.md` — runtime install, sign-in methods, models, skills,
  subagents, limits, `/plan`, `/goal`, compaction and rewind limits.
- `packages/effect-acp/test/fixtures/antigravity-initialize.json` — ACP initialize fixture
  (`protocolVersion: 2`, older `agy_acp_server_1.1.1`).
- `packages/effect-acp/src/rpc.ts` — `session/load`, `session/set_model`, `session/set_mode`,
  `session/request_permission`.
- `packages/provider-acp/src/server/adapter.ts` — ACP transport (`acp.ndjson-jsonrpc`).
- `packages/contracts/src/model.ts`, `packages/contracts/src/editor.ts` — driver kind, default
  model, and the `antigravity-ide` / `agy-ide` editor commands.
- `apps/server/src/textGeneration/AntigravityTextGeneration.ts` — structured JSON over ACP and
  the empty-global-config gate.
- `docs/research/t3code.md:43,47,353` (this repo) — ACP family row and `mcpOverHttp: true` note.

Google docs referenced by T3 and not independently re-read here:
<https://antigravity.google/docs/getting-started>, <https://antigravity.google/docs/plans>.
