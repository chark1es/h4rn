// @vitest-environment jsdom
import { act } from "react";
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
} from "@/gen";

import { type IdeSession, useIdeSession, type WorkspaceOption } from "./useIdeSession";

type Request = { type: string; path?: string; against?: FsDiff["against"] };

const state = vi.hoisted(() => ({
  requests: [] as Request[],
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
    base: scope.kind === "base" ? "main" : null,
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

function Harness() {
  latest = useIdeSession(workspace);
  return null;
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

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  project += 1;
  // A fresh path keeps the hook's per-project tab cache (module state) out of
  // the next test.
  workspace.path = `/work-${project}`;
  state.requests = [];
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
