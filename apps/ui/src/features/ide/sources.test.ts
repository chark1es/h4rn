import { describe, expect, it } from "vitest";

import type { FsDiffCommitList, FsDiffFiles } from "@/gen";

import {
  againstForScope,
  baseOptions,
  chooseCommit,
  commitLabel,
  firstDiffPath,
  sourceDescription,
} from "./sources";

describe("diff sources", () => {
  it("turns each scope into the daemon's DiffAgainst", () => {
    expect(
      againstForScope({ source: "head", threadId: null, turnId: null, base: null, commit: null }),
    ).toEqual({ kind: "head" });
    expect(
      againstForScope({
        source: "agent",
        threadId: "th_1" as never,
        turnId: "turn_1" as never,
        base: null,
        commit: null,
      }),
    ).toEqual({ kind: "agent_turn", thread_id: "th_1", turn_id: "turn_1" });
    expect(
      againstForScope({
        source: "branch",
        threadId: null,
        turnId: null,
        base: "origin/main",
        commit: null,
      }),
    ).toEqual({ kind: "base", base: "origin/main" });
    expect(
      againstForScope({
        source: "commit",
        threadId: null,
        turnId: null,
        base: null,
        commit: "1a2b3c4",
      }),
    ).toEqual({ kind: "commit", sha: "1a2b3c4" });
  });

  it("refuses a scope the workspace cannot serve", () => {
    expect(
      againstForScope({ source: "agent", threadId: null, turnId: null, base: null, commit: null }),
    ).toBeNull();
    expect(
      againstForScope({ source: "commit", threadId: null, turnId: null, base: null, commit: null }),
    ).toBeNull();
  });

  it("names the source the way a review comment should carry it", () => {
    expect(sourceDescription("head", "HEAD")).toBe("uncommitted (HEAD)");
    expect(sourceDescription("agent", "Turn 4")).toBe("agent turn");
    expect(sourceDescription("branch", "vs main")).toBe("branch vs main");
    expect(sourceDescription("commit", "Commit 1a2b3c4")).toBe("commit 1a2b3c4");
  });

  it("keeps the open file when it is still in the range", () => {
    const files: FsDiffFiles = {
      against: { kind: "base", base: "main" },
      label: "vs main",
      base: "main",
      merge_base: "abc",
      truncated: false,
      note: null,
      files: [
        {
          path: "a.ts",
          old_path: null,
          status: "modified" as const,
          added: 1,
          removed: 0,
          binary: false,
        },
        {
          path: "b.ts",
          old_path: null,
          status: "added" as const,
          added: 2,
          removed: 0,
          binary: false,
        },
      ],
    };
    expect(firstDiffPath(files, "b.ts")).toBe("b.ts");
    expect(firstDiffPath(files, "gone.ts")).toBe("a.ts");
    expect(firstDiffPath(null, "b.ts")).toBeNull();
  });

  it("chooses the newest commit when none is open", () => {
    const list = {
      commits: [
        { sha: "new", short_sha: "new", subject: "newest", author: "a", authored_at: "" },
        { sha: "old", short_sha: "old", subject: "older", author: "a", authored_at: "" },
      ],
    } as unknown as FsDiffCommitList;
    expect(chooseCommit(list, null)).toBe("new");
    expect(chooseCommit(list, "old")).toBe("old");
    expect(chooseCommit(null, "old")).toBe("old");
    expect(chooseCommit({ commits: [] }, null)).toBeNull();
  });

  it("keeps a commit label to one quiet line", () => {
    expect(commitLabel({ short_sha: "1a2b3c4", subject: "Fix the flaky test" })).toBe(
      "1a2b3c4 Fix the flaky test",
    );
    expect(commitLabel({ short_sha: "1a2b3c4", subject: "  " })).toBe("1a2b3c4");
  });

  it("keeps the current base visible even before the branch list loads", () => {
    expect(baseOptions(["main", "feature"], "origin/main")[0]).toEqual({
      value: "origin/main",
      label: "origin/main",
      hint: "current base",
    });
    expect(baseOptions(["main"], "main")).toEqual([{ value: "main", label: "main" }]);
  });
});
