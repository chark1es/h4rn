import { describe, expect, test } from "vitest";

import type {
  ForgeAccount,
  ForgeComment,
  ForgeDiffPage,
  ForgeFileDiff,
  ForgePrSummary,
  ForgeRepoRef,
  ForgeReviewComment,
  ForgeReviewThread,
} from "@/gen";

import {
  accountForRepo,
  appendComment,
  checksLabel,
  forgeLabel,
  mergeableLabel,
  mergeDiffPages,
  parsePatch,
  removeComment,
  replaceComment,
  reviewChip,
  stateLabel,
  summaryChecksLabel,
  threadForLine,
} from "./store";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

const account: ForgeAccount = {
  account_id: "a1",
  forge: "github",
  base_url: "https://github.com/",
  username: "you",
  display_name: null,
  scopes: ["repo"],
  source: "pat",
  created_at: "2026-01-01T00:00:00Z",
};

describe("optimistic comments", () => {
  const first: ForgeComment = {
    comment_id: "c1",
    author: "you",
    body: "one",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: null,
    web_url: null,
  };

  test("append ignores a duplicate id", () => {
    const once = appendComment([], first);
    expect(appendComment(once, first)).toHaveLength(1);
  });

  test("replace swaps the temporary entry in place", () => {
    const temporary = { ...first, comment_id: "tmp_1" };
    const updated = replaceComment([first, temporary], "tmp_1", {
      ...first,
      comment_id: "c2",
    });
    expect(updated.map((item) => item.comment_id)).toEqual(["c1", "c2"]);
  });

  test("remove drops only the failed id", () => {
    const temporary = { ...first, comment_id: "tmp_1" };
    expect(removeComment([first, temporary], "tmp_1")).toEqual([first]);
  });
});

describe("diff paging", () => {
  const file = (path: string): ForgeFileDiff => ({
    path,
    previous_path: null,
    status: "modified",
    additions: 1,
    deletions: 0,
    patch: null,
    binary: false,
    truncated: false,
  });

  test("appends a page without repeating a loaded file", () => {
    const page: ForgeDiffPage = {
      repo,
      number: 1,
      files: [file("a.ts"), file("b.ts")],
      page: 2,
      per_page: 2,
      has_more: false,
    };
    expect(mergeDiffPages([file("a.ts")], page).map((item) => item.path)).toEqual(["a.ts", "b.ts"]);
  });
});

describe("account matching", () => {
  test("ignores a trailing slash on the host", () => {
    expect(accountForRepo([account], repo)?.account_id).toBe("a1");
  });

  test("does not match another forge on the same host", () => {
    expect(accountForRepo([account], { ...repo, forge: "gitlab" })).toBeUndefined();
  });

  test("does not match a self-hosted account", () => {
    expect(
      accountForRepo([account], { ...repo, base_url: "https://ghe.example.com" }),
    ).toBeUndefined();
  });
});

describe("labels", () => {
  test("names every forge", () => {
    expect((["github", "gitlab", "gitea", "forgejo"] as const).map(forgeLabel)).toEqual([
      "GitHub",
      "GitLab",
      "Gitea",
      "Forgejo",
    ]);
  });

  test("maps state to a tone", () => {
    expect(stateLabel("open")).toEqual({ label: "Open", tone: "ok" });
    expect(stateLabel("closed")).toEqual({ label: "Closed", tone: "danger" });
  });

  test("summarises checks", () => {
    expect(checksLabel("passing", 4, 5)).toBe("4 of 5 checks passed");
    expect(checksLabel("failing", 4, 5)).toBe("1 failing");
    expect(checksLabel("pending", 0, 0)).toBe("Checks running");
    expect(checksLabel("unknown", 0, 0)).toBe("No checks");
  });

  test("a list row never invents check counts", () => {
    expect(summaryChecksLabel("passing")).toBe("All checks passed");
    expect(summaryChecksLabel("failing")).toBe("Checks failing");
    expect(summaryChecksLabel("pending")).toBe("Checks running");
    expect(summaryChecksLabel("unknown")).toBeNull();
  });

  test("review chips match the design's words", () => {
    expect(reviewChip("changes_requested")).toEqual({
      label: "Changes requested",
      tone: "warn",
    });
    expect(reviewChip("pending")?.label).toBe("Review requested");
    expect(reviewChip("dismissed")).toBeNull();
  });

  test("mergeability says nothing when the forge cannot tell", () => {
    expect(mergeableLabel("conflicting")).toEqual({ label: "Merge conflicts", tone: "danger" });
    expect(mergeableLabel("unknown")).toBeNull();
  });
});

describe("patch parsing", () => {
  test("numbers both sides and skips the git envelope", () => {
    const rows = parsePatch(
      [
        "diff --git a/a.ts b/a.ts",
        "--- a/a.ts",
        "+++ b/a.ts",
        "@@ -1,2 +1,3 @@",
        " keep",
        "-old",
        "+new",
        "+extra",
      ].join("\n"),
    );
    expect(rows.map((row) => row.kind)).toEqual(["hunk", "context", "del", "add", "add"]);
    expect(rows[1]).toMatchObject({ oldLine: 1, newLine: 1 });
    expect(rows[2]).toMatchObject({ oldLine: 2, newLine: null });
    expect(rows[4]).toMatchObject({ oldLine: null, newLine: 3 });
  });

  test("an absent patch is an empty list", () => {
    expect(parsePatch(null)).toEqual([]);
  });
});

describe("inline threads", () => {
  const thread: ForgeReviewThread = {
    thread_id: "rt1",
    path: "a.ts",
    line: 4,
    side: "right",
    resolved: false,
    outdated: false,
    comments: [] as ForgeReviewComment[],
  };

  test("finds the thread anchored to a line", () => {
    expect(threadForLine([thread], "a.ts", 4)?.thread_id).toBe("rt1");
    expect(threadForLine([thread], "a.ts", 5)).toBeUndefined();
  });

  test("a null line matches the file's thread", () => {
    expect(threadForLine([thread], "a.ts", null)?.thread_id).toBe("rt1");
  });
});

describe("PR rows", () => {
  test("the summary type is usable as a row", () => {
    const row: ForgePrSummary = {
      repo,
      number: 1,
      title: "One",
      state: "draft",
      author: "you",
      head_branch: "head",
      base_branch: "main",
      head_sha: null,
      updated_at: "2026-01-01T00:00:00Z",
      web_url: "https://github.com/h4rn/api/pull/1",
      checks: "unknown",
      review: "pending",
      draft: true,
    };
    expect(stateLabel(row.state).label).toBe("Draft");
  });
});
