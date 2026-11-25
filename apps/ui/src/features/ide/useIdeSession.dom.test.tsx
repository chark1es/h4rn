// @vitest-environment jsdom
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  FsBranchList,
  FsDiff,
  FsDiffCommitList,
  FsDiffFiles,
  FsFileContent,
  FsListing,
  FsTurnChanges,
  HostId,
  ThreadId,
  TurnId,
} from "@/gen";

import { type IdeSession, useIdeSession, type WorkspaceOption } from "./useIdeSession";

type Request = { type: string; path?: string; against?: FsDiff["against"] };

const state = vi.hoisted(() => ({
  requests: [] as Request[],
  /** The next request of this type waits for `promise` before it answers. */
  holds: [] as Array<{ type: string; promise: Promise<void> }>,
  /** The next request of this type fails after being recorded. */
  fail: null as { type: string; error: Error } | null,
  branches: {
    branches: ["main", "feature"],
    default_base: "main",
  } as FsBranchList,
  commits: {
    base: "main",
    merge_base: "base-sha",
    commits: [
      {
        sha: "commit-1",
        short_sha: "c1",
        subject: "first",
        author: "you",
        authored_at: "2026-03-04T12:00:00Z",
      },
    ],
    truncated: false,
  } as FsDiffCommitList,
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({
    request: async (request: Request) => {
      state.requests.push(request);
      const held = state.holds.findIndex((hold) => hold.type === request.type);
      if (held >= 0) {
        const heldRequest = state.holds[held];
        state.holds.splice(held, 1);
        if (heldRequest) await heldRequest.promise;
      }
      if (state.fail && state.fail.type === request.type) {
        const fail = state.fail;
        state.fail = null;
        throw fail.error;
      }
      switch (request.type) {
        case "fs_list":
          return { type: "fs_listing", listing: listing(request.path ?? "") };
        case "fs_read":
          return { type: "fs_file", file: file(request.path ?? "") };
        case "fs_diff":
          return { type: "fs_diff", diff: diff(request.path ?? "", request.against) };
        case "fs_diff_files":
          return { type: "fs_diff_files", files: diffFiles(request.against) };
        case "fs_diff_commits":
          return { type: "fs_diff_commits", commits: state.commits };
        case "fs_branches":
          return { type: "fs_branches", branches: state.branches };
        case "fs_turn_changes":
          return { type: "fs_turn_changes", changes: changes() };
        default:
          throw new Error(`unexpected request ${request.type}`);
      }
    },
    onEvent: () => () => undefined,
  }),
}));

vi.mock("@/lib/client/fsWatch", () => ({
  retainWorkspaceWatch: () => () => undefined,
}));

const workspace: WorkspaceOption = {
  path: "/work",
  host: "host_1" as HostId,
  threadId: null,
  threadTitle: null,
  turnId: null,
  harness: null,
};

let project = 0;

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
  return { workspace: "/work", path, entries, truncated: false };
}

function file(path: string): FsFileContent {
  const content = `${path}\n`;
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

function diff(path: string, against?: FsDiff["against"]): FsDiff {
  const scope = against ?? { kind: "head" };
  return {
    path,
    against: scope,
    label:
      scope.kind === "head"
        ? "HEAD"
        : scope.kind === "base"
          ? "vs main"
          : scope.kind === "commit"
            ? "Commit c1"
            : "Turn 1",
    before: "before\n",
    after: `${path}\n`,
    binary: false,
    too_large: false,
    note: null,
  };
}

function diffFiles(against?: FsDiff["against"]): FsDiffFiles {
  const scope = against ?? { kind: "base", base: null };
  return {
    against: scope,
    label: scope.kind === "commit" ? "Commit c1" : "vs main",
    base: scope.kind === "base" ? (scope.base ?? "main") : null,
    merge_base: scope.kind === "base" ? "base-sha" : null,
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
    ],
    truncated: false,
    note: null,
  };
}

function changes(): FsTurnChanges {
  return { thread_id: "th_1", turn_id: "turn_1", files: [], note: null };
}

let latest: IdeSession | null = null;
let roots: Root[] = [];
let setEpoch: ((updater: (value: number) => number) => void) | null = null;

function Harness() {
  const [epoch, set] = useState(0);
  setEpoch = set;
  latest = useIdeSession(workspace);
  // The hook only re-reads the workspace when the pane renders again.
  void epoch;
  return null;
}

/** Moves the mounted pane to another workspace, through a real re-render. */
async function switchWorkspace(path: string): Promise<void> {
  workspace.path = path;
  await act(async () => {
    setEpoch?.((value) => value + 1);
  });
  await flush();
}

async function mount(): Promise<void> {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => root.render(<Harness />));
  roots.push(root);
  await flush();
}

/** Lets queued promise callbacks run inside act. */
async function flush(times = 8): Promise<void> {
  await act(async () => {
    for (let index = 0; index < times; index += 1) await Promise.resolve();
  });
}

async function waitFor(check: () => boolean, tries = 50): Promise<void> {
  for (let index = 0; index < tries; index += 1) {
    if (check()) return;
    await flush(1);
  }
  throw new Error("condition never became true");
}

/** Makes the next request of `type` wait; call the returned function to answer. */
function holdNext(type: string): () => void {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  state.holds.push({ type, promise });
  return release;
}

/** Makes the next request of `type` fail, as a dropped connection would. */
function failNext(type: string): void {
  state.fail = { type, error: new Error("The daemon connection is offline.") };
}

/** Releases a held reply and lets its callbacks run. */
async function releaseHold(release: () => void): Promise<void> {
  await act(async () => {
    release();
    await Promise.resolve();
  });
  await flush();
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  project += 1;
  // A fresh path keeps the hook's per-project tab cache (module state) out of
  // the next test.
  workspace.path = `/work-${project}`;
  workspace.threadId = null;
  workspace.turnId = null;
  state.requests = [];
  state.holds = [];
  state.fail = null;
  state.branches = { branches: ["main", "feature"], default_base: "main" };
  state.commits = {
    base: "main",
    merge_base: "base-sha",
    commits: [
      {
        sha: "commit-1",
        short_sha: "c1",
        subject: "first",
        author: "you",
        authored_at: "2026-03-04T12:00:00Z",
      },
    ],
    truncated: false,
  };
  latest = null;
});

afterEach(() => {
  for (const root of roots) act(() => root.unmount());
  roots = [];
  document.body.innerHTML = "";
});

describe("the diff pane follows the file in front of it", () => {
  it("shows the active tab's file when the pane is entered, not a stale diff path", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    await act(async () => {
      await latest?.openDiff("a.ts", "head");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");

    await act(async () => {
      latest?.openFile("b.ts");
    });
    await waitFor(() => latest?.active?.path === "b.ts");

    state.requests = [];
    await act(async () => {
      await latest?.selectDiffSource("head");
    });
    await waitFor(() => latest?.state.diff?.path === "b.ts");

    const diffRequests = state.requests.filter((request) => request.type === "fs_diff");
    expect(diffRequests.at(-1)?.path).toBe("b.ts");
    // A comment is filed under this path, so it must be the tab's file too.
    expect(latest?.state.diffPath).toBe("b.ts");
  });
});

describe("the wider review lists stay current", () => {
  it("re-reads the branch list each time Branch is entered", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diffSource === "branch");
    expect(state.requests.filter((request) => request.type === "fs_branches")).toHaveLength(1);

    state.branches = {
      branches: ["main", "feature", "feature/just-fetched"],
      default_base: "main",
    };
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diffSource === "branch");

    expect(state.requests.filter((request) => request.type === "fs_branches")).toHaveLength(2);
    expect(latest?.state.diffBranches?.branches).toContain("feature/just-fetched");
  });

  it("re-reads the commit list on entry and on an explicit refresh", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diffSource === "commit");
    expect(state.requests.filter((request) => request.type === "fs_diff_commits")).toHaveLength(1);

    // The agent just committed. Entering Commits again must not answer from
    // the list the pane fetched before that.
    state.commits = {
      ...state.commits,
      commits: [
        {
          sha: "commit-2",
          short_sha: "c2",
          subject: "the agent's new commit",
          author: "agent",
          authored_at: "2026-03-04T12:01:00Z",
        },
        ...state.commits.commits,
      ],
    };
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diffSource === "commit");
    expect(state.requests.filter((request) => request.type === "fs_diff_commits")).toHaveLength(2);
    expect(latest?.state.diffCommits?.commits[0]?.sha).toBe("commit-2");

    state.commits = {
      ...state.commits,
      commits: [
        {
          sha: "commit-3",
          short_sha: "c3",
          subject: "another commit",
          author: "agent",
          authored_at: "2026-03-04T12:02:00Z",
        },
        ...state.commits.commits,
      ],
    };
    await act(async () => {
      await latest?.refreshDiffLists();
    });
    await waitFor(() => latest?.state.diffCommits?.commits[0]?.sha === "commit-3");
    expect(state.requests.filter((request) => request.type === "fs_diff_commits")).toHaveLength(3);
  });
});

describe("a dropped reply never leaves a spinner behind", () => {
  it("clears the abandoned diff spinner when a Branch list takes over", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // A HEAD diff is out and its reply is held.
    const releaseDiff = holdNext("fs_diff");
    await act(async () => {
      void latest?.openDiff("a.ts", "head");
    });
    await waitFor(() => latest?.state.diffLoading === true);

    // Branch is picked while that reply is still missing, and its own file
    // list is held too. The old diff request is abandoned: its spinner must
    // not speak for the Branch load.
    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diffLoading).toBe(false);
    expect(latest?.state.diff).toBeNull();

    // The abandoned reply landing later changes nothing.
    await releaseHold(releaseDiff);
    expect(latest?.state.diffLoading).toBe(false);
    expect(latest?.state.diff).toBeNull();

    await releaseHold(releaseFiles);
    await waitFor(() => latest?.state.diff?.path === "a.ts");
    expect(latest?.state.diffLoading).toBe(false);
  });

  it("clears the changed-file spinner when another source supersedes it", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    await act(async () => {
      await latest?.selectDiffSource("head");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");
    expect(latest?.state.filesLoading).toBe(false);

    await releaseHold(releaseFiles);
    // The abandoned Branch list never paints over the Uncommitted choice.
    expect(latest?.state.diffFiles).toBeNull();
    expect(latest?.state.filesLoading).toBe(false);
  });

  it("clears the commit-list spinner when another source supersedes it", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    const releaseCommits = holdNext("fs_diff_commits");
    await act(async () => {
      void latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.commitsLoading === true);

    await act(async () => {
      await latest?.selectDiffSource("head");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");
    expect(latest?.state.commitsLoading).toBe(false);

    await releaseHold(releaseCommits);
    expect(latest?.state.diffCommits).toBeNull();
    expect(latest?.state.commitsLoading).toBe(false);
  });

  it("clears a branch-list spinner left loading when its workspace is revisited", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // The Branch list is still out when the file list and diff have opened.
    const releaseBranches = holdNext("fs_branches");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(
      () => latest?.state.branchesLoading === true && latest?.state.diff?.path === "a.ts",
    );

    const home = workspace.path;
    await switchWorkspace(`${home}-other`);
    await switchWorkspace(home);

    // Nothing is in flight for the restored workspace, so its cached spinner
    // must not claim otherwise.
    expect(latest?.state.branchesLoading).toBe(false);
    await releaseHold(releaseBranches);
    expect(latest?.state.branchesLoading).toBe(false);
    expect(latest?.state.diffBranches).toBeNull();
  });

  it("clears the diff spinner when the connection drops", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    failNext("fs_diff");
    await act(async () => {
      await latest?.openDiff("a.ts", "head");
    });
    await waitFor(() => latest?.state.diffError !== null);
    expect(latest?.state.diffLoading).toBe(false);
    expect(latest?.state.diff).toBeNull();
  });

  it("clears the changed-file spinner when the connection drops", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    failNext("fs_diff_files");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesError !== null);
    expect(latest?.state.filesLoading).toBe(false);
    expect(latest?.state.diffLoading).toBe(false);
  });

  it("clears the commit-list spinner when the connection drops", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    failNext("fs_diff_commits");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.commitsError !== null);
    expect(latest?.state.commitsLoading).toBe(false);
    expect(latest?.state.diffLoading).toBe(false);
  });

  it("clears the branch-list spinner when the connection drops", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // Keep the file list out of the way: no later diff request may clear the
    // branch spinner on the failed read's behalf.
    const releaseFiles = holdNext("fs_diff_files");
    failNext("fs_branches");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.branchesError !== null);
    expect(latest?.state.branchesLoading).toBe(false);
    await releaseHold(releaseFiles);
  });

  it.each(["head", "agent", "branch", "commit"] as const)(
    "clears the %s spinner when the workspace changes mid-load",
    async (source) => {
      workspace.threadId = "th_1" as ThreadId;
      workspace.turnId = "turn_1" as TurnId;
      await mount();
      await waitFor(() => latest?.active?.path === "a.ts");

      const release = holdNext("fs_diff");
      await act(async () => {
        if (source === "commit") {
          void latest?.setDiffCommit("commit-1");
        } else {
          void latest?.openDiff("a.ts", source);
        }
      });
      await waitFor(() => latest?.state.diffLoading === true);

      await switchWorkspace(`${workspace.path}-other`);
      expect(latest?.state.diffLoading).toBe(false);
      expect(latest?.state.diff).toBeNull();

      await releaseHold(release);
      expect(latest?.state.diffLoading).toBe(false);
      expect(latest?.state.diff).toBeNull();
    },
  );
});

describe("a superseded diff never shows under a newer choice", () => {
  /** Loads a HEAD diff and then starts a held Branch diff for another file. */
  async function startHeldBranchDiff(): Promise<() => void> {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.openDiff("a.ts", "head");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");

    const releaseDiff = holdNext("fs_diff");
    await act(async () => {
      void latest?.openDiff("b.ts", "branch");
    });
    await waitFor(() => latest?.state.diffLoading === true);
    // The previous file's body is still held in state while its replacement
    // loads; only a newer scope choice has to take it off the screen.
    expect(latest?.state.diff?.path).toBe("a.ts");
    return releaseDiff;
  }

  it("clears the held body when the base changes", async () => {
    const releaseDiff = await startHeldBranchDiff();

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffBase("feature");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    // a.ts-vs-main must not stay readable under b.ts and "feature".
    expect(latest?.state.diff).toBeNull();
    expect(latest?.state.diffPath).toBe("b.ts");

    await releaseHold(releaseFiles);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "feature" });

    // The abandoned read never paints over the newer comparison.
    await releaseHold(releaseDiff);
    expect(latest?.state.diff?.path).toBe("b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "feature" });
  });

  it("clears the held body when the commit changes", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-1" });

    const releaseDiff = holdNext("fs_diff");
    await act(async () => {
      void latest?.openDiff("b.ts", "commit", { commit: "commit-2" });
    });
    await waitFor(() => latest?.state.diffLoading === true);
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-1" });

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffCommit("commit-2");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diff).toBeNull();

    await releaseHold(releaseFiles);
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-2" });

    await releaseHold(releaseDiff);
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-2" });
  });

  it("clears the held body when the lists refresh", async () => {
    const releaseDiff = await startHeldBranchDiff();

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.refreshDiffLists();
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diff).toBeNull();

    await releaseHold(releaseFiles);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "main" });

    await releaseHold(releaseDiff);
    expect(latest?.state.diff?.path).toBe("b.ts");
  });
});

describe("a tab switch follows the scope being loaded, not the one before it", () => {
  it("re-lists Commits re-entered with a held commit list", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "base");

    // Back to Commits with a slow commit list: the commit and the Branch file
    // list from before are still in state while it loads.
    const releaseCommits = holdNext("fs_diff_commits");
    await act(async () => {
      void latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.commitsLoading === true);
    expect(latest?.state.diffCommit).toBe("commit-1");
    expect(latest?.state.diffFiles?.against.kind).toBe("base");

    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await releaseHold(releaseCommits);
    await waitFor(() => latest?.state.diff?.path === "b.ts");

    const fileLists = state.requests.filter((request) => request.type === "fs_diff_files");
    expect(fileLists.at(-1)?.against).toEqual({ kind: "commit", sha: "commit-1" });
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "commit", sha: "commit-1" });
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-1" });
    expect(latest?.state.commitsLoading).toBe(false);
  });

  it("keeps a newly chosen base when the tab changes while its list loads", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diffBase === "main" && latest?.state.diff !== null);

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffBase("feature");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    await releaseHold(releaseFiles);

    expect(latest?.state.diffBase).toBe("feature");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "feature" });
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "base", base: "feature" });
  });
});

describe("a refreshed commit list is not dropped by a tab switch", () => {
  it("re-lists the same commit when the tab changes during a refresh", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");

    // The commit and its file list on screen match; only the refresh of the
    // commit list is out.
    const releaseCommits = holdNext("fs_diff_commits");
    await act(async () => {
      void latest?.refreshDiffLists();
    });
    await waitFor(() => latest?.state.commitsLoading === true);
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "commit", sha: "commit-1" });

    const commitReads = state.requests.filter(
      (request) => request.type === "fs_diff_commits",
    ).length;
    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await releaseHold(releaseCommits);
    await waitFor(() => latest?.state.diff?.path === "b.ts");

    // The follow asked for the commit list again instead of dropping the
    // refresh, and nothing is left loading.
    expect(state.requests.filter((request) => request.type === "fs_diff_commits")).toHaveLength(
      commitReads + 1,
    );
    expect(latest?.state.commitsLoading).toBe(false);
    expect(latest?.state.diffCommits).not.toBeNull();
  });
});

describe("a tab the pane already shows needs no read", () => {
  it("skips a follow to the file that is open with no list out", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts" && !latest?.state.filesLoading);

    const before = state.requests.length;
    await act(async () => {
      await latest?.followDiffTab("a.ts");
    });
    await flush();
    expect(state.requests).toHaveLength(before);
  });
});

describe("a new comparison takes the finished body off the screen", () => {
  it("clears a loaded Branch diff when the base changes", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff !== null && !latest?.state.diffLoading);

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffBase("feature");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diff).toBeNull();
    await releaseHold(releaseFiles);
  });

  it("clears a loaded commit diff when the commit changes", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff !== null && !latest?.state.diffLoading);

    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffCommit("commit-2");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diff).toBeNull();
    await releaseHold(releaseFiles);
  });
});

describe("a stale branch reply leaves its newer load alone", () => {
  it("keeps the newer spinner and paints only the newer list", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    const releaseOlder = holdNext("fs_branches");
    const releaseNewer = holdNext("fs_branches");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(
      () => state.requests.filter((request) => request.type === "fs_branches").length === 1,
    );

    // A second read of the picker's list starts while the first is still out.
    state.branches = { branches: ["main", "newer"], default_base: "main" };
    await act(async () => {
      void latest?.fetchBranches();
    });
    await waitFor(
      () => state.requests.filter((request) => request.type === "fs_branches").length === 2,
    );
    expect(latest?.state.branchesLoading).toBe(true);

    // The older reply lands first. The newer read owns the spinner and the
    // list, so the older answer must neither stop the spinner nor paint.
    await releaseHold(releaseOlder);
    expect(latest?.state.branchesLoading).toBe(true);
    expect(latest?.state.diffBranches).toBeNull();

    await releaseHold(releaseNewer);
    expect(latest?.state.branchesLoading).toBe(false);
    expect(latest?.state.diffBranches?.branches).toEqual(["main", "newer"]);
  });
});

describe("a tab switch keeps an empty commit range empty", () => {
  it("re-lists the range instead of asking for a commit it no longer has", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");

    // The range emptied out since the list was read, so the refresh clears the
    // commit that was selected from the older list.
    state.commits = { ...state.commits, commits: [] };
    await act(async () => {
      await latest?.refreshDiffLists();
    });
    expect(latest?.state.diffCommit).toBeNull();
    expect(latest?.state.diffError).toBeNull();

    // Switching tabs on the empty range must re-read the list: opening a
    // commit scope with no commit would answer with an error about a missing
    // sha instead of the range's own empty state.
    const diffs = state.requests.filter((request) => request.type === "fs_diff").length;
    await act(async () => {
      await latest?.followDiffTab("b.ts");
    });
    await flush();
    expect(latest?.state.diffError).toBeNull();
    expect(state.requests.filter((request) => request.type === "fs_diff")).toHaveLength(diffs);
    expect(latest?.state.diffCommit).toBeNull();
    expect(latest?.state.diffPath).toBeNull();
  });
});

describe("a follow re-reads a branch list that never arrived", () => {
  it("asks for the failed list once when the tab switches", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // The picker's list fails and the first file list is still out, so the
    // follow has to take over the load.
    const releaseFiles = holdNext("fs_diff_files");
    failNext("fs_branches");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.branchesError !== null);
    expect(latest?.state.diffBranches).toBeNull();
    expect(latest?.state.branchesLoading).toBe(false);
    expect(latest?.state.filesLoading).toBe(true);

    // The picker has no list to offer; the follow reads it again, exactly once,
    // so the base picker recovers without a manual refresh.
    state.requests = [];
    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    await releaseHold(releaseFiles);
    expect(state.requests.filter((request) => request.type === "fs_branches")).toHaveLength(1);
    expect(latest?.state.diffBranches?.branches).toEqual(["main", "feature"]);
    expect(latest?.state.branchesError).toBeNull();
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "main" });
  });
});

describe("a tab switch during a held commit file list", () => {
  it("opens the new tab's files and drops the older list", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-1" });

    // The picker moves to commit-2, and its file list is held back.
    state.commits = {
      ...state.commits,
      commits: [
        {
          sha: "commit-2",
          short_sha: "c2",
          subject: "second",
          author: "you",
          authored_at: "2026-03-04T12:03:00Z",
        },
        ...state.commits.commits,
      ],
    };
    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffCommit("commit-2");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    // The person moves to another tab before it lands.
    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-2" });
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "commit", sha: "commit-2" });

    // The held list for a.ts belongs to the request the follow replaced; it
    // must not paint its own file list or open a.ts over the tab on screen.
    await releaseHold(releaseFiles);
    await flush();
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "commit", sha: "commit-2" });
    expect(latest?.state.diff?.path).toBe("b.ts");
    expect(latest?.state.diffPath).toBe("b.ts");
    expect(latest?.state.filesLoading).toBe(false);
  });
});

describe("a resumed review yields to a newer tab switch", () => {
  it("opens the tab switched back to, and drops the held replies", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // Branch is started for a.ts and its file list is held.
    const releaseFirst = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    // The person leaves for another workspace mid-load.
    const home = workspace.path;
    await switchWorkspace(`${home}-other`);

    // Coming back resumes the review for a.ts, and that read is held too.
    const releaseResumed = holdNext("fs_diff_files");
    await switchWorkspace(home);
    await waitFor(() => latest?.state.filesLoading === true);

    // A tab switch to b.ts reads its list while the resumed one is still out.
    const listReads = () =>
      state.requests.filter((request) => request.type === "fs_diff_files").length;
    const before = listReads();
    const releaseB = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await waitFor(() => listReads() === before + 1);

    // Back to a.ts: the resume's note names a.ts, but it belongs to the older
    // request, so it must not swallow this switch. Without the generation
    // check the follow would return here and no reply would ever open a.ts.
    await act(async () => {
      void latest?.followDiffTab("a.ts");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "main" });

    // The three held replies belong to requests the switch replaced. The b.ts
    // one would open b.ts if it were applied; a.ts must stay on screen.
    const diffs = state.requests.filter((request) => request.type === "fs_diff").length;
    await releaseHold(releaseB);
    await releaseHold(releaseResumed);
    await releaseHold(releaseFirst);
    expect(latest?.state.diff?.path).toBe("a.ts");
    expect(latest?.state.diffPath).toBe("a.ts");
    expect(latest?.state.diffFiles?.against).toEqual({ kind: "base", base: null });
    expect(latest?.state.filesLoading).toBe(false);
    expect(state.requests.filter((request) => request.type === "fs_diff")).toHaveLength(diffs);
  });
});

describe("a direct open replaces the finished list's file", () => {
  it("resumes the file being opened, not the one the list was read for", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // Branch is read for a.ts and its diff lands.
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts");

    // The list is on screen, so a tab switch opens b.ts directly; that read is
    // held back.
    const releaseDiff = holdNext("fs_diff");
    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    await waitFor(() => latest?.state.diffLoading === true);
    expect(latest?.state.diffPath).toBe("b.ts");

    // The person leaves mid-read and comes back. The open was for b.ts, so the
    // resume must open b.ts; the finished list's file (a.ts) is history.
    const home = workspace.path;
    await switchWorkspace(`${home}-other`);
    await releaseHold(releaseDiff);
    await switchWorkspace(home);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    await flush();
    expect(latest?.state.diffPath).toBe("b.ts");
    expect(latest?.state.diff?.path).toBe("b.ts");
  });
});

describe("a base change keeps the tab a pending review was started for", () => {
  it("reopens that tab after a round-trip and reads its list once", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts" && !latest?.state.filesLoading);

    // The pane is closed while the person moves to b.ts and picks Branch: the
    // load is for b.ts, but `diffPath` still names a.ts until it lands.
    await act(async () => {
      latest?.openFile("b.ts");
    });
    await waitFor(() => latest?.active?.path === "b.ts");
    const releaseTabLoad = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diffPath).toBe("a.ts");

    // The base changes before that list lands; its own list is held too.
    const releaseBase = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffBase("feature");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    // Leave mid-read and come back. The resumed read reopens b.ts, the file the
    // base change was started for, and the tab follow leaves its read alone.
    const home = workspace.path;
    await switchWorkspace(`${home}-other`);
    state.requests = [];
    const listReads = () =>
      state.requests.filter((request) => request.type === "fs_diff_files").length;
    const releaseResume = holdNext("fs_diff_files");
    await switchWorkspace(home);
    await waitFor(() => latest?.state.filesLoading === true);
    expect(listReads()).toBe(1);

    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    expect(listReads()).toBe(1);

    await releaseHold(releaseResume);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diffPath).toBe("b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "feature" });

    // The two earlier reads belong to requests the round-trip replaced.
    await releaseHold(releaseBase);
    await releaseHold(releaseTabLoad);
    expect(latest?.state.diff?.path).toBe("b.ts");
    expect(listReads()).toBe(1);
  });
});

describe("a refresh keeps the tab a pending review was started for", () => {
  it("reopens that tab after a round-trip and reads its list once", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.diff?.path === "a.ts" && !latest?.state.filesLoading);

    // The pane is closed while the person moves to b.ts and picks Branch: the
    // load is for b.ts, but `diffPath` still names a.ts until it lands.
    await act(async () => {
      latest?.openFile("b.ts");
    });
    await waitFor(() => latest?.active?.path === "b.ts");
    const releaseTabLoad = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diffPath).toBe("a.ts");

    // The Refresh button stays enabled while that list is out, and it is
    // pressed before the reply lands; its own list is held too.
    const releaseRefresh = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.refreshDiffLists();
    });
    await waitFor(() => latest?.state.filesLoading === true);

    // Leave mid-read and come back. The resumed read reopens b.ts, the file the
    // refresh was started for, and the tab follow leaves its read alone.
    const home = workspace.path;
    await switchWorkspace(`${home}-other`);
    state.requests = [];
    const listReads = () =>
      state.requests.filter((request) => request.type === "fs_diff_files").length;
    const releaseResume = holdNext("fs_diff_files");
    await switchWorkspace(home);
    await waitFor(() => latest?.state.filesLoading === true);
    expect(listReads()).toBe(1);

    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    expect(listReads()).toBe(1);

    await releaseHold(releaseResume);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diffPath).toBe("b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "base", base: "main" });

    // The two earlier reads belong to requests the round-trip replaced.
    await releaseHold(releaseRefresh);
    await releaseHold(releaseTabLoad);
    expect(latest?.state.diff?.path).toBe("b.ts");
    expect(listReads()).toBe(1);
  });
});

describe("a commit change keeps the tab a pending review was started for", () => {
  it("reopens that tab after a round-trip and reads its lists once", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");
    await act(async () => {
      await latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.diff?.against?.kind === "commit");

    // The pane is closed while the person moves to b.ts and picks Commits: the
    // commit list is for b.ts, but `diffPath` still names a.ts.
    await act(async () => {
      latest?.openFile("b.ts");
    });
    await waitFor(() => latest?.active?.path === "b.ts");
    const releaseTabFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("commit");
    });
    await waitFor(() => latest?.state.filesLoading === true);
    expect(latest?.state.diffPath).toBe("a.ts");
    // The commit list landed, so the picker is enabled: only the file list for
    // b.ts is still out.
    expect(latest?.state.commitsLoading).toBe(false);
    expect(latest?.state.diffCommits?.commits[0]?.sha).toBe("commit-1");

    // The file list is still out and the person changes the commit while it
    // is: that list was for b.ts, so the change must keep b.ts.
    state.commits = {
      ...state.commits,
      commits: [
        {
          sha: "commit-2",
          short_sha: "c2",
          subject: "second",
          author: "you",
          authored_at: "2026-03-04T12:03:00Z",
        },
        ...state.commits.commits,
      ],
    };
    const releaseCommit = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.setDiffCommit("commit-2");
    });
    await waitFor(() => latest?.state.filesLoading === true);

    // Leave mid-read and come back. The resumed read reopens b.ts with the new
    // commit, and the tab follow leaves its lists alone.
    const home = workspace.path;
    await switchWorkspace(`${home}-other`);
    state.requests = [];
    const commitReads = () =>
      state.requests.filter((request) => request.type === "fs_diff_commits").length;
    const fileReads = () =>
      state.requests.filter((request) => request.type === "fs_diff_files").length;
    const releaseResumeCommits = holdNext("fs_diff_commits");
    await switchWorkspace(home);
    await waitFor(() => latest?.state.commitsLoading === true);
    expect(commitReads()).toBe(1);

    await act(async () => {
      void latest?.followDiffTab("b.ts");
    });
    expect(commitReads()).toBe(1);

    await releaseHold(releaseResumeCommits);
    await waitFor(() => latest?.state.diff?.path === "b.ts");
    expect(latest?.state.diffPath).toBe("b.ts");
    expect(latest?.state.diff?.against).toEqual({ kind: "commit", sha: "commit-2" });
    expect(commitReads()).toBe(1);
    expect(fileReads()).toBe(1);

    // The two earlier reads belong to requests the round-trip replaced.
    await releaseHold(releaseCommit);
    await releaseHold(releaseTabFiles);
    expect(latest?.state.diff?.path).toBe("b.ts");
    expect(commitReads()).toBe(1);
    expect(fileReads()).toBe(1);
  });
});

describe("a workspace change advances the branch generation", () => {
  it("ignores the previous workspace's branch list and file list", async () => {
    await mount();
    await waitFor(() => latest?.active?.path === "a.ts");

    // A distinctive branch list for the workspace the pane is about to leave.
    state.branches = { branches: ["old-only"], default_base: "old-only" };
    const releaseBranches = holdNext("fs_branches");
    const releaseFiles = holdNext("fs_diff_files");
    await act(async () => {
      void latest?.selectDiffSource("branch");
    });
    await waitFor(
      () => latest?.state.branchesLoading === true && latest?.state.filesLoading === true,
    );

    await switchWorkspace(`${workspace.path}-other`);
    expect(latest?.state.branchesLoading).toBe(false);
    expect(latest?.state.filesLoading).toBe(false);

    // Both replies belong to the workspace the person left.
    await releaseHold(releaseBranches);
    await releaseHold(releaseFiles);
    expect(latest?.state.diffBranches).toBeNull();
    expect(latest?.state.diffFiles).toBeNull();
    expect(latest?.state.diffBase).toBeNull();
    expect(state.requests.filter((request) => request.type === "fs_diff")).toHaveLength(0);
  });
});
