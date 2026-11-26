/**
 * The MCP servers surface state.
 *
 * The daemon owns every decision: this module only sends typed `McpRequest`
 * values, keeps the last `McpServerView` list, and applies the transient
 * `McpServerStatus` / `McpToolCalled` events. Toggles are optimistic because a
 * switch must feel immediate; a failed request rolls the matrix back and says
 * what happened in a toast.
 */
import { useSyncExternalStore } from "react";
import type {
  Event,
  HarnessKind,
  HostId,
  McpCaller,
  McpConflictResolution,
  McpEnabledMatrix,
  McpImportPreview,
  McpRequest,
  McpServerInput,
  McpServerView,
  McpShareApproval,
  McpSharePolicy,
  McpTestCallResult,
  McpToolInfo,
  ProtoError,
  ResponseBody,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { i18n } from "@/lib/i18n";
import { joinSentences } from "@/lib/i18n/sentences";
import { getClient, getState, pushToast } from "@/lib/store/appStore";

export type McpState = {
  servers: McpServerView[];
  loading: boolean;
  refreshing: boolean;
  /** Why the list could not load, kept as sent so a client error relabels. */
  error: ProtoError | null;
  /** Which request failed, worded by the page as `library:errors.actions.<kind>`. */
  errorAction: McpAction | null;
  /** Wall-clock time of the last authoritative list or status event. */
  checkedAt: number | null;
  /** `server` -> tools from the last `ListTools` call. */
  tools: Record<string, McpToolInfo[]>;
  /** Last diagnostic call per server. */
  calls: Record<string, McpTestCallResult>;
  /** Recent live tool calls, newest first. */
  activity: McpToolActivity[];
  /** In-flight mutations, keyed by `name:kind`. */
  pending: Record<string, boolean>;
  /** Shared tool calls waiting for this machine's decision, oldest first. */
  approvals: McpShareApproval[];
};

/** A request whose failure the MCP page shows in its error banner. */
export type McpAction = "listServers";

export type McpToolActivity = {
  id: string;
  server: string;
  tool: string;
  qualifiedName: string;
  caller: McpCaller;
  ok: boolean;
  durationMs: number;
  error: string | null;
  at: string;
};

const MAX_ACTIVITY = 40;

const initialState: McpState = {
  servers: [],
  loading: false,
  refreshing: false,
  error: null,
  errorAction: null,
  checkedAt: null,
  tools: {},
  calls: {},
  activity: [],
  pending: {},
  approvals: [],
};

let state: McpState = initialState;
const listeners = new Set<() => void>();

function set(patch: Partial<McpState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export function getMcpState(): McpState {
  return state;
}

/** Test seam: replace the protocol client with a deterministic responder. */
let requester: ((request: McpRequest) => Promise<ResponseBody>) | null = null;

export function setMcpRequesterForTests(
  next: ((request: McpRequest) => Promise<ResponseBody>) | null,
): void {
  requester = next;
}

async function send(request: McpRequest): Promise<ResponseBody> {
  if (requester) return requester(request);
  return getClient().request({ type: "mcp", request });
}

export function resetMcpStore(): void {
  state = initialState;
  for (const listener of listeners) listener();
}

/** Subscribes one component to a slice of the MCP surface. */
export function useMcp<T>(selector: (value: McpState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

function sortServers(servers: McpServerView[]): McpServerView[] {
  return [...servers].sort((left, right) => left.name.localeCompare(right.name));
}

function sortApprovals(approvals: McpShareApproval[]): McpShareApproval[] {
  return [...approvals].sort((left, right) => {
    const byTime = Date.parse(left.requested_at) - Date.parse(right.requested_at);
    return byTime === 0 ? left.approval_id.localeCompare(right.approval_id) : byTime;
  });
}

/** Approvals that have not already expired locally. */
export function pendingMcpApprovals(
  approvals: McpShareApproval[] = state.approvals,
  now: number = Date.now(),
): McpShareApproval[] {
  return approvals.filter((approval) => {
    const expires = Date.parse(approval.expires_at);
    return Number.isNaN(expires) || expires > now;
  });
}

function replaceServer(server: McpServerView): McpServerView[] {
  const exists = state.servers.some((entry) => entry.name === server.name);
  return sortServers(
    exists
      ? state.servers.map((entry) => (entry.name === server.name ? server : entry))
      : [...state.servers, server],
  );
}

function fail(action: McpAction, error: unknown): ProtoError {
  const proto = toProtoError(error);
  set({ loading: false, refreshing: false, error: proto, errorAction: action });
  return proto;
}

/** Lists every server, keeping the previous rows on screen while refreshing. */
export async function loadMcpServers(options: { force?: boolean } = {}): Promise<void> {
  if (state.loading || (state.refreshing && !options.force)) return;
  set({
    loading: state.servers.length === 0,
    refreshing: state.servers.length > 0,
    error: null,
    errorAction: null,
  });
  try {
    const response = await send({ type: "list_servers" });
    if (response.type !== "mcp_servers") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the server list.`);
    }
    set({
      servers: sortServers(response.servers),
      loading: false,
      refreshing: false,
      error: null,
      errorAction: null,
      checkedAt: Date.now(),
    });
  } catch (error) {
    fail("listServers", error);
  }
}

/** Adds a server or replaces the configuration of one with the same name. */
export async function upsertMcpServer(input: McpServerInput): Promise<McpServerView | null> {
  const key = `${input.name}:upsert`;
  set({ pending: { ...state.pending, [key]: true } });
  try {
    const response = await send({ type: "upsert", input });
    if (response.type !== "mcp_server") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the saved server.`);
    }
    set({ servers: replaceServer(response.server), checkedAt: Date.now() });
    return response.server;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.saveFailed", { name: input.name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.nothingChanged"), proto.hint]),
      variant: "error",
    });
    return null;
  } finally {
    const pending = { ...state.pending };
    delete pending[key];
    set({ pending });
  }
}

/** Removes a server and its secrets. */
export async function removeMcpServer(name: string): Promise<boolean> {
  const before = state.servers;
  set({
    servers: state.servers.filter((server) => server.name !== name),
    pending: { ...state.pending, [`${name}:remove`]: true },
  });
  try {
    await send({ type: "remove", name });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ servers: before });
    pushToast({
      title: i18n.t("shell:mcp.removeFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.stillConfigured"), proto.hint]),
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[`${name}:remove`];
    set({ pending });
  }
}

function matrixWithHarness(
  matrix: McpEnabledMatrix,
  harness: HarnessKind,
  enabled: boolean,
): McpEnabledMatrix {
  const byHarness = matrix.by_harness.filter((entry) => entry.harness !== harness);
  byHarness.push({ harness, enabled });
  return { ...matrix, by_harness: byHarness };
}

function matrixWithMachine(
  matrix: McpEnabledMatrix,
  machine: HostId,
  enabled: boolean,
): McpEnabledMatrix {
  const byMachine = matrix.by_machine.filter((entry) => entry.machine !== machine);
  byMachine.push({ machine, enabled });
  return { ...matrix, by_machine: byMachine };
}

export type McpToggleTarget = { harness?: HarnessKind; machine?: HostId };

/**
 * Toggles one harness or machine switch.
 *
 * The switch moves before the request leaves; a rejection restores the prior
 * matrix so the screen never claims a state the daemon refused.
 */
export async function setMcpEnabled(
  name: string,
  target: McpToggleTarget,
  enabled: boolean,
): Promise<boolean> {
  const current = state.servers.find((server) => server.name === name);
  if (!current) return false;
  const before = current.enabled;
  const optimistic = target.harness
    ? matrixWithHarness(before, target.harness, enabled)
    : target.machine
      ? matrixWithMachine(before, target.machine, enabled)
      : before;
  const dimension = target.harness ?? target.machine ?? "all";
  const key = `${name}:toggle:${dimension}`;
  set({
    servers: replaceServer({ ...current, enabled: optimistic }),
    pending: { ...state.pending, [key]: true },
  });
  try {
    const response = await send({
      type: "set_enabled",
      name,
      harness: target.harness ?? null,
      machine: target.machine ?? null,
      enabled,
    });
    if (response.type !== "mcp_server") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the updated server.`);
    }
    set({ servers: replaceServer(response.server), checkedAt: Date.now() });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ servers: replaceServer({ ...current, enabled: before }) });
    pushToast({
      title: i18n.t(enabled ? "shell:mcp.turnOnFailed" : "shell:mcp.turnOffFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.switchBack"), proto.hint]),
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[key];
    set({ pending });
  }
}

/** Reads a native harness config and returns a preview; nothing is written. */
export async function previewMcpImport(
  harness: HarnessKind,
  workspace: string | null,
): Promise<McpImportPreview | null> {
  try {
    const response = await send({ type: "import", harness, workspace });
    if (response.type !== "mcp_import_preview") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not an import preview.`);
    }
    return response.preview;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.readConfigFailed"),
      description: joinSentences([proto.message, proto.hint ?? i18n.t("shell:mcp.checkPath")]),
      variant: "error",
    });
    return null;
  }
}

/** Applies every non-conflicting candidate from a preview. */
export async function applyMcpImport(
  harness: HarnessKind,
  workspace: string | null,
): Promise<boolean> {
  try {
    const response = await send({ type: "apply_import", harness, workspace });
    if (response.type !== "mcp_servers") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the imported servers.`);
    }
    set({ servers: sortServers(response.servers), checkedAt: Date.now() });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.importFailed"),
      description: joinSentences([proto.message, i18n.t("shell:mcp.noneAdded"), proto.hint]),
      variant: "error",
    });
    return false;
  }
}

/** Settles one import conflict. Conflicts need an explicit answer. */
export async function resolveMcpConflict(
  harness: HarnessKind,
  name: string,
  resolution: McpConflictResolution,
  workspace: string | null,
): Promise<boolean> {
  try {
    const response = await send({ type: "resolve_conflict", harness, name, resolution, workspace });
    if (response.type === "mcp_server") {
      set({ servers: replaceServer(response.server), checkedAt: Date.now() });
    } else if (response.type === "ack") {
      await loadMcpServers({ force: true });
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.resolveFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.conflictOpen"), proto.hint]),
      variant: "error",
    });
    return false;
  }
}

/** Shares a local server with another machine, with an owner-enforced policy. */
export async function shareMcpServer(
  name: string,
  toMachine: HostId,
  policy: McpSharePolicy,
): Promise<boolean> {
  try {
    const response = await send({ type: "share", name, to_machine: toMachine, policy });
    if (response.type !== "mcp_server") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the shared server.`);
    }
    set({ servers: replaceServer(response.server) });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.shareFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.stillLocal"), proto.hint]),
      variant: "error",
    });
    return false;
  }
}

/** Revokes one machine's grant. */
export async function unshareMcpServer(name: string, machine: HostId): Promise<boolean> {
  try {
    await send({ type: "unshare", name, machine });
    await loadMcpServers({ force: true });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.unshareFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.stillReachable")]),
      variant: "error",
    });
    return false;
  }
}

/** Lists the tools one server exposes, starting it when needed. */
export async function loadMcpTools(name: string): Promise<McpToolInfo[]> {
  const key = `${name}:tools`;
  set({ pending: { ...state.pending, [key]: true } });
  try {
    const response = await send({ type: "list_tools", name });
    if (response.type !== "mcp_tools") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the tool list.`);
    }
    set({ tools: { ...state.tools, [name]: response.tools }, checkedAt: Date.now() });
    return response.tools;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.toolsFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:mcp.stillStarting"), proto.hint]),
      variant: "error",
    });
    return [];
  } finally {
    const pending = { ...state.pending };
    delete pending[key];
    set({ pending });
  }
}

/** Calls one tool with the local user's authority. */
export async function testMcpTool(
  name: string,
  tool: string,
  args: unknown,
): Promise<McpTestCallResult | null> {
  const key = `${name}:call:${tool}`;
  set({ pending: { ...state.pending, [key]: true } });
  try {
    const response = await send({ type: "test_call", name, tool, arguments: args });
    if (response.type !== "mcp_call") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not a call result.`);
    }
    set({ calls: { ...state.calls, [name]: response.result }, checkedAt: Date.now() });
    return response.result;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.callFailed", { tool: `${name}__${tool}` }),
      description: joinSentences([proto.message, proto.hint ?? i18n.t("shell:mcp.checkServer")]),
      variant: "error",
    });
    return null;
  } finally {
    const pending = { ...state.pending };
    delete pending[key];
    set({ pending });
  }
}

/** Re-reads the shared calls waiting for this machine's decision. */
export async function loadMcpApprovals(): Promise<void> {
  try {
    const response = await send({ type: "pending_approvals" });
    if (response.type !== "mcp_approvals") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the pending approvals.`);
    }
    set({ approvals: sortApprovals(response.approvals) });
  } catch (error) {
    // The inbox must not block on a nicety; the event stream will repair it.
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:mcp.approvalsFailed"),
      description: joinSentences([proto.message, proto.hint]),
      variant: "error",
    });
  }
}

/**
 * Approves or denies one shared tool call.
 *
 * The card disappears immediately; a rejection restores it so the owner never
 * believes a call ran when it did not.
 */
export async function respondMcpApproval(
  approvalId: string,
  decision: "approve" | "deny" | "cancel",
): Promise<boolean> {
  const before = state.approvals;
  set({
    approvals: state.approvals.filter((approval) => approval.approval_id !== approvalId),
  });
  try {
    await send({ type: "respond_approval", approval_id: approvalId, decision });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ approvals: before });
    pushToast({
      title: i18n.t("shell:mcp.answerFailed"),
      description: joinSentences([proto.message, i18n.t("shell:mcp.callBlocked"), proto.hint]),
      variant: "error",
    });
    return false;
  }
}

// ---------------------------------------------------------------------------
// Transient daemon events
// ---------------------------------------------------------------------------

let unsubscribe: (() => void) | null = null;
let unsubscribeReady: (() => void) | null = null;

/** Subscribes to hub events once; call from the surface's first mount. */
export function ensureMcpEvents(): void {
  if (unsubscribe) return;
  const client = getClient();
  unsubscribe = client.onEvent(({ event }) => applyMcpEvent(event));
  // Approvals are transient. Re-read them after every handshake so a reconnect
  // never loses a call that is still waiting; before the first handshake the
  // ready callback does the read.
  unsubscribeReady = client.onReady(() => {
    void loadMcpApprovals();
  });
  if (getState().connection.status === "online") void loadMcpApprovals();
}

export function stopMcpEvents(): void {
  unsubscribe?.();
  unsubscribe = null;
  unsubscribeReady?.();
  unsubscribeReady = null;
}

/** Applies one event; exported so tests do not need a socket. */
export function applyMcpEvent(event: Event): void {
  if (event.type === "mcp_server_status") {
    const server = state.servers.find((entry) => entry.name === event.server);
    if (!server) return;
    set({
      servers: replaceServer({
        ...server,
        status: event.status,
        tool_count: event.tool_count,
        last_error: event.message,
      }),
      checkedAt: Date.parse(event.at) || Date.now(),
    });
    return;
  }
  if (event.type === "mcp_share_approval_requested") {
    const approvals = state.approvals.filter(
      (approval) => approval.approval_id !== event.approval.approval_id,
    );
    set({ approvals: sortApprovals([...approvals, event.approval]) });
    return;
  }
  if (event.type === "mcp_share_approval_resolved") {
    set({
      approvals: state.approvals.filter((approval) => approval.approval_id !== event.approval_id),
    });
    return;
  }
  if (event.type === "mcp_tool_called") {
    const activity: McpToolActivity = {
      id: `${event.server}:${event.tool}:${event.at}:${state.activity.length}`,
      server: event.server,
      tool: event.tool,
      qualifiedName: event.qualified_name,
      caller: event.caller,
      ok: event.ok,
      durationMs: Number(event.duration_ms),
      error: event.error,
      at: event.at,
    };
    set({ activity: [activity, ...state.activity].slice(0, MAX_ACTIVITY) });
    const server = state.servers.find((entry) => entry.name === event.server);
    if (server && event.ok) {
      set({ checkedAt: Date.parse(event.at) || Date.now() });
    }
  }
}

/** The host id this daemon runs on, or null while connecting. */
export function localHostId(): HostId | null {
  return getState().localHostId;
}
