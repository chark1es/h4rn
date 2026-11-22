import type { HarnessKind } from "@/gen";

/** What a harness can reason with, as its own CLI names it. */
export type ThinkingSupport = {
  /** Short label for the kind of control the harness offers. */
  kind: string;
  levels: string[];
};

export type HarnessModelInfo = {
  default: string;
  models: string[];
  thinking: ThinkingSupport | null;
};

/**
 * Models and thinking modes each harness accepts. A model from one harness is
 * not valid on another, so the picker reads this table by harness.
 */
export const HARNESS_MODELS: Record<HarnessKind, HarnessModelInfo> = {
  claude_code: {
    default: "claude-sonnet-5-5",
    models: ["claude-fable-5-2", "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
    thinking: { kind: "Effort", levels: ["low", "medium", "high", "xhigh", "max"] },
  },
  codex: {
    default: "gpt-5.1-codex",
    models: ["gpt-5.1-codex", "gpt-5.1-codex-mini", "gpt-5.1"],
    thinking: { kind: "Reasoning effort", levels: ["minimal", "low", "medium", "high"] },
  },
  opencode: {
    default: "claude-sonnet-5-5",
    models: ["claude-sonnet-5-5", "claude-opus-5-5", "gpt-5.1-codex"],
    thinking: { kind: "Variant", levels: ["low", "medium", "high"] },
  },
  antigravity: {
    default: "gemini-4-argon-medium",
    models: ["gemini-4-argon-low", "gemini-4-argon-medium", "gemini-4-argon-high"],
    // Antigravity encodes the thinking level in the model id itself.
    thinking: { kind: "Thinking level (part of the model)", levels: ["low", "medium", "high"] },
  },
  // A local ACP command reports its own models through `initialize` and
  // `session/new`; the picker fills from that probe, never from this table.
  acp: {
    default: "default",
    models: [],
    thinking: null,
  },
};

/**
 * Harnesses to offer: those whose CLI was detected. `installed` is the
 * daemon's detection list (null while it is unknown); `hostHarnesses` is what
 * the host advertises. A harness must pass both when both are known.
 */
export function detectedHarnesses(
  all: HarnessKind[],
  installed: HarnessKind[] | null,
  hostHarnesses: HarnessKind[] | undefined,
): HarnessKind[] {
  return all.filter(
    (harness) =>
      (installed === null || installed.includes(harness)) &&
      (!hostHarnesses || hostHarnesses.length === 0 || hostHarnesses.includes(harness)),
  );
}
