import { describe, expect, test } from "vitest";

import type { ClientRequest, FsDiff, FsTurnFile, ResponseBody, ThreadSummary } from "@/gen";

import {
  againstFor,
  latestTurn,
  loadTurnChanges,
  revertFile,
  revertFiles,
  totalLines,
} from "./store";

const thread = {
  thread_id: "thr_1",
  host: "atlas",
  workspace_path: "/home/you/code/api",
  title: "Fix the test",
  harness: "claude_code",
  model: "claude-sonnet-5-5",
} as ThreadSummary;

const file: FsTurnFile = {
  path: "tests/setup.ts",
  added: 4,
  removed: 1,
  before_etag: "before",
  after_etag: "after",
};

const modified: FsDiff = {
  path: "tests/setup.ts",
  against: { kind: "head" },
  label: "HEAD",
  before: "old\n",
  after: "new\n",
  binary: false,
  too_large: false,
  note: null,
};

describe("touch counting", () => {
  test("sums added and removed across files", () => {
    expect(totalLines([file, { ...file, path: "b", added: 2, removed: 3 }])).toEqual({
      added: 6,
      removed: 4,
    });
  });

  test("an empty change set is zero", () => {
    expect(totalLines([])).toEqual({ added: 0, removed: 0 });
  });
});

describe("turn selection", () => {
  test("defaults to the latest turn", () => {
    expect(
      latestTurn([
        { id: "a", label: "Turn 1" },
        { id: "b", label: "Turn 2" },
      ]),
    ).toBe("b");
  });

  test("has no selection when there are no turns", () => {
    expect(latestTurn([])).toBeNull();
  });
});

describe("comparison scope", () => {
  test("turns into the daemon's DiffAgainst shape", () => {
    expect(againstFor("head", thread, "turn_1")).toEqual({ kind: "head" });
    expect(againstFor("agent", thread, "turn_1")).toEqual({
      kind: "agent_turn",
      thread_id: "thr_1",
      turn_id: "turn_1",
    });
  });
});

describe("loadTurnChanges", () => {
  test("names the thread and turn", async () => {
    const seen: ClientRequest[] = [];
    const response: ResponseBody = {
      type: "fs_turn_changes",
      changes: { thread_id: "thr_1", turn_id: "turn_1", files: [file], note: null },
    };
    const changes = await loadTurnChanges(
      async (request) => {
        seen.push(request);
        return response;
      },
      thread,
      "turn_1",
    );
    expect(changes.files).toHaveLength(1);
    expect(seen[0]).toMatchObject({
      type: "fs_turn_changes",
      thread_id: "thr_1",
      turn_id: "turn_1",
      workspace: "/home/you/code/api",
    });
  });

  test("rejects a mismatched response", async () => {
    await expect(loadTurnChanges(async () => ({ type: "ack" }), thread, "turn_1")).rejects.toThrow(
      "did not return a change set",
    );
  });
});

describe("revertFile", () => {
  test("writes the pre-turn bytes with the current etag as the base", async () => {
    const seen: ClientRequest[] = [];
    const result = await revertFile(
      async (request) => {
        seen.push(request);
        return {
          type: "fs_written",
          result: {
            path: "tests/setup.ts",
            etag: "new",
            size_bytes: 4,
            modified_at: null,
            conflict: null,
          },
        };
      },
      thread,
      file,
      modified,
      { kind: "head" },
    );
    expect(result.outcome).toBe("reverted");
    expect(seen[0]).toMatchObject({
      type: "fs_write",
      path: "tests/setup.ts",
      content: "old\n",
      base_etag: "after",
    });
  });

  test("deletes a file the turn created", async () => {
    const seen: ClientRequest[] = [];
    const result = await revertFile(
      async (request) => {
        seen.push(request);
        return {
          type: "fs_deleted",
          result: { path: "tests/new.ts", trash_id: "t1", trashed_to: "/tmp" },
        };
      },
      thread,
      { ...file, path: "tests/new.ts", before_etag: null },
      { ...modified, path: "tests/new.ts", before: null },
      { kind: "agent_turn", thread_id: "thr_1", turn_id: "turn_1" },
    );
    expect(result.outcome).toBe("deleted");
    expect(seen[0]).toMatchObject({ type: "fs_delete", path: "tests/new.ts" });
  });

  test("reports a write conflict instead of pretending the revert worked", async () => {
    const result = await revertFile(
      async () => ({
        type: "fs_written",
        result: {
          path: "tests/setup.ts",
          etag: "moved",
          size_bytes: 4,
          modified_at: null,
          conflict: {
            expected_etag: "after",
            current_etag: "moved",
            current_content: "newer\n",
            current_encoding: "utf-8",
          },
        },
      }),
      thread,
      file,
      modified,
      { kind: "head" },
    );
    expect(result.outcome).toBe("failed");
    expect(result.message).toContain("changed");
  });

  test("refuses to revert a binary diff", async () => {
    const seen: ClientRequest[] = [];
    const result = await revertFile(
      async (request) => {
        seen.push(request);
        return { type: "ack" };
      },
      thread,
      file,
      { ...modified, binary: true },
      { kind: "head" },
    );
    expect(result.outcome).toBe("skipped");
    expect(seen).toHaveLength(0);
  });

  test("fetches a diff when the caller has not loaded one", async () => {
    const seen: ClientRequest[] = [];
    const result = await revertFile(
      async (request) => {
        seen.push(request);
        return request.type === "fs_diff"
          ? { type: "fs_diff", diff: modified }
          : {
              type: "fs_written",
              result: {
                path: "tests/setup.ts",
                etag: "x",
                size_bytes: 4,
                modified_at: null,
                conflict: null,
              },
            };
      },
      thread,
      { ...file, before_etag: "before" },
      null,
      { kind: "head" },
    );
    expect(seen[0]?.type).toBe("fs_diff");
    expect(result.outcome).toBe("reverted");
  });

  test("keeps going when one file fails", async () => {
    const results = await revertFiles(
      async (request) =>
        request.type === "fs_write" && request.path === "tests/setup.ts"
          ? { type: "ack" }
          : {
              type: "fs_written",
              result: {
                path: "b",
                etag: "x",
                size_bytes: 1,
                modified_at: null,
                conflict: null,
              },
            },
      thread,
      [file, { ...file, path: "b" }],
      { "tests/setup.ts": modified, b: { ...modified, path: "b" } },
      { kind: "head" },
    );
    expect(results.map((item) => item.outcome)).toEqual(["failed", "reverted"]);
  });
});
