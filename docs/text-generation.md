# Thread titles written by a model

A new thread appears as soon as its first prompt is accepted, with the first line of that prompt
(or the workspace folder name when the prompt starts with a blank line) as its title. A moment
later, h4rn can replace that title with a shorter one written by a model.

Picked in **Settings → Threads → Title model**:

- **Off** keeps the first-line title. Nothing leaves the machine.
- **Coding agent** runs an installed CLI once, non-interactively: Claude Code (`claude -p`) or
  Codex (`codex exec`). It runs in an empty temporary folder with no tools, a 30-second timeout and
  no session to resume. Claude Code uses its `haiku` alias; Codex uses its configured default
  model. The picker lists only installed agents that have this one-shot mode.
- **Text generator** calls a model API that only writes text, configured under
  **Settings → Providers → Text generators**. Two shapes are supported:
  - **OpenAI-compatible** chat completions: OpenAI, OpenRouter, or any server that copies the
    shape, such as Ollama (`http://localhost:11434/v1`) or LM Studio. The key is optional.
  - **Anthropic** messages (`https://api.anthropic.com`).

The setting lives on the daemon, not in the app, so every device sees the same choice and titles
are written even when no app is open.

## What is sent

Only two things:

1. the text of the thread's **first prompt**, and
2. the **name of the workspace folder** (the last path component).

The prompt is capped at 4,000 characters. No files, diffs, paths, environment variables or thread
history are sent. The request asks for a single plain-text title of at most 60 characters. A
reply that is empty, more than one line, or longer than a title is rejected; the first-line title
stays.

## Keys

Generator keys live on the daemon host, in the OS keychain when one is available and otherwise in
the transport crate's encrypted `0600` secret file (ADR-016), the same storage as forge tokens.
The daemon never returns a key: the protocol reports only whether one exists, and the app shows
"Key stored on <host>". Adding, changing, removing or testing a generator needs the `admin` scope
on a paired device; listing generators and reading the title setting needs only `view`.

## Base URLs

A generator base URL must use `https` anywhere on a network. Plain `http` is accepted only for a
loopback host: `localhost`, `127.0.0.1` or `[::1]`. A local server is reached from the daemon, so
on a remote host `localhost` means that host, not the device showing the settings.

For an OpenAI-compatible generator the base URL includes the API prefix, for example
`https://api.openai.com/v1`; h4rn appends `/chat/completions`. For Anthropic h4rn appends
`/v1/messages` (or `/messages` when the base already ends in `/v1`).

## Failures, renames and subagents

- Any failure — no backend, timeout, a rejected key, an empty or oversized answer — keeps the
  first-line title and writes one line to the daemon log. The thread is never left untitled.
- A title you rename yourself (thread menu → **Rename…**) is final. A generated title never
  replaces it, including when a rename lands while the model is still answering; the rename wins.
- Subagent threads keep the title their delegation task gave them; h4rn does not start a second
  generation for them.
- Only one generation runs per thread.

## Testing a generator

**Test** sends a tiny prompt ("Reply with the single word: ok") through the selected generator and
model. On success it shows the reply and the round-trip time. On failure it shows the provider's
own message — for example an HTTP status and body — so a wrong model name or a revoked key is
visible instead of being guessed at.

## Related

- [ADR-016](decisions.md) — host-local secrets.
- [Forges](forges.md) — the same key storage for forge tokens.
