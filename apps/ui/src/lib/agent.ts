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
  | { kind: "pause"; label: string }
  | { kind: "you"; label: string }
  | { kind: "offline"; label: string };

/**
 * Whether the agent is doing something right now.
 *
 * A turn can still be recorded as running after its work settled — an imported
 * turn has no completion event at all — so "live" also has to be something the
 * agent state actually says.
 */
export function agentStateIsActive(state: AgentState): boolean {
  switch (state) {
    case "thinking":
    case "searching":
    case "reading":
    case "editing":
    case "running_tool":
      return true;
    default:
      return false;
  }
}

export function glyphForAgentState(state: AgentState, toolName?: string): AgentGlyphSpec {
  switch (state) {
    case "queued":
      return { kind: "orb", state: "breathing", speed: 0.35, paused: true, label: "Queued" };
    case "scheduled":
      return { kind: "orb", state: "breathing", speed: 0.3, label: "Auto-resume scheduled" };
    case "unreachable":
      return { kind: "offline", label: "Host unreachable" };
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
      return { kind: "you", label: "Needs approval" };
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
 * Whether the thread has a turn in flight right now.
 *
 * The daemon's `active_turn_id` is the authority: a turn that a usage limit
 * paused is still running (its agent may be mid-tool), and the agent state on
 * its own would call that `rate_limited` and claim nothing is in flight. The
 * state list stays as the fallback for a daemon too old to report the id.
 */
export function isTurnRunning(summary: ThreadSummary): boolean {
  if (summary.active_turn_id) return true;
  return (
    summary.agent_state === "thinking" ||
    summary.agent_state === "searching" ||
    summary.agent_state === "reading" ||
    summary.agent_state === "editing" ||
    summary.agent_state === "running_tool" ||
    summary.agent_state === "awaiting_approval"
  );
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
  if (summary.agent_state === "unreachable") {
    return { kind: "offline", label: "Host unreachable" };
  }
  if (!hostOnline && summary.tab === "working" && summary.agent_state !== "done") {
    return { kind: "offline", label: "Host unreachable" };
  }
  if (summary.agent_state === "scheduled") {
    return { kind: "limit", label: "Auto-resume scheduled" };
  }
  if (summary.agent_state === "queued") {
    return { kind: "orb", state: "breathing", speed: 0.35, paused: true, label: "Queued" };
  }
  if (summary.tab === "settled") {
    return { kind: "check", label: "Settled", dim: true };
  }
  if (summary.agent_state === "rate_limited") {
    return summary.needs_attention
      ? { kind: summary.tab === "done" ? "pause" : "limit", label: "Limited · needs you" }
      : { kind: "limit", label: "Limited" };
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
