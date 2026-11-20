import { beforeEach, describe, expect, it, vi } from "vitest";

import type { TrackedWorktree } from "@/gen";
import { H4rnRequestError } from "@/lib/client/errors";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  useApp: () => undefined,
}));

import {
  clearableWorktrees,
  clearWorktrees,
  dismissWorktreeOutcome,
  getWorktreeState,
  inUseWorktrees,
  loadWorktrees,
  resetWorktreeStoreForTests,
  settledWorktrees,
} from "./worktreeStore";

function row(overrides: Partial<TrackedWorktree> = {}): TrackedWorktree {
  return {
    path: "/data/subagents/aa/worktree",
    repo: "/code/api",
    branch: "h4rn/sa-aa",
    origin: "subagent",
    status: "settled",
    exists: true,
    dirty: false,
    owned: true,
    subagent_id: "aa",
    task: "Do a thing",
    accessed_by: [],
    first_seen: "2026-11-20T10:00:00Z",
    last_seen: "2026-11-20T10:30:00Z",
    ...overrides,
  };
}

describe("worktree store", () => {
  beforeEach(() => {
    resetWorktreeStoreForTests();
    request.mockReset();
  });

  it("loads the list and splits it into in use and settled", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [
        row({ path: "/a", status: "active" }),
        row({ path: "/b", status: "orphaned" }),
        row({ path: "/c" }),
      ],
    });
    await loadWorktrees();
    const { rows, loaded } = getWorktreeState();
    expect(loaded).toBe(true);
    expect(inUseWorktrees(rows).map((r) => r.path)).toEqual(["/a"]);
    expect(settledWorktrees(rows).map((r) => r.path)).toEqual(["/b", "/c"]);
  });

  it("reports a failed read as an error, not a rejection", async () => {
    request.mockRejectedValueOnce(new Error("socket closed"));
    await loadWorktrees();
    expect(getWorktreeState().loaded).toBe(true);
    expect(getWorktreeState().error).not.toBeNull();
  });

  it("clears with the given paths and keeps the skipped list", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees_cleared",
      removed: ["/b"],
      skipped: [{ path: "/c", reason: "has uncommitted changes" }],
      worktrees: [row({ path: "/c", dirty: true })],
    });
    await clearWorktrees(["/b", "/c"]);
    expect(request).toHaveBeenCalledWith({
      type: "clear_worktrees",
      paths: ["/b", "/c"],
      force: false,
    });
    const { rows, outcome, clearing } = getWorktreeState();
    expect(clearing).toBe(false);
    expect(rows.map((r) => r.path)).toEqual(["/c"]);
    expect(outcome).toEqual({
      removed: 1,
      skipped: [{ path: "/c", reason: "has uncommitted changes" }],
    });
  });

  it("sends null paths for clear all and force for a retry", async () => {
    request.mockResolvedValue({
      type: "worktrees_cleared",
      removed: [],
      skipped: [],
      worktrees: [],
    });
    await clearWorktrees(null);
    expect(request).toHaveBeenLastCalledWith({
      type: "clear_worktrees",
      paths: null,
      force: false,
    });
    await clearWorktrees(["/c"], true);
    expect(request).toHaveBeenLastCalledWith({
      type: "clear_worktrees",
      paths: ["/c"],
      force: true,
    });
  });

  it("surfaces a refused clear with its hint and leaves the list alone", async () => {
    request.mockResolvedValueOnce({ type: "worktrees", worktrees: [row()] });
    await loadWorktrees();
    request.mockRejectedValueOnce(
      new H4rnRequestError({
        code: "forbidden",
        message: "Clearing worktrees needs admin access.",
        hint: "Ask the host owner.",
      }),
    );
    await clearWorktrees(null);
    const state = getWorktreeState();
    expect(state.clearing).toBe(false);
    expect(state.clearError).toContain("needs admin access");
    expect(state.rows).toHaveLength(1);
    dismissWorktreeOutcome();
    expect(getWorktreeState().clearError).toBeNull();
  });
});

describe("clearableWorktrees", () => {
  it("leaves thread workspaces and in-use worktrees out of Clear all", () => {
    const rows = [
      row({ path: "/a" }),
      row({ path: "/b", status: "orphaned" }),
      row({ path: "/c", origin: "thread", owned: false }),
      row({ path: "/d", status: "active" }),
      row({ path: "/e", origin: "harness", owned: false }),
    ];
    expect(clearableWorktrees(rows).map((r) => r.path)).toEqual(["/a", "/b", "/e"]);
  });
});
