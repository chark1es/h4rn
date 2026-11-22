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
    // Flush what the previous test left queued before clearing storage, or a
    // pending write lands in the next test's storage and leaks into its box.
    flushDrafts();
    window.localStorage.clear();
    mock.createThread.mockReset();
    mock.createThread.mockResolvedValue(true);
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

  it("keeps text typed while the automatic first project is shown with that project", async () => {
    mock.state.projects = [
      { host: "host_ui", path: "~/code/api", name: "api" },
      { host: "host_ui", path: "~/code/web", name: "web" },
    ];
    const first = render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));
    await waitFor(() => expect(screen.getByRole("button", { name: "Project: api" })).toBeTruthy());

    // The chip shows "api"; the text belongs to that folder's slot, even
    // though nobody picked it and it is not remembered as the last folder.
    fireEvent.change(taskBox(), { target: { value: "api task" } });
    flushDrafts();

    first.unmount();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));
    await waitFor(() => expect(screen.getByRole("button", { name: "Project: api" })).toBeTruthy());

    expect(taskBox().value).toBe("api task");
    expect(drafts.readLastWorkspace("host_ui")).toBe("");

    fireEvent.click(screen.getByRole("button", { name: "Start thread" }));
    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      host: "host_ui",
      workspacePath: "~/code/api",
      prompt: "api task",
    });
  });

  it("keeps the remembered default folder when the box already holds text", async () => {
    mock.state.projects = [
      { host: "host_ui", path: "~/code/api", name: "api" },
      { host: "host_ui", path: "~/code/web", name: "web" },
    ];
    writeNewThreadDraft("host_ui", "~/code", "work in progress");
    flushDrafts();

    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    expect(taskBox().value).toBe("work in progress");
    expect(screen.getByRole("button", { name: "Project: ~/code" })).toBeTruthy();
    expect(drafts.readLastWorkspace("host_ui")).toBe("");
  });

  it("keeps the task when Tab moves focus from the folder field to Start", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "visible task" } });
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });

    const start = screen.getByRole("button", { name: "Start thread" });
    // Tabbing to Start blurs the folder field with the button as the related
    // target; no pointer press sets the flag first.
    fireEvent.blur(folderField(), { relatedTarget: start });
    fireEvent.focus(start);

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

  it("never writes a keystroke into the typed folder's saved draft", async () => {
    writeNewThreadDraft("host_ui", "~/code/api", "saved api draft");
    flushDrafts();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "visible task" } });
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });

    const start = screen.getByRole("button", { name: "Start thread" });
    // Tab skips the folder field's blur so Start sends the folder as typed.
    fireEvent.blur(folderField(), { relatedTarget: start });
    fireEvent.focus(start);
    // Going back to the box is not a Start press either: the person is still
    // writing here, so the box keeps the text they see.
    fireEvent.blur(start, { relatedTarget: taskBox() });
    expect(taskBox().value).toBe("visible task");

    fireEvent.change(taskBox(), { target: { value: "visible task!" } });
    flushDrafts();

    // The api slot is not on screen and was never typed over.
    expect(readNewThreadDraft("host_ui", "~/code/api")).toBe("saved api draft");
    expect(readNewThreadDraft("host_ui", "~/code")).toBe("visible task!");
  });

  it("clears the draft the box came from when the box is emptied on another folder", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "hello" } });
    fireEvent.change(folderField(), { target: { value: "~/code/web" } });

    const start = screen.getByRole("button", { name: "Start thread" });
    fireEvent.blur(folderField(), { relatedTarget: start });
    fireEvent.focus(start);
    fireEvent.blur(start, { relatedTarget: taskBox() });

    // Deleting everything empties the slot "hello" was saved in; it must not
    // come back the next time that folder's draft loads.
    fireEvent.change(taskBox(), { target: { value: "" } });
    flushDrafts();

    expect(readNewThreadDraft("host_ui", "~/code")).toBe("");
    expect(readNewThreadDraft("host_ui", "~/code/web")).toBe("");
  });

  it("commits the typed folder when Start loses focus outside the composer", async () => {
    writeNewThreadDraft("host_ui", "~/code/api", "saved api draft");
    flushDrafts();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "visible task" } });
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });

    const start = screen.getByRole("button", { name: "Start thread" });
    fireEvent.blur(folderField(), { relatedTarget: start });
    fireEvent.focus(start);
    fireEvent.blur(start, {
      relatedTarget: screen.getByRole("button", { name: "Host: atlas" }),
    });

    // Start was not pressed, so the skipped commit happens now, exactly like
    // the folder field's own blur: the box shows that folder's draft and the
    // session is on the folder the next Start would use.
    expect(taskBox().value).toBe("saved api draft");
    expect(drafts.readLastWorkspace("host_ui")).toBe("~/code/api");
  });

  it("does not blank a folder's draft when a late Start finishes", async () => {
    let finish: (ok: boolean) => void = () => undefined;
    mock.createThread.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    writeNewThreadDraft("host_ui", "~/code/api", "other folder draft");
    flushDrafts();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "start me" } });
    fireEvent.click(screen.getByRole("button", { name: "Start thread" }));
    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));

    // The person commits another folder, with its own draft, while Start waits.
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });
    fireEvent.blur(folderField(), { relatedTarget: null });
    expect(taskBox().value).toBe("other folder draft");

    await act(async () => {
      finish(true);
    });

    // That draft is neither blanked nor cleared: this send never saw it.
    expect(taskBox().value).toBe("other folder draft");
    expect(readNewThreadDraft("host_ui", "~/code/api")).toBe("other folder draft");
    expect(readNewThreadDraft("host_ui", "~/code")).toBe("");
  });

  it("writes text typed under a typed folder to that folder's slot", async () => {
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    // The field shows a folder nobody committed yet; text typed now belongs
    // to that folder, not to the default slot the session started on.
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });
    fireEvent.change(taskBox(), { target: { value: "api task" } });
    flushDrafts();

    expect(readNewThreadDraft("host_ui", "~/code/api")).toBe("api task");
    expect(readNewThreadDraft("host_ui", "~/code")).toBe("");
    expect(drafts.readLastWorkspace("host_ui")).toBe("");
  });

  it("keeps a typed folder's saved draft when the box already holds text", async () => {
    writeNewThreadDraft("host_ui", "~/code/api", "saved api draft");
    flushDrafts();
    render(<InboxDetailEmpty />);
    await waitFor(() => expect(taskBox().disabled).toBe(false));

    fireEvent.change(taskBox(), { target: { value: "visible task" } });
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });
    fireEvent.change(taskBox(), { target: { value: "visible task!" } });
    flushDrafts();

    // The typed folder has its own unsent text, so the edit stays with the
    // slot it was typed in rather than replacing that draft unseen.
    expect(readNewThreadDraft("host_ui", "~/code/api")).toBe("saved api draft");
    expect(readNewThreadDraft("host_ui", "~/code")).toBe("visible task!");
    expect(taskBox().value).toBe("visible task!");
  });

  it("shows the first project without remembering it", async () => {
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
