// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MessageRole, ThreadSummary } from "@/gen";
import type { StreamTurn, ThreadLog, TurnItem } from "@/lib/store/projection";

const mock = vi.hoisted(() => {
  const state: {
    connection: { status: string };
    hosts: Record<string, { online: boolean; name: string }>;
    logs: Record<string, unknown>;
    ui: { focusComposerNonce: number; newThreadOpen: boolean };
  } = {
    connection: { status: "online" },
    hosts: { host_ui: { online: true, name: "atlas" } },
    logs: {},
    ui: { focusComposerNonce: 0, newThreadOpen: false },
  };
  return {
    state,
    sendMessage:
      vi.fn<(threadId: string, text: string, attachments: unknown[]) => Promise<boolean>>(),
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
  useIsPhone: () => false,
}));

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

describe("Composer drafts and history", () => {
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
    mock.sendMessage.mockReset();
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

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
