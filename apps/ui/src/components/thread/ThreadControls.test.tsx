// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import { ThreadControls } from "./ThreadControls";

const mock = vi.hoisted(() => ({
  actions: {
    setThreadFast: vi.fn(async () => true),
    setThreadModel: vi.fn(async () => true),
    setThreadReasoningEffort: vi.fn(async () => true),
    setThreadRuntimeMode: vi.fn(async () => true),
  },
  agents: [] as { id: string; modes: { id: string }[]; models: { id: string }[] }[],
}));

vi.mock("@/lib/store/appStore", () => ({
  setThreadFast: mock.actions.setThreadFast,
  setThreadModel: mock.actions.setThreadModel,
  setThreadReasoningEffort: mock.actions.setThreadReasoningEffort,
  setThreadRuntimeMode: mock.actions.setThreadRuntimeMode,
}));

vi.mock("@/features/harnesses/harnessStore", () => ({
  useHarnesses: () => mock.agents,
}));

vi.mock("@/features/proxy/proxyStore", () => ({
  useProxyCatalog: () => undefined,
  proxyModelsFor: () => [],
}));

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

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  window.localStorage.clear();
});

describe("ThreadControls", () => {
  it("shows the model and only the settings that differ from the default", () => {
    render(<ThreadControls summary={summary({ runtime_mode: "full_access" })} />);
    expect(screen.getByText("claude-sonnet-5-5")).toBeTruthy();
    expect(screen.getByText("Full access")).toBeTruthy();
  });

  it("changes the permission mode from the panel", () => {
    render(<ThreadControls summary={summary()} />);
    fireEvent.click(screen.getByText("claude-sonnet-5-5"));
    expect(screen.getByRole("dialog", { name: "Thread settings" })).toBeTruthy();
    fireEvent.click(screen.getByRole("menuitemradio", { name: "Plan" }));
    expect(mock.actions.setThreadRuntimeMode).toHaveBeenCalledWith("th_1", "plan");
  });

  it("opens the permission picker from the shortcut event", () => {
    render(<ThreadControls summary={summary()} />);
    fireEvent(window, new CustomEvent("h4rn:permissions"));
    expect(screen.getByRole("dialog", { name: "Thread settings" })).toBeTruthy();
  });

  it("offers effort and fast only where the harness has them", () => {
    const { unmount } = render(
      <ThreadControls summary={summary({ harness: "codex", reasoning_effort: "high" })} />,
    );
    fireEvent.click(screen.getByText("claude-sonnet-5-5"));
    expect(screen.getByLabelText("Reasoning effort")).toBeTruthy();
    expect(screen.getByText("Fast")).toBeTruthy();
    unmount();

    cleanup();
    render(<ThreadControls summary={summary({ harness: "opencode" })} />);
    fireEvent.click(screen.getByText("claude-sonnet-5-5"));
    expect(screen.queryByLabelText("Reasoning effort")).toBeNull();
    expect(screen.queryByText("Fast")).toBeNull();
  });
});
