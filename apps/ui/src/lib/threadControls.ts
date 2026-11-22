/**
 * The permission, model and reasoning controls a thread offers.
 *
 * The daemon refuses a value a harness cannot honour, so the picker must not
 * offer one in the first place. These helpers are the client's half of that
 * rule: a harness without a real plan mode, effort control or fast tier shows
 * no row for it.
 */

import type { HarnessKind, RuntimeMode, ThreadSummary } from "@/gen";
import { HARNESS_MODELS } from "@/lib/models";

export type ModeChoice = {
  /** h4rn's permission mode. */
  value: RuntimeMode;
  /** Label shown in the picker. */
  label: string;
  /** The harness's own mode id, when it has one. */
  modeId: string;
};

/** Every permission mode h4rn knows, with the id harnesses use for it. */
export const MODE_CHOICES: ModeChoice[] = [
  { value: "approval_required", label: "Approval required", modeId: "default" },
  { value: "auto_accept_edits", label: "Auto-accept edits", modeId: "accept-edits" },
  { value: "plan", label: "Plan", modeId: "plan" },
  { value: "full_access", label: "Full access", modeId: "auto" },
];

/** The modes each built-in harness can honour. */
const HARNESS_MODES: Record<HarnessKind, RuntimeMode[]> = {
  claude_code: ["approval_required", "auto_accept_edits", "plan", "full_access"],
  codex: ["approval_required", "auto_accept_edits", "full_access"],
  opencode: ["approval_required", "auto_accept_edits", "full_access"],
  antigravity: ["approval_required", "auto_accept_edits", "plan", "full_access"],
  // A configured agent names its own modes. When its catalogue is known, only
  // the modes it advertised are offered; without one, plan stays available
  // because the agent may offer it and the daemon refuses a mode it does not.
  acp: ["approval_required", "auto_accept_edits", "plan", "full_access"],
};

export function modeLabel(mode: RuntimeMode): string {
  return MODE_CHOICES.find((choice) => choice.value === mode)?.label ?? "Approval required";
}

/** The permission modes a thread's harness offers. */
export function threadModeChoices(
  harness: HarnessKind,
  advertisedAgentModes?: readonly string[],
): ModeChoice[] {
  const offered = HARNESS_MODES[harness];
  if (harness !== "acp" || !advertisedAgentModes || advertisedAgentModes.length === 0) {
    return MODE_CHOICES.filter((choice) => offered.includes(choice.value));
  }
  // The cached agent catalogue is authoritative when it is known: never offer
  // a label the agent did not name.
  const ids = new Set(advertisedAgentModes);
  return MODE_CHOICES.filter((choice) => offered.includes(choice.value) && ids.has(choice.modeId));
}

/** The reasoning efforts a harness exposes, or an empty list. */
export function threadEffortChoices(
  harness: HarnessKind,
  advertised?: readonly string[],
): string[] {
  // A live session's catalogue is the truth: a configured ACP agent names its
  // effort option at session start, and Codex may narrow the list per model.
  if (advertised && advertised.length > 0) return [...advertised];
  if (harness === "claude_code" || harness === "codex") {
    return HARNESS_MODELS[harness].thinking?.levels ?? [];
  }
  // OpenCode model variants and Antigravity's model-encoded thinking level are
  // not a thread-level effort control.
  return [];
}

/** Whether the harness has a real fast tier. */
export function fastSupported(harness: HarnessKind, agentId?: string | null): boolean {
  // Only Codex publishes a fast service tier. An ACP agent's catalogue is not
  // known here, so it shows no fast row.
  return harness === "codex" && !agentId;
}

/** A human label for one effort value. */
export function effortLabel(effort: string): string {
  switch (effort) {
    case "xhigh":
      return "Extra high";
    case "max":
      return "Max";
    case "ultra":
      return "Ultra";
    case "minimal":
      return "Minimal";
    default:
      return effort.charAt(0).toUpperCase() + effort.slice(1);
  }
}

/**
 * The settings a thread shows next to its model, quietly.
 *
 * The mode is compared with the device's default for new threads; effort and
 * fast are compared with the harness defaults (unset). `reasoning_effort` is
 * optional on the wire, so an older daemon's thread simply has none.
 */
export function nonDefaultSettings(summary: ThreadSummary, defaultMode: RuntimeMode): string[] {
  const parts: string[] = [];
  if ((summary.runtime_mode ?? "approval_required") !== defaultMode) {
    parts.push(modeLabel(summary.runtime_mode ?? "approval_required"));
  }
  if (summary.reasoning_effort) parts.push(effortLabel(summary.reasoning_effort));
  if (summary.fast) parts.push("Fast");
  return parts;
}

/** The choices the model picker offers: the harness's list plus proxy models. */
export function threadModelChoices(
  harness: HarnessKind,
  current: string,
  agentModels: readonly string[],
  proxyModels: readonly string[],
): string[] {
  const known =
    harness === "acp" ? agentModels : [...(HARNESS_MODELS[harness]?.models ?? []), ...proxyModels];
  return [...new Set([current, ...known])].filter((model) => model.length > 0);
}
