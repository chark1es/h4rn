import { describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import {
  effortLabel,
  fastSupported,
  modeLabel,
  newThreadModel,
  nonDefaultSettings,
  runtimeModeForModeId,
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
    models: [],
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
    pending_guest_requests: 0,
    unread: false,
    resume_at: null,
    limit: null,
    queued_count: 0,
    queue_revision: 0,
    steer: true,
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

  it("offers no ACP mode until the agent names its catalogue", () => {
    // An unknown catalogue is not a licence to offer every mode: the daemon
    // cannot map one it was not told about, so the picker stays closed and the
    // thread says the agent manages its own mode.
    expect(threadModeChoices("acp")).toEqual([]);
    expect(threadModeChoices("acp", [])).toEqual([]);
    // A catalogue whose ids h4rn maps none of is just as unknown.
    expect(threadModeChoices("acp", ["code", "architect"])).toEqual([]);
  });

  it("accepts the agent's own spellings of the modes the daemon maps", () => {
    const offered = threadModeChoices("acp", ["acceptEdits", "bypassPermissions"]).map(
      (choice) => choice.value,
    );
    expect(offered).toEqual(["auto_accept_edits", "full_access"]);
    expect(threadModeChoices("acp", ["accept_edits", "read-only"]).map((c) => c.value)).toEqual([
      "auto_accept_edits",
      "plan",
    ]);
    expect(runtimeModeForModeId("BypassPermissions")).toBe("full_access");
    expect(runtimeModeForModeId("code")).toBeUndefined();
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

  it("offers the live session's model catalogue before the static list", () => {
    // Codex publishes `model/list` at session start; the picker must offer
    // those ids, not the adapter's static table.
    const models = threadModelChoices("codex", "gpt-a", [], [], ["gpt-a", "gpt-b", "gpt-c"]);
    expect(models).toEqual(["gpt-a", "gpt-b", "gpt-c"]);
    expect(models).not.toContain("gpt-5.1-codex");

    // Before the first session there is no catalogue, so the static list (and
    // a configured proxy's models) still fills the picker.
    const fallback = threadModelChoices("codex", "gpt-5.1-codex", [], ["proxy/model"]);
    expect(fallback).toContain("gpt-5.1-codex-mini");
    expect(fallback).toContain("proxy/model");
  });

  it("starts a new Codex thread on the model the host's CLI really lists", () => {
    // codex-cli 0.171.0 no longer offers the static default, so a new thread
    // would be refused if it asked for it once a live catalogue is known.
    const reported = summary({
      thread_id: "th_codex",
      harness: "codex",
      model: "gpt-6.2-sol",
      models: ["gpt-6.2-sol", "gpt-6.2-astra"],
    });
    expect(newThreadModel("codex", "host_1", [reported])).toBe("gpt-6.2-sol");
    // The same list from another host says nothing about this one.
    expect(newThreadModel("codex", "host_2", [reported])).toBe("gpt-5.1-codex");
    // Before any thread reported a catalogue the static default stands.
    expect(newThreadModel("codex", "host_1", [])).toBe("gpt-5.1-codex");
    // Other harnesses keep their static default; their live lists are not a
    // per-host catalogue the dialog can read.
    expect(
      newThreadModel("claude_code", "host_1", [
        summary({ harness: "claude_code", models: ["claude-opus-5-5"] }),
      ]),
    ).toBe("claude-sonnet-5-5");
  });
});
