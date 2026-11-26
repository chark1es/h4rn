// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => {
  const summary = {
    thread_id: "thread_ui",
    parent_thread_id: null,
    host: "host_ui",
    harness: "claude_code",
    title: "A thread",
    workspace_path: "~/code",
    model: "claude-sonnet-4-5",
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00.000Z",
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
    rewind: "handoff",
    file_rewind: false,
    limit: null,
  };
  const state = {
    threads: { thread_ui: summary },
    queues: {},
    hosts: { host_ui: { online: true, name: "atlas" } },
    logs: {
      thread_ui: { turns: [], pending: [], loaded: true, loading: false, approvals: {} },
    },
    threadErrors: {},
    connection: { status: "online" },
    lastSyncAt: null,
    ui: {
      focusComposerNonce: 0,
      newThreadOpen: false,
      layout: { terminalOpen: false, view: "thread" as const },
    },
  };
  return {
    state,
    ensureThread: vi.fn(),
    interruptThread: vi.fn(),
    navigate: vi.fn(),
    requestComposerFocus: vi.fn(),
    retryThread: vi.fn(),
    setPanelTab: vi.fn(),
    toggleTerminal: vi.fn(),
    toggleView: vi.fn(),
    openNewThread: vi.fn(),
    openInIde: vi.fn(),
    openThread: vi.fn(),
    sendMessage: vi.fn(),
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getState: () => mock.state,
  PREVIEW_EVENT: "h4rn:preview",
  ensureThread: mock.ensureThread,
  interruptThread: mock.interruptThread,
  navigate: mock.navigate,
  requestComposerFocus: mock.requestComposerFocus,
  retryThread: mock.retryThread,
  setPanelTab: mock.setPanelTab,
  toggleTerminal: mock.toggleTerminal,
  toggleView: mock.toggleView,
  openNewThread: mock.openNewThread,
  openInIde: mock.openInIde,
  openThread: mock.openThread,
  sendMessage: mock.sendMessage,
  rewindRefusal: () => null,
  filesRewindAvailable: () => false,
  planRewind: vi.fn(async () => ({
    method: "handoff",
    files_enabled: false,
    files_available: false,
    left_alone: [],
  })),
  rewindThread: vi.fn(),
  consumeComposerRestore: vi.fn(),
  editQueuedMessage: async () => true,
  setUi: () => undefined,
  getClient: () => {
    throw new Error("The test never reads the workspace.");
  },
}));

vi.mock("@/hooks/use-media-query", () => ({
  useIsPhone: () => true,
}));

vi.mock("@/features/attachments/use-attachments", () => ({
  useAttachmentDrafts: () => ({
    attachments: [],
    tasks: [],
    uploading: false,
    failed: false,
    blockedReason: null,
    inputRef: { current: null },
    addFiles: () => undefined,
    addPastedText: () => false,
    openPicker: () => undefined,
    onInputChange: () => undefined,
    remove: () => undefined,
    retry: () => undefined,
    clear: () => undefined,
    restore: () => undefined,
  }),
  useAttachShortcut: () => undefined,
}));

vi.mock("@/features/proxy/proxyStore", () => ({
  useProxyCatalog: () => undefined,
  useProxy: () => undefined,
  proxyOverrideFor: () => null,
  proxyModelsFor: () => [],
}));

vi.mock("@/features/subagents/DelegateDialog", () => ({
  DelegateDialog: () => null,
}));

vi.mock("@/features/subagents/subagentStore", () => ({
  loadSubagents: vi.fn(async () => undefined),
  openDelegate: vi.fn(),
  useSubagentCount: () => ({ total: 0, waiting: 0 }),
  useSubagentViews: () => [],
}));

vi.mock("@/components/panel/usePanelChanges", () => ({
  usePanelChanges: () => ({
    changes: { files: [] },
    turns: [],
    added: 0,
    removed: 0,
    error: null,
    loading: false,
    diffs: {},
    diffsLoading: false,
  }),
}));

vi.mock("./ThreadHeader", () => ({ ThreadHeader: () => null }));
vi.mock("./ThreadStream", () => ({
  ThreadStream: () => null,
  ThreadStreamSkeleton: () => null,
}));
vi.mock("./ApprovalCard", () => ({ ApprovalCard: () => null }));
vi.mock("./PhoneApproval", () => ({
  PhoneApprovalSheet: () => null,
  PhoneApprovalSummary: () => null,
}));
vi.mock("@/features/terminal/TerminalPanel", () => ({ TerminalPanel: () => null }));
vi.mock("@/features/usage/AutoResumeCard", () => ({ AutoResumeCards: () => null }));
vi.mock("@/features/usage/UsageFooter", () => ({ UsageFooter: () => null }));

import { ThreadPane } from "./ThreadPane";

function composer(): HTMLTextAreaElement {
  return screen.getByRole("textbox") as HTMLTextAreaElement;
}

function typeInto(value: string): void {
  const input = composer();
  fireEvent.change(input, { target: { value } });
  input.setSelectionRange(value.length, value.length);
  fireEvent.select(input);
}

beforeEach(() => {
  window.localStorage.clear();
  mock.setPanelTab.mockReset();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ThreadPane phone / commands", () => {
  it("opens the phone sheet on the Changes segment from the composer", async () => {
    render(<ThreadPane threadId="thread_ui" />);

    typeInto("/chan");
    await waitFor(() => expect(screen.getByRole("option", { name: /Open changes/ })).toBeTruthy());
    const input = composer();
    const focus = vi.spyOn(input, "focus");
    fireEvent.keyDown(input, { key: "Enter" });

    const dialog = await screen.findByRole("dialog", { name: "Thread panel" });
    expect(dialog).toBeTruthy();
    // The sheet is modal: focus moves into it, so typing no longer edits the
    // draft behind it.
    await waitFor(() => expect(document.activeElement).toBe(dialog));
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    // The sheet owns the interaction; the composer must not steal focus back.
    expect(focus).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: /Changes/ }).getAttribute("aria-selected")).toBe(
        "true",
      ),
    );
    expect(mock.setPanelTab).not.toHaveBeenCalled();
  });

  it("switches the open sheet to the Subagents segment", async () => {
    render(<ThreadPane threadId="thread_ui" />);

    typeInto("/chan");
    await waitFor(() => expect(screen.getByRole("option", { name: /Open changes/ })).toBeTruthy());
    fireEvent.keyDown(composer(), { key: "Enter" });
    await screen.findByRole("dialog", { name: "Thread panel" });

    typeInto("/sub");
    await waitFor(() =>
      expect(screen.getByRole("option", { name: /Open subagents/ })).toBeTruthy(),
    );
    fireEvent.keyDown(composer(), { key: "Enter" });

    await waitFor(() =>
      expect(screen.getByRole("tab", { name: /Subagents/ }).getAttribute("aria-selected")).toBe(
        "true",
      ),
    );
    expect(mock.setPanelTab).not.toHaveBeenCalled();
  });
});
