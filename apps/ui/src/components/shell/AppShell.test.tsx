// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => {
  const state = {
    hosts: {
      host_ui: {
        host_id: "host_ui",
        name: "atlas",
        platform: "linux",
        online: true,
        last_seen: "2026-01-01T00:00:00.000Z",
        harnesses: ["claude_code" as const],
        latency_ms: null,
        path: null,
      },
    },
    localHostId: "host_ui",
    connection: { status: "online" },
    threads: {},
    ui: { newThreadOpen: false },
  };
  return {
    state,
    createThread: vi.fn(async () => true),
    navigate: vi.fn(),
    openAddProject: vi.fn(),
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  createThread: mock.createThread,
  navigate: mock.navigate,
  openAddProject: mock.openAddProject,
}));

vi.mock("@/features/projects/store", () => ({
  useProjects: (selector: (state: { projects: unknown[] }) => unknown) =>
    selector({ projects: [] }),
  shortPath: (path: string) => path,
}));

vi.mock("@/features/proxy/proxyStore", () => ({
  proxyDefaultModel: () => null,
  proxyModelsFor: () => [],
  useProxy: () => undefined,
  useProxyCatalog: () => undefined,
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
  }),
  useAttachShortcut: () => undefined,
}));

vi.mock("@/features/mcp/store", () => ({
  ensureMcpEvents: () => undefined,
}));

import { flushDrafts, readNewThreadDraft } from "@/lib/drafts";
import { InboxDetailEmpty } from "./AppShell";

function taskBox(): HTMLTextAreaElement {
  return screen.getByLabelText("Describe the task") as HTMLTextAreaElement;
}

describe("inbox start composer drafts", () => {
  beforeEach(() => {
    window.localStorage.clear();
    flushDrafts();
    mock.createThread.mockClear();
    mock.state.connection.status = "online";
    mock.state.ui.newThreadOpen = false;
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("persists its text per host and folder across a reload", async () => {
    const first = render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "remember this prompt" } });
    flushDrafts();

    first.unmount();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));
    expect(taskBox().value).toBe("remember this prompt");
    expect(readNewThreadDraft("host_ui", "~/code")).toBe("remember this prompt");
  });

  it("clears only the draft that was started", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "start me" } });
    fireEvent.click(screen.getByRole("button", { name: "Start thread" }));
    await waitFor(() => expect(mock.createThread).toHaveBeenCalled());

    expect(readNewThreadDraft("host_ui", "~/code")).toBe("");
    expect(taskBox().value).toBe("");
  });
});
