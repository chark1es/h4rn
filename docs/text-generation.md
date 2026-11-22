# Thread titles written by a model

A new thread appears as soon as its first prompt is accepted, with the first line of that prompt
(or the workspace folder name when the prompt starts with a blank line) as its title. A moment
later, h4rn can replace that title with a shorter one written by a model.

Picked in **Settings → Threads → Title model**:

- **Off** keeps the first-line title. Nothing leaves the machine.
- **Coding agent** runs an installed CLI once, non-interactively: Claude Code (`claude -p`) or
  Codex (`codex exec`). With no explicit agent picked, h4rn uses **the thread's own harness** when
  that harness has a one-shot mode; a thread on any other harness keeps its first-line title. A
  prompt is never handed to a different provider without being asked. The picker lists only
  installed agents that have this one-shot mode.
- **Text generator** calls a model API that only writes text, configured under
  **Settings → Providers → Text generators**. Two shapes are supported:
  - **OpenAI-compatible** chat completions: OpenAI, OpenRouter, or any server that copies the
    shape, such as Ollama (`http://localhost:11434/v1`) or LM Studio. The key is optional.
  - **Anthropic** messages (`https://api.anthropic.com`).

The setting lives on the daemon, not in the app, so every device sees the same choice and titles
are written even when no app is open. Removing the generator a title setting points at turns the
setting **Off**; it never falls back to a coding agent the person did not choose.

## What is sent

Only two things:

1. the text of the thread's **first prompt**, and
2. the **name of the workspace folder** (the last path component).

The prompt is capped at 4,000 characters. No files, diffs, paths, environment variables or thread
history are sent.

The trusted instruction ("write one short title…") is kept apart from that text. Claude Code gets
it as its system prompt (`--system-prompt`); Codex, which has no such flag, gets it once above the
untrusted text. The prompt itself is wrapped once in `<untrusted_request>` tags and is never
repeated, so a prompt that looks like an instruction is still only data.

## How the coding agents are isolated

Both CLIs run in an empty temporary directory that is removed afterwards, with a **30-second
deadline** covering version discovery, the stdin write, the run and the cleanup. A run that
overruns is killed, and the first-line title stays.

- **Claude Code** gets no tools (`--tools ""`), no slash commands, no MCP configuration
  (`--strict-mcp-config` with no config file) and no user, project or local settings source
  (`--setting-sources ""`), so hooks and plugins from the host's own configuration never load. It
  cannot ask a question (`--permission-mode dontAsk`) and uses the `haiku` alias.
- **Codex** runs `exec` with `--ephemeral` and a read-only sandbox, MCP servers switched off
  (`-c mcp_servers={}`), web search and the patch tool off, and no repo check. It uses its
  configured default model.

The output of either CLI is read up to 64 KiB; anything larger is treated as a model that ignored
the prompt, not a title. A provider HTTP body is capped the same way.

## Keys

Generator keys live on the daemon host, in the OS keychain when one is available and otherwise in
the transport crate's encrypted `0600` secret file (ADR-016), the same storage as forge tokens.
The daemon never returns a key: the protocol reports only whether one exists, and the app shows
"Key stored on <host>". Adding, changing, removing or testing a generator needs the `admin` scope
on a paired device; listing generators and reading the title setting needs only `view`.

A key is scrubbed from everything the daemon builds or hands back: provider error bodies, Test
results, logs and diagnostics — as the bare key and in its `Bearer …` and `x-api-key: …` forms,
always before the text is truncated so no fragment survives. A model reply that echoes the stored
key is refused and no title is applied.

## Base URLs

A generator base URL must use `https` anywhere on a network. Plain `http` is accepted only for a
loopback host: `localhost`, `127.0.0.1` or `[::1]`. A local server is reached from the daemon, so
on a remote host `localhost` means that host, not the device showing the settings.

For an OpenAI-compatible generator the base URL includes the API prefix, for example
`https://api.openai.com/v1`; h4rn appends `/chat/completions`. For Anthropic h4rn appends
`/v1/messages` (or `/messages` when the base already ends in `/v1`).

No redirect is ever followed. A `3xx` answer is reported as an error, so a key and a prompt can
never be forwarded to a host the person did not configure.

OpenAI-compatible requests do not send `max_tokens`; the title sanitizer caps the output anyway.
For OpenAI's own API (`api.openai.com`) h4rn sends `max_completion_tokens: 64`, the name that
endpoint uses. Local servers get no cap field at all.

A generator's base URL, kind, model and key are read as **one snapshot**, and an update writes the
key and the endpoint together under the same lock. A title job therefore uses either the old
configuration or the new one, never the old key sent to a new URL.

## The title itself

The first non-empty line is used. Control characters and the bidirectional override controls
(U+202A–U+202E, U+2066–U+2069) are removed first. A line longer than 240 characters is rejected as
a reply that is not a title; anything shorter is cut to 60 characters. Wrapping quotes, a leading
`Title:` and trailing punctuation are removed.

## Failures, renames and subagents

- Any failure — no backend, timeout, a rejected key, an empty or oversized answer — keeps the
  first-line title and writes one line to the daemon log. The thread is never left untitled.
- A title you rename yourself (thread menu → **Rename…**) is final. A generated title never
  replaces it, including when a rename lands while the model is still answering; the rename wins.
  The rename dialog keeps what you type while it is open, even if a generated title arrives, and
  saving an untouched draft still records your rename when the title changed underneath.
- A generated title that lands while a client is still creating the thread is not rolled back by
  the create reply: the client keeps the newer summary. Both arrival orders show the same title.
- Subagent threads keep the title their delegation task gave them; h4rn does not start a second
  generation for them.
- Only one generation runs per thread.

## Storage

The generator list and the title setting are two JSON files under `text-generation/` in the daemon
data directory. Every read-modify-write holds one lock through the save, the file is written to a
unique temporary file, fsynced and renamed, and a new configuration is published in memory only
after that write has landed. A crash therefore leaves the old file or the new one, never half of
either, and concurrent saves lose nothing.

## Testing a generator

**Test** sends a tiny prompt ("Reply with the single word: ok") through the selected generator and
model. On success it shows the reply and the round-trip time. On failure it shows the provider's
own message — for example an HTTP status and body — so a wrong model name or a revoked key is
visible instead of being guessed at. The provider has 30 seconds to answer and the app waits 40,
so the honest error arrives before the client gives up. The test runs on its own task: the same
connection keeps answering other requests while it is pending.

## Related

- [ADR-016](decisions.md) — host-local secrets.
- [Forges](forges.md) — the same key storage for forge tokens.
