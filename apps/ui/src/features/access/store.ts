/**
 * Sharing & access: outside agents connected through OAuth, the MCP URL an
 * agent puts in its client, and h4rn's own per-harness tool switches.
 *
 * The daemon owns every decision. The store keeps the last authoritative
 * answer, moves a switch or a revocation optimistically, and puts it back when
 * the daemon refuses.
 */
import { useSyncExternalStore } from "react";
import type {
  AgentToolSettings,
  ApprovalDecision,
  ClientRequest,
  HarnessKind,
  McpEnabledMatrix,
  OutsideAgentLevel,
  OutsideAgentSignIn,
  OutsideAgentView,
  ProtoError,
  ResponseBody,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { i18n } from "@/lib/i18n";
import { getClient, pushToast } from "@/lib/store/appStore";

export type AccessState = {
  agents: OutsideAgentView[];
  /** Sign-ins waiting for the person's decision inside h4rn. */
  signIns: OutsideAgentSignIn[];
  /** Streamable-HTTP endpoint an outside agent connects to. */
  mcpUrl: string;
  /** Whether Settings → Labs → Outside agents (OAuth) is on; `null` until loaded. */
  outsideEnabled: boolean | null;
  settings: AgentToolSettings | null;
  loading: boolean;
  refreshing: boolean;
  error: ProtoError | null;
  /** Client ids or harness names with a request in flight. */
  pending: Record<string, boolean>;
};

const initialState: AccessState = {
  agents: [],
  signIns: [],
  mcpUrl: "",
  outsideEnabled: null,
  settings: null,
  loading: false,
  refreshing: false,
  error: null,
  pending: {},
};

let state: AccessState = initialState;
const listeners = new Set<() => void>();

function set(patch: Partial<AccessState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

type Requester = (body: ClientRequest) => Promise<ResponseBody>;
let requester: Requester | null = null;

/** Replaces the transport for tests. */
export function setAccessRequesterForTests(next: Requester | null): void {
  requester = next;
}

async function request(body: ClientRequest): Promise<ResponseBody> {
  return requester ? requester(body) : getClient().request(body);
}

/** Clears the store; tests call this between renders. */
export function resetAccessStore(): void {
  state = initialState;
  for (const listener of listeners) listener();
}

export function getAccessState(): AccessState {
  return state;
}

export function useAccess<T>(selector: (value: AccessState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

function applyAgents(response: ResponseBody): boolean {
  if (response.type !== "outside_agents") return false;
  set({ agents: response.agents, mcpUrl: response.mcp_url, error: null });
  return true;
}

function applySignIns(response: ResponseBody): boolean {
  if (response.type !== "outside_agent_sign_ins") return false;
  set({ signIns: response.sign_ins });
  return true;
}

function applySettings(response: ResponseBody): boolean {
  if (response.type !== "agent_tools") return false;
  set({ settings: response.settings });
  return true;
}

function applyOutsideSettings(response: ResponseBody): boolean {
  if (response.type !== "outside_agent_settings") return false;
  set({ outsideEnabled: response.settings.enabled });
  return true;
}

/** Reads the connected agents, the waiting sign-ins, the MCP URL and the tools. */
export async function loadAccess(quiet = false): Promise<void> {
  set(quiet ? { refreshing: true } : { loading: true, error: null });
  try {
    const outside = await request({
      type: "mcp",
      request: { type: "outside_agent_settings" },
    });
    applyOutsideSettings(outside);
    const agents = await request({ type: "mcp", request: { type: "list_outside_agents" } });
    applyAgents(agents);
    const signIns = await request({
      type: "mcp",
      request: { type: "list_outside_agent_sign_ins" },
    });
    applySignIns(signIns);
    const settings = await request({ type: "mcp", request: { type: "agent_tools" } });
    applySettings(settings);
    set({ loading: false, refreshing: false });
  } catch (error) {
    const proto = toProtoError(error);
    set({ loading: false, refreshing: false, error: proto });
  }
}

/** Answers one waiting sign-in. Nothing is granted until a level is chosen. */
export async function respondSignIn(
  signIn: OutsideAgentSignIn,
  decision: ApprovalDecision,
  level: OutsideAgentLevel | null,
): Promise<boolean> {
  const before = state.signIns;
  set({
    signIns: before.filter((row) => row.request_id !== signIn.request_id),
    pending: { ...state.pending, [signIn.request_id]: true },
  });
  try {
    if (decision === "approve" && !level) {
      // i18n-ignore-next-line: the page always passes a level with Approve; none is a bug
      throw new Error("choose read-only, supervised or full access");
    }
    await request({
      type: "mcp",
      request: {
        type: "respond_outside_agent_sign_in",
        request_id: signIn.request_id,
        decision,
        level: decision === "approve" ? level : null,
      },
    });
    if (decision === "approve" && level) {
      pushToast({
        title: i18n.t("settings:access.toast.connected", { name: signIn.client_name }),
        description: `${levelLabel(level)}: ${levelDescription(level)}`,
      });
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ signIns: before });
    pushToast({
      title: i18n.t("settings:access.toast.answerFailed", { name: signIn.client_name }),
      description: `${proto.message}${proto.hint ? ` ${proto.hint}` : ""}`,
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[signIn.request_id];
    set({ pending });
  }
}

/** Cuts one outside agent off immediately. */
export async function revokeAccessAgent(agent: OutsideAgentView): Promise<boolean> {
  const before = state.agents;
  set({
    agents: before.filter((row) => row.client_id !== agent.client_id),
    pending: { ...state.pending, [agent.client_id]: true },
  });
  try {
    await request({
      type: "mcp",
      request: { type: "revoke_outside_agent", client_id: agent.client_id },
    });
    pushToast({
      title: i18n.t("settings:access.toast.revoked", { name: agent.name }),
      description: i18n.t("settings:access.toast.revokedHint"),
    });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ agents: before });
    pushToast({
      title: i18n.t("settings:access.toast.revokeFailed", { name: agent.name }),
      description: `${proto.message}${proto.hint ? ` ${proto.hint}` : ""}`,
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[agent.client_id];
    set({ pending });
  }
}

function matrixWithHarness(
  matrix: McpEnabledMatrix,
  harness: HarnessKind,
  enabled: boolean,
): McpEnabledMatrix {
  return {
    ...matrix,
    by_harness: [
      ...matrix.by_harness.filter((entry) => entry.harness !== harness),
      { harness, enabled },
    ],
  };
}

/** Turns h4rn's own tools on or off for one harness. */
export async function setAgentToolsEnabled(
  harness: HarnessKind,
  enabled: boolean,
): Promise<boolean> {
  const current = state.settings;
  if (!current) return false;
  const before = current.enabled;
  set({
    settings: { ...current, enabled: matrixWithHarness(before, harness, enabled) },
    pending: { ...state.pending, [`harness:${harness}`]: true },
  });
  try {
    const response = await request({
      type: "mcp",
      request: { type: "set_agent_tools_enabled", harness, enabled },
    });
    if (!applySettings(response)) {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the tool settings.`);
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ settings: { ...current, enabled: before } });
    pushToast({
      title: enabled
        ? i18n.t("settings:access.toast.toolsOnFailed")
        : i18n.t("settings:access.toast.toolsOffFailed"),
      description: `${proto.message} ${i18n.t("settings:access.toast.switchRestored")}${proto.hint ? ` ${proto.hint}` : ""}`,
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[`harness:${harness}`];
    set({ pending });
  }
}

/** Reads only the outside-agent Labs switch. */
export async function loadOutsideAgentSettings(): Promise<void> {
  try {
    const response = await request({
      type: "mcp",
      request: { type: "outside_agent_settings" },
    });
    applyOutsideSettings(response);
  } catch (error) {
    set({ error: toProtoError(error) });
  }
}

/**
 * Turns outside-agent access on or off for this machine.
 *
 * Turning it off revokes every grant and drops every waiting sign-in in one
 * durable write on the daemon. The local copies are cleared so the page does
 * not keep showing agents that are already gone.
 */
export async function setOutsideAgentEnabled(enabled: boolean): Promise<boolean> {
  const before = state.outsideEnabled;
  set({ pending: { ...state.pending, "outside:switch": true }, error: null });
  try {
    const response = await request({
      type: "mcp",
      request: { type: "set_outside_agent_settings_enabled", enabled },
    });
    if (!applyOutsideSettings(response)) {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the outside-agent setting.`);
    }
    if (!enabled) set({ agents: [], signIns: [], mcpUrl: "" });
    pushToast({
      title: enabled
        ? i18n.t("settings:access.toast.outsideOn")
        : i18n.t("settings:access.toast.outsideOff"),
      description: enabled
        ? i18n.t("settings:access.toast.outsideOnHint")
        : i18n.t("settings:access.toast.outsideOffHint"),
    });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ outsideEnabled: before });
    pushToast({
      title: enabled
        ? i18n.t("settings:access.toast.outsideOnFailed")
        : i18n.t("settings:access.toast.outsideOffFailed"),
      description: `${proto.message}${proto.hint ? ` ${proto.hint}` : ""}`,
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending["outside:switch"];
    set({ pending });
  }
}

/** Human words for a granted level. */
export function levelLabel(level: OutsideAgentLevel): string {
  switch (level) {
    case "read_only":
      return i18n.t("settings:access.levels.read_only.label");
    case "supervised":
      return i18n.t("settings:access.levels.supervised.label");
    case "full_access":
      return i18n.t("settings:access.levels.full_access.label");
  }
}

/** What the level lets an agent do, in one line. */
export function levelDescription(level: OutsideAgentLevel): string {
  switch (level) {
    case "read_only":
      return i18n.t("settings:access.levels.read_only.description");
    case "supervised":
      return i18n.t("settings:access.levels.supervised.description");
    case "full_access":
      return i18n.t("settings:access.levels.full_access.description");
  }
}
