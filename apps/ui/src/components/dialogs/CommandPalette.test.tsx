// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";

const mock = vi.hoisted(() => {
  const thread: ThreadSummary = {
    thread_id: "th_1",
    parent_thread_id: null,
    host: "host_1",
    harness: "claude_code",
    title: "Watch the PR",
    workspace_path: "/work/api",
    branch: "h4rn/pr-watch",
    model: "claude-sonnet",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
    tab: "working",
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00Z",
    agent_state: "done",
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
    placed_by_user: false,
    resume_at: null,
    rewind: "handoff",
    file_rewind: false,
    limit: null,
    queued_count: 0,
    queue_revision: 0,
    steer: true,
  };
  return {
    state: {
      connection: { status: "online" },
      hosts: {},
      threads: { th_1: thread } as Record<string, ThreadSummary>,
      logs: {},
      ui: {
        paletteOpen: true,
        route: { kind: "thread", threadId: "th_1" },
        layout: { sidebarOpen: true, panelOpen: false },
      },
    },
    request: vi.fn(async () => ({ type: "forge_repos", repos: [] })),
    closePalette: vi.fn(),
    openLinkPullRequest: vi.fn(),
    getClient: () => ({ request: mock.request }),
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  acknowledgeThread: vi.fn(),
  closePalette: mock.closePalette,
  openLinkPullRequest: mock.openLinkPullRequest,
  getClient: mock.getClient,
  interruptThread: vi.fn(),
  navigate: vi.fn(),
  openNewThread: vi.fn(),
  openShortcuts: vi.fn(),
  openThread: vi.fn(),
  pendingApprovalIn: () => null,
  pushToast: vi.fn(),
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

vi.mock("@/hooks/use-media-query", () => ({ useIsPhone: () => false }));

import { CommandPalette } from "./CommandPalette";

beforeEach(() => {
  cleanup();
  mock.request.mockClear();
  mock.closePalette.mockClear();
  mock.openLinkPullRequest.mockClear();
  // cmdk measures its list; jsdom has neither of these APIs.
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  Element.prototype.scrollIntoView = vi.fn();
});

afterEach(cleanup);

describe("command palette: link pull request", () => {
  it("offers the action for the focused thread and opens the dialog", async () => {
    render(<CommandPalette />);

    const action = screen.getByText("Link pull request…");
    fireEvent.click(action);

    expect(mock.openLinkPullRequest).toHaveBeenCalled();
  });
});
