// @vitest-environment jsdom
// The whole pane, not just the hook: the diff pane's own effects are what this
// file exists to hold down. The app shell and the editor surface are replaced
// so the test exercises the wiring between the tab strip, the source picker,
// the diff and the session.

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  FsDiff,
  FsDiffFiles,
  FsFileContent,
  FsListing,
  FsTurnChanges,
  HarnessKind,
  HostId,
  ThreadId,
  TurnId,
} from "@/gen";

const mock = vi.hoisted(() => {
  const state = {
    threads: {} as Record<string, unknown>,
    hosts: {} as Record<string, unknown>,
    connection: { status: "online" as string },
    logs: {} as Record<string, unknown>,
    ui: {
      route: { kind: "ide" } as { kind: string },
      selectedThreadId: null as string | null,
      pendingIdePath: null as string | null,
      ideActivePath: null as string | null,
      layout: { terminalOpen: false, panelOpen: false, view: "ide", sidebarOpen: true },
    },
  };
  const listeners = new Set<() => void>();
  return {
    state,
    requests: [] as Array<{ type: string; path?: string; against?: unknown }>,
    phone: false,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    notify: () => {
      for (const listener of [...listeners]) listener();
    },
  };
});

vi.mock("@/lib/store/appStore", async () => {
  const { useSyncExternalStore } = await import("react");
  return {
    // The real store notifies subscribers; the Files panel's request to open a
    // file and its `pendingIdePath` only reach the pane that way.
    useApp: (selector: (state: unknown) => unknown) =>
      useSyncExternalStore(
        mock.subscribe,
        () => selector(mock.state),
        () => selector(mock.state),
      ),
    getClient: () => ({
      request: async (request: { type: string; path?: string; against?: unknown }) => {
        mock.requests.push(request);
        switch (request.type) {
          case "fs_list":
            return { type: "fs_listing", listing: listing(request.path ?? "") };
          case "fs_read":
            return { type: "fs_file", file: file(request.path ?? "") };
          case "fs_diff":
            return {
              type: "fs_diff",
              diff: diff(request.path ?? "", request.against as FsDiff["against"] | undefined),
            };
          case "fs_turn_changes":
            return { type: "fs_turn_changes", changes: changes() };
          case "fs_diff_files":
            return { type: "fs_diff_files", files: diffFiles() };
          case "fs_diff_commits":
            return {
              type: "fs_diff_commits",
              commits: { base: "main", merge_base: "abc", commits: [], truncated: false },
            };
          case "fs_branches":
            return { type: "fs_branches", branches: { branches: ["main"], default_base: "main" } };
          default:
            throw new Error(`unexpected request ${request.type}`);
        }
      },
      onEvent: () => () => undefined,
    }),
    ensureThread: () => undefined,
    navigate: () => undefined,
    openNewThread: () => undefined,
    pushToast: () => undefined,
    setIdeActivePath: (path: string | null) => {
      mock.state.ui.ideActivePath = path;
    },
    setPanelTab: () => undefined,
    setView: () => undefined,
    takePendingIdePath: () => {
      const path = mock.state.ui.pendingIdePath;
      mock.state.ui.pendingIdePath = null;
      return path;
    },
    toggleTerminal: () => undefined,
  };
});

vi.mock("@/lib/client/fsWatch", () => ({
  retainWorkspaceWatch: () => () => undefined,
}));

// The shell and the CodeMirror surface are other features' business.
vi.mock("@/components/shell/Sidebar", () => ({ SidebarReveal: () => null }));
vi.mock("@/components/thread/ThreadHeader", () => ({ ViewCluster: () => null }));
vi.mock("@/features/terminal/TerminalPanel", () => ({ TerminalPanel: () => null }));
vi.mock("./CodeEditor", () => ({ CodeEditor: () => null }));

import { IdePane } from "./IdePane";
import { listReviewComments, resetReviewComments } from "./reviewComments";

const thread = "th_1" as ThreadId;
const turn = "turn_1" as TurnId;
const host = "host_1" as HostId;
const harness = "claude_code" as HarnessKind;

let project = 0;
let workspacePath = "/work";

function listing(path: string): FsListing {
  const entries =
    path === ""
      ? ["a.ts", "b.ts"].map((name) => ({
          name,
          path: name,
          kind: "file" as const,
          size_bytes: 10,
          modified_at: null,
          git_status: null,
        }))
      : [];
  return { workspace: workspacePath, path, entries, truncated: false };
}

function file(path: string): FsFileContent {
  const content = after(path);
  return {
    path,
    content,
    etag: `etag-${path}`,
    encoding: "utf-8",
    size_bytes: content.length,
    binary: false,
    truncated: false,
    bytes_base64: null,
    bytes_omitted: null,
    content_omitted: null,
    modified_at: null,
    hint: null,
  };
}

/** One identifier per side, so a line's accessible name is exactly its text. */
function after(path: string): string {
  return path === "b.ts" ? "bnew\n" : "anew\n";
}

function before(path: string): string {
  return path === "b.ts" ? "bold\n" : "aold\n";
}

function diff(path: string, against?: FsDiff["against"]): FsDiff {
  const scope = against ?? { kind: "head" };
  return {
    path,
    against: scope,
    label:
      scope.kind === "head"
        ? "HEAD"
        : scope.kind === "agent_turn"
          ? "Turn 1"
          : scope.kind === "commit"
            ? "Commit c1"
            : "vs main",
    before: before(path),
    after: after(path),
    binary: false,
    too_large: false,
    note: null,
  };
}

function changes(): FsTurnChanges {
  return {
    thread_id: thread,
    turn_id: turn,
    files: [
      {
        path: "a.ts",
        added: 1,
        removed: 1,
        before_etag: "a0",
        after_etag: "a1",
      },
      {
        path: "b.ts",
        added: 1,
        removed: 1,
        before_etag: "b0",
        after_etag: "b1",
      },
    ],
    note: null,
  };
}

function diffFiles(): FsDiffFiles {
  return {
    against: { kind: "base", base: null },
    label: "vs main",
    base: "main",
    merge_base: "abc",
    files: [],
    truncated: false,
    note: null,
  };
}

/** A turn-scoped session: the thread reports one turn and its changes. */
function seed() {
  mock.state.threads = {
    [thread]: {
      thread_id: thread,
      parent_thread_id: null,
      host,
      harness,
      title: "Fix the auth test",
      workspace_path: workspacePath,
      model: "claude-sonnet-5-5",
      tab: "done",
      placed_by_user: false,
      pinned: false,
      snoozed_until: null,
      last_activity: "2026-03-04T12:00:00Z",
      agent_state: "done",
      usage: {},
      ttft_ms: null,
      context_window: null,
      context_fill_percent: null,
      needs_attention: false,
      unread: false,
      resume_at: null,
      limit: null,
    },
  };
  mock.state.hosts = {
    [host]: {
      host_id: host,
      name: "atlas",
      platform: "linux",
      online: true,
      last_seen: "2026-03-04T12:00:00Z",
      harnesses: [harness],
      latency_ms: null,
      path: null,
    },
  };
  mock.state.logs = { [thread]: { turns: [{ id: turn }] } };
  mock.state.ui.selectedThreadId = thread;
}

/** Opens a ChipSelect's menu; Radix listens for the pointer down, not the click. */
function openChip(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.click(trigger);
}

function diffRequests() {
  return mock.requests.filter((request) => request.type === "fs_diff");
}

function diffLines() {
  return screen.getByRole("region", { name: "Diff lines" });
}

/** The pane's path bar: the one place that names what the main area shows. */
function pathBar(): HTMLElement {
  const bar = document.querySelector<HTMLElement>(".q-ebar .q-mono");
  if (!bar) throw new Error("the path bar is not rendered");
  return bar;
}

beforeAll(() => {
  // Radix's dropdown and the media query hook expect APIs jsdom lacks.
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: mock.phone && query.includes("max-width: 639px"),
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
});

beforeEach(() => {
  project += 1;
  // A fresh path keeps the hook's per-project tab cache (module state) out of
  // the next test.
  workspacePath = `/work-${project}`;
  mock.requests = [];
  mock.phone = false;
  seed();
  resetReviewComments();
});

afterEach(() => {
  cleanup();
});

/** Mounts the pane and waits for the first file to open. */
async function mount() {
  render(<IdePane />);
  await waitFor(() => expect(screen.getByRole("tab", { name: "a.ts" })).toBeTruthy());
}

async function openDiff() {
  fireEvent.click(screen.getByRole("tab", { name: "Diff" }));
  await waitFor(() =>
    expect(within(diffLines()).getByRole("button", { name: /Line 1: anew/ })).toBeTruthy(),
  );
}

describe("the diff pane stays on the file the person picked", () => {
  it("keeps a file picked from the phone Turn picker instead of snapping back to the tab", async () => {
    mock.phone = true;
    await mount();
    await openDiff();

    // Turn mode on a phone offers the turn's files as a picker.
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    const picker = await screen.findByRole("button", { name: "File: a.ts" });
    const afterTurn = diffRequests().length;
    openChip(picker);
    fireEvent.click(await screen.findByRole("menuitem", { name: /b\.ts/ }));

    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew/ })).toBeTruthy(),
    );
    // The effect that follows the editor tab must not answer for the pick.
    expect(diffRequests()).toHaveLength(afterTurn + 1);
    expect(diffRequests().at(-1)?.path).toBe("b.ts");
    expect(within(diffLines()).queryByRole("button", { name: /Line 1: anew/ })).toBeNull();
  });

  it("follows a new editor tab while the pane is open", async () => {
    await mount();
    // The Files panel asks the IDE to open a file; the pane consumes it.
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() => expect(screen.getByRole("tab", { name: "b.ts" })).toBeTruthy());

    // Back to the first file, then read its diff.
    fireEvent.click(screen.getByRole("tab", { name: "a.ts" }));
    await openDiff();
    expect(diffRequests().at(-1)?.path).toBe("a.ts");

    fireEvent.click(screen.getByRole("tab", { name: "b.ts" }));

    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew/ })).toBeTruthy(),
    );
    expect(diffRequests().at(-1)?.path).toBe("b.ts");
  });

  it("names the diff's file after its tab is closed", async () => {
    await mount();
    await openDiff();
    expect(pathBar().textContent).toBe("a.ts");

    fireEvent.click(screen.getByRole("button", { name: "Close a.ts" }));
    await waitFor(() => expect(screen.queryByRole("tab", { name: "a.ts" })).toBeNull());

    // The diff outlives its tab, so the path bar names the file it shows
    // rather than the workspace.
    expect(pathBar().textContent).toBe("a.ts");
    expect(pathBar().title).toBe("a.ts");
    expect(within(diffLines()).getByRole("button", { name: /Line 1: anew/ })).toBeTruthy();

    fireEvent.click(screen.getByRole("tab", { name: "Diff" }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew/ })).toBeTruthy(),
    );
    expect(pathBar().textContent).toBe("a.ts");
  });

  it("labels a review comment with the turn it was written in", async () => {
    await mount();
    await openDiff();

    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    const added = await waitFor(() =>
      within(diffLines()).getByRole("button", { name: /Line 1: anew/ }),
    );

    fireEvent.keyDown(added, { key: "Enter" });
    fireEvent.change(screen.getByLabelText("Review comment"), {
      target: { value: "Rename this." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    await waitFor(() => expect(listReviewComments(thread)).toHaveLength(1));
    expect(listReviewComments(thread)[0]?.source).toBe("turn 1");
    expect(screen.getByText(/line 1 · turn 1/)).toBeTruthy();
  });
});
