// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    projects: [] as Array<{ host: string; path: string; name: string }>,
    ui: { newThreadOpen: false },
  };
  return {
    state,
    createThread: vi.fn<(input: Record<string, unknown>) => Promise<boolean>>(async () => true),
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
    selector({ projects: mock.state.projects }),
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

import {
  draftStorageKey,
  drafts,
  flushDrafts,
  readNewThreadDraft,
  writeNewThreadDraft,
} from "@/lib/drafts";
import { NewThreadDraftSession } from "@/lib/newThreadDraft";
import { InboxDetailEmpty } from "./AppShell";

function taskBox(): HTMLTextAreaElement {
  return screen.getByLabelText("Describe the task") as HTMLTextAreaElement;
}

function folderField(): HTMLInputElement {
  return screen.getByPlaceholderText("~/code/api") as HTMLInputElement;
}

describe("inbox start composer drafts", () => {
  beforeEach(() => {
    window.localStorage.clear();
    flushDrafts();
    mock.createThread.mockClear();
    mock.state.connection.status = "online";
    mock.state.ui.newThreadOpen = false;
    mock.state.projects = [];
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

  it("sends the task on screen to the folder as typed when Start is pressed", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "visible task" } });
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });

    const start = screen.getByRole("button", { name: "Start thread" });
    // A real press blurs the folder field on the way to the button.
    fireEvent.pointerDown(start, { button: 0, pointerType: "mouse" });
    fireEvent.blur(folderField());
    fireEvent.pointerUp(start, { button: 0, pointerType: "mouse" });
    // The blur must not swap the box to the typed folder's (empty) draft.
    expect(taskBox().value).toBe("visible task");

    fireEvent.click(start);

    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      host: "host_ui",
      workspacePath: "~/code/api",
      prompt: "visible task",
    });
  });

  it("does not blank the box when another tab saved newer text", async () => {
    let finish: (ok: boolean) => void = () => undefined;
    mock.createThread.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "start me" } });
    fireEvent.click(screen.getByRole("button", { name: "Start thread" }));
    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));

    // Another tab saves newer text for the same slot while Start waits.
    writeNewThreadDraft("host_ui", "~/code", "newer from another tab");
    flushDrafts();

    await act(async () => {
      finish(true);
    });

    expect(readNewThreadDraft("host_ui", "~/code")).toBe("newer from another tab");
    expect(taskBox().value).toBe("newer from another tab");
  });

  it("shows what the New thread dialog typed into the same slot", async () => {
    const view = render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    // The dialog opens on top and binds the same host and folder slot.
    mock.state.ui.newThreadOpen = true;
    view.rerender(<InboxDetailEmpty />);
    const dialog = new NewThreadDraftSession(drafts);
    dialog.adoptHost("host_ui");
    dialog.setText("typed in the dialog");

    mock.state.ui.newThreadOpen = false;
    view.rerender(<InboxDetailEmpty />);

    await waitFor(() => expect(taskBox().value).toBe("typed in the dialog"));
  });

  it("adopts another tab's text for its slot", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    const key = draftStorageKey("new-thread", "host_ui", "~/code");
    const remote = JSON.stringify({
      text: "from the other tab",
      updatedAt: Date.now(),
      nonce: "other",
    });
    window.localStorage.setItem(key, remote);
    window.dispatchEvent(new StorageEvent("storage", { key, newValue: remote }));

    await waitFor(() => expect(taskBox().value).toBe("from the other tab"));
  });

  it("restores the folder the person picked with its draft", async () => {
    mock.state.projects = [
      { host: "host_ui", path: "~/code/api", name: "api" },
      { host: "host_ui", path: "~/code/web", name: "web" },
    ];
    const first = render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));
    await waitFor(() => expect(screen.getByRole("button", { name: "Project: api" })).toBeTruthy());

    fireEvent.pointerDown(screen.getByRole("button", { name: "Project: api" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /web/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Project: web" })).toBeTruthy());

    fireEvent.change(taskBox(), { target: { value: "web task" } });
    flushDrafts();
    expect(drafts.readLastWorkspace("host_ui")).toBe("~/code/web");

    first.unmount();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    expect(taskBox().value).toBe("web task");
    expect(screen.getByRole("button", { name: "Project: web" })).toBeTruthy();
    // Mounting read the remembered folder; it did not write another one.
    expect(drafts.readLastWorkspace("host_ui")).toBe("~/code/web");
  });

  it("shows the first project without remembering it or switching the slot", async () => {
    mock.state.projects = [
      { host: "host_ui", path: "~/code/api", name: "api" },
      { host: "host_ui", path: "~/code/web", name: "web" },
    ];
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    await waitFor(() => expect(screen.getByRole("button", { name: "Project: api" })).toBeTruthy());
    expect(taskBox().value).toBe("");
    // A folder the person never picked is not the one the next open remembers.
    expect(drafts.readLastWorkspace("host_ui")).toBe("");
  });
});
