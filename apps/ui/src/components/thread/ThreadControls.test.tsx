// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import { suggestionsOf } from "@/test/popups";
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
    rewind: "handoff",
    file_rewind: false,
    limit: null,
    queued_count: 0,
    queue_revision: 0,
    steer: true,
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mock.agents = [];
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
    fireEvent.click(screen.getByRole("option", { name: "Plan" }));
    expect(mock.actions.setThreadRuntimeMode).toHaveBeenCalledWith("th_1", "plan");
  });

  it("opens the permission picker from the shortcut event", () => {
    render(<ThreadControls summary={summary()} />);
    fireEvent(window, new CustomEvent("h4rn:permissions"));
    expect(screen.getByRole("dialog", { name: "Thread settings" })).toBeTruthy();
  });

  it("offers effort and fast only where the harness has them", () => {
    const { unmount } = render(
      <ThreadControls
        summary={summary({ harness: "codex", reasoning_effort: "high", fast_available: true })}
      />,
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

  it("hides the Fast row when the model's tier is unknown", () => {
    // Nothing has described this model (an older daemon, or a model this
    // thread never ran): the harness-wide answer must not offer the row.
    render(<ThreadControls summary={summary({ harness: "codex", model: "gpt-c" })} />);
    fireEvent.click(screen.getByText("gpt-c"));
    expect(screen.queryByText("Fast")).toBeNull();
  });

  it("drops the Fast row when a switch lands on a model without one", () => {
    render(
      <ThreadControls
        summary={summary({
          harness: "codex",
          model: "gpt-a",
          models: ["gpt-a", "gpt-b"],
          fast_available: true,
        })}
      />,
    );
    fireEvent.click(screen.getByText("gpt-a"));
    expect(screen.getByText("Fast")).toBeTruthy();
    cleanup();

    // The daemon reports the new model has no service tier: the row is gone.
    render(
      <ThreadControls
        summary={summary({
          harness: "codex",
          model: "gpt-b",
          models: ["gpt-a", "gpt-b"],
          fast_available: false,
        })}
      />,
    );
    fireEvent.click(screen.getByText("gpt-b"));
    expect(screen.queryByText("Fast")).toBeNull();
  });

  it("keeps a checked Fast row when the model has no tier, so it can be turned off", () => {
    render(
      <ThreadControls
        summary={summary({ harness: "codex", model: "gpt-b", fast: true, fast_available: false })}
      />,
    );
    fireEvent.click(screen.getByText("gpt-b"));
    fireEvent.click(screen.getByRole("option", { name: /^Fast/ }));
    expect(mock.actions.setThreadFast).toHaveBeenCalledWith("th_1", false);
  });

  it("shows no permission rows for an agent whose modes are unknown", () => {
    mock.agents = [{ id: "demo", modes: [{ id: "code" }], models: [{ id: "m-1" }] }];
    render(
      <ThreadControls summary={summary({ harness: "acp", agent_id: "demo", model: "m-1" })} />,
    );
    fireEvent.click(screen.getByText("m-1"));
    // h4rn maps none of the agent's ids, so it offers no mode instead of
    // pretending the built-in four are available.
    expect(screen.queryByRole("menu", { name: "Permission mode" })).toBeNull();
    expect(screen.getByText(/the agent manages its own mode/)).toBeTruthy();
  });

  it("offers the agent's advertised modes and models in the composer", async () => {
    mock.agents = [
      {
        id: "demo",
        modes: [{ id: "acceptEdits" }, { id: "bypassPermissions" }],
        models: [{ id: "m-1" }, { id: "m-2" }],
      },
    ];
    render(
      <ThreadControls
        summary={summary({ harness: "acp", agent_id: "demo", model: "m-1", models: ["m-3"] })}
      />,
    );
    fireEvent.click(screen.getByText("m-1"));
    const rows = screen.getAllByRole("option").map((row) => row.textContent);
    expect(rows.some((text) => text?.includes("Auto-accept edits"))).toBe(true);
    expect(rows.some((text) => text?.includes("Full access"))).toBe(true);
    // The live session's catalogue wins over the probe's cached list.
    const models = await suggestionsOf(screen.getByRole("combobox", { name: "Model" }));
    expect(models).toEqual(["m-1", "m-3"]);
  });
});
