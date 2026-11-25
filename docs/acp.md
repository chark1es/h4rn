# Local ACP agents

h4rn can run any command that speaks the [Agent Client Protocol](https://agentclientprotocol.com)
over stdio, alongside the four built-in harnesses. You configure the command; h4rn launches it
directly, drives the protocol, and shows the same thread, tool, approval, usage and resume surfaces
as a built-in harness.

There is **no registry download** in this release. h4rn does not install, pin, sign in to or update
an ACP agent: you install it, keep it current and manage its credentials. Settings says
**Managed outside h4rn** for exactly that reason.

## Add an agent

1. Open **Settings → Harnesses → Local ACP agents**.
2. Choose **Add ACP agent** and fill in:
   - **Display name** — what the picker and Settings show.
   - **Instance id** — a lowercase slug, unique on this host (`deepseek`, `my-agent`).
   - **Command** — an executable name resolved on `PATH`, or an absolute path.
   - **Arguments** — one literal argument per row.
   - **Environment** — optional extra variables, one row per `NAME=value`.
   - **Working directory** — the thread workspace (default), or one fixed absolute path.
3. Save, then choose **Check**. h4rn starts the command once, performs the ACP `initialize`
   handshake and opens a session, then stops it. The row shows the version the agent reported and
   whether its executable was found.

A configured agent appears in the **New thread** harness picker next to the built-ins. Selecting it
reads the models the agent advertises; leave the model as `default` to let the agent choose, or type
an id it advertises.

### Example

For an executable called `dsh` that takes `--profile acp`:

| Field | Value |
|---|---|
| Display name | DeepSeek Harness |
| Instance id | `deepseek` |
| Command | `dsh` |
| Arguments | `--profile`, `acp` |
| Environment | (empty) |
| Working directory | Thread workspace |

## How h4rn runs it

- **Directly, without a shell.** The executable and each argument are passed as an argv array. A
  shell never parses them, so quotes, globs, `$VARS`, pipes and `&&` are literal characters, not
  syntax. A path with spaces works; a command that needs shell parsing does not.
- **From an absolute path.** The command is resolved once, before the child starts: an absolute
  configured path that is an executable file, or a name found through an absolute `PATH` entry.
  Relative `PATH` entries are ignored, because the child starts in the thread workspace. On Windows
  `.bat` and `.cmd` files are refused: they only run through `cmd.exe`, and h4rn never uses one.
- **In its own process group.** Interrupt and shutdown reach the tools the agent started, exactly as
  they do for a built-in harness.
- **With your privileges.** The command runs as the same user as the h4rn daemon. Environment
  overrides apply to that child only; secrets you put there are visible to the agent process.
- **In the chosen working directory.** The thread workspace by default, or the fixed path you set.
- **With the h4rn MCP hub** when the thread provides one, over HTTP if the agent advertises
  `mcpCapabilities.http`, otherwise over the stdio bridge.

## What the adapter supports

The generic adapter is the shared ACP runtime used by Antigravity, so an agent that implements the
protocol gets:

- one session per thread, with prompts and streamed assistant text;
- tool calls and their output;
- plans and thought chunks (thoughts stay out of the transcript);
- `session/request_permission` approvals, answered by the thread's approval card;
- cancel, which asks the agent to stop the current turn without killing the process;
- `session/load` resume when the agent advertises `loadSession`/`sessionCapabilities.load`, and
  `session/resume` when it advertises only `sessionCapabilities.resume` (resume reattaches without
  replaying history); when it advertises neither, a restart starts a fresh session and replaces the
  stored id rather than failing the thread;
- the models and modes the agent advertises, whether in `session/new`'s legacy `models`/`modes`
  objects or in the ACP v1 `configOptions`. A generic agent that advertises both uses
  `configOptions` and selects with `session/set_config_option`; the legacy catalogue is used only
  when no v1 model or mode option is offered.

Antigravity keeps its exact catalogue of models, its case-sensitive default aliases and its own
wording ("Antigravity stopped the turn: …", "Antigravity is requesting permission", "the account
offers: …").

Editing an agent's command line or working directory clears the session ids of threads that used the
old configuration, so a later resume never replays a conversation the new command never saw. The id
is cleared in the stored thread too, not just for the attempt: a start that fails before the agent
opens a session cannot leave the old id behind to be replayed by the next try.

Model choices are read from the last successful `Check` result, which is stored in the list, so a
`view`-scoped device can offer them without starting a process. The probe reads the same catalogue a
start selects from: when an agent publishes both the legacy `models`/`modes` objects and ACP v1
`configOptions`, the picker offers the `configOptions` ids. It leaves the model as `default` until
you choose one. A probe is admin-only; when one is refused, the picker shows the error and lets you
type a model id instead of retrying the probe. A failed `Check` (a refused request, or an answer that
carries the agent's own error) falls back to the previous models; a probe whose configuration was
edited while it ran stores nothing and reports a conflict, because its answer describes the command
that no longer exists. That conflict is the same whether the superseded probe succeeded or failed.
One probe runs per agent at a time: the Check control stays busy until the original probe settles, so
an edit cannot start a second handshake. A probe never holds the connection: other requests on the
same socket keep answering while an agent finishes its handshake.

A `session/resume` or `session/load` reply may name the session without repeating its catalogue. The
agent already knows the thread's model and mode from the session it is continuing, so h4rn keeps the
saved model and sends the prompt rather than rejecting it against an empty list. The mode is the
exception: a resumed reply with no mode catalogue cannot prove which native permission level the
agent restored, and it may be wider than the turn's ceiling (for example a supervised outside
agent's). h4rn therefore refuses such a resume unless the turn asked for full access, which no native
mode can exceed, and says the thread must start a new session instead. When the resumed session
reports the model it actually restored, the thread keeps that model: the summary, the stored thread
and the next session all name it, instead of the `default` alias or an id the agent is no longer
running. Typing `default` afterwards returns the thread to the agent's own choice. A load also
replays the old conversation as session updates; h4rn consumes that history before the new turn, so
it is neither shown twice nor able to fill the event queue and stall a long conversation. A slow
consumer cannot grow the reader's memory either: every session update, unknown notification and
parse error the reader holds for it is counted against one bound of four session queues, and beyond
that the newest is dropped and counted. A permission request has its own small bound, and one that
arrives when that bound is full is answered at once with the cancelled outcome instead of being
dropped, so every request id the agent sent gets exactly one reply and none is left waiting. The
terminal reports (a transport close, stdout closing and the process exit report) take a separate
queue that is never counted or dropped, so a permission backlog can never delay an exit. Under
pressure a reserved event gets the next slot, so a flood of text never hides the event that decides
what a request means or that the agent is gone. While the consumer keeps up the lanes are merged by
arrival order, so a permission request that arrived before an update is not overtaken and a close
never arrives before the words read ahead of it. A turn's settlement also keeps its place behind the
updates already read.

The new-thread and composer mode pickers only offer a choice h4rn can map onto an advertised mode id.
The mapping is the daemon's: Approval required → `default`, `ask`, `manual` or `supervised`;
Auto-accept edits → `accept-edits`, `acceptEdits` or `accept_edits`; Plan → `plan`, `read-only` or
`readonly`; Full access → `auto`, `full-access`, `bypass` or `bypassPermissions`. An agent whose
ids map to none of those, or that advertises no mode catalogue at all, shows **Mode is managed by the
agent** in the dialog and no permission rows in the thread; h4rn does not guess the four built-in
modes for a catalogue it cannot see. A mode the agent does not advertise is refused with the
advertised list rather than silently started in another mode, and a fresh session asked for a
non-default mode without any catalogue is refused the same way (only Approval required, which asks
for no mode change, starts). Plan is offered at creation and mid-thread wherever the agent
advertises it.

Choosing the `default` model on a live agent is deferred rather than refused: ACP has no "reset to
the agent's default" method and the catalogue does not say which entry is the default, so the change
applies from the next session instead of pretending the running one switched. The summary shows
`default`, and the next turn starts a session on the agent's own choice. Once that session starts it
reports the model it restored, and the thread keeps that model (above), so `default` is a way back to
the agent's choice rather than a label that stays on the thread.

When an agent has a fixed working directory, **New thread** shows that directory before Start and
the thread's workspace (Files and Changes) is that directory, not the thread workspace a built-in
harness would use.

A plan an agent publishes is shown in the turn as a quiet checklist: each step with a mark for
pending, in progress or done. The newest plan replaces the previous one for that turn.

What h4rn cannot normalize stays the agent's own business: its sign-in flow, sandbox, model names,
configuration files and any vendor-specific extensions. Antigravity keeps its own fallback
transport, login behaviour and usage-limit parsing; a generic agent gets none of those.

## Protocol surface

Writes need the `admin` pairing scope; listing is available to `view`.

| Request | Answer |
|---|---|
| `ListAcpAgents` | Configured agents with installed state, executable path, last reported version and the models/modes from the last successful probe. Environment values are masked and the literal argument list is omitted. |
| `GetAcpAgent` | One configuration with its real argument list and environment values, for the edit form. Admin only. |
| `AddAcpAgent` | Stores one configuration; the id must be unique. |
| `UpdateAcpAgent` | Replaces one configuration. The id cannot change; any edit clears the old version, models and modes. |
| `RemoveAcpAgent` | Removes the configuration. Threads and their history are kept. |
| `ProbeAcpAgent` | Runs the ACP handshake and one session, and reports version, models and modes. Admin only. |

The list lives in `acp-agents.json` in the daemon's data directory. Entries are validated on load as
well as on save: a hand-edited entry the form would reject (a NUL in an argument, an empty command)
is dropped with one log line instead of failing a thread at spawn time. When two entries claim the
same instance id, the first is kept and the later one is dropped with a warning, so a configuration
never has two answers to "which command is this agent".

## Security

A local ACP agent is third-party code. Adding one is equivalent to running that executable in a
terminal as the daemon user:

- review the agent's source and license before adding it;
- prefer an absolute path or a binary you installed and control;
- environment overrides and the agent's own credential store are the only secret paths — h4rn never
  puts a secret in argv;
- `ListAcpAgents` masks environment values and omits the argument list; only an admin-scoped
  `GetAcpAgent` returns them for the edit form, and `acp-agents.json` is readable by the daemon user
  alone;
- removing an agent from Settings does not uninstall it, revoke its credentials or erase its files.

h4rn validates the form (id shape, non-empty name and command, environment names, absolute fixed
working directory) and the executable's existence, but cannot verify that a command is trustworthy
or that it implements ACP correctly. A command that is not an ACP agent fails its `Check` with the
daemon's ACP error.

The form accepts either platform's absolute path form (`/dir`, `C:\dir`, or a UNC share) because the
daemon that validates it may run on a different platform than the browser or phone you configured
it from; the daemon has the final say. The display-name cap counts characters, not bytes, on both
sides.
