import type {
  HarnessKind,
  McpCaller,
  McpConflictResolution,
  McpServerStatusKind,
  McpServerView,
  McpTransportView,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";

/** Every harness the hub can deliver a server to. */
export const MCP_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/**
 * How each adapter receives the hub entry (docs/mcp.md).
 *
 * This is a property of the adapter, not of one server, so it is presented
 * here rather than shipped over the wire.
 */
export const DELIVERY: Record<HarnessKind, string> = {
  claude_code: "Host config file",
  codex: "Session request",
  opencode: "Harness API",
  antigravity: "ACP or stdio bridge",
  acp: "ACP",
};

export function transportKind(transport: McpTransportView): string {
  return transport.type === "stdio" ? "stdio" : transport.type === "http" ? "HTTP" : "SSE";
}

export function transportSummary(transport: McpTransportView): string {
  if (transport.type === "stdio") {
    return [transport.command, ...transport.args].join(" ");
  }
  return transport.url;
}

export function secretNames(transport: McpTransportView): string[] {
  return transport.type === "stdio" ? transport.env_keys : transport.header_keys;
}

export function secretLabel(transport: McpTransportView, count = 2): string {
  if (transport.type === "stdio")
    return count === 1 ? "environment variable" : "environment variables";
  return count === 1 ? "header" : "headers";
}

/** The daemon reports missing credentials as a redacted start or call error. */
function needsCredential(server: McpServerView): boolean {
  return Boolean(
    server.last_error &&
      /credential|token|api[ _-]?key|unauthori[sz]ed|not set|not configured|missing/i.test(
        server.last_error,
      ),
  );
}

export function serverStatus(server: McpServerView): {
  label: string;
  tone: "ok" | "warn" | "danger" | "off";
} {
  if (server.runs_on) {
    return server.status === "error"
      ? { label: "Unreachable", tone: "danger" }
      : { label: `Shared from ${server.runs_on}`, tone: "off" };
  }
  if (needsCredential(server)) return { label: "Needs credential", tone: "warn" };
  switch (server.status) {
    case "ready":
      return { label: "Healthy", tone: "ok" };
    case "starting":
      return { label: "Starting", tone: "warn" };
    case "error":
      return { label: "Error", tone: "danger" };
    case "disabled":
      return { label: "Off", tone: "off" };
    case "remote":
      return { label: "Remote", tone: "off" };
    case "stopped":
      return { label: "Stopped", tone: "off" };
    default:
      return { label: server.status, tone: "off" };
  }
}

export function statusLabel(status: McpServerStatusKind): string {
  switch (status) {
    case "ready":
      return "Healthy";
    case "starting":
      return "Starting";
    case "error":
      return "Error";
    case "disabled":
      return "Off";
    case "remote":
      return "Remote";
    case "stopped":
      return "Stopped";
    default:
      return status;
  }
}

export function harnessName(harness: HarnessKind): string {
  return HARNESS_LABELS[harness];
}

/** A conflict is reported by the daemon as a redacted error string. */
export function conflictText(server: McpServerView): string | null {
  const error = server.last_error;
  if (!error) return null;
  if (/already has a server named|name conflict/i.test(error)) return error;
  return null;
}

export function isRemote(server: McpServerView): boolean {
  return Boolean(server.runs_on);
}

export function enabledFor(
  server: McpServerView,
  harness: HarnessKind,
  machine: string | null,
): boolean {
  const byHarness = server.enabled.by_harness.find((entry) => entry.harness === harness);
  const harnessOn = byHarness?.enabled ?? true;
  if (!machine) return harnessOn;
  const byMachine = server.enabled.by_machine.find((entry) => entry.machine === machine);
  return harnessOn && (byMachine?.enabled ?? true);
}

/** Who asked for a tool call, in words rather than protocol variants. */
export function callerLabel(caller: McpCaller): string {
  switch (caller.kind) {
    case "harness": {
      const thread = caller.thread_id ? ` · ${caller.thread_id}` : "";
      return `${harnessName(caller.harness)}${thread}`;
    }
    case "client":
      return "This device";
    case "remote": {
      const machine = caller.machine ? ` · ${caller.machine}` : "";
      return `Remote${machine}`;
    }
  }
}

/** The four ways to settle a name conflict, in the order the popover lists them. */
export const CONFLICT_CHOICES = [
  {
    value: "keep_mine",
    label: "Keep mine",
    hint: "h4rn's server stays; the harness's copy is ignored",
  },
  { value: "use_theirs", label: "Use theirs", hint: "The harness's configuration replaces mine" },
  { value: "rename", label: "Rename theirs", hint: "Import the harness's server under a new name" },
  { value: "skip", label: "Skip", hint: "Decide later; nothing changes" },
] as const;

export type ConflictChoice = (typeof CONFLICT_CHOICES)[number]["value"];

/**
 * The typed resolution for a choice, or `null` when it cannot be sent yet
 * (a rename with no name). The daemon validates the name itself.
 */
export function conflictResolution(
  choice: ConflictChoice,
  rename: string,
): McpConflictResolution | null {
  switch (choice) {
    case "keep_mine":
      return { action: "keep_mine" };
    case "use_theirs":
      return { action: "use_theirs" };
    case "skip":
      return { action: "skip" };
    case "rename": {
      const name = rename.trim();
      return name ? { action: "rename", name } : null;
    }
  }
}
