import type { OrbState, ThinkingOrbProps } from "thinking-orbs";
import type { AgentState, ThreadSummary } from "@/gen";

export type AgentGlyphSpec =
  | {
      kind: "orb";
      state: OrbState;
      speed?: number;
      paused?: boolean;
      opts?: ThinkingOrbProps["opts"];
      label: string;
    }
  | { kind: "check"; label: string; dim?: boolean }
  | { kind: "strike"; label: string }
  | { kind: "limit"; label: string }
  | { kind: "offline"; label: string };

export function agentStateLabel(state: AgentState): string {
  switch (state) {
    case "idle":
      return "Idle";
    case "thinking":
      return "Thinking";
    case "searching":
      return "Searching";
    case "reading":
      return "Reading";
    case "editing":
      return "Editing";
    case "running_tool":
      return "Running tool";
    case "awaiting_approval":
      return "Waiting for approval";
    case "rate_limited":
      return "Limited";
    case "error":
      return "Failed";
    case "done":
      return "Done";
  }
}

export function glyphForAgentState(state: AgentState, toolName?: string): AgentGlyphSpec {
  switch (state) {
    case "thinking":
      return { kind: "orb", state: "working", speed: 0.9, label: "Thinking" };
    case "searching":
      return { kind: "orb", state: "searching", label: "Searching" };
    case "reading":
      return { kind: "orb", state: "listening", speed: 0.9, label: "Reading" };
    case "editing":
      return { kind: "orb", state: "weaving", label: "Editing" };
    case "running_tool":
      return {
        kind: "orb",
        state: "solving",
        label: toolName ? `Running ${toolName}` : "Running tool",
      };
    case "awaiting_approval":
      return {
        kind: "orb",
        state: "shaping",
        speed: 0.8,
        opts: { shape: 0 },
        label: "Waiting for approval",
      };
    case "rate_limited":
      return { kind: "orb", state: "breathing", speed: 0.3, label: "Limited" };
    case "error":
      return { kind: "strike", label: "Failed" };
    case "done":
      return { kind: "check", label: "Done" };
    default:
      return { kind: "orb", state: "breathing", speed: 0.5, paused: true, label: "Idle" };
  }
}

/**
 * Thread-level mapping. Host reachability and tab placement outrank the raw
 * agent state, because an animation must never imply liveness the host lost.
 */
export function glyphForThread(
  summary: ThreadSummary,
  hostOnline: boolean,
  toolName?: string,
): AgentGlyphSpec {
  if (!hostOnline && summary.tab === "working" && summary.agent_state !== "done") {
    return { kind: "offline", label: "Host unreachable" };
  }
  if (summary.tab === "settled") {
    return { kind: "check", label: "Settled", dim: true };
  }
  if (summary.agent_state === "rate_limited") {
    return summary.needs_attention
      ? { kind: "limit", label: "Limited · needs you" }
      : { kind: "orb", state: "breathing", speed: 0.3, label: "Limited" };
  }
  if (summary.agent_state === "done") {
    return { kind: "check", label: summary.unread ? "Done · unread" : "Done" };
  }
  if (summary.agent_state === "idle" && summary.pinned) {
    return { kind: "orb", state: "breathing", speed: 0.5, paused: true, label: "Idle · pinned" };
  }
  return glyphForAgentState(summary.agent_state, toolName);
}

export function glyphForToolStatus(
  status: "running" | "completed" | "failed" | "denied",
): AgentGlyphSpec {
  switch (status) {
    case "running":
      return { kind: "orb", state: "solving", label: "Running tool" };
    case "completed":
      return { kind: "check", label: "Completed" };
    case "failed":
      return { kind: "strike", label: "Failed" };
    case "denied":
      return { kind: "strike", label: "Denied" };
  }
}
