import type { OrbState, ThinkingOrbProps } from "thinking-orbs";
import type { AgentState, ThreadSummary } from "@/gen";
import { i18n } from "@/lib/i18n";
import type glyphCatalogue from "@/lib/i18n/locales/en/thread.json";

/** A glyph label in the active language; it is also the glyph's hover title. */
function label(key: keyof (typeof glyphCatalogue)["glyph"], values?: { tool: string }): string {
  return values ? i18n.t(`thread:glyph.${key}`, values) : i18n.t(`thread:glyph.${key}`);
}

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
      return { kind: "orb", state: "breathing", speed: 0.35, paused: true, label: label("queued") };
    case "scheduled":
      return { kind: "orb", state: "breathing", speed: 0.3, label: label("scheduled") };
    case "unreachable":
      return { kind: "offline", label: label("unreachable") };
    case "thinking":
      return { kind: "orb", state: "working", speed: 0.9, label: label("thinking") };
    case "searching":
      return { kind: "orb", state: "searching", label: label("searching") };
    case "reading":
      return { kind: "orb", state: "listening", speed: 0.9, label: label("reading") };
    case "editing":
      return { kind: "orb", state: "weaving", label: label("editing") };
    case "running_tool":
      return {
        kind: "orb",
        state: "solving",
        label: toolName ? label("runningNamed", { tool: toolName }) : label("runningTool"),
      };
    case "awaiting_approval":
      return { kind: "you", label: label("needsApproval") };
    case "rate_limited":
      return { kind: "orb", state: "breathing", speed: 0.3, label: label("limited") };
    case "error":
      return { kind: "strike", label: label("failed") };
    case "done":
      return { kind: "check", label: label("done") };
    default:
      return { kind: "orb", state: "breathing", speed: 0.5, paused: true, label: label("idle") };
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
    return { kind: "offline", label: label("unreachable") };
  }
  if (!hostOnline && summary.tab === "working" && summary.agent_state !== "done") {
    return { kind: "offline", label: label("unreachable") };
  }
  if (summary.agent_state === "scheduled") {
    return { kind: "limit", label: label("scheduled") };
  }
  if (summary.agent_state === "queued") {
    return { kind: "orb", state: "breathing", speed: 0.35, paused: true, label: label("queued") };
  }
  if (summary.tab === "settled") {
    return { kind: "check", label: label("settled"), dim: true };
  }
  if (summary.agent_state === "rate_limited") {
    return summary.needs_attention
      ? { kind: summary.tab === "done" ? "pause" : "limit", label: label("limitedNeedsYou") }
      : { kind: "limit", label: label("limited") };
  }
  if (summary.agent_state === "done") {
    return { kind: "check", label: summary.unread ? label("doneUnread") : label("done") };
  }
  if (summary.agent_state === "idle" && summary.pinned) {
    return {
      kind: "orb",
      state: "breathing",
      speed: 0.5,
      paused: true,
      label: label("idlePinned"),
    };
  }
  return glyphForAgentState(summary.agent_state, toolName);
}

export function glyphForToolStatus(
  status: "running" | "completed" | "failed" | "denied",
): AgentGlyphSpec {
  switch (status) {
    case "running":
      return { kind: "orb", state: "solving", label: label("runningTool") };
    case "completed":
      return { kind: "check", label: label("completed") };
    case "failed":
      return { kind: "strike", label: label("failed") };
    case "denied":
      return { kind: "strike", label: label("denied") };
  }
}
