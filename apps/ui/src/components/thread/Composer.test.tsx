// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MessageRole, ThreadSummary } from "@/gen";
import type { StreamTurn, ThreadLog, TurnItem } from "@/lib/store/projection";

const mock = vi.hoisted(() => {
  const state: {
    connection: { status: string };
    hosts: Record<string, { online: boolean; name: string }>;
    logs: Record<string, unknown>;
    ui: {
      focusComposerNonce: number;
      newThreadOpen: boolean;
      phone: boolean;
      layout: { terminalOpen: boolean; view: "thread" | "ide" };
    };
  } = {
    connection: { status: "online" },
    hosts: { host_ui: { online: true, name: "atlas" } },
    logs: {},
    ui: {
      focusComposerNonce: 0,
      newThreadOpen: false,
      phone: false,
      layout: { terminalOpen: false, view: "thread" },
    },
  };
  return {
    state,
    sendMessage:
      vi.fn<(threadId: string, text: string, attachments: unknown[]) => Promise<boolean>>(),
    interruptThread: vi.fn(),
    toggleTerminal: vi.fn(),
    toggleView: vi.fn(),
    setPanelTab: vi.fn(),
    openNewThread: vi.fn(),
    attachments: () => ({
      attachments: [] as unknown[],
      tasks: [] as unknown[],
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
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getState: () => mock.state,
  sendMessage: mock.sendMessage,
  interruptThread: mock.interruptThread,
  toggleTerminal: mock.toggleTerminal,
  toggleView: mock.toggleView,
  setPanelTab: mock.setPanelTab,
  openNewThread: mock.openNewThread,
  getClient: () => {
    throw new Error("Tests inject a workspace file lister.");
  },
}));

vi.mock("@/features/attachments/use-attachments", () => ({
  useAttachmentDrafts: () => mock.attachments(),
  useAttachShortcut: () => undefined,
}));

vi.mock("@/features/proxy/proxyStore", () => ({
  useProxyCatalog: () => undefined,
  useProxy: () => undefined,
  proxyOverrideFor: () => null,
  proxyModelsFor: () => [],
}));

vi.mock("@/hooks/use-media-query", () => ({
  useIsPhone: () => mock.state.ui.phone,
}));

import {
  clearWorkspaceFilesCache,
  setWorkspaceFileListerForTests,
} from "@/features/composer/workspaceFiles";
import type { FileEntry, FsListing } from "@/gen";
import { draftStorageKey, flushDrafts, readThreadDraft, writeThreadDraft } from "@/lib/drafts";
import { Composer } from "./Composer";

const HOST = "host_ui";
const THREAD = "thread_ui";

const summary: ThreadSummary = {
  thread_id: THREAD,
  parent_thread_id: null,
  host: HOST,
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
  limit: null,
};

function message(role: MessageRole, text: string): TurnItem {
  return {
    kind: "message",
    itemId: `${role}:${text}`,
    role,
    text,
    attachments: [],
    streaming: false,
  };
}

function log(prompts: string[]): ThreadLog {
  const turns: StreamTurn[] = prompts.map((text, index) => ({
    id: `turn-${index}`,
    startedAt: "2026-01-01T12:00:00.000Z",
    status: "completed",
    items: [message("user", text)],
    usage: null,
    changes: null,
  }));
  return { turns, nextSeq: 0, loaded: true, loading: false, approvals: {}, pending: [] };
}

let rafQueue: FrameRequestCallback[] = [];

function runAnimationFrames(): void {
  act(() => {
    const queued = rafQueue;
    rafQueue = [];
    for (const callback of queued) callback(0);
  });
}

function renderComposer() {
  return render(<Composer summary={summary} />);
}

function textarea(): HTMLTextAreaElement {
  return screen.getByRole("textbox") as HTMLTextAreaElement;
}

const WORKSPACE_FILES = [
  "README.md",
  "apps/ui/src/components/Composer.tsx",
  "apps/ui/src/lib/composerMenu.ts",
  "docs/ux.md",
  "src/with space.ts",
];

function fileEntry(path: string): FileEntry {
  return {
    name: path.split("/").pop() ?? path,
    path,
    kind: "file",
    size_bytes: 10,
    modified_at: null,
    git_status: null,
  };
}

function workspaceListing(path: string, files: string[], truncated = false): FsListing {
  return {
    workspace: "/work/repo",
    path,
    entries: path === "" ? files.map(fileEntry) : [],
    truncated,
  };
}

function stubWorkspaceFiles(files: string[] = WORKSPACE_FILES, truncated = false): void {
  setWorkspaceFileListerForTests(async ({ path }) => workspaceListing(path, files, truncated));
}

/** Type, then put the caret at the end the way a browser would. */
function typeInto(input: HTMLTextAreaElement, value: string, caret = value.length): void {
  fireEvent.change(input, { target: { value } });
  input.setSelectionRange(caret, caret);
  fireEvent.select(input);
}

function option(pattern: RegExp): HTMLElement | null {
  return screen.queryByRole("option", { name: pattern });
}

beforeEach(() => {
  flushDrafts();
  window.localStorage.clear();
  rafQueue = [];
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    rafQueue.push(callback);
    return rafQueue.length;
  });
  mock.state.logs = {};
  mock.state.connection.status = "online";
  mock.state.ui.phone = false;
  mock.state.ui.layout = { terminalOpen: false, view: "thread" };
  mock.sendMessage.mockReset();
  mock.interruptThread.mockReset();
  mock.toggleTerminal.mockReset();
  mock.toggleView.mockReset();
  mock.setPanelTab.mockReset();
  mock.openNewThread.mockReset();
  clearWorkspaceFilesCache();
  stubWorkspaceFiles();
});

afterEach(() => {
  cleanup();
  setWorkspaceFileListerForTests(null);
  clearWorkspaceFilesCache();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  delete (Element.prototype as unknown as Record<string, unknown>).scrollIntoView;
});

describe("Composer drafts and history", () => {
  it("leaves the stored draft alone while browsing history", () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    const view = renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "half typed" } });
    fireEvent.keyDown(input, { key: "ArrowUp" });

    expect(input.value).toBe("earlier prompt");
    expect(readThreadDraft(HOST, THREAD)).toBe("half typed");

    // A reload builds a fresh composer from the store.
    flushDrafts();
    view.unmount();
    renderComposer();

    expect(textarea().value).toBe("half typed");
  });

  it("keeps walking back and forward through multi-line prompts", () => {
    mock.state.logs[THREAD] = log(["alpha\nbeta", "gamma\ndelta", "epsilon\nzeta"]);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "in progress" } });
    input.setSelectionRange(0, 0);

    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("epsilon\nzeta");
    expect(input.selectionStart).toBe(0);

    // The caret starts at the top of the recalled multi-line prompt, so the
    // first-line rule does not stop the second ArrowUp.
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("gamma\ndelta");

    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("alpha\nbeta");

    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("alpha\nbeta");

    fireEvent.keyDown(input, { key: "ArrowDown" });
    runAnimationFrames();
    expect(input.value).toBe("gamma\ndelta");

    fireEvent.keyDown(input, { key: "ArrowDown" });
    runAnimationFrames();
    expect(input.value).toBe("epsilon\nzeta");

    fireEvent.keyDown(input, { key: "ArrowDown" });
    runAnimationFrames();
    expect(input.value).toBe("in progress");
    expect(readThreadDraft(HOST, THREAD)).toBe("in progress");
  });

  it("does not start history from a caret below the first line", () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "line one\nline two" } });
    input.setSelectionRange(input.value.length, input.value.length);
    fireEvent.keyDown(input, { key: "ArrowUp" });

    expect(input.value).toBe("line one\nline two");
  });

  it("clears the draft after a successful send", async () => {
    mock.sendMessage.mockResolvedValue(true);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "send me" } });
    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));

    await waitFor(() => expect(input.value).toBe(""));
    expect(mock.sendMessage).toHaveBeenCalledWith(THREAD, "send me", []);
    expect(readThreadDraft(HOST, THREAD)).toBe("");
  });

  it("clears the draft it loaded, before any typing", async () => {
    writeThreadDraft(HOST, THREAD, "loaded draft");
    flushDrafts();
    mock.sendMessage.mockResolvedValue(true);
    renderComposer();
    const input = textarea();
    expect(input.value).toBe("loaded draft");

    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));

    await waitFor(() => expect(input.value).toBe(""));
    expect(readThreadDraft(HOST, THREAD)).toBe("");
  });

  it("clears against the revision it showed, not a newer one in storage", async () => {
    let finish: (ok: boolean) => void = () => undefined;
    mock.sendMessage.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "being sent" } });
    flushDrafts();
    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));
    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalled());

    // Another tab saves newer text without this tab hearing about it.
    const key = draftStorageKey("thread", HOST, THREAD);
    window.localStorage.setItem(
      key,
      JSON.stringify({
        text: "newer from elsewhere",
        updatedAt: Date.now() + 1000,
        nonce: "other",
      }),
    );

    await act(async () => {
      finish(true);
    });

    expect(readThreadDraft(HOST, THREAD)).toBe("newer from elsewhere");
  });

  it("keeps the draft when the send fails", async () => {
    mock.sendMessage.mockResolvedValue(false);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "keep me" } });
    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));

    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalled());
    expect(input.value).toBe("keep me");
    expect(readThreadDraft(HOST, THREAD)).toBe("keep me");
  });

  it("leaves history mode when the user types after a recall", () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "half typed" } });
    input.setSelectionRange(0, 0);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("earlier prompt");

    fireEvent.change(input, { target: { value: "earlier prompt edited" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    runAnimationFrames();

    expect(input.value).toBe("earlier prompt edited");
    expect(readThreadDraft(HOST, THREAD)).toBe("earlier prompt edited");
  });

  it("keeps a newer draft from another tab when the send finishes", async () => {
    let finish: (ok: boolean) => void = () => undefined;
    mock.sendMessage.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "being sent" } });
    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));
    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalled());

    // The other tab saves newer text for the same thread while the send waits.
    const key = draftStorageKey("thread", HOST, THREAD);
    const remote = JSON.stringify({ text: "written elsewhere", updatedAt: Date.now() + 1000 });
    window.localStorage.setItem(key, remote);
    window.dispatchEvent(new StorageEvent("storage", { key, newValue: remote }));
    await waitFor(() => expect(input.value).toBe("written elsewhere"));

    await act(async () => {
      finish(true);
    });

    expect(readThreadDraft(HOST, THREAD)).toBe("written elsewhere");
    expect(input.value).toBe("written elsewhere");
  });

  it("does not clear the next thread's box when a late send finishes", async () => {
    let finish: (ok: boolean) => void = () => undefined;
    mock.sendMessage.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    const secondThread = "thread_ui_two";
    writeThreadDraft(HOST, secondThread, "second thread draft");
    flushDrafts();

    const view = renderComposer();
    fireEvent.change(textarea(), { target: { value: "sent from the first thread" } });
    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));
    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalled());

    // The pane switches threads while the send is still in flight.
    view.rerender(<Composer summary={{ ...summary, thread_id: secondThread }} />);
    await waitFor(() => expect(textarea().value).toBe("second thread draft"));

    await act(async () => {
      finish(true);
    });

    expect(textarea().value).toBe("second thread draft");
    expect(readThreadDraft(HOST, secondThread)).toBe("second thread draft");
    expect(readThreadDraft(HOST, THREAD)).toBe("");
  });

  it("adopts another tab's draft only when it has no unsaved edit", async () => {
    writeThreadDraft(HOST, THREAD, "saved elsewhere");
    flushDrafts();
    renderComposer();
    const input = textarea();
    expect(input.value).toBe("saved elsewhere");

    const key = draftStorageKey("thread", HOST, THREAD);
    const remote = JSON.stringify({ text: "from the other tab", updatedAt: Date.now() });
    window.localStorage.setItem(key, remote);
    window.dispatchEvent(new StorageEvent("storage", { key, newValue: remote }));
    await waitFor(() => expect(input.value).toBe("from the other tab"));

    fireEvent.change(input, { target: { value: "my unsaved edit" } });
    window.localStorage.setItem(
      key,
      JSON.stringify({ text: "clobbered", updatedAt: Date.now() + 1 }),
    );
    window.dispatchEvent(
      new StorageEvent("storage", {
        key,
        newValue: JSON.stringify({ text: "clobbered", updatedAt: Date.now() + 1 }),
      }),
    );
    expect(input.value).toBe("my unsaved edit");
  });
});

describe("Composer @ file picker", () => {
  it("opens at a word boundary and lists workspace files", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "ask @");

    expect(await screen.findByRole("listbox", { name: "Files" })).toBeTruthy();
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());
    expect(option(/with space/)).not.toBeNull();
    expect(input.value).toBe("ask @");
  });

  it("does not open inside a word", async () => {
    renderComposer();
    typeInto(textarea(), "me@example.com");
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("opens after punctuation and keeps filtering as more is typed", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "look (@");
    expect(await screen.findByRole("listbox", { name: "Files" })).toBeTruthy();

    typeInto(input, "look (@read");
    expect(await screen.findByRole("option", { name: /README\.md/ })).toBeTruthy();
    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();
    expect(input.value).toBe("look (@README.md ");
  });

  it("filters with path-aware matching after a debounce", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "@components/comp");

    await waitFor(() => expect(option(/components\/Composer\.tsx/)).not.toBeNull(), {
      timeout: 1_000,
    });
    await waitFor(() => expect(option(/README\.md/)).toBeNull(), { timeout: 1_000 });
  });

  it("inserts the highlighted file with Enter, quotes spaces and moves the caret", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "open @with");
    await waitFor(() => expect(option(/src\/with space\.ts/)).not.toBeNull(), {
      timeout: 1_000,
    });
    await waitFor(() => expect(option(/README\.md/)).toBeNull(), { timeout: 1_000 });

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();

    expect(input.value).toBe('open @"src/with space.ts" ');
    expect(input.selectionStart).toBe(input.value.length);
    expect(input.selectionEnd).toBe(input.value.length);
    expect(mock.sendMessage).not.toHaveBeenCalled();
    expect(readThreadDraft(HOST, THREAD)).toBe('open @"src/with space.ts" ');
  });

  it("inserts with Tab and also inserts on click", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "@read");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull(), { timeout: 1_000 });
    fireEvent.keyDown(input, { key: "Tab" });
    runAnimationFrames();
    expect(input.value).toBe("@README.md ");

    typeInto(input, "@menu");
    await waitFor(() => expect(option(/composerMenu\.ts/)).not.toBeNull(), {
      timeout: 1_000,
    });
    fireEvent.click(option(/composerMenu\.ts/) as HTMLElement);
    runAnimationFrames();
    expect(input.value).toBe("@apps/ui/src/lib/composerMenu.ts ");
  });

  it("moves with arrows without recalling history, and Enter never sends while open", async () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    renderComposer();
    const input = textarea();
    typeInto(input, "@");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());

    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("@");
    expect(input.value).not.toBe("earlier prompt");

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();
    expect(mock.sendMessage).not.toHaveBeenCalled();
  });

  it("closes on Escape, leaves the typed text, then normal keys work again", async () => {
    mock.sendMessage.mockResolvedValue(true);
    renderComposer();
    const input = textarea();
    typeInto(input, "@comp");
    await waitFor(() => expect(option(/Composer\.tsx/)).not.toBeNull(), { timeout: 1_000 });

    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(input.value).toBe("@comp");

    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalledWith(THREAD, "@comp", []));
  });

  it("opens from the @ button", async () => {
    renderComposer();
    fireEvent.click(screen.getByRole("button", { name: "Mention a file" }));

    expect(await screen.findByRole("listbox", { name: "Files" })).toBeTruthy();
    expect(textarea().value).toBe("@");
  });

  it("separates the @ button from a word and keeps the menu usable", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "ask");

    fireEvent.click(screen.getByRole("button", { name: "Mention a file" }));
    runAnimationFrames();
    expect(input.value).toBe("ask @");
    expect(await screen.findByRole("listbox", { name: "Files" })).toBeTruthy();

    // The next keystroke filters instead of closing the menu.
    typeInto(input, "ask @read");
    expect(await screen.findByRole("option", { name: /README\.md/ })).toBeTruthy();
  });

  it("offers a quiet footer when the index hit a cap", async () => {
    stubWorkspaceFiles(WORKSPACE_FILES, true);
    renderComposer();
    const input = textarea();
    typeInto(input, "@");
    await waitFor(() => expect(screen.getByText("Showing the first 5 files.")).toBeTruthy());

    typeInto(input, "@nothing-matches");
    expect(await screen.findByText("No files match “nothing-matches”.")).toBeTruthy();
    expect(screen.getByText("Showing the first 5 files.")).toBeTruthy();
  });

  it("gives every option an id, announces the highlight and scrolls it into view", async () => {
    const scrollIntoView = vi.fn();
    (Element.prototype as unknown as { scrollIntoView: unknown }).scrollIntoView = scrollIntoView;
    renderComposer();
    const input = textarea();
    typeInto(input, "@");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());

    const first = screen.getByRole("option", { name: /README\.md/ });
    expect(first.id).toBe(`composer-menu-${THREAD}-option-0`);
    expect(input.getAttribute("aria-activedescendant")).toBe(`composer-menu-${THREAD}-option-0`);
    expect(within(screen.getByRole("listbox")).getAllByRole("option")[1]?.id).toBe(
      `composer-menu-${THREAD}-option-1`,
    );

    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.getAttribute("aria-activedescendant")).toBe(`composer-menu-${THREAD}-option-1`);
    expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
  });

  it("does not open or move during IME composition", async () => {
    renderComposer();
    const input = textarea();
    fireEvent.compositionStart(input);
    fireEvent.change(input, { target: { value: "@に" } });
    input.setSelectionRange(2, 2);
    fireEvent.select(input);

    expect(screen.queryByRole("listbox")).toBeNull();
    fireEvent.keyDown(input, { key: "ArrowDown", isComposing: true });
    expect(screen.queryByRole("listbox")).toBeNull();

    fireEvent.compositionEnd(input);
  });

  it("keeps Shift+Enter as a newline while the picker is open", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "@read");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull(), { timeout: 1_000 });

    fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
    runAnimationFrames();
    // The default newline is the textarea's; no row was accepted.
    expect(input.value).toBe("@read");
    expect(screen.queryByRole("listbox", { name: "Files" })).not.toBeNull();

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();
    expect(input.value).toBe("@README.md ");
  });

  it("points the popover at the caret's visible line when the textarea is scrolled", async () => {
    // jsdom has no layout: the mirror's caret sits 120 px down the content and
    // the wrapper is 200 px tall. With 40 px scrolled away the caret line is
    // 80 px down, so the popover sits 126 px above the wrapper's bottom.
    vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.tagName === "SPAN" ? 120 : 0;
    });
    vi.spyOn(Element.prototype, "clientHeight", "get").mockImplementation(function (this: Element) {
      return this.classList.contains("q-comp-in") ? 200 : 0;
    });

    renderComposer();
    const input = textarea();
    input.scrollTop = 40;
    typeInto(input, "@");

    const menu = (await screen.findByRole("listbox", { name: "Files" })).parentElement;
    expect(menu?.className).toContain("q-comp-menu");
    await waitFor(() => expect((menu as HTMLElement).style.bottom).toBe("126px"));
  });

  it("shows the read error instead of claiming an empty workspace", async () => {
    setWorkspaceFileListerForTests(async () => {
      throw new Error("The workspace could not be read.");
    });
    renderComposer();
    typeInto(textarea(), "@");

    expect(await screen.findByText("The workspace could not be read.")).toBeTruthy();
    expect(screen.queryByText("No files in code.")).toBeNull();
  });

  it("shows an empty workspace and an empty filter state", async () => {
    stubWorkspaceFiles([]);
    renderComposer();
    const input = textarea();
    typeInto(input, "@");
    expect(await screen.findByText("No files in code.")).toBeTruthy();

    typeInto(input, "@zzz");
    expect(await screen.findByText("No files match “zzz”.")).toBeTruthy();
  });
});

describe("Composer / command menu", () => {
  it("opens only when / is the first character", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "say /stop");
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByRole("listbox")).toBeNull();

    typeInto(input, "/");
    expect(await screen.findByRole("listbox", { name: "Commands" })).toBeTruthy();
    expect(option(/Stop the turn/)).not.toBeNull();
    expect(option(/Open the terminal/)).not.toBeNull();
    expect(option(/New thread/)).not.toBeNull();
  });

  it("runs the highlighted h4rn action and clears the slash text", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/term");
    await waitFor(() => expect(option(/Open the terminal/)).not.toBeNull());

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();

    expect(mock.toggleTerminal).toHaveBeenCalledTimes(1);
    expect(input.value).toBe("");
    expect(readThreadDraft(HOST, THREAD)).toBe("");
    expect(mock.sendMessage).not.toHaveBeenCalled();
  });

  it("offers Stop the turn only while the thread is working", async () => {
    const view = renderComposer();
    const input = textarea();
    typeInto(input, "/stop");
    expect(await screen.findByRole("option", { name: /Stop the turn/ })).toBeTruthy();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(mock.interruptThread).toHaveBeenCalledWith(THREAD);
    mock.interruptThread.mockReset();

    view.rerender(<Composer summary={{ ...summary, tab: "done" }} />);
    const second = textarea();
    typeInto(second, "/stop");
    expect(await screen.findByRole("listbox", { name: "Commands" })).toBeTruthy();
    expect(screen.queryByRole("option", { name: /Stop the turn/ })).toBeNull();
  });

  it("starts a new thread in this workspace", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/new");
    await waitFor(() => expect(option(/New thread/)).not.toBeNull());
    fireEvent.keyDown(input, { key: "Enter" });
    expect(mock.openNewThread).toHaveBeenCalledWith({ host: HOST, workspace: "~/code" });
    expect(input.value).toBe("");
  });

  it("shows an empty command state and keeps Enter from sending", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/zzz");
    expect(await screen.findByText("No commands match “zzz”.")).toBeTruthy();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(mock.sendMessage).not.toHaveBeenCalled();
    expect(input.value).toBe("/zzz");
  });

  it("opens the phone panel sheet for Changes and Subagents on a phone", async () => {
    mock.state.ui.phone = true;
    const seen: string[] = [];
    const listener = (event: Event) => {
      seen.push((event as CustomEvent<string>).detail);
    };
    window.addEventListener("h4rn:phone-panel", listener);
    try {
      renderComposer();
      const input = textarea();
      typeInto(input, "/chan");
      await waitFor(() => expect(option(/Open changes/)).not.toBeNull());
      fireEvent.keyDown(input, { key: "Enter" });

      typeInto(input, "/sub");
      await waitFor(() => expect(option(/Open subagents/)).not.toBeNull());
      fireEvent.keyDown(input, { key: "Enter" });

      expect(seen).toEqual(["changes", "subagents"]);
      expect(mock.setPanelTab).not.toHaveBeenCalled();
    } finally {
      window.removeEventListener("h4rn:phone-panel", listener);
    }
  });

  it("still opens the desktop panel tab from the / menu", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/chan");
    await waitFor(() => expect(option(/Open changes/)).not.toBeNull());
    fireEvent.keyDown(input, { key: "Enter" });
    expect(mock.setPanelTab).toHaveBeenCalledWith("changes");
  });

  it("does not offer the IDE from the phone / menu", async () => {
    mock.state.ui.phone = true;
    renderComposer();
    const input = textarea();
    typeInto(input, "/");

    await waitFor(() => expect(option(/Open changes/)).not.toBeNull());
    // The phone shell never renders the IDE view, so offering it would only
    // persist a layout the phone cannot show.
    expect(option(/Open the IDE/)).toBeNull();
    expect(option(/Back to the thread/)).toBeNull();
    expect(mock.toggleView).not.toHaveBeenCalled();
  });

  it("renders the same list as a bottom sheet on a phone", async () => {
    mock.state.ui.phone = true;
    renderComposer();
    const input = textarea();
    typeInto(input, "@read");

    const sheet = await screen.findByRole("dialog", { name: "Files" });
    // The shared bottom-sheet styles must not pull it over the composer: the
    // composer placement class puts it above the input it belongs to.
    expect(sheet.className).toContain("q-ph-sheet");
    expect(sheet.className).toContain("q-comp-sheet");
    expect(sheet.getAttribute("data-placement")).toBe("above");
    expect(within(sheet).getByText("Files in code")).toBeTruthy();
    fireEvent.click(await within(sheet).findByRole("option", { name: /README\.md/ }));
    runAnimationFrames();
    expect(input.value).toBe("@README.md ");
    expect(screen.queryByRole("dialog", { name: "Files" })).toBeNull();
  });

  it("keeps the phone sheet closed after Done, like Escape", async () => {
    mock.state.ui.phone = true;
    renderComposer();
    const input = textarea();
    typeInto(input, "@read");

    const sheet = await screen.findByRole("dialog", { name: "Files" });
    fireEvent.click(within(sheet).getByRole("button", { name: "Done" }));
    expect(screen.queryByRole("dialog", { name: "Files" })).toBeNull();

    // The same @token stays dismissed on the next keystroke, as with Esc.
    typeInto(input, "@rea");
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByRole("dialog", { name: "Files" })).toBeNull();
  });
});
