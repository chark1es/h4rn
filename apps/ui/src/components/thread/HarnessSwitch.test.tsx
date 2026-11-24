// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import type { SwitchHarnessResult } from "@/lib/store/appStore";
import { HarnessSwitch } from "./HarnessSwitch";

const mock = vi.hoisted(() => ({
  switchThreadHarness: vi.fn(async (): Promise<SwitchHarnessResult> => ({ ok: true, dropped: [] })),
  pushToast: vi.fn(),
  rows: [
    { harness: "claude_code", installed: "2.1.0" },
    { harness: "codex", installed: "0.171.0" },
  ],
  agents: [] as { id: string; name: string; installed: boolean; models: { id: string }[] }[],
}));

vi.mock("@/lib/store/appStore", () => ({
  switchThreadHarness: mock.switchThreadHarness,
  pushToast: mock.pushToast,
}));

vi.mock("@/features/harnesses/harnessStore", () => ({
  useHarnesses: (selector: (state: { rows: unknown[]; agents: unknown[] }) => unknown) =>
    selector({ rows: mock.rows, agents: mock.agents }),
  useHarnessVersions: () => undefined,
  useAcpAgents: () => undefined,
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
    runtime_mode: "plan",
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
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mock.agents = [];
  window.localStorage.clear();
});

describe("HarnessSwitch", () => {
  it("moves the thread to another installed harness and says what it dropped", async () => {
    mock.switchThreadHarness.mockResolvedValueOnce({
      ok: true,
      dropped: [{ setting: "runtime_mode", value: "plan" }],
    });
    render(<HarnessSwitch summary={summary()} />);

    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    expect(screen.getByRole("dialog", { name: "Switch harness" })).toBeTruthy();
    // Only the other installed harness is offered.
    expect(screen.queryByRole("menuitemradio", { name: "Claude Code" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Switch" }));

    expect(mock.switchThreadHarness).toHaveBeenCalledWith(
      "th_1",
      "codex",
      expect.stringContaining("gpt"),
      null,
    );
    await waitFor(() =>
      expect(mock.pushToast).toHaveBeenCalledWith(
        expect.objectContaining({
          variant: "success",
          title: "Switched to Codex.",
          description: expect.stringContaining("Plan mode"),
        }),
      ),
    );
  });

  it("shows a refusal in the panel instead of pretending the switch happened", async () => {
    mock.switchThreadHarness.mockResolvedValueOnce({
      ok: false,
      message: "this thread is still working",
      hint: "Wait for the turn to finish, then switch harness.",
    });
    render(<HarnessSwitch summary={summary()} />);

    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    fireEvent.click(screen.getByRole("button", { name: "Switch" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("this thread is still working");
    expect(alert.textContent).toContain("Wait for the turn to finish");
    expect(screen.getByRole("dialog", { name: "Switch harness" })).toBeTruthy();
  });

  it("opens from the palette's request event", () => {
    render(<HarnessSwitch summary={summary()} />);
    fireEvent(window, new CustomEvent("h4rn:switch-harness"));
    expect(screen.getByRole("dialog", { name: "Switch harness" })).toBeTruthy();
  });
});
