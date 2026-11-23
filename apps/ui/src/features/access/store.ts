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
import { getClient, pushToast } from "@/lib/store/appStore";

export type AccessState = {
  agents: OutsideAgentView[];
  /** Sign-ins waiting for the person's decision inside h4rn. */
  signIns: OutsideAgentSignIn[];
  /** Streamable-HTTP endpoint an outside agent connects to. */
  mcpUrl: string;
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

/** Reads the connected agents, the waiting sign-ins, the MCP URL and the tools. */
export async function loadAccess(quiet = false): Promise<void> {
  set(quiet ? { refreshing: true } : { loading: true, error: null });
  try {
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
        title: `${signIn.client_name} is connected.`,
        description: `${levelLabel(level)}: ${levelDescription(level)}`,
      });
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ signIns: before });
    pushToast({
      title: `Couldn't answer ${signIn.client_name}.`,
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
      title: `${agent.name} is cut off.`,
      description: "Every token it held is revoked; it must ask to sign in again.",
    });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ agents: before });
    pushToast({
      title: `Couldn't revoke ${agent.name}.`,
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
      throw new Error(`The daemon answered \`${response.type}\`, not the tool settings.`);
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ settings: { ...current, enabled: before } });
    pushToast({
      title: `Couldn't ${enabled ? "turn on" : "turn off"} h4rn tools.`,
      description: `${proto.message} The switch is back where it was.${proto.hint ? ` ${proto.hint}` : ""}`,
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[`harness:${harness}`];
    set({ pending });
  }
}

/** Human words for a granted level. */
export function levelLabel(level: OutsideAgentLevel): string {
  switch (level) {
    case "read_only":
      return "Read only";
    case "supervised":
      return "Supervised";
    case "full_access":
      return "Full access";
  }
}

/** What the level lets an agent do, in one line. */
export function levelDescription(level: OutsideAgentLevel): string {
  switch (level) {
    case "read_only":
      return "Reads projects, threads and providers; nothing changes.";
    case "supervised":
      return "Starts and messages threads at approval-required.";
    case "full_access":
      return "Starts and messages threads at the mode it asks for.";
  }
}
