import type {
  HarnessKind,
  McpCaller,
  McpConflictResolution,
  McpServerStatusKind,
  McpServerView,
  McpTransportView,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { i18n } from "@/lib/i18n";

/** Every harness the hub can deliver a server to. */
export const MCP_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/**
 * How each adapter receives the hub entry (docs/mcp.md).
 *
 * This is a property of the adapter, not of one server, so it is presented
 * here rather than shipped over the wire.
 */
export function deliveryLabel(harness: HarnessKind): string {
  return i18n.t(`library:mcp.delivery.${harness}`);
}

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

/** The daemon reports missing credentials as a redacted start or call error. */
function needsCredential(server: McpServerView): boolean {
  return Boolean(
    server.last_error &&
      /credential|token|api[ _-]?key|unauthori[sz]ed|not set|not configured|missing/i.test(
        server.last_error,
      ),
  );
}

/** What a server's health is, as a kind the Sidebar and the page both word. */
export type ServerStatusKind =
  | "unreachable"
  | "sharedFrom"
  | "needsCredential"
  | McpServerStatusKind;

export function serverStatus(server: McpServerView): {
  kind: ServerStatusKind;
  label: string;
  tone: "ok" | "warn" | "danger" | "off";
} {
  const status = (kind: ServerStatusKind, tone: "ok" | "warn" | "danger" | "off") => ({
    kind,
    tone,
    // A state a newer daemon adds has no words yet; it is shown as sent.
    label: i18n.t(`library:mcp.status.${kind}`, { host: server.runs_on ?? "", defaultValue: kind }),
  });
  if (server.runs_on) {
    return server.status === "error"
      ? status("unreachable", "danger")
      : status("sharedFrom", "off");
  }
  if (needsCredential(server)) return status("needsCredential", "warn");
  switch (server.status) {
    case "ready":
      return status("ready", "ok");
    case "starting":
      return status("starting", "warn");
    case "error":
      return status("error", "danger");
    default:
      return status(server.status, "off");
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
    case "harness":
      return caller.thread_id
        ? i18n.t("library:mcp.caller.harnessThread", {
            harness: harnessName(caller.harness),
            thread: caller.thread_id,
          })
        : harnessName(caller.harness);
    case "client":
      return i18n.t("library:mcp.caller.device");
    case "remote":
      return caller.machine
        ? i18n.t("library:mcp.caller.remoteFrom", { machine: caller.machine })
        : i18n.t("library:mcp.caller.remote");
  }
}

/** The four ways to settle a name conflict, in the order the popover lists them. */
export const CONFLICT_CHOICES = [
  { value: "keep_mine" },
  { value: "use_theirs" },
  { value: "rename" },
  { value: "skip" },
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
