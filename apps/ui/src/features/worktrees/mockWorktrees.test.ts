import { describe, expect, it } from "vitest";

import type { TrackedWorktree } from "@/gen";
import { clearMockWorktrees } from "@/lib/client/mock";

function row(path: string, overrides: Partial<TrackedWorktree> = {}): TrackedWorktree {
  return {
    path,
    repo: "/code/api",
    branch: null,
    origin: "harness",
    status: "settled",
    exists: true,
    dirty: false,
    owned: false,
    subagent_id: null,
    task: null,
    accessed_by: [],
    first_seen: "2026-11-20T10:00:00Z",
    last_seen: "2026-11-20T10:30:00Z",
    ...overrides,
  };
}

describe("clearMockWorktrees", () => {
  const rows = [
    row("/wt/agent-a"),
    row("/wt/agent-b"),
    row("/wt/agent-bc"),
    row("/wt/thread", { origin: "thread" }),
    row("/wt/busy", { status: "active" }),
  ];
  const workspaces = ["/wt/agent-b/packages/web"];

  it("skips thread workspaces, by origin or by a workspace inside, in Clear all", () => {
    const all = clearMockWorktrees(rows, null, false, false, workspaces);
    expect(all.removed).toEqual(["/wt/agent-a", "/wt/agent-bc"]);
    expect(all.skipped).toEqual([
      { path: "/wt/agent-b", reason: "is a thread workspace" },
      { path: "/wt/thread", reason: "is a thread workspace" },
    ]);

    const bulk = clearMockWorktrees(rows, ["/wt/agent-a", "/wt/agent-b"], true, true, workspaces);
    expect(bulk.removed).toEqual(["/wt/agent-a"]);
    expect(bulk.skipped).toEqual([{ path: "/wt/agent-b", reason: "is a thread workspace" }]);
  });

  it("compares Windows paths by whole components too", () => {
    const windows = [row("C:\\wt\\agent-a"), row("C:\\wt\\agent-ab")];
    const all = clearMockWorktrees(windows, null, false, false, ["C:\\wt\\agent-a\\src"]);
    expect(all.removed).toEqual(["C:\\wt\\agent-ab"]);
    expect(all.skipped).toEqual([{ path: "C:\\wt\\agent-a", reason: "is a thread workspace" }]);
  });

  it("removes a thread workspace named on its own", () => {
    const one = clearMockWorktrees(rows, ["/wt/agent-b", "/wt/thread"], false, false, workspaces);
    expect(one.removed).toEqual(["/wt/agent-b", "/wt/thread"]);
    expect(one.skipped).toEqual([]);
  });
});
