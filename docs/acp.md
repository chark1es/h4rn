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
- `session/load` resume when the agent advertises it; without it a restart starts a fresh session
  and replaces the stored id rather than failing the thread;
- plans, forwarded as plan events so a step's status stays visible;
- the models and modes the agent advertises, whether in `session/new`'s legacy `models`/`modes`
  objects or in the ACP v1 `configOptions` (selections use `session/set_config_option` for the
  latter).

Antigravity keeps its exact catalogue of models, its case-sensitive default aliases and its own
wording ("Antigravity stopped the turn: …", "Antigravity is requesting permission").

Editing an agent's command line clears the session ids of threads that used the old command, so a
later resume never replays a conversation the new command never saw.

Model choices are read from the last successful `Check` result, which is stored in the list, so a
`view`-scoped device can offer them without starting a process. A probe is admin-only; when one is
refused, the picker shows the error and lets you type a model id instead of retrying the probe.

What h4rn cannot normalize stays the agent's own business: its sign-in flow, sandbox, model names,
configuration files and any vendor-specific extensions. Antigravity keeps its own fallback
transport, login behaviour and usage-limit parsing; a generic agent gets none of those.

## Protocol surface

Writes need the `admin` pairing scope; listing is available to `view`.

| Request | Answer |
|---|---|
| `ListAcpAgents` | Configured agents with installed state, executable path, last reported version and the models/modes from the last successful probe. Environment values are masked. |
| `GetAcpAgent` | One configuration with its real environment values, for the edit form. Admin only. |
| `AddAcpAgent` | Stores one configuration; the id must be unique. |
| `UpdateAcpAgent` | Replaces one configuration. The id cannot change; a new command line clears the old version, models and modes. |
| `RemoveAcpAgent` | Removes the configuration. Threads and their history are kept. |
| `ProbeAcpAgent` | Runs the ACP handshake and one session, and reports version, models and modes. Admin only. |

The list lives in `acp-agents.json` in the daemon's data directory.

## Security

A local ACP agent is third-party code. Adding one is equivalent to running that executable in a
terminal as the daemon user:

- review the agent's source and license before adding it;
- prefer an absolute path or a binary you installed and control;
- environment overrides and the agent's own credential store are the only secret paths — h4rn never
  puts a secret in argv;
- `ListAcpAgents` masks environment values; only an admin-scoped `GetAcpAgent` returns them for the
  edit form, and `acp-agents.json` is readable by the daemon user alone;
- removing an agent from Settings does not uninstall it, revoke its credentials or erase its files.

h4rn validates the form (id shape, non-empty name and command, environment names, absolute fixed
working directory) and the executable's existence, but cannot verify that a command is trustworthy
or that it implements ACP correctly. A command that is not an ACP agent fails its `Check` with the
daemon's ACP error.
