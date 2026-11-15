import { beforeEach, describe, expect, it, vi } from "vitest";

import type { UsageReport, UsageStreamEvent } from "@/gen";

const request = vi.fn();
const applyResumeSchedule = vi.fn();
const applyLimitObservation = vi.fn();

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  getState: () => ({
    ui: { selectedThreadId: null, route: { kind: "usage" } },
    threads: {},
  }),
  applyResumeSchedule,
  applyLimitObservation,
}));

let streaming: ((event: UsageStreamEvent) => void) | null = null;

vi.mock("@/lib/client/frames", () => ({
  onUsageFrame: (listener: (event: UsageStreamEvent) => void) => {
    streaming = listener;
    return () => {
      streaming = null;
    };
  },
}));

const {
  cancelResume,
  failOver,
  loadFailoverOptions,
  getUsageState,
  loadLimits,
  loadUsage,
  resetUsageStoreForTests,
  resumeNow,
  setAutoResumePolicy,
  startUsageWatch,
} = await import("./usageStore");

function report(overrides: Partial<UsageReport> = {}): UsageReport {
  return {
    scope: "global",
    scope_id: null,
    range: { since: null, until: null },
    group_by: "harness",
    bucket: "day",
    series_group_by: "harness",
    totals: {
      input_tokens: 10,
      output_tokens: 5,
      cached_input_tokens: 2,
      cached_creation_tokens: 0,
      cost_micros: 0,
      ttft_ms: 0,
      tps: 0,
      wall_ms: 0,
      generation_ms: 0,
      active_ms: 0,
      idle_ms: 0,
      runtime_ms: 0,
      turns: 1,
    },
    buckets: [],
    series: [],
    groups: [],
    ...overrides,
  };
}

describe("usage store requests", () => {
  beforeEach(() => {
    request.mockReset();
    applyResumeSchedule.mockReset();
    applyLimitObservation.mockReset();
    resetUsageStoreForTests();
  });

  it("asks the daemon for a stacked series in the visible range", async () => {
    request.mockResolvedValue({ type: "usage_report", report: report() });
    await loadUsage();

    expect(request).toHaveBeenCalledWith(
      expect.objectContaining({
        type: "get_usage",
        scope: "global",
        group_by: "harness",
        series_group_by: "harness",
        bucket: "day",
        include_children: false,
      }),
    );
    expect(getUsageState().report?.totals.input_tokens).toBe(10);
    expect(getUsageState().reportLoading).toBe(false);
  });

  it("keeps the last report on screen when a load fails", async () => {
    request.mockResolvedValueOnce({ type: "usage_report", report: report() });
    await loadUsage();
    request.mockRejectedValueOnce(new Error("daemon went away"));
    await loadUsage();

    expect(getUsageState().report).not.toBeNull();
    expect(getUsageState().reportError?.message).toContain("daemon went away");
    expect(getUsageState().reportLoading).toBe(false);
  });

  it("stores the limits report and the policy it carries", async () => {
    request.mockResolvedValue({
      type: "limits",
      report: {
        limits: [],
        policy: {
          failover: true,
          max_wait_ms: 60_000,
          prompt: "Continue.",
          jitter_ms: 5_000,
        },
      },
    });
    await loadLimits();
    expect(getUsageState().limits?.policy.prompt).toBe("Continue.");
  });
});

describe("usage stream", () => {
  beforeEach(() => {
    request.mockReset();
    applyResumeSchedule.mockReset();
    applyLimitObservation.mockReset();
    resetUsageStoreForTests();
  });

  it("keeps one live tick per thread", () => {
    startUsageWatch();
    streaming?.({
      type: "usage_tick",
      thread_id: "thr_a",
      run_id: null,
      usage: {
        input_tokens: 12,
        output_tokens: 4,
        cached_input_tokens: 0,
        cost_micros: 0,
        duration_ms: 1_000,
        generation_ms: 1_000,
        tps: 0,
      },
      ttft_ms: 420,
      tps: 4,
      elapsed_ms: 1_000,
      tool_ms: 0,
      at: "2026-11-13T10:00:00Z",
    });
    expect(getUsageState().live.thr_a?.ttft_ms).toBe(420);
    expect(getUsageState().live.thr_a?.usage.output_tokens).toBe(4);
  });

  it("routes a limit observation through the app store", () => {
    startUsageWatch();
    const status = {
      thread_id: "thr_a",
      harness: "claude_code" as const,
      account: "atlas",
      provider: "anthropic",
      model: "claude-sonnet-5-5",
      reset_at: "2026-11-13T11:00:00Z",
      message: null,
      consumed: 90,
      limit: 100,
      observed_at: "2026-11-13T10:00:00Z",
      source: "harness" as const,
    };
    streaming?.({ type: "limit_status", status });
    expect(applyLimitObservation).toHaveBeenCalledWith(status);
  });

  it("pushes a resume schedule into the thread list", () => {
    startUsageWatch();
    const schedule = {
      schedule_id: "sched_1",
      thread_id: "thr_a",
      scheduled_at: "2026-11-13T10:00:00Z",
      resume_at: "2026-11-13T11:00:00Z",
      attempt: 0,
      state: "scheduled" as const,
      reason: "Known reset",
      prompt: "Continue.",
      failover: null,
      last_error: null,
    };
    streaming?.({ type: "schedule_changed", schedule });
    expect(applyResumeSchedule).toHaveBeenCalledWith(schedule);
  });
});

describe("auto-resume actions", () => {
  beforeEach(() => {
    request.mockReset();
    resetUsageStoreForTests();
  });

  it("sends Cancel and clears the pending marker", async () => {
    request.mockResolvedValue({ type: "resume_changed", schedule: null });
    await cancelResume("thr_a");
    expect(request).toHaveBeenCalledWith({ type: "cancel_resume", thread_id: "thr_a" });
    expect(getUsageState().pending.thr_a).toBeUndefined();
    expect(getUsageState().actionError).toBeNull();
  });

  it("sends Resume now and clears the pending marker", async () => {
    request.mockResolvedValue({ type: "resume_changed", schedule: null });
    await resumeNow("thr_a");
    expect(request).toHaveBeenCalledWith({ type: "resume_now", thread_id: "thr_a" });
    expect(getUsageState().pending.thr_a).toBeUndefined();
  });

  it("keeps the daemon's failover options per thread", async () => {
    const options = {
      thread_id: "thr_a",
      current_model: "claude-sonnet-5",
      default: { model: "claude-opus-5", account: null, reason: "same family" },
      options: [
        { model: "claude-opus-5", account: null, reason: "same family" },
        { model: "gpt-6.1-sol", account: "acct-b", reason: "other vendor" },
      ],
      unavailable: null,
    };
    request.mockResolvedValueOnce({ type: "failover_options", options });
    await loadFailoverOptions("thr_a");
    expect(request).toHaveBeenCalledWith({ type: "list_failover_options", thread_id: "thr_a" });
    expect(getUsageState().failover.thr_a).toEqual(options);
  });

  it("fails over to a chosen option, then forgets the stale options", async () => {
    request.mockResolvedValue({ type: "resume_changed", schedule: null });
    await failOver("thr_a", { model: "gpt-6.1-sol", account: "acct-b" });
    expect(request).toHaveBeenCalledWith({
      type: "fail_over",
      thread_id: "thr_a",
      model: "gpt-6.1-sol",
      account: "acct-b",
    });
    expect(getUsageState().pending.thr_a).toBeUndefined();
    expect(getUsageState().failover.thr_a).toBeUndefined();
  });

  it("lets the daemon pick when no choice is named", async () => {
    request.mockResolvedValue({ type: "resume_changed", schedule: null });
    await failOver("thr_a", null);
    expect(request).toHaveBeenCalledWith({
      type: "fail_over",
      thread_id: "thr_a",
      model: null,
      account: null,
    });
  });

  it("shows the daemon's refusal instead of throwing", async () => {
    request.mockRejectedValue(new Error("`gpt-x` is not an eligible failover"));
    await failOver("thr_a", { model: "gpt-x", account: null });
    expect(getUsageState().actionError?.message).toContain("not an eligible failover");
    expect(getUsageState().pending.thr_a).toBeUndefined();
  });

  it("reports a cancelled resume instead of throwing at the view", async () => {
    request.mockRejectedValue(new Error("the schedule already fired"));
    await cancelResume("thr_a");
    expect(getUsageState().actionError?.message).toContain("already fired");
    expect(getUsageState().pending.thr_a).toBeUndefined();
  });

  it("saves the policy and reports failure", async () => {
    request.mockResolvedValueOnce({ type: "ack" });
    const policy = {
      failover: false,
      max_wait_ms: 30 * 60_000,
      prompt: "Continue.",
      jitter_ms: 5_000,
    };
    expect(await setAutoResumePolicy(policy)).toBe(true);
    expect(request).toHaveBeenCalledWith({ type: "set_auto_resume_policy", policy });

    request.mockRejectedValueOnce(new Error("invalid policy"));
    expect(await setAutoResumePolicy(policy)).toBe(false);
    expect(getUsageState().policyError?.message).toContain("invalid policy");
  });
});
