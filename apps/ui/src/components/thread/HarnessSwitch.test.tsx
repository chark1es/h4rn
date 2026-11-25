// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CommandPalette } from "@/components/dialogs/CommandPalette";
import type { ThreadSummary } from "@/gen";
import type { SwitchHarnessResult } from "@/lib/store/appStore";
import { HarnessSwitch, HarnessSwitchCommand } from "./HarnessSwitch";

const mock = vi.hoisted(() => ({
  switchThreadHarness: vi.fn(async (): Promise<SwitchHarnessResult> => ({ ok: true, dropped: [] })),
  pushToast: vi.fn(),
  phone: false,
  rows: [
    { harness: "claude_code", installed: "2.1.0" },
    { harness: "codex", installed: "0.171.0" },
  ],
  agents: [] as { id: string; name: string; installed: boolean; models: { id: string }[] }[],
  state: {
    connection: { status: "online" },
    hosts: {},
    threads: {} as Record<string, ThreadSummary>,
    logs: {},
    ui: {
      paletteOpen: true,
      route: { kind: "thread", threadId: "th_1" },
      selectedThreadId: "th_1",
      layout: { sidebarOpen: true, panelOpen: false, terminalOpen: false },
    },
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  switchThreadHarness: mock.switchThreadHarness,
  pushToast: mock.pushToast,
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  // The real palette's actions and state reads.
  acknowledgeThread: vi.fn(),
  closePalette: vi.fn(),
  getClient: () => ({ request: vi.fn(async () => ({ type: "forge_repos", repos: [] })) }),
  interruptThread: vi.fn(),
  navigate: vi.fn(),
  openLinkPullRequest: vi.fn(),
  openNewThread: vi.fn(),
  openShortcuts: vi.fn(),
  openThread: vi.fn(),
  pendingApprovalIn: () => null,
  requestSearchFocus: vi.fn(),
  respondApproval: vi.fn(),
  setPanelTab: vi.fn(),
  setTab: vi.fn(),
  setThreadTab: vi.fn(),
  togglePanel: vi.fn(),
  togglePin: vi.fn(),
  toggleSidebar: vi.fn(),
  toggleTerminal: vi.fn(),
  toggleTheme: vi.fn(),
  toggleView: vi.fn(),
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

vi.mock("@/hooks/use-media-query", () => ({ useIsPhone: () => mock.phone }));

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
    queued_count: 0,
    queue_revision: 0,
    rewind: "native",
    file_rewind: false,
    steer: true,
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mock.agents = [];
  mock.phone = false;
  mock.state.threads = {};
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
          // The picker's own label for the mode, not a slug with a suffix.
          description: expect.stringContaining("Dropped Plan;"),
        }),
      ),
    );
  });

  it("says the thread continues from a handoff, not a summary", () => {
    render(<HarnessSwitch summary={summary()} />);
    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    const dialog = screen.getByRole("dialog", { name: "Switch harness" });
    expect(dialog.textContent).toContain("handoff");
    expect(dialog.textContent).not.toContain("summary");
  });

  it("defaults to the model the host's live catalogue reports", () => {
    mock.state.threads = {
      th_codex: summary({
        thread_id: "th_codex",
        harness: "codex",
        host: "host_1",
        model: "gpt-5.4",
        models: ["gpt-5.4", "gpt-5.4-mini"],
        last_activity: "2026-02-01T00:00:00Z",
      }),
    };
    render(<HarnessSwitch summary={summary()} />);
    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    const input = screen.getByLabelText("Model for the new harness") as HTMLInputElement;
    expect(input.value).toBe("gpt-5.4");
  });

  it("lists an ACP target's own models, never a sibling agent's", () => {
    mock.agents = [
      { id: "agent-a", name: "Agent A", installed: true, models: [{ id: "model-a" }] },
      { id: "agent-b", name: "Agent B", installed: true, models: [{ id: "model-b" }] },
    ];
    // A thread on agent A reported these to the host. Agent B must not offer
    // them just because it is also an ACP agent on the same host.
    mock.state.threads = {
      th_a: summary({
        thread_id: "th_a",
        harness: "acp",
        agent_id: "agent-a",
        host: "host_1",
        model: "model-a",
        models: ["model-a", "model-a-old"],
      }),
    };
    render(<HarnessSwitch summary={summary()} />);
    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "ACP · Agent B" }));

    const input = screen.getByLabelText("Model for the new harness") as HTMLInputElement;
    expect(input.value).toBe("model-b");
    const options = Array.from(
      document.querySelectorAll('datalist[id="h4rn-switch-models-th_1"] option'),
    ).map((option) => option.getAttribute("value"));
    expect(options).toContain("model-b");
    expect(options).not.toContain("model-a");
    expect(options).not.toContain("model-a-old");
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

  it("opens a viewport-bounded sheet with full-size rows on a phone", () => {
    mock.phone = true;
    render(<HarnessSwitch summary={summary()} />);
    fireEvent.click(screen.getByRole("button", { name: /Switch harness/i }));
    const dialog = screen.getByRole("dialog", { name: "Switch harness" });
    expect(dialog.className).toContain("q-ph-sheet");
    expect(dialog.className).toContain("q-switch-sheet");
    // The phone gets buttons, not a desktop popover.
    expect(screen.queryByRole("button", { name: "Cancel" })).not.toBeNull();
    expect(dialog.textContent).toContain("Codex");
  });

  it("asks for a switch from the command palette", () => {
    globalThis.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof ResizeObserver;
    Element.prototype.scrollIntoView = vi.fn();
    mock.state.threads = { th_1: summary() };
    const heard = vi.fn();
    window.addEventListener("h4rn:switch-harness", heard);
    render(<CommandPalette />);
    fireEvent.click(screen.getByText("Switch harness…"));
    expect(heard).toHaveBeenCalledTimes(1);
    window.removeEventListener("h4rn:switch-harness", heard);
  });

  it("opens the switch panel for the focused thread without a thread header", () => {
    mock.state.threads = { th_1: summary() };
    // No ThreadHeader anywhere: the palette's surface must still reach the
    // thread the shell is showing.
    render(<HarnessSwitchCommand />);
    fireEvent(window, new CustomEvent("h4rn:switch-harness"));
    const dialog = screen.getByRole("dialog", { name: "Switch harness" });
    expect(dialog.textContent).toContain("Codex");
    expect(screen.getByLabelText("Model for the new harness")).toBeTruthy();
  });
});
