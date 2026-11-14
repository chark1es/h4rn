import { describe, expect, test, vi } from "vitest";

import type {
  ForgeAccount,
  ForgeChecks,
  ForgeComment,
  ForgeCommits,
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
  ForgeApi,
  forgeLabel,
  logCheck,
  mergeableLabel,
  mergeDiffPages,
  parsePatch,
  removeComment,
  replaceComment,
  requiredMark,
  rerunAction,
  reviewChip,
  shortSha,
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

const gitHubChecks: ForgeChecks = {
  repo,
  number: 7,
  head_sha: "9f2c1d0e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d",
  state: "failing",
  total: 3,
  passed: 1,
  failed: 2,
  pending: 0,
  checks: [
    {
      name: "build",
      status: "success",
      details_url: "https://github.com/acme/widget/runs/1",
      description: null,
      started_at: null,
      completed_at: null,
      duration_ms: null,
      required: true,
    },
    {
      name: "test",
      status: "failure",
      details_url: "https://github.com/acme/widget/runs/2",
      description: null,
      started_at: null,
      completed_at: null,
      duration_ms: null,
      required: true,
    },
    {
      name: "lint",
      status: "failure",
      details_url: null,
      description: null,
      started_at: null,
      completed_at: null,
      duration_ms: null,
      required: null,
    },
  ],
  rerun: { supported: true, reason: null },
  updated_at: "2026-11-13T00:00:00Z",
};

describe("check actions", () => {
  test("marks only the checks the forge says are required", () => {
    const [build, test_, lint] = gitHubChecks.checks as [
      ForgeChecks["checks"][number],
      ForgeChecks["checks"][number],
      ForgeChecks["checks"][number],
    ];
    expect(requiredMark(build)).toBe("required");
    expect(requiredMark(test_)).toBe("required");
    // Unknown (`null`) is not shown as optional.
    expect(requiredMark(lint)).toBeNull();
    expect(requiredMark({ ...build, required: false })).toBeNull();
  });

  test("View log picks a failing check that has a link", () => {
    expect(logCheck(gitHubChecks)?.name).toBe("test");
    const passing: ForgeChecks = {
      ...gitHubChecks,
      checks: gitHubChecks.checks.map((check) => ({ ...check, status: "success" as const })),
    };
    expect(logCheck(passing)?.name).toBe("build");
    expect(logCheck({ ...gitHubChecks, checks: [] })).toBeNull();
  });

  test("Re-run failed is ready, idle or unsupported with the daemon's reason", () => {
    expect(rerunAction(gitHubChecks)).toEqual({ kind: "ready" });
    expect(rerunAction({ ...gitHubChecks, failed: 0 })).toEqual({
      kind: "nothing",
      reason: "No checks failed",
    });
    expect(
      rerunAction({
        ...gitHubChecks,
        rerun: { supported: false, reason: "Gitea posts CI results as commit statuses" },
      }),
    ).toEqual({ kind: "unsupported", reason: "Gitea posts CI results as commit statuses" });
    // A missing reason still gives the tooltip something to say.
    expect(
      rerunAction({ ...gitHubChecks, rerun: { supported: false, reason: null } }),
    ).toMatchObject({ kind: "unsupported" });
  });

  test("abbreviates a commit id to 7 characters", () => {
    expect(shortSha("5d1c0aa83e7f4b6c9d2a1e0f3b4c5d6e7f8a9b0c")).toBe("5d1c0aa");
  });
});

describe("ForgeApi commits and re-run", () => {
  const commits: ForgeCommits = {
    repo,
    number: 7,
    truncated: false,
    commits: [
      {
        sha: "1111111111111111111111111111111111111111",
        title: "Add a failing test",
        body: null,
        author: "Alice Example",
        author_login: "alice",
        authored_at: "2026-11-12T09:00:00Z",
        web_url: null,
      },
    ],
  };

  test("lists commits through the typed request", async () => {
    const request = vi.fn().mockResolvedValue({ type: "forge_commits", commits });
    const result = await new ForgeApi(request).commits(repo, 7);
    expect(request).toHaveBeenCalledWith({
      type: "forge_list_pr_commits",
      account_id: null,
      repo,
      number: 7,
    });
    expect(result.commits[0]?.title).toBe("Add a failing test");
  });

  test("re-runs failed checks and returns the refreshed snapshot", async () => {
    const refreshed = { ...gitHubChecks, state: "pending" as const, failed: 0, pending: 2 };
    const request = vi.fn().mockResolvedValue({ type: "forge_checks", checks: refreshed });
    const result = await new ForgeApi(request).rerunChecks(repo, 7);
    expect(request).toHaveBeenCalledWith({
      type: "forge_rerun_checks",
      account_id: null,
      repo,
      number: 7,
    });
    expect(result.state).toBe("pending");
  });

  test("rejects an unexpected response instead of guessing", async () => {
    const request = vi.fn().mockResolvedValue({ type: "ack" });
    await expect(new ForgeApi(request).commits(repo, 7)).rejects.toThrow("expected forge_commits");
  });
});
