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
  linkedPullLabel,
  logCheck,
  mergeableLabel,
  mergeDiffPages,
  parsePatch,
  parsePullRequestUrl,
  prWebUrl,
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

  test("matches an Azure DevOps DefaultCollection alias in both directions", () => {
    const azure = (baseUrl: string): ForgeAccount => ({
      ...account,
      forge: "azure_devops",
      base_url: baseUrl,
    });
    const azureRepo = (baseUrl: string): ForgeRepoRef => ({
      forge: "azure_devops",
      base_url: baseUrl,
      owner: "platform",
      repo: "widget",
    });

    expect(
      accountForRepo(
        [azure("https://acme.visualstudio.com/DefaultCollection")],
        azureRepo("https://acme.visualstudio.com"),
      )?.base_url,
    ).toBe("https://acme.visualstudio.com/DefaultCollection");
    expect(
      accountForRepo(
        [azure("https://acme.visualstudio.com")],
        azureRepo("https://acme.visualstudio.com/DefaultCollection/"),
      )?.base_url,
    ).toBe("https://acme.visualstudio.com");
  });

  test("compares the Azure DevOps host case-insensitively", () => {
    const azure: ForgeAccount = {
      ...account,
      forge: "azure_devops",
      base_url: "https://ACME.VisualStudio.com/DefaultCollection",
    };
    expect(
      accountForRepo([azure], {
        forge: "azure_devops",
        base_url: "https://acme.visualstudio.com",
        owner: "platform",
        repo: "widget",
      })?.account_id,
    ).toBe("a1");
  });

  test("matches a lowercase DefaultCollection suffix", () => {
    const azure: ForgeAccount = {
      ...account,
      forge: "azure_devops",
      base_url: "https://acme.visualstudio.com",
    };
    expect(
      accountForRepo([azure], {
        forge: "azure_devops",
        base_url: "https://acme.visualstudio.com/defaultcollection/",
        owner: "platform",
        repo: "widget",
      })?.account_id,
    ).toBe("a1");
  });

  test("keeps DefaultCollection distinct for a non-Visual Studio host", () => {
    const azure: ForgeAccount = {
      ...account,
      forge: "azure_devops",
      base_url: "https://ado.example.test/DefaultCollection",
    };
    expect(
      accountForRepo([azure], {
        forge: "azure_devops",
        base_url: "https://ado.example.test",
        owner: "platform",
        repo: "widget",
      }),
    ).toBeUndefined();
  });
});

describe("labels", () => {
  test("names every forge", () => {
    expect(
      (["github", "gitlab", "gitea", "forgejo", "bitbucket", "azure_devops"] as const).map(
        forgeLabel,
      ),
    ).toEqual(["GitHub", "GitLab", "Gitea", "Forgejo", "Bitbucket Cloud", "Azure DevOps"]);
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

  test("adds an account with the detected repository hint", async () => {
    const bitbucketRepo: ForgeRepoRef = {
      forge: "bitbucket",
      base_url: "https://bitbucket.org",
      owner: "acme",
      repo: "widget",
    };
    const saved: ForgeAccount = {
      ...account,
      account_id: "a2",
      forge: "bitbucket",
      base_url: "https://bitbucket.org",
      username: null,
      display_name: null,
      scopes: [],
      source: "bitbucket_access_token",
    };
    const request = vi.fn().mockResolvedValue({ type: "forge_account", account: saved });
    const result = await new ForgeApi(request).addAccount(
      "bitbucket",
      "https://bitbucket.org",
      "ATBB-token",
      null,
      bitbucketRepo,
    );
    expect(request).toHaveBeenCalledWith({
      type: "forge_add_account",
      forge: "bitbucket",
      base_url: "https://bitbucket.org",
      token: "ATBB-token",
      username: null,
      source: "pat",
      repository: bitbucketRepo,
    });
    expect(result.account_id).toBe("a2");
  });

  test("rejects an unexpected response instead of guessing", async () => {
    const request = vi.fn().mockResolvedValue({ type: "ack" });
    await expect(new ForgeApi(request).commits(repo, 7)).rejects.toThrow("expected forge_commits");
  });
});

describe("pull request links", () => {
  test("prWebUrl follows each forge's route shape", () => {
    expect(prWebUrl(repo, 12)).toBe("https://github.com/h4rn/api/pull/12");
    expect(prWebUrl({ ...repo, forge: "gitlab" }, 12)).toBe(
      "https://github.com/h4rn/api/-/merge_requests/12",
    );
    expect(prWebUrl({ ...repo, forge: "forgejo" }, 12)).toBe(
      "https://github.com/h4rn/api/pulls/12",
    );
  });

  test("a GitHub pull URL names its repository and number", () => {
    expect(parsePullRequestUrl("https://github.com/h4rn/api/pull/42/files")).toEqual({
      repo,
      number: 42,
    });
  });

  test("a GitLab merge request keeps nested groups", () => {
    expect(parsePullRequestUrl("https://gitlab.com/team/platform/api/-/merge_requests/9")).toEqual({
      repo: {
        forge: "gitlab",
        base_url: "https://gitlab.com",
        owner: "team/platform",
        repo: "api",
      },
      number: 9,
    });
  });

  test("a Gitea pull URL uses the pulls route", () => {
    expect(parsePullRequestUrl("https://codeberg.org/owner/repo/pulls/3")).toEqual({
      repo: {
        forge: "forgejo",
        base_url: "https://codeberg.org",
        owner: "owner",
        repo: "repo",
      },
      number: 3,
    });
  });

  test("a custom host is only accepted when a detected repository names it", () => {
    expect(parsePullRequestUrl("https://ghe.example.test/h4rn/api/pull/5")).toBeNull();
    expect(
      parsePullRequestUrl("https://ghe.example.test/h4rn/api/pull/5", [
        {
          forge: "github",
          base_url: "https://ghe.example.test",
          owner: "h4rn",
          repo: "api",
        },
      ]),
    ).toEqual({
      repo: {
        forge: "github",
        base_url: "https://ghe.example.test",
        owner: "h4rn",
        repo: "api",
      },
      number: 5,
    });
  });

  test("unrelated and malformed URLs are refused", () => {
    expect(parsePullRequestUrl("https://github.com/h4rn/api/issues/5")).toBeNull();
    expect(parsePullRequestUrl("not a url")).toBeNull();
    expect(parsePullRequestUrl("ftp://github.com/h4rn/api/pull/5")).toBeNull();
  });

  test("the badge label names the number and state", () => {
    const summary: ForgePrSummary = {
      repo,
      number: 12,
      title: "Add watching",
      state: "open",
      author: "alice",
      head_branch: "h4rn/watch",
      base_branch: "main",
      head_sha: "abc",
      updated_at: "2026-01-01T00:00:00Z",
      web_url: "https://github.com/h4rn/api/pull/12",
      checks: "pending",
      review: "pending",
      draft: false,
    };
    expect(
      linkedPullLabel({
        repo,
        number: 12,
        account_id: "a1",
        summary,
        error: null,
        watching: true,
        watch_notice: null,
        linked_at: "2026-01-01T00:00:00Z",
      }),
    ).toBe("#12 Open");
  });
});

describe("Bitbucket and Azure DevOps pull request links", () => {
  test("a Bitbucket Cloud pull request URL", () => {
    expect(parsePullRequestUrl("https://bitbucket.org/acme/widget/pull-requests/12")).toEqual({
      repo: {
        forge: "bitbucket",
        base_url: "https://bitbucket.org",
        owner: "acme",
        repo: "widget",
      },
      number: 12,
    });
  });

  test("an Azure DevOps pull request URL keeps the organization in the base", () => {
    expect(
      parsePullRequestUrl("https://dev.azure.com/acme/My%20Project/_git/api/pullrequest/7"),
    ).toEqual({
      repo: {
        forge: "azure_devops",
        base_url: "https://dev.azure.com/acme",
        owner: "My Project",
        repo: "api",
      },
      number: 7,
    });
  });

  test("a legacy Azure DevOps organization URL", () => {
    expect(
      parsePullRequestUrl("https://acme.visualstudio.com/Project/_git/api/pullrequest/7"),
    ).toEqual({
      repo: {
        forge: "azure_devops",
        base_url: "https://acme.visualstudio.com",
        owner: "Project",
        repo: "api",
      },
      number: 7,
    });
  });

  test("prWebUrl uses the provider route for both", () => {
    expect(
      prWebUrl(
        { forge: "bitbucket", base_url: "https://bitbucket.org", owner: "acme", repo: "widget" },
        12,
      ),
    ).toBe("https://bitbucket.org/acme/widget/pull-requests/12");
    expect(
      prWebUrl(
        {
          forge: "azure_devops",
          base_url: "https://dev.azure.com/acme",
          owner: "Project",
          repo: "api",
        },
        7,
      ),
    ).toBe("https://dev.azure.com/acme/Project/_git/api/pullrequest/7");
  });
});
