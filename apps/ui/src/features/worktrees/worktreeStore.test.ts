import { beforeEach, describe, expect, it, vi } from "vitest";

import type { TrackedWorktree } from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { H4rnRequestError } from "@/lib/client/errors";

/**
 * Two clients, so a test can prove that rows read through one are never used
 * to clear through the other (a machine switch rebuilds the client).
 */
const harness = vi.hoisted(() => {
  const clientA = { request: vi.fn() };
  const clientB = { request: vi.fn() };
  const current = { value: clientA };
  return { clientA, clientB, current };
});

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => harness.current.value,
  useApp: () => undefined,
}));

import {
  captureClear,
  clearableWorktrees,
  clearWorktrees,
  dismissWorktreeOutcome,
  forceCanFix,
  getWorktreeState,
  inUseWorktrees,
  loadWorktrees,
  removeDescription,
  resetWorktreeStoreForTests,
  settledWorktrees,
  shouldReloadWorktrees,
  worktreeClientCurrent,
  worktreeOutcomeForClient,
  worktreePage,
  worktreeSubline,
} from "./worktreeStore";

const request = harness.clientA.request;
const clientA = harness.clientA as unknown as H4rnClient;
const clientB = harness.clientB as unknown as H4rnClient;

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
    harness.current.value = harness.clientA;
    request.mockReset();
    harness.clientB.request.mockReset();
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
    const { rows, loaded, source } = getWorktreeState();
    expect(loaded).toBe(true);
    expect(source).toBe(clientA);
    expect(inUseWorktrees(rows).map((r) => r.path)).toEqual(["/a"]);
    expect(settledWorktrees(rows).map((r) => r.path)).toEqual(["/b", "/c"]);
    expect(request).toHaveBeenCalledWith({ type: "list_worktrees" }, { timeoutMs: 60_000 });
  });

  it("reports a failed read as an error, not a rejection", async () => {
    request.mockRejectedValueOnce(new Error("socket closed"));
    await loadWorktrees();
    expect(getWorktreeState().loaded).toBe(true);
    expect(getWorktreeState().error).not.toBeNull();
  });

  it("clears with the given paths and keeps the skipped list", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [row({ path: "/b" }), row({ path: "/c", dirty: true })],
    });
    await loadWorktrees();
    request.mockResolvedValueOnce({
      type: "worktrees_cleared",
      removed: ["/b"],
      skipped: [{ path: "/c", reason: "has uncommitted changes" }],
      worktrees: [row({ path: "/c", dirty: true })],
    });
    await clearWorktrees(["/b", "/c"]);
    expect(request).toHaveBeenLastCalledWith(
      {
        type: "clear_worktrees",
        paths: ["/b", "/c"],
        force: false,
        bulk: false,
      },
      { timeoutMs: 600_000 },
    );
    const { rows, outcome, clearing } = getWorktreeState();
    expect(clearing).toBe(false);
    expect(rows.map((r) => r.path)).toEqual(["/c"]);
    expect(outcome).toEqual({
      removed: 1,
      skipped: [{ path: "/c", reason: "has uncommitted changes" }],
      bulk: false,
    });
  });

  it("sends bulk for Clear all and keeps it for the forced retry of its skips", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [row({ path: "/b" }), row({ path: "/c", dirty: true })],
    });
    await loadWorktrees();
    request.mockResolvedValue({
      type: "worktrees_cleared",
      removed: ["/b"],
      skipped: [{ path: "/c", reason: "has uncommitted changes" }],
      worktrees: [row({ path: "/c", dirty: true })],
    });

    // Clear all: the captured paths go out with `bulk: true`.
    const all = captureClear(["/b", "/c"], true);
    expect(all.bulk).toBe(true);
    await clearWorktrees(all.paths, false, all.client, all.bulk);
    expect(request).toHaveBeenLastCalledWith(
      { type: "clear_worktrees", paths: ["/b", "/c"], force: false, bulk: true },
      { timeoutMs: 600_000 },
    );
    const outcome = getWorktreeState().outcome;
    expect(outcome?.bulk).toBe(true);

    // The forced retry of Clear all's skips keeps `bulk`.
    const retry = captureClear(["/c"], outcome?.bulk ?? false);
    await clearWorktrees(retry.paths, true, retry.client, retry.bulk);
    expect(request).toHaveBeenLastCalledWith(
      { type: "clear_worktrees", paths: ["/c"], force: true, bulk: true },
      { timeoutMs: 600_000 },
    );

    // A per-row Remove is never bulk.
    const one = captureClear(["/c"]);
    expect(one.bulk).toBe(false);
    await clearWorktrees(one.paths, false, one.client, one.bulk);
    expect(request).toHaveBeenLastCalledWith(
      { type: "clear_worktrees", paths: ["/c"], force: false, bulk: false },
      { timeoutMs: 600_000 },
    );
    expect(getWorktreeState().outcome?.bulk).toBe(false);
  });

  it("sends the explicit paths it is given, and force for a retry", async () => {
    request.mockResolvedValueOnce({ type: "worktrees", worktrees: [row({ path: "/a" })] });
    await loadWorktrees();
    request.mockResolvedValue({
      type: "worktrees_cleared",
      removed: [],
      skipped: [],
      worktrees: [],
    });
    await clearWorktrees(["/a"]);
    expect(request).toHaveBeenLastCalledWith(
      {
        type: "clear_worktrees",
        paths: ["/a"],
        force: false,
        bulk: false,
      },
      { timeoutMs: 600_000 },
    );
    await clearWorktrees(["/c"], true);
    expect(request).toHaveBeenLastCalledWith(
      {
        type: "clear_worktrees",
        paths: ["/c"],
        force: true,
        bulk: false,
      },
      { timeoutMs: 600_000 },
    );
  });

  it("resets the outcome when a clear starts and clears a previous read error", async () => {
    request.mockRejectedValueOnce(new Error("socket closed"));
    await loadWorktrees();
    expect(getWorktreeState().error).not.toBeNull();

    request.mockResolvedValueOnce({
      type: "worktrees_cleared",
      removed: ["/b"],
      skipped: [{ path: "/c", reason: "in use by Fix it" }],
      worktrees: [row({ path: "/c" })],
    });
    await clearWorktrees(["/b", "/c"]);
    expect(getWorktreeState().error).toBeNull();
    expect(getWorktreeState().outcome?.removed).toBe(1);

    let resolve!: (value: unknown) => void;
    request.mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    );
    const running = clearWorktrees(["/c"]);
    expect(getWorktreeState().outcome).toBeNull();
    expect(getWorktreeState().clearError).toBeNull();
    resolve({ type: "worktrees_cleared", removed: [], skipped: [], worktrees: [] });
    await running;
  });

  it("discards a list response from a client that was replaced", async () => {
    let resolve!: (value: unknown) => void;
    request.mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    );
    const loading = loadWorktrees();
    harness.current.value = harness.clientB;
    resolve({ type: "worktrees", worktrees: [row({ path: "/a" })] });
    await loading;

    const state = getWorktreeState();
    expect(state.rows).toEqual([]);
    expect(state.loaded).toBe(false);
    expect(state.source).toBeNull();
    expect(state.loading).toBe(false);
  });

  it("drops the old machine's rows when the new machine's first read fails", async () => {
    harness.clientA.request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [row({ path: "/a" })],
    });
    await loadWorktrees();
    expect(getWorktreeState().rows).toHaveLength(1);

    harness.current.value = harness.clientB;
    harness.clientB.request.mockRejectedValueOnce(new Error("offline"));
    await loadWorktrees();

    const state = getWorktreeState();
    expect(state.rows).toEqual([]);
    expect(state.error).not.toBeNull();
    expect(state.source).toBe(harness.clientB as unknown as H4rnClient);
  });

  it("discards a clear response from a client that was replaced", async () => {
    request.mockResolvedValueOnce({ type: "worktrees", worktrees: [row({ path: "/a" })] });
    await loadWorktrees();

    let resolve!: (value: unknown) => void;
    request.mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    );
    const clearing = clearWorktrees(["/a"]);
    harness.current.value = harness.clientB;
    resolve({ type: "worktrees_cleared", removed: ["/a"], skipped: [], worktrees: [] });
    await clearing;

    const state = getWorktreeState();
    expect(state.clearing).toBe(false);
    expect(state.rows.map((r) => r.path)).toEqual(["/a"]);
    expect(state.outcome).toBeNull();
  });

  it("will not clear with rows read through a replaced client", async () => {
    request.mockResolvedValueOnce({ type: "worktrees", worktrees: [row({ path: "/a" })] });
    await loadWorktrees();
    harness.current.value = harness.clientB;

    await clearWorktrees(["/a"]);
    expect(harness.clientB.request).not.toHaveBeenCalled();
    expect(getWorktreeState().clearError).not.toBeNull();
    expect(getWorktreeState().rows.map((r) => r.path)).toEqual(["/a"]);
  });

  it("refuses a captured confirmation after the client changes", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [row({ path: "/a" }), row({ path: "/b" })],
    });
    await loadWorktrees();
    const confirmation = captureClear(["/a", "/b"]);
    expect(confirmation.client).toBe(clientA);

    // The machine switch rebuilds the client while the dialog is open.
    harness.current.value = harness.clientB;
    await clearWorktrees(confirmation.paths, false, confirmation.client);

    expect(harness.clientB.request).not.toHaveBeenCalled();
    expect(request).toHaveBeenCalledTimes(1);
    expect(getWorktreeState().clearError).not.toBeNull();
  });

  it("does not offer one machine's outcome for a retry on another", async () => {
    request.mockResolvedValueOnce({
      type: "worktrees",
      worktrees: [row({ path: "/a" }), row({ path: "/b", dirty: true })],
    });
    await loadWorktrees();
    request.mockResolvedValueOnce({
      type: "worktrees_cleared",
      removed: ["/a"],
      skipped: [{ path: "/b", reason: "has uncommitted changes" }],
      worktrees: [row({ path: "/b", dirty: true })],
    });
    await clearWorktrees(["/a", "/b"]);

    expect(worktreeOutcomeForClient(getWorktreeState())).not.toBeNull();
    harness.current.value = harness.clientB;
    expect(worktreeOutcomeForClient(getWorktreeState())).toBeNull();
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
    await clearWorktrees([row().path]);
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

describe("worktreeClientCurrent", () => {
  it("only trusts rows whose client is still the one in use", () => {
    expect(worktreeClientCurrent(null)).toBe(false);
    expect(worktreeClientCurrent(clientA)).toBe(true);
    expect(worktreeClientCurrent(clientB)).toBe(false);
    harness.current.value = harness.clientB;
    expect(worktreeClientCurrent(clientB)).toBe(true);
    expect(worktreeClientCurrent(clientA)).toBe(false);
  });
});

describe("forceCanFix", () => {
  it("offers a forced retry only for work force can discard", () => {
    expect(forceCanFix("has uncommitted changes")).toBe(true);
    expect(forceCanFix("could not check for uncommitted changes")).toBe(true);
    expect(forceCanFix("branch h4rn/sa-x has unmerged commits")).toBe(true);
    expect(forceCanFix("checkout is no longer on h4rn/sa-x")).toBe(true);
    expect(forceCanFix("detached HEAD has commits on no branch")).toBe(true);
    expect(forceCanFix("could not check for nested checkouts")).toBe(true);
    expect(forceCanFix("branch h4rn/sa-x changed during the clear")).toBe(true);
    expect(forceCanFix("contains submodules")).toBe(true);
    expect(forceCanFix("could not read the registration's HEAD")).toBe(true);
    expect(forceCanFix("has files hidden from git status")).toBe(true);
    for (const blocked of [
      "in use by Fix it",
      "not tracked",
      "checkout removed, but branch h4rn/sa-x could not be deleted: busy",
      "contains another worktree at /code/api/.claude/worktrees/child",
      "contains another git checkout at /code/api/vendor/lib",
      "checkout removed, but branch h4rn/sa-x could not be checked: fatal: bad ref",
      "is the repository's main worktree",
      "is locked; unlock it first",
      "its repository could not be found",
      "git timed out after 30s",
      "registration left in place: fatal: not a working tree",
      "branch h4rn/sa-sym is a symbolic ref",
      "checkout removed, but branch h4rn/sa-x was left in place",
      "branch h4rn/sa-x could not be checked: warning: ignoring broken ref",
      "the checkout was recreated during the clear",
      "not a registered worktree",
      "is a thread workspace",
      "could not read every worktree registration",
      "checkout removed, but branch h4rn/sa-x could not be checked: the worktree list is incomplete",
    ]) {
      expect(forceCanFix(blocked), blocked).toBe(false);
    }
  });
});

describe("worktreeSubline", () => {
  it("keeps the branch wording for a removed subagent checkout", () => {
    const line = worktreeSubline(row({ exists: false }));
    expect(line).toContain("Directory removed, branch kept");
    expect(line).not.toContain("registration left");
  });

  it("never promises a branch the daemon reports gone", () => {
    const line = worktreeSubline(row({ exists: false, branch: null }));
    expect(line).toContain("Directory removed, registration left");
    expect(line).not.toContain("branch kept");
  });

  it("says the registration is left for a removed non-subagent checkout", () => {
    const line = worktreeSubline(row({ exists: false, origin: "harness", owned: false }));
    expect(line).toContain("Directory removed, registration left");
    expect(line).not.toContain("branch kept");
  });

  it("names the origin, its users and uncommitted work", () => {
    const line = worktreeSubline(
      row({
        origin: "thread",
        owned: false,
        dirty: true,
        accessed_by: [
          {
            thread_id: "th_a",
            title: "Fix it",
            subagent_id: null,
            working: false,
            last_seen: "2026-11-20T10:00:00Z",
          },
        ],
      }),
    );
    expect(line).toContain("Thread workspace");
    expect(line).toContain("Fix it");
    expect(line).toContain("Uncommitted changes");
  });
});

describe("removeDescription", () => {
  it("mentions unmerged work only for an owned branch", () => {
    const unowned = removeDescription(row({ owned: false, branch: "feature/x" }));
    expect(unowned).toContain("Uncommitted changes are skipped.");
    expect(unowned).not.toContain("unmerged work");

    const owned = removeDescription(row({ owned: true, branch: "h4rn/sa-aa" }));
    expect(owned).toContain("Uncommitted changes or unmerged work are skipped.");
  });

  it("says only the registration goes when no branch is left", () => {
    const text = removeDescription(row({ exists: false, branch: null }));
    expect(text).toContain("No checkout remains on disk; its registration is removed.");
    expect(text).not.toContain("branch");
  });

  it("says ignored files go with an existing checkout", () => {
    expect(removeDescription(row({ exists: true }))).toContain(
      "Ignored files such as build output are deleted with it.",
    );
  });
});

describe("worktreePage", () => {
  const failure = {
    code: "internal" as const,
    message: "The daemon did not answer.",
    hint: null,
  };

  it("keeps loaded rows and shows a failed refetch above them", () => {
    const page = worktreePage(true, [row({ path: "/a" })], failure);
    expect(page).toEqual({
      kind: "groups",
      notice: "The daemon did not answer.",
      empty: false,
    });
  });

  it("does not claim the list is empty when the read failed", () => {
    const page = worktreePage(true, [], failure);
    expect(page).toEqual({
      kind: "groups",
      notice: "The daemon did not answer.",
      empty: false,
    });
  });

  it("shows the empty copy only after a successful empty read", () => {
    expect(worktreePage(true, [], null)).toEqual({ kind: "groups", notice: null, empty: true });
    expect(worktreePage(false, [], null)).toEqual({ kind: "loading" });
  });
});

describe("shouldReloadWorktrees", () => {
  it("reloads on mount while online, with or without cached rows", () => {
    // The reconnect-tracking ref starts at `false`, so mounting is a change.
    expect(shouldReloadWorktrees(true, false, true)).toBe(true);
    expect(shouldReloadWorktrees(true, false, false)).toBe(true);
  });

  it("refetches when the connection comes back even if rows are loaded", () => {
    expect(shouldReloadWorktrees(true, true, true)).toBe(false);
    expect(shouldReloadWorktrees(false, true, true)).toBe(false);
  });

  it("refetches when the stored rows belong to a replaced client", () => {
    expect(shouldReloadWorktrees(true, true, false)).toBe(true);
    expect(shouldReloadWorktrees(false, true, false)).toBe(false);
  });
});
