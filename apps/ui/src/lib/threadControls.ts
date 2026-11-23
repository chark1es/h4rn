/**
 * The permission, model and reasoning controls a thread offers.
 *
 * The daemon refuses a value a harness cannot honour, so the picker must not
 * offer one in the first place. These helpers are the client's half of that
 * rule: a harness without a real plan mode, effort control or fast tier shows
 * no row for it.
 */

import type { HarnessKind, HostId, RuntimeMode, ThreadSummary } from "@/gen";
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

/**
 * The ACP mode ids h4rn reads as each permission mode.
 *
 * This mirrors the daemon's `runtime_mode_for_id`. An agent names its modes
 * itself (`acceptEdits`, `bypassPermissions`, `read-only`), and the picker must
 * map those onto the same h4rn mode the daemon does: offering a second table
 * here would offer labels the daemon refuses, or hide modes it accepts.
 */
const MODE_IDS: Record<RuntimeMode, readonly string[]> = {
  approval_required: ["default", "ask", "manual", "supervised"],
  auto_accept_edits: ["accept-edits", "acceptedits", "accept_edits"],
  plan: ["plan", "read-only", "readonly"],
  full_access: ["auto", "full-access", "full_access", "bypass", "bypasspermissions"],
};

/** The h4rn mode an advertised ACP mode id maps to, when it maps to one. */
export function runtimeModeForModeId(id: string): RuntimeMode | undefined {
  const normalized = id.trim().toLowerCase();
  return (Object.keys(MODE_IDS) as RuntimeMode[]).find((mode) =>
    MODE_IDS[mode].includes(normalized),
  );
}

/** The modes each built-in harness can honour. */
const HARNESS_MODES: Record<HarnessKind, RuntimeMode[]> = {
  claude_code: ["approval_required", "auto_accept_edits", "plan", "full_access"],
  codex: ["approval_required", "auto_accept_edits", "full_access"],
  opencode: ["approval_required", "auto_accept_edits", "full_access"],
  antigravity: ["approval_required", "auto_accept_edits", "plan", "full_access"],
  // A configured agent names its own modes. Until it does, no mode is offered:
  // the dialog and the composer say the agent manages its own mode rather than
  // guessing, because the daemon refuses a mode the agent never advertised.
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
  if (harness !== "acp") {
    return MODE_CHOICES.filter((choice) => offered.includes(choice.value));
  }
  // A configured agent's catalogue is authoritative, and an empty or unknown
  // one is not a licence to offer everything: h4rn cannot map a mode it was not
  // told about, so there is no choice to show until the agent names its modes.
  if (!advertisedAgentModes || advertisedAgentModes.length === 0) return [];
  // Never offer a label the agent did not name, and use the daemon's id mapping
  // so an agent's `acceptEdits`/`bypassPermissions` counts as the mode it names.
  const mapped = new Set(
    advertisedAgentModes
      .map(runtimeModeForModeId)
      .filter((mode): mode is RuntimeMode => mode !== undefined),
  );
  return MODE_CHOICES.filter(
    (choice) => offered.includes(choice.value) && mapped.has(choice.value),
  );
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

/**
 * The choices the model picker offers: the live session's catalogue, the
 * harness's static list before the first session, and proxy models.
 *
 * A live session is the truth: Codex publishes `model/list` at session start
 * and a configured ACP agent names its own models. Only when no session has
 * reported a catalogue yet does the static list stand in.
 */
export function threadModelChoices(
  harness: HarnessKind,
  current: string,
  agentModels: readonly string[],
  proxyModels: readonly string[],
  sessionModels: readonly string[] = [],
): string[] {
  const known =
    sessionModels.length > 0
      ? [...sessionModels, ...proxyModels]
      : harness === "acp"
        ? agentModels
        : [...(HARNESS_MODELS[harness]?.models ?? []), ...proxyModels];
  return [...new Set([current, ...known])].filter((model) => model.length > 0);
}

/**
 * The model a new thread should start on for `harness` on `host`.
 *
 * The static table is only a guess: an installed Codex CLI can drop the model
 * it names (`gpt-5.1-codex` is absent from codex-cli 0.171.0's `model/list`),
 * and the daemon refuses a model the live catalogue does not offer. Once a
 * thread on that host has reported Codex's own list, its first entry is the
 * default; the static default stands only before any catalogue is known.
 */
export function newThreadModel(
  harness: HarnessKind,
  host: HostId | "",
  threads: readonly ThreadSummary[],
): string {
  if (harness === "codex" && host !== "") {
    const reported = threads
      .filter(
        (thread) => thread.harness === harness && thread.host === host && thread.models.length > 0,
      )
      .sort((left, right) => right.last_activity.localeCompare(left.last_activity))[0];
    const first = reported?.models[0];
    if (first) return first;
  }
  return HARNESS_MODELS[harness].default;
}
