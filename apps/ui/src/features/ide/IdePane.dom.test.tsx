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
import { readThreadDraft, writeThreadDraft } from "@/lib/drafts";

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
    requests: [] as Array<{
      type: string;
      path?: string;
      workspace?: string;
      against?: unknown;
      base?: unknown;
      turn_id?: TurnId;
    }>,
    phone: false,
    /** Makes the commit list empty, as a branch with nothing beyond its base. */
    emptyCommits: false,
    /** Adds a file the reopened Branch list should notice. */
    extraFile: false,
    /** The next request of this type waits for `promise` before it answers. */
    hold: null as { type: string; promise: Promise<void> } | null,
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
      request: async (request: {
        type: string;
        path?: string;
        workspace?: string;
        against?: unknown;
        base?: unknown;
        turn_id?: TurnId;
      }) => {
        mock.requests.push(request);
        if (mock.hold && mock.hold.type === request.type) {
          const hold = mock.hold;
          mock.hold = null;
          await hold.promise;
        }
        switch (request.type) {
          case "fs_list":
            return {
              type: "fs_listing",
              listing: listing(request.path ?? "", request.workspace),
            };
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
            return {
              type: "fs_diff_files",
              files: diffFiles(request.against as FsDiff["against"] | undefined),
            };
          case "fs_diff_commits":
            return {
              type: "fs_diff_commits",
              commits: {
                base: (request.base as string | null) ?? "main",
                merge_base: "abc",
                commits: mock.emptyCommits
                  ? []
                  : [
                      {
                        sha: "commit1",
                        short_sha: "c1",
                        subject: "first",
                        author: "you",
                        authored_at: "2026-03-04T12:00:00Z",
                      },
                    ],
                truncated: false,
              },
            };
          case "fs_branches":
            return {
              type: "fs_branches",
              branches: { branches: ["main", "release"], default_base: "main" },
            };
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

/**
 * The label this mock prints above a diff.
 *
 * It deliberately reads the turn id's *first* characters and so is the same
 * for two turns a moment apart. The daemon now prints the trailing characters,
 * but a label is display text either way: the test keeps the colliding text to
 * prove a comment follows the turn id and never the label.
 */
function turnLabel(id: TurnId): string {
  return `Turn ${id.replace(/^turn_/, "").slice(0, 8)}`;
}

let project = 0;
let workspacePath = "/work";
/** The turn the thread reports as its latest; a test can move it forward. */
let activeTurn: TurnId = turn;
/**
 * The thread's stream rows, when a test needs more than one.
 *
 * A harness switch adds a divider row after the turns, and the IDE must read
 * the newest turn an agent really ran from it, not the divider.
 */
let streamTurns: Array<{ id: TurnId; divider?: boolean }> | null = null;

function listing(path: string, workspace?: string): FsListing {
  const empty = workspace?.endsWith("-empty") ?? false;
  const entries =
    path === "" && !empty
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
  // The scope tags each side, so a stale reply is visible in the body itself.
  const tag =
    scope.kind === "agent_turn"
      ? "turn"
      : scope.kind === "base"
        ? "branch"
        : scope.kind === "commit"
          ? "commit"
          : "head";
  return {
    path,
    against: scope,
    label:
      scope.kind === "head"
        ? "HEAD"
        : scope.kind === "agent_turn"
          ? turnLabel(scope.turn_id)
          : scope.kind === "commit"
            ? "Commit c1"
            : "vs main",
    before: `${before(path).trimEnd()} ${tag}\n`,
    after: `${after(path).trimEnd()} ${tag}\n`,
    binary: false,
    too_large: false,
    note: null,
  };
}

function changes(): FsTurnChanges {
  return {
    thread_id: thread,
    turn_id: activeTurn,
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

function diffFiles(against?: FsDiff["against"]): FsDiffFiles {
  const scope = against ?? { kind: "base", base: null };
  return {
    against: scope,
    label: "vs main",
    base: scope.kind === "base" ? (scope.base ?? "main") : null,
    merge_base: "abc",
    files: [
      {
        path: "a.ts",
        old_path: null,
        status: "modified",
        added: 1,
        removed: 1,
        binary: false,
        counts_omitted: false,
      },
      {
        path: "b.ts",
        old_path: null,
        status: "modified",
        added: 1,
        removed: 1,
        binary: false,
        counts_omitted: false,
      },
      ...(mock.extraFile
        ? [
            {
              path: "c.ts",
              old_path: null,
              status: "modified" as const,
              added: 1,
              removed: 1,
              binary: false,
              counts_omitted: false,
            },
          ]
        : []),
    ],
    truncated: false,
    note: null,
  };
}

/** One thread row, as the app store would hold it. */
function threadRow(id: ThreadId, path: string, title: string) {
  return {
    thread_id: id,
    parent_thread_id: null,
    host,
    harness,
    title,
    workspace_path: path,
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
  };
}

/** A turn-scoped session: the thread reports one turn and its changes. */
function seed() {
  mock.state.threads = {
    [thread]: threadRow(thread, workspacePath, "Fix the auth test"),
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
  mock.state.logs = {
    [thread]: { turns: streamTurns ?? [{ id: activeTurn }] },
  };
  mock.state.ui.selectedThreadId = thread;
}

/** Moves the thread on to another turn; callers notify when the pane is mounted. */
function setTurn(id: TurnId) {
  activeTurn = id;
  mock.state.logs = { [thread]: { turns: [{ id }] } };
}

/** Opens a ChipSelect's menu; Radix listens for the pointer down, not the click. */
function openChip(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.click(trigger);
}

function diffRequests() {
  return mock.requests.filter((request) => request.type === "fs_diff");
}

/** Makes the next request of `type` wait; call the returned function to answer. */
function holdNext(type: string): () => void {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  mock.hold = { type, promise };
  return release;
}

/** Releases a held reply and lets its callbacks run. */
async function releaseHold(release: () => void): Promise<void> {
  await act(async () => {
    release();
    await Promise.resolve();
    await Promise.resolve();
  });
}

/**
 * Waits for the render a diff reply caused to finish committing its effects.
 * The pane clears a stale line selection when the diff changes; typing before
 * that effect runs would open an editor it then throws away.
 */
async function settle(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
  });
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
  activeTurn = turn;
  streamTurns = null;
  mock.requests = [];
  mock.phone = false;
  mock.emptyCommits = false;
  mock.extraFile = false;
  mock.hold = null;
  seed();
  resetReviewComments();
  // The draft store is device-wide; a sent comment must not reach the next test.
  writeThreadDraft(host, thread, "");
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
    // Nor may the pick fall back to another source while it loads.
    expect(diffRequests().at(-1)?.against).toMatchObject({
      kind: "agent_turn",
      thread_id: thread,
      turn_id: turn,
    });
    expect(within(diffLines()).queryByRole("button", { name: /Line 1: anew/ })).toBeNull();
  });

  it("follows the editor tab in a Branch review", async () => {
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );

    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew branch/ })).toBeTruthy(),
    );

    // Every mode follows the tab with the source it is showing.
    expect(diffRequests().at(-1)?.path).toBe("b.ts");
    expect(diffRequests().at(-1)?.against).toEqual({ kind: "base", base: "main" });
    expect(pathBar().textContent).toBe("b.ts");
    // One read for entering the mode, one for the tab that replaced its file.
    expect(diffRequests()).toHaveLength(3);
  });

  it("follows the editor tab in a Commits review", async () => {
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Commits/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew commit/ })).toBeTruthy(),
    );

    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew commit/ })).toBeTruthy(),
    );

    expect(diffRequests().at(-1)?.path).toBe("b.ts");
    expect(diffRequests().at(-1)?.against).toEqual({ kind: "commit", sha: "commit1" });
    expect(pathBar().textContent).toBe("b.ts");
    // One read for entering the mode, one for the tab that replaced its file.
    expect(diffRequests()).toHaveLength(3);
  });

  it("keeps a Commits review when the tab switch lands while its list is out", async () => {
    await mount();
    await openDiff();

    // The commit list is still out when the person moves to another tab.
    const releaseCommits = holdNext("fs_diff_commits");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Commits/ }));
    await waitFor(() =>
      expect(mock.requests.some((request) => request.type === "fs_diff_commits")).toBe(true),
    );

    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    // Following re-asks the scope for the tab instead of opening a commit
    // scope that has no sha yet.
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew commit/ })).toBeTruthy(),
    );
    expect(screen.queryByText(/The diff did not load/)).toBeNull();
    expect(screen.queryByText(/Choose a commit to compare/)).toBeNull();
    expect(diffRequests().at(-1)?.against).toEqual({ kind: "commit", sha: "commit1" });

    // The abandoned list never paints or opens anything after the follow.
    await releaseHold(releaseCommits);
    expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew commit/ })).toBeTruthy();
    expect(diffRequests()).toHaveLength(2);
  });

  it("keeps an empty commit range empty across a tab switch", async () => {
    mock.emptyCommits = true;
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Commits/ }));

    await waitFor(() => expect(screen.getByText("No commits beyond main.")).toBeTruthy());
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    // The follow re-reads the same empty range; it must not become an error
    // about a missing sha.
    await waitFor(() => expect(screen.getByText("No commits beyond main.")).toBeTruthy());
    expect(screen.queryByText(/Choose a commit to compare/)).toBeNull();
    expect(screen.queryByText(/The diff did not load/)).toBeNull();
  });

  it("keeps a Branch review's file list when the tab switch lands while it is out", async () => {
    await mount();
    await openDiff();

    const releaseFiles = holdNext("fs_diff_files");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(mock.requests.some((request) => request.type === "fs_diff_files")).toBe(true),
    );

    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    // The follow re-reads the scope's file list rather than superseding it.
    await waitFor(() => expect(screen.getByText("2 files")).toBeTruthy());
    const files = screen.getByRole("complementary", { name: "Changed files" });
    expect(within(files).getByText("b.ts")).toBeTruthy();
    expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew branch/ })).toBeTruthy();

    // The held first list belongs to the request the follow replaced.
    await releaseHold(releaseFiles);
    expect(screen.getByText("2 files")).toBeTruthy();
    expect(diffRequests()).toHaveLength(2);
  });

  it("follows a switch back to the first tab while the list is still out", async () => {
    await mount();
    await openDiff();

    // Entering Branch and the follow to b.ts both wait on their file lists.
    // The mock holds one request at a time: arm the second hold once the
    // entry's list read has taken the first.
    const releaseEntry = holdNext("fs_diff_files");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(mock.requests.some((request) => request.type === "fs_diff_files")).toBe(true),
    );
    expect(mock.hold).toBeNull();
    const releaseFollow = holdNext("fs_diff_files");
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(mock.requests.filter((request) => request.type === "fs_diff_files")).toHaveLength(2),
    );
    expect(mock.hold).toBeNull();

    // Back to a.ts before either list answers: the pane still names a.ts from
    // before the Branch load, but the pending load would open b.ts.
    act(() => {
      mock.state.ui.pendingIdePath = "a.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "a.ts" }).getAttribute("aria-selected")).toBe("true"),
    );
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );

    await releaseHold(releaseFollow);
    await releaseHold(releaseEntry);
    expect(pathBar().textContent).toBe("a.ts");
    expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy();
    // The follows keep the base picker's list: one branch read, from entry.
    expect(mock.requests.filter((request) => request.type === "fs_branches")).toHaveLength(1);
  });

  it("reopening the pane in Branch mode asks for the diff once", async () => {
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );

    // Close the pane, move to another tab, then reopen it in Branch mode.
    fireEvent.click(screen.getByRole("tab", { name: "Edit" }));
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );
    // The repository gained a file while the pane was closed. Reopening in
    // Branch mode re-reads the list; the follow effect must not supersede that
    // read with its own diff.
    mock.extraFile = true;
    const before = diffRequests().length;
    const filesBefore = mock.requests.filter((request) => request.type === "fs_diff_files").length;
    fireEvent.click(screen.getByRole("tab", { name: "Diff" }));
    await waitFor(() => expect(diffRequests().length).toBeGreaterThan(before));
    await settle();

    // Entering the pane already asks for the tab's file and the list; the
    // follow effect must not add a second read of either.
    expect(diffRequests()).toHaveLength(before + 1);
    expect(mock.requests.filter((request) => request.type === "fs_diff_files")).toHaveLength(
      filesBefore + 1,
    );
    expect(diffRequests().at(-1)?.against).toEqual({ kind: "base", base: "main" });
    expect(screen.getByText("3 files")).toBeTruthy();
  });

  it("asks once when the source picker starts a review on a moved tab", async () => {
    await mount();
    await openDiff();

    // Close the pane, then move the editor to another tab.
    fireEvent.click(screen.getByRole("tab", { name: "Edit" }));
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    // Picking Branch from the path bar starts the review for the tab in front
    // of the editor; the pane's follow effect must not read it a second time.
    const before = diffRequests().length;
    const filesBefore = mock.requests.filter((request) => request.type === "fs_diff_files").length;
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew branch/ })).toBeTruthy(),
    );
    await settle();

    expect(diffRequests()).toHaveLength(before + 1);
    expect(mock.requests.filter((request) => request.type === "fs_diff_files")).toHaveLength(
      filesBefore + 1,
    );
  });

  it("names the base Commit mode reuses", async () => {
    await mount();
    await openDiff();

    // Pick a non-default base in Branch mode.
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );
    openChip(await screen.findByRole("button", { name: "Base branch: main" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "release" }));
    await waitFor(() =>
      expect(diffRequests().at(-1)?.against).toEqual({ kind: "base", base: "release" }),
    );

    // Commit mode reuses it; the header must name it rather than leave the
    // range implicit.
    openChip(screen.getByRole("button", { name: "Diff source: Branch" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Commits/ }));

    await waitFor(() => expect(screen.getByText(/since release/)).toBeTruthy());
    expect(mock.requests.filter((request) => request.type === "fs_diff_commits").at(-1)?.base).toBe(
      "release",
    );
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

  it("asks for the newest real turn's diff when a switch divider is the newest row", async () => {
    // A harness switch appends a divider row after the turns. It has no work of
    // its own, so Turn must name the last turn an agent really ran.
    streamTurns = [{ id: turn }, { id: "switch:claude_code:codex" as TurnId, divider: true }];
    seed();
    await mount();
    await openDiff();

    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));

    await waitFor(() =>
      expect(diffRequests().at(-1)?.against).toEqual({
        kind: "agent_turn",
        thread_id: thread,
        turn_id: turn,
      }),
    );
    const changes = mock.requests.filter((request) => request.type === "fs_turn_changes");
    expect(changes.at(-1)?.turn_id).toBe(turn);
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew turn/ })).toBeTruthy(),
    );
  });

  it("labels a review comment with the turn it was written in", async () => {
    await mount();
    await openDiff();

    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    const added = await waitFor(() =>
      within(diffLines()).getByRole("button", { name: /Line 1: anew/ }),
    );

    await settle();
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

describe("a slower reply never overrides a later choice", () => {
  it("stays on the source picked after a slow file list", async () => {
    await mount();
    await openDiff();

    const releaseList = holdNext("fs_diff_files");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));

    // The picker shows the new source while its list is still loading.
    const branchChip = await screen.findByRole("button", { name: "Diff source: Branch" });
    await waitFor(() =>
      expect(mock.requests.some((request) => request.type === "fs_diff_files")).toBe(true),
    );

    // Meanwhile the person picks Turn; that choice came after the Branch list.
    openChip(branchChip);
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew turn/ })).toBeTruthy(),
    );

    await releaseHold(releaseList);

    // The late Branch list must not open a branch diff over the Turn choice.
    await waitFor(() =>
      expect(
        diffRequests().filter(
          (request) => (request.against as { kind?: string } | undefined)?.kind === "base",
        ),
      ).toHaveLength(0),
    );
    expect(within(diffLines()).getByRole("button", { name: /Line 1: anew turn/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Diff source: Turn" })).toBeTruthy();
  });

  it("names the newest choice in the body, the path bar and the comment", async () => {
    await mount();
    await openDiff();

    // Opening b.ts asks for its uncommitted diff; the reply is held back.
    const releaseHead = holdNext("fs_diff");
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(diffRequests().some((request) => request.path === "b.ts")).toBe(true),
    );
    expect(diffRequests().at(-1)?.against).toEqual({ kind: "head" });

    // Picking Turn asks for the same file's turn diff, and that reply lands
    // first: the two answers arrive in reverse order.
    const releaseTurn = holdNext("fs_diff");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    await waitFor(() =>
      expect(diffRequests().at(-1)?.against).toMatchObject({ kind: "agent_turn" }),
    );

    await releaseHold(releaseTurn);
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew turn/ })).toBeTruthy(),
    );
    await releaseHold(releaseHead);

    expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew turn/ })).toBeTruthy();
    expect(within(diffLines()).queryByRole("button", { name: /Line 1: bnew head/ })).toBeNull();
    expect(pathBar().textContent).toBe("b.ts");
    expect(screen.getByText("Turn 1")).toBeTruthy();

    // A comment written now belongs to the Turn diff, not the late head one.
    await settle();
    fireEvent.keyDown(within(diffLines()).getByRole("button", { name: /Line 1: bnew turn/ }), {
      key: "Enter",
    });
    fireEvent.change(screen.getByLabelText("Review comment"), {
      target: { value: "Keep the injected clock." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    await waitFor(() => expect(listReviewComments(thread)).toHaveLength(1));
    expect(listReviewComments(thread)[0]?.path).toBe("b.ts");
    expect(listReviewComments(thread)[0]?.source).toBe("turn 1");
  });
});

describe("comments are keyed on the turn, not its label", () => {
  it("keeps a comment off a second turn whose 8-character label reads the same", async () => {
    // Two UUIDv7 ids a moment apart: the first 8 characters are identical. The
    // mock labels them with that prefix on purpose, so the labels collide and
    // only the turn id can keep the comments apart.
    const first = "0192f0c1-2345-7abc-8def-0123456789ab" as TurnId;
    const second = "0192f0c1-9876-7abc-8def-fedcba987654" as TurnId;
    setTurn(first);

    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Turn/ }));
    const added = await waitFor(() =>
      within(diffLines()).getByRole("button", { name: /Line 1: anew/ }),
    );
    await settle();
    fireEvent.keyDown(added, { key: "Enter" });
    fireEvent.change(screen.getByLabelText("Review comment"), {
      target: { value: "Keep the clock injected." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    await waitFor(() => expect(listReviewComments(thread)).toHaveLength(1));
    expect(listReviewComments(thread)[0]?.scope).toBe(`turn:${thread}:${first}`);
    expect(listReviewComments(thread)[0]?.source).toBe("turn 0192f0c1");

    // The thread starts its next turn; the label has not ticked over.
    act(() => {
      setTurn(second);
      mock.notify();
    });
    fireEvent.click(screen.getByRole("tab", { name: "Diff" }));
    await waitFor(() =>
      expect(diffRequests().at(-1)?.against).toMatchObject({ kind: "agent_turn", turn_id: second }),
    );
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew turn/ })).toBeTruthy(),
    );

    // Same label, different turn: the comment is listed, not drawn inline.
    expect(within(diffLines()).queryByText("Keep the clock injected.")).toBeNull();
    expect(screen.getByText("Keep the clock injected.")).toBeTruthy();
    expect(screen.getByText(/another diff source/)).toBeTruthy();

    // Send to agent names the turn the comment was written in.
    fireEvent.click(screen.getByRole("button", { name: "Send to agent" }));
    await waitFor(() => expect(listReviewComments(thread)).toHaveLength(0));
    expect(readThreadDraft(host, thread)).toContain("turn 0192f0c1");
  });
});

describe("a branch review resumed after a workspace round-trip", () => {
  it("asks again with the base this workspace had picked", async () => {
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );
    await settle();

    // Picking `release` starts a new file list, which is held back.
    const filesRequests = () => mock.requests.filter((request) => request.type === "fs_diff_files");
    const releaseFiles = holdNext("fs_diff_files");
    openChip(await screen.findByRole("button", { name: "Base branch: main" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "release" }));
    await waitFor(() =>
      expect(filesRequests().at(-1)?.against).toEqual({ kind: "base", base: "release" }),
    );

    // The person leaves for a thread in another workspace mid-load, whose own
    // state has no base picked, and the held list is dropped there.
    const other = "th_2" as ThreadId;
    mock.state.threads = {
      ...mock.state.threads,
      [other]: threadRow(other, `${workspacePath}-empty`, "Another task"),
    };
    act(() => {
      mock.state.ui.selectedThreadId = other;
      mock.notify();
    });
    await waitFor(() => expect(pathBar().textContent).toBe(`${workspacePath}-empty`));
    await releaseHold(releaseFiles);
    const before = filesRequests().length;

    // Coming back resumes the interrupted review against `release`, not the
    // other workspace's (empty) choice.
    act(() => {
      mock.state.ui.selectedThreadId = thread;
      mock.notify();
    });
    await waitFor(() => expect(filesRequests().length).toBeGreaterThan(before));
    expect(filesRequests().at(-1)?.against).toEqual({ kind: "base", base: "release" });
    // One read, not two: the resumed load already opens the tab's file, so the
    // pane's follow must not ask for the same list again.
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );
    await settle();
    expect(filesRequests()).toHaveLength(before + 1);
  });
});

describe("a review that was still loading when its workspace left", () => {
  /**
   * Starts the Branch or Commits review from the source picker with its first
   * read held back. The returned release answers that read.
   */
  async function startHeldReview(source: "Branch" | "Commits", type: string): Promise<() => void> {
    const release = holdNext(type);
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: new RegExp(source) }));
    await waitFor(() =>
      expect(mock.requests.filter((request) => request.type === type).length).toBeGreaterThan(0),
    );
    return release;
  }

  /** Leaves for another task's workspace mid-load. */
  async function leave(): Promise<void> {
    const other = "th_2" as ThreadId;
    mock.state.threads = {
      ...mock.state.threads,
      [other]: threadRow(other, `${workspacePath}-empty`, "Another task"),
    };
    act(() => {
      mock.state.ui.selectedThreadId = other;
      mock.notify();
    });
    await waitFor(() => expect(pathBar().textContent).toBe(`${workspacePath}-empty`));
  }

  /** Comes back to this task's workspace. */
  async function comeBack(): Promise<void> {
    act(() => {
      mock.state.ui.selectedThreadId = thread;
      mock.notify();
    });
  }

  const reads = (type: string) => mock.requests.filter((request) => request.type === type).length;

  it("reopens the tab the Branch load was started for, reading the list once", async () => {
    await mount();
    await openDiff();

    // Close the pane, then move the editor on to another tab. The diff stays
    // on a.ts: nothing follows the tab while the pane is closed.
    fireEvent.click(screen.getByRole("tab", { name: "Edit" }));
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    // Starting Branch opens the review for b.ts; its file list is held, and the
    // person leaves before it lands.
    const releaseFiles = await startHeldReview("Branch", "fs_diff_files");
    await leave();
    await releaseHold(releaseFiles);
    const before = reads("fs_diff_files");

    // The resumed load reopens the file the load was started for, and the tab
    // follow leaves that one read alone.
    await comeBack();
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew branch/ })).toBeTruthy(),
    );
    await settle();
    expect(reads("fs_diff_files")).toBe(before + 1);
    expect(pathBar().textContent).toBe("b.ts");
  });

  it("reopens the tab the Commits load was started for, listing commits once", async () => {
    await mount();
    await openDiff();

    fireEvent.click(screen.getByRole("tab", { name: "Edit" }));
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "b.ts" }).getAttribute("aria-selected")).toBe("true"),
    );

    const releaseCommits = await startHeldReview("Commits", "fs_diff_commits");
    await leave();
    await releaseHold(releaseCommits);
    const before = reads("fs_diff_commits");

    await comeBack();
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew commit/ })).toBeTruthy(),
    );
    await settle();
    expect(reads("fs_diff_commits")).toBe(before + 1);
    expect(pathBar().textContent).toBe("b.ts");
  });

  it("reopens the active tab's Branch review when no diff had opened yet", async () => {
    await mount();

    // Branch is picked straight from Edit, before any diff has named a file:
    // `diffPath` is still null, so only the load's own preferred file can say
    // what to reopen.
    const releaseFiles = await startHeldReview("Branch", "fs_diff_files");
    await leave();
    await releaseHold(releaseFiles);
    const before = reads("fs_diff_files");

    await comeBack();
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );
    await settle();
    expect(reads("fs_diff_files")).toBe(before + 1);
    expect(pathBar().textContent).toBe("a.ts");
  });
});

describe("one comparison has one branch list and one comment key", () => {
  it("keeps a slow base-branch list after the file list and diff have opened", async () => {
    await mount();
    await openDiff();

    const releaseBranches = holdNext("fs_branches");
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));

    // The file list and the diff answer while the branch list is still out.
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ })).toBeTruthy(),
    );

    await releaseHold(releaseBranches);

    // The branch list does not depend on the open file, so it still arrives
    // and the picker offers the repository's other branches.
    const baseChip = await screen.findByRole("button", { name: "Base branch: main" });
    expect(screen.queryByRole("button", { name: /Loading branches/ })).toBeNull();
    openChip(baseChip);
    expect(await screen.findByRole("menuitem", { name: "release" })).toBeTruthy();
  });

  it("keys a branch comment on the resolved base, not on the request", async () => {
    await mount();
    await openDiff();
    openChip(screen.getByRole("button", { name: "Diff source: Uncommitted" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Branch/ }));
    const added = await waitFor(() =>
      within(diffLines()).getByRole("button", { name: /Line 1: anew branch/ }),
    );
    await settle();
    fireEvent.keyDown(added, { key: "Enter" });
    fireEvent.change(screen.getByLabelText("Review comment"), {
      target: { value: "Keep the base honest." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    // The default base was asked for as `null` and resolved to `main`; the
    // comment is keyed on that branch, not on the empty request.
    await waitFor(() => expect(listReviewComments(thread)).toHaveLength(1));
    expect(listReviewComments(thread)[0]?.scope).toBe("base:main");

    // Picking `main` by name is the same comparison, so the comment stays
    // inline instead of moving to "another diff source".
    const baseChip = await screen.findByRole("button", { name: "Base branch: main" });
    openChip(baseChip);
    fireEvent.click(await screen.findByRole("menuitem", { name: "main" }));
    await waitFor(() =>
      expect(diffRequests().at(-1)?.against).toEqual({ kind: "base", base: "main" }),
    );
    await settle();
    expect(within(diffLines()).getByText("Keep the base honest.")).toBeTruthy();
    expect(screen.queryByText(/another diff source/)).toBeNull();
  });

  it("drops a diff reply that lands after the workspace changed", async () => {
    await mount();
    await openDiff();

    // The editor opens b.ts and its diff reply is held back.
    const release = holdNext("fs_diff");
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() => expect(diffRequests().at(-1)?.path).toBe("b.ts"));

    // Another thread works in another, empty workspace: it has no file to open
    // and so asks for no diff of its own. Only the workspace change itself can
    // keep the held reply out.
    const other = "th_2" as ThreadId;
    mock.state.threads = {
      ...mock.state.threads,
      [other]: threadRow(other, `${workspacePath}-empty`, "Another task"),
    };
    act(() => {
      mock.state.ui.selectedThreadId = other;
      mock.notify();
    });
    await waitFor(() => expect(screen.getByText("Open a file to compare it.")).toBeTruthy());

    await releaseHold(release);

    // The reply for the previous workspace never paints this one.
    expect(screen.getByText("Open a file to compare it.")).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Diff lines" })).toBeNull();
    expect(pathBar().textContent).toBe(`${workspacePath}-empty`);
  });

  it("re-reads a diff that was still loading when the person left and came back", async () => {
    await mount();
    await openDiff();
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: anew head/ })).toBeTruthy(),
    );

    // The editor opens b.ts and its diff reply is held back.
    const release = holdNext("fs_diff");
    act(() => {
      mock.state.ui.pendingIdePath = "b.ts";
      mock.notify();
    });
    await waitFor(() => expect(diffRequests().at(-1)?.path).toBe("b.ts"));
    const before = diffRequests().length;

    // The person moves to a thread in another workspace and the held reply is
    // dropped there.
    const other = "th_2" as ThreadId;
    mock.state.threads = {
      ...mock.state.threads,
      [other]: threadRow(other, `${workspacePath}-empty`, "Another task"),
    };
    act(() => {
      mock.state.ui.selectedThreadId = other;
      mock.notify();
    });
    await waitFor(() => expect(screen.getByText("Open a file to compare it.")).toBeTruthy());
    await releaseHold(release);

    // Coming back must not show a.ts's lines under b.ts: the interrupted read
    // is asked again and b.ts's own diff is shown.
    act(() => {
      mock.state.ui.selectedThreadId = thread;
      mock.notify();
    });
    await waitFor(() => expect(diffRequests().length).toBeGreaterThan(before));
    expect(diffRequests().at(-1)?.path).toBe("b.ts");
    await waitFor(() =>
      expect(within(diffLines()).getByRole("button", { name: /Line 1: bnew head/ })).toBeTruthy(),
    );
    expect(within(diffLines()).queryByRole("button", { name: /Line 1: anew head/ })).toBeNull();
    expect(pathBar().textContent).toBe("b.ts");
  });
});
