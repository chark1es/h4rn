// @vitest-environment jsdom

import {
  act,
  cleanup,
  createEvent,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
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
  type WorkspaceFileLister,
  WorkspaceFilesContext,
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
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  fast: false,
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
  return render(
    <WorkspaceFilesContext.Provider value={fileLister}>
      <Composer summary={summary} />
    </WorkspaceFilesContext.Provider>,
  );
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

/** jsdom has no layout: build the box a browser would report. */
function domRect(left: number, top: number, width: number, height: number): DOMRect {
  return {
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    x: left,
    y: top,
    toJSON: () => ({}),
  } as DOMRect;
}

/** The lister every test composer reads its workspace through. */
let fileLister: WorkspaceFileLister = async () => {
  throw new Error("No workspace file lister was stubbed.");
};

function stubWorkspaceFiles(files: string[] = WORKSPACE_FILES, truncated = false): void {
  fileLister = async ({ path }) => workspaceListing(path, files, truncated);
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

type TextareaHandlers = {
  onChange: (event: { target: HTMLTextAreaElement }) => void;
  onKeyDown: (event: {
    key: string;
    shiftKey: boolean;
    nativeEvent: { isComposing: boolean };
    currentTarget: HTMLTextAreaElement;
    preventDefault: () => void;
  }) => void;
};

/** The handlers React last rendered the textarea with. */
function textareaHandlers(element: HTMLTextAreaElement): TextareaHandlers {
  const key = Object.keys(element).find((name) => name.startsWith("__reactProps$"));
  if (!key) throw new Error("The textarea has no React props");
  return (element as unknown as Record<string, TextareaHandlers>)[key] as TextareaHandlers;
}

/**
 * Type and press a key in one batch: the key is handled before React
 * re-renders the rows for the new text. RTL's per-event `act` flush would
 * otherwise hide the stale-row race the composer has to survive in a browser.
 */
function typeAndPressInOneBatch(input: HTMLTextAreaElement, value: string, key: string): void {
  const handlers = textareaHandlers(input);
  input.value = value;
  input.setSelectionRange(value.length, value.length);
  handlers.onChange({ target: input });
  handlers.onKeyDown({
    key,
    shiftKey: false,
    nativeEvent: { isComposing: false },
    currentTarget: input,
    preventDefault: () => undefined,
  });
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

  it("restores the preserved draft after sending a recalled prompt", async () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    mock.sendMessage.mockResolvedValue(true);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "half typed" } });
    input.setSelectionRange(0, 0);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("earlier prompt");

    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));

    await waitFor(() =>
      expect(mock.sendMessage).toHaveBeenCalledWith(THREAD, "earlier prompt", []),
    );
    await waitFor(() => expect(input.value).toBe("half typed"));
    // The recalled prompt was sent; the unsent draft it replaced was not.
    expect(readThreadDraft(HOST, THREAD)).toBe("half typed");
  });

  it("keeps a newer recall on screen when a recalled send finishes", async () => {
    mock.state.logs[THREAD] = log(["older prompt", "newer prompt"]);
    let finish: (ok: boolean) => void = () => undefined;
    mock.sendMessage.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "half typed" } });
    input.setSelectionRange(0, 0);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("newer prompt");

    fireEvent.click(screen.getByRole("button", { name: "Send reply" }));
    await waitFor(() => expect(mock.sendMessage).toHaveBeenCalledWith(THREAD, "newer prompt", []));

    // The person recalls farther back while the send waits. That text is their
    // newer intent, so the send must not put the old preserved draft back.
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();
    expect(input.value).toBe("older prompt");

    await act(async () => {
      finish(true);
    });

    expect(input.value).toBe("older prompt");
    expect(readThreadDraft(HOST, THREAD)).toBe("half typed");
  });

  it("does not recall history during IME composition", () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "composition text" } });
    input.setSelectionRange(0, 0);
    fireEvent.compositionStart(input);
    fireEvent.keyDown(input, { key: "ArrowUp", isComposing: true });
    runAnimationFrames();

    expect(input.value).toBe("composition text");
    expect(readThreadDraft(HOST, THREAD)).toBe("composition text");
    fireEvent.compositionEnd(input);
  });

  it("does not recall history while the file menu is open", async () => {
    mock.state.logs[THREAD] = log(["earlier prompt"]);
    renderComposer();
    const input = textarea();

    typeInto(input, "@");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());
    input.setSelectionRange(1, 1);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    runAnimationFrames();

    // The arrow moved the menu highlight; history stayed put.
    expect(input.value).toBe("@");
    expect(readThreadDraft(HOST, THREAD)).toBe("@");
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
    view.rerender(
      <WorkspaceFilesContext.Provider value={fileLister}>
        <Composer summary={{ ...summary, thread_id: secondThread }} />
      </WorkspaceFilesContext.Provider>,
    );
    await waitFor(() => expect(textarea().value).toBe("second thread draft"));

    await act(async () => {
      finish(true);
    });

    expect(textarea().value).toBe("second thread draft");
    expect(readThreadDraft(HOST, secondThread)).toBe("second thread draft");
    expect(readThreadDraft(HOST, THREAD)).toBe("");
  });

  it("re-renders when a flush gives up its queued edit to newer text", () => {
    renderComposer();
    const input = textarea();

    fireEvent.change(input, { target: { value: "my queued edit" } });
    // The other tab's newer text is already in storage, but this tab has not
    // heard the storage event yet (it ignored one while its write was queued).
    const key = draftStorageKey("thread", HOST, THREAD);
    window.localStorage.setItem(
      key,
      JSON.stringify({
        text: "newer from elsewhere",
        updatedAt: Date.now() + 1000,
        nonce: "other",
      }),
    );

    act(() => {
      flushDrafts();
    });

    expect(input.value).toBe("newer from elsewhere");
    expect(readThreadDraft(HOST, THREAD)).toBe("newer from elsewhere");
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

  it("does not open after a Unicode letter", async () => {
    renderComposer();
    typeInto(textarea(), "café@src");
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

  it("filters with path-aware matching as the query changes", async () => {
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

  it("keeps the word after the caret out of the mention", async () => {
    renderComposer();
    const input = textarea();
    // The caret sits inside "Review this part", just before "this".
    typeInto(input, "Review this part", 7);

    fireEvent.click(screen.getByRole("button", { name: "Mention a file" }));
    runAnimationFrames();
    expect(input.value).toBe("Review @ this part");
    expect(input.selectionStart).toBe(8);
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();
    expect(input.value).toBe("Review @README.md this part");
  });

  it("accepts only rows computed for the query on screen", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "@");
    // Under an empty query README.md is the highlighted row.
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());
    expect(within(screen.getByRole("listbox")).getAllByRole("option")[0]?.textContent).toContain(
      "README.md",
    );

    // Change the query and press Enter in one batch, so the component has not
    // re-rendered between the two events. The rank for the new query must be
    // used - a different file from the one highlighted under the old query.
    act(() => {
      typeAndPressInOneBatch(input, "@doc", "Enter");
    });
    runAnimationFrames();

    expect(input.value).toBe("@docs/ux.md ");
    expect(option(/README\.md/)).toBeNull();
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

    const shiftEnter = createEvent.keyDown(input, { key: "Enter", shiftKey: true });
    fireEvent(input, shiftEnter);
    runAnimationFrames();
    // The textarea's own default newline is untouched: no row was accepted and
    // the key was not swallowed on the way.
    expect(shiftEnter.defaultPrevented).toBe(false);
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

  it("anchors to the real caret inside a scrolled, wrapped token", async () => {
    // The marker's distance from the mirror's top follows the text after it.
    // Looking at the token end instead of the selection would measure less
    // text and put the popover on the wrong line.
    vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.tagName === "SPAN" ? (this.textContent?.length ?? 0) * 10 : 0;
    });
    vi.spyOn(Element.prototype, "clientHeight", "get").mockImplementation(function (this: Element) {
      return this.classList.contains("q-comp-in") ? 400 : 0;
    });

    renderComposer();
    const input = textarea();
    input.scrollTop = 30;
    // The caret is after "do" in "@docs"; the token runs to "next".
    typeInto(input, "a @docs next", 4);

    const menu = (await screen.findByRole("listbox", { name: "Files" })).parentElement;
    // "ocs next" is 80 px down; 30 px is scrolled away; 400 - 50 + 6 = 356.
    await waitFor(() => expect((menu as HTMLElement).style.bottom).toBe("356px"));
  });

  it("copies computed styles to the mirror under CSS property names", async () => {
    renderComposer();
    const getPropertyValue = vi.fn((property: string) =>
      property === "line-height" ? "21px" : property === "font-size" ? "13px" : "",
    );
    vi.spyOn(window, "getComputedStyle").mockReturnValue({
      getPropertyValue,
      lineHeight: "21px",
    } as unknown as CSSStyleDeclaration);

    typeInto(textarea(), "@read");
    await waitFor(() => expect(option(/README\.md/)).not.toBeNull());

    // Chromium's getPropertyValue ignores camelCase names, so the mirror must
    // ask for the CSS spelling of every property it copies.
    expect(getPropertyValue).toHaveBeenCalledWith("font-family");
    expect(getPropertyValue).toHaveBeenCalledWith("font-size");
    expect(getPropertyValue).toHaveBeenCalledWith("line-height");
    expect(getPropertyValue).toHaveBeenCalledWith("padding-left");
    expect(getPropertyValue).not.toHaveBeenCalledWith("fontFamily");
  });

  it("opens below the caret by the measured line height", async () => {
    // The composer's line height is not the 21 px fallback.
    vi.spyOn(window, "getComputedStyle").mockReturnValue({
      getPropertyValue: (property: string) => (property === "line-height" ? "34px" : ""),
      lineHeight: "34px",
    } as unknown as CSSStyleDeclaration);
    // The caret sits 120 px down a 200 px composer; a 200 px menu above it
    // does not fit, so it opens below the caret line.
    vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.tagName === "SPAN" ? 120 : 0;
    });
    vi.spyOn(Element.prototype, "clientHeight", "get").mockImplementation(function (this: Element) {
      return this.classList.contains("q-comp-in") ? 200 : 0;
    });
    vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.classList.contains("q-comp-menu") ? 200 : 0;
    });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.classList.contains("q-comp-menu") ? 200 : 0;
    });
    vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.classList.contains("q-comp-menu") ? document.body : null;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (
      this: Element,
    ) {
      return this === document.body ? domRect(0, 0, 400, 200) : domRect(0, 0, 0, 0);
    });
    vi.spyOn(window, "innerHeight", "get").mockReturnValue(600);

    renderComposer();
    typeInto(textarea(), "@read");

    const menu = (await screen.findByRole("listbox", { name: "Files" })).parentElement;
    await waitFor(() => expect((menu as HTMLElement).style.top).toBe("158px"));
    // 120 (caret) + 34 (line) + 4, not 120 + 21 + 4.
    expect((menu as HTMLElement).style.top).not.toBe("145px");
  });

  it("shows the read error instead of claiming an empty workspace", async () => {
    fileLister = async () => {
      throw new Error("The workspace could not be read.");
    };
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

  it("runs the command ranked for the query on screen, not the old row", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/");
    // Under the empty query the highlighted row is Stop the turn.
    await waitFor(() => expect(option(/Stop the turn/)).not.toBeNull());

    // Type the rest of the query and press Enter before React re-renders.
    act(() => {
      typeAndPressInOneBatch(input, "/term", "Enter");
    });
    runAnimationFrames();

    expect(mock.toggleTerminal).toHaveBeenCalledTimes(1);
    expect(mock.interruptThread).not.toHaveBeenCalled();
    expect(input.value).toBe("");
    expect(readThreadDraft(HOST, THREAD)).toBe("");
  });

  it("keeps the slash text when nothing matches the query on screen", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/");
    await waitFor(() => expect(option(/Stop the turn/)).not.toBeNull());

    act(() => {
      typeAndPressInOneBatch(input, "/zzz", "Enter");
    });
    runAnimationFrames();

    expect(mock.interruptThread).not.toHaveBeenCalled();
    expect(mock.toggleTerminal).not.toHaveBeenCalled();
    expect(input.value).toBe("/zzz");
    expect(readThreadDraft(HOST, THREAD)).toBe("/zzz");
  });

  it("does not run a command once the live text is no longer one", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/");
    await waitFor(() => expect(option(/Stop the turn/)).not.toBeNull());

    // `/` becomes `@` and Enter lands before React re-renders the rows. The
    // command menu's row must not accept text that is now a file query.
    act(() => {
      typeAndPressInOneBatch(input, "@", "Enter");
    });
    runAnimationFrames();

    expect(mock.interruptThread).not.toHaveBeenCalled();
    expect(mock.toggleTerminal).not.toHaveBeenCalled();
    expect(mock.openNewThread).not.toHaveBeenCalled();
    expect(mock.setPanelTab).not.toHaveBeenCalled();
    expect(mock.toggleView).not.toHaveBeenCalled();
    expect(mock.sendMessage).not.toHaveBeenCalled();
    expect(input.value).toBe("@");
    expect(readThreadDraft(HOST, THREAD)).toBe("@");
  });

  it("offers Stop the turn only while the thread is working", async () => {
    const view = renderComposer();
    const input = textarea();
    typeInto(input, "/stop");
    expect(await screen.findByRole("option", { name: /Stop the turn/ })).toBeTruthy();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(mock.interruptThread).toHaveBeenCalledWith(THREAD);
    mock.interruptThread.mockReset();

    view.rerender(
      <WorkspaceFilesContext.Provider value={fileLister}>
        <Composer summary={{ ...summary, tab: "done" }} />
      </WorkspaceFilesContext.Provider>,
    );
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

  it("does not pull focus back after a command that opens a dialog", async () => {
    renderComposer();
    const input = textarea();
    typeInto(input, "/new");
    await waitFor(() => expect(option(/New thread/)).not.toBeNull());
    const focus = vi.spyOn(input, "focus");

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();

    expect(mock.openNewThread).toHaveBeenCalled();
    expect(focus).not.toHaveBeenCalled();
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

  it("does not pull focus back after a phone command opens the sheet", async () => {
    mock.state.ui.phone = true;
    renderComposer();
    const input = textarea();
    typeInto(input, "/chan");
    await waitFor(() => expect(option(/Open changes/)).not.toBeNull());
    const focus = vi.spyOn(input, "focus");

    fireEvent.keyDown(input, { key: "Enter" });
    runAnimationFrames();

    expect(focus).not.toHaveBeenCalled();
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

  it("places the phone sheet inside the visible viewport and above the composer", async () => {
    mock.state.ui.phone = true;
    // The keyboard leaves the composer at 700 px, below a visible viewport
    // that ends at 600 px. A sheet pinned to the composer's top would run
    // under the keyboard.
    vi.stubGlobal("visualViewport", {
      height: 600,
      offsetTop: 0,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.classList.contains("q-comp-sheet") ? document.body : null;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (
      this: Element,
    ) {
      // The composer fieldset: top 700, bottom 796.
      if (this === document.body) return domRect(0, 700, 390, 96);
      return domRect(0, 0, 0, 0);
    });

    renderComposer();
    const input = textarea();
    typeInto(input, "@read");

    const sheet = await screen.findByRole("dialog", { name: "Files" });
    await waitFor(() => expect(sheet.style.bottom).toBe("204px"));
    // Bottom edge 796 - 204 = 592, 8 px above the visible viewport's bottom
    // (600), not at the composer's top (700). The sheet keeps 584 px on screen.
    expect(sheet.style.maxHeight).toBe("584px");
    expect(sheet.className).toContain("q-ph-sheet");
    expect(sheet.className).toContain("q-comp-sheet");
    // Geometry: the container's bottom is 796, so the sheet's top edge in
    // viewport coordinates is 796 - 204 - 584 = 8 and its bottom edge is 592,
    // which is above the composer (700) and inside the visible viewport (600).
    const containerBottom = 796;
    const sheetTop = containerBottom - Number.parseFloat(sheet.style.bottom) - 584;
    const sheetBottom = containerBottom - Number.parseFloat(sheet.style.bottom);
    expect(sheetTop).toBeGreaterThanOrEqual(0);
    expect(sheetBottom).toBeLessThanOrEqual(600);
    expect(sheetBottom).toBeLessThan(700);
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
