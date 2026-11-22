import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import {
  effortLabel,
  fastSupported,
  modeLabel,
  nonDefaultSettings,
  threadEffortChoices,
  threadModeChoices,
  threadModelChoices,
} from "@/lib/threadControls";

function summary(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: "th_1",
    parent_thread_id: null,
    host: "host_1",
    harness: "claude_code",
    title: "A thread",
    workspace_path: "/w",
    model: "claude-sonnet-5-5",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    fast: false,
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00Z",
    agent_state: "idle",
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    needs_attention: false,
    unread: false,
    resume_at: null,
    limit: null,
    ...overrides,
  };
}

describe("thread controls", () => {
  it("offers plan only where the harness has a real plan mode", () => {
    const plan = (harness: ThreadSummary["harness"]) =>
      threadModeChoices(harness).some((choice) => choice.value === "plan");
    expect(plan("claude_code")).toBe(true);
    expect(plan("antigravity")).toBe(true);
    expect(plan("codex")).toBe(false);
    expect(plan("opencode")).toBe(false);
  });

  it("offers an ACP agent only the modes it advertised", () => {
    const offered = threadModeChoices("acp", ["default", "auto"]).map((choice) => choice.value);
    expect(offered).toEqual(["approval_required", "full_access"]);
  });

  it("shows an effort picker only for harnesses with a real effort control", () => {
    expect(threadEffortChoices("claude_code")).toContain("xhigh");
    expect(threadEffortChoices("codex")).toContain("ultra");
    expect(threadEffortChoices("opencode")).toEqual([]);
    expect(threadEffortChoices("antigravity")).toEqual([]);
    expect(threadEffortChoices("acp")).toEqual([]);
    // A live ACP session's catalogue wins over the static guess.
    expect(threadEffortChoices("acp", ["low", "high"])).toEqual(["low", "high"]);
    expect(threadEffortChoices("codex", ["low", "high"])).toEqual(["low", "high"]);
  });

  it("offers fast mode only on Codex", () => {
    expect(fastSupported("codex")).toBe(true);
    expect(fastSupported("claude_code")).toBe(false);
    expect(fastSupported("acp")).toBe(false);
  });

  it("shows only settings that differ from the defaults", () => {
    expect(nonDefaultSettings(summary(), "approval_required")).toEqual([]);
    expect(
      nonDefaultSettings(summary({ runtime_mode: "full_access" }), "approval_required"),
    ).toEqual(["Full access"]);
    expect(
      nonDefaultSettings(summary({ reasoning_effort: "xhigh", fast: true }), "approval_required"),
    ).toEqual(["Extra high", "Fast"]);
    // The device's default is what a mode is compared with.
    expect(nonDefaultSettings(summary({ runtime_mode: "plan" }), "plan")).toEqual([]);
  });

  it("labels modes and efforts in the harness's vocabulary", () => {
    expect(modeLabel("auto_accept_edits")).toBe("Auto-accept edits");
    expect(effortLabel("xhigh")).toBe("Extra high");
    expect(effortLabel("medium")).toBe("Medium");
  });

  it("keeps the current model at the head of the picker", () => {
    const models = threadModelChoices("claude_code", "custom-model", [], ["proxy/model"]);
    expect(models[0]).toBe("custom-model");
    expect(models).toContain("proxy/model");
  });
});
