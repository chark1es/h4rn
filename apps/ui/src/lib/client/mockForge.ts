import type {
  ClientRequest,
  ForgeAccount,
  ForgeCapabilityEntry,
  ForgeChecks,
  ForgeComment,
  ForgeCommentPage,
  ForgeDiffPage,
  ForgeFileDiff,
  ForgeFileStatus,
  ForgeHandoff,
  ForgePrFilter,
  ForgePrSummary,
  ForgePullRequest,
  ForgeRepoRef,
  ForgeReview,
  ForgeReviewComment,
  ForgeReviewThread,
  ProtoError,
  ResponseBody,
  ThreadId,
} from "@/gen";

type MockHooks = {
  ok: (body: ResponseBody) => void;
  err: (code: ProtoError["code"], message: string, hint: string | null) => void;
  later: (ms: number, fn: () => void) => void;
  transient: (event: import("@/gen").Event) => void;
};

const REPO: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

const now = (offsetMs = 0) => new Date(Date.now() + offsetMs).toISOString();

const CAPABILITIES: ForgeCapabilityEntry[] = [
  {
    forge: "github",
    capabilities: {
      device_flow: true,
      review_threads: true,
      resolve_threads: true,
      nested_replies: true,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: true,
      graphql: true,
      rerun_checks: true,
      conditional_poll: true,
    },
  },
  {
    forge: "gitlab",
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: true,
      nested_replies: true,
      inline_review_comments: true,
      checks: false,
      pipelines: true,
      diff_pagination: true,
      graphql: false,
      rerun_checks: true,
      conditional_poll: false,
    },
  },
  {
    forge: "gitea",
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: true,
      nested_replies: false,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: false,
      graphql: false,
      rerun_checks: false,
      conditional_poll: false,
    },
  },
  {
    forge: "forgejo",
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: true,
      nested_replies: false,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: false,
      graphql: false,
      rerun_checks: false,
      conditional_poll: false,
    },
  },
  {
    forge: "bitbucket",
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: false,
      nested_replies: true,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: false,
      graphql: false,
      rerun_checks: false,
      conditional_poll: false,
    },
  },
  {
    forge: "azure_devops",
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: false,
      nested_replies: true,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: false,
      graphql: false,
      rerun_checks: false,
      conditional_poll: false,
    },
  },
];

const SUMMARIES: ForgePrSummary[] = [
  {
    repo: REPO,
    number: 482,
    title: "Fix flaky auth test",
    state: "open",
    author: "you",
    head_branch: "h4rn/fix-flaky-auth",
    base_branch: "main",
    head_sha: "9f2c1a4d7e3b5c60718293a4b5c6d7e8f9012345",
    updated_at: now(-12 * 60_000),
    web_url: "https://github.com/h4rn/api/pull/482",
    checks: "failing",
    review: "changes_requested",
    draft: false,
  },
  {
    repo: REPO,
    number: 479,
    title: "Relay retry with backoff",
    state: "open",
    author: "you",
    head_branch: "h4rn/relay-backoff",
    base_branch: "main",
    head_sha: "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
    updated_at: now(-40 * 60_000),
    web_url: "https://github.com/h4rn/api/pull/479",
    checks: "pending",
    review: "pending",
    draft: false,
  },
  {
    repo: REPO,
    number: 476,
    title: "Audit dependency tree",
    state: "open",
    author: "mara-k",
    head_branch: "mara/audit-deps",
    base_branch: "main",
    head_sha: "5e6f7a8b9c0d1e2f3a4b5c6d7e8f901a2b3c4d5e",
    updated_at: now(-3 * 3_600_000),
    web_url: "https://github.com/h4rn/api/pull/476",
    checks: "passing",
    review: "pending",
    draft: false,
  },
];

const FILE_DIFFS: ForgeFileDiff[] = [
  {
    path: "tests/setup.ts",
    previous_path: null,
    status: "modified" as ForgeFileStatus,
    additions: 4,
    deletions: 1,
    patch: [
      '@@ -7,7 +7,10 @@ import { afterAll, beforeAll, vi } from "vitest";',
      ' import { closePool, createPool } from "../src/db/pool";',
      " ",
      "-const pool = createPool();",
      "+// The clock is installed before the pool opens so the first query cannot",
      "+// race the fake timer on slower CI runners.",
      '+const clock = vi.useFakeTimers({ now: new Date("2026-01-01T00:00:00Z") });',
      "+const pool = createPool({ now: () => clock.now() });",
      " ",
      " beforeAll(async () => {",
      "   await pool.migrate();",
      "@@ -20,6 +23,7 @@ beforeAll(async () => {",
      " afterAll(async () => {",
      "+  await closePool(pool);",
      "   vi.useRealTimers();",
      " });",
    ].join("\n"),
    binary: false,
    truncated: false,
  },
  {
    path: "src/auth/session.test.ts",
    previous_path: null,
    status: "modified" as ForgeFileStatus,
    additions: 12,
    deletions: 2,
    patch: [
      '@@ -38,9 +38,19 @@ describe("verifySession", () => {',
      '   it("expires at the boundary", async () => {',
      "-    await clock.tick(30_000);",
      "+    await vi.advanceTimersByTimeAsync(30_000);",
      '     await expect(verifySession(token)).rejects.toThrow("expired");',
      "   });",
      "+",
      '+  it("does not expire one millisecond early", async () => {',
      "+    await vi.advanceTimersByTimeAsync(29_999);",
      "+    await expect(verifySession(token)).resolves.toBeTruthy();",
      "+  });",
      "+",
      '+  it("uses the injected clock", () => {',
      "+    expect(pool.now()).toBe(0);",
      "+  });",
      " });",
    ].join("\n"),
    binary: false,
    truncated: false,
  },
  {
    path: "src/db/pool.ts",
    previous_path: null,
    status: "modified" as ForgeFileStatus,
    additions: 6,
    deletions: 4,
    patch: [
      "@@ -22,10 +22,12 @@ type PoolOptions = {",
      "   size?: number;",
      "+  now?: () => number;",
      " };",
      " ",
      "-export function createPool(): Pool {",
      "-  const openedAt = Date.now();",
      "+export function createPool(options: PoolOptions = {}): Pool {",
      "+  const clock = options.now ?? Date.now;",
      "+  const openedAt = clock();",
      "   return new Pool({",
    ].join("\n"),
    binary: false,
    truncated: false,
  },
];

const GENERAL_COMMENTS: Record<number, ForgeComment[]> = {
  482: [
    {
      comment_id: "c_482_1",
      author: "mara-k",
      body: "The pool is never closed, so the e2e job leaks a connection per file. Close it in `afterAll` and the e2e check should go green.",
      created_at: now(-24 * 60_000),
      updated_at: null,
      web_url: null,
    },
    {
      comment_id: "c_482_2",
      author: "you",
      body: "That was it. The pool now closes in `afterAll` and the clock is installed before it opens.",
      created_at: now(-10 * 60_000),
      updated_at: null,
      web_url: null,
    },
  ],
  479: [],
  476: [],
};

const THREADS: Record<number, ForgeReviewThread[]> = {
  482: [
    {
      thread_id: "rt_482_1",
      path: "tests/setup.ts",
      line: 10,
      side: "right",
      resolved: false,
      outdated: false,
      comments: [
        {
          comment_id: "rc_482_1",
          thread_id: "rt_482_1",
          author: "mara-k",
          body: "This pool is never closed, so the e2e job leaks a connection per file. Close it in `afterAll`.",
          created_at: now(-26 * 60_000),
          updated_at: null,
          web_url: null,
          path: "tests/setup.ts",
          line: 10,
          side: "right",
          in_reply_to: null,
        },
      ],
    },
    {
      thread_id: "rt_482_2",
      path: "src/auth/session.test.ts",
      line: 42,
      side: "right",
      resolved: true,
      outdated: false,
      comments: [
        {
          comment_id: "rc_482_2",
          thread_id: "rt_482_2",
          author: "tyler-o",
          body: "Nice — the boundary test now moves the clock explicitly instead of reading `Date.now()`.",
          created_at: now(-60 * 60_000),
          updated_at: null,
          web_url: null,
          path: "src/auth/session.test.ts",
          line: 42,
          side: "right",
          in_reply_to: null,
        },
      ],
    },
  ],
  479: [],
  476: [],
};

const REVIEWS: Record<number, ForgeReview[]> = {
  482: [
    {
      author: "mara-k",
      state: "changes_requested",
      submitted_at: now(-22 * 60_000),
      body: "One blocking comment on the leaked pool.",
    },
    { author: "tyler-o", state: "pending", submitted_at: null, body: null },
  ],
  479: [{ author: "mara-k", state: "approved", submitted_at: now(-30 * 60_000), body: null }],
  476: [],
};

function checksFor(repo: ForgeRepoRef, summary: ForgePrSummary): ForgeChecks {
  const failing = summary.number === 482;
  const pending = summary.number === 479;
  const checks = [
    { name: "build", status: "success" as const, duration: 72_000 },
    { name: "unit", status: "success" as const, duration: 124_000 },
    { name: "lint", status: "success" as const, duration: 18_000 },
    { name: "typecheck", status: "success" as const, duration: 41_000 },
    {
      name: "e2e",
      status: failing
        ? ("failure" as const)
        : pending
          ? ("in_progress" as const)
          : ("success" as const),
      duration: pending ? null : 42_000,
    },
  ];
  const passed = checks.filter((check) => check.status === "success").length;
  return {
    repo,
    number: summary.number,
    head_sha: summary.head_sha ?? "",
    state: failing ? "failing" : pending ? "pending" : "passing",
    total: checks.length,
    passed,
    failed: failing ? checks.length - passed : 0,
    pending: pending ? checks.length - passed : 0,
    checks: checks.map((check) => ({
      name: check.name,
      status: check.status,
      details_url: `https://github.com/${repo.owner}/${repo.repo}/actions/runs/${summary.number}`,
      description: check.status === "failure" ? "Connection leaked in afterAll" : null,
      started_at: now(-10 * 60_000),
      completed_at: check.duration === null ? null : now(-8 * 60_000),
      // The wire is JSON, so a u64 arrives as a number even though the
      // generated type says bigint. Keep the mock serializable.
      duration_ms: check.duration === null ? null : (check.duration as unknown as bigint),
      // e2e gates the merge in the mock repo; the others are advisory.
      required: check.name === "e2e" || check.name === "build",
    })),
    rerun:
      repo.forge === "gitea" || repo.forge === "forgejo"
        ? {
            supported: false,
            reason: `${repo.forge === "gitea" ? "Gitea" : "Forgejo"} posts CI results as commit statuses and has no re-run API; re-run it in ${repo.forge === "gitea" ? "Gitea" : "Forgejo"}'s Actions tab`,
          }
        : repo.forge === "bitbucket"
          ? {
              supported: false,
              reason:
                "Bitbucket Cloud has no API to re-run build statuses; re-run the pipeline in Bitbucket",
            }
          : repo.forge === "azure_devops"
            ? {
                supported: false,
                reason:
                  "Azure DevOps can only re-queue a pipeline through the Builds API with extra scopes; re-run it in Azure Pipelines",
              }
            : { supported: true, reason: null },
    updated_at: now(-60_000),
  };
}

/**
 * A forge that answers the way the real daemon does.
 *
 * It keeps account, comment and thread state so adding an account, posting a
 * reply or handing a PR to an agent behaves like the live protocol instead of
 * failing with "unsupported".
 */
export class MockForge {
  private accounts: ForgeAccount[] = [
    {
      account_id: "acct_mock_github",
      forge: "github",
      base_url: "https://github.com",
      username: "you",
      display_name: "You",
      scopes: ["repo", "read:user"],
      source: "pat",
      created_at: now(-30 * 24 * 3_600_000),
    },
  ];

  private accountsSeq = 1;
  private commentSeq = 100;
  private openedSeq = 500;
  private comments = new Map<number, ForgeComment[]>(
    Object.entries(GENERAL_COMMENTS).map(([number, list]) => [Number(number), [...list]]),
  );
  private threads = new Map<number, ForgeReviewThread[]>(
    Object.entries(THREADS).map(([number, list]) => [Number(number), cloneThreads(list)]),
  );

  handle(body: { type: string }, hooks: MockHooks): boolean {
    switch (body.type) {
      case "forge_list_accounts": {
        hooks.ok({
          type: "forge_accounts",
          accounts: this.accounts,
          capabilities: CAPABILITIES,
        });
        return true;
      }
      case "forge_add_account": {
        const request = body as Extract<ClientRequest, { type: "forge_add_account" }>;
        if (request.token.trim() === "") {
          hooks.err("invalid_request", "The token is empty.", "Paste a token from the forge.");
          return true;
        }
        this.accountsSeq += 1;
        const account: ForgeAccount = {
          account_id: `acct_mock_${this.accountsSeq}`,
          forge: request.forge,
          base_url: request.base_url.replace(/\/$/, ""),
          username: request.username ?? "you",
          display_name: null,
          scopes: request.forge === "github" ? ["repo"] : ["api"],
          source: request.source,
          created_at: now(),
        };
        this.accounts = [...this.accounts, account];
        hooks.ok({ type: "forge_account", account });
        return true;
      }
      case "forge_remove_account": {
        const request = body as Extract<ClientRequest, { type: "forge_remove_account" }>;
        this.accounts = this.accounts.filter(
          (account) => account.account_id !== request.account_id,
        );
        hooks.ok({ type: "ack" });
        return true;
      }
      case "forge_import_cli": {
        const request = body as Extract<ClientRequest, { type: "forge_import_cli" }>;
        const existing = this.accounts.find((account) => account.forge === request.forge);
        if (existing) {
          hooks.ok({ type: "forge_account", account: existing });
          return true;
        }
        this.accountsSeq += 1;
        const account: ForgeAccount = {
          account_id: `acct_mock_${this.accountsSeq}`,
          forge: request.forge,
          base_url:
            request.base_url ??
            (request.forge === "gitlab" ? "https://gitlab.com" : "https://github.com"),
          username: "you",
          display_name: null,
          scopes: ["repo"],
          source: request.forge === "gitlab" ? "glab_cli" : "gh_cli",
          created_at: now(),
        };
        this.accounts = [...this.accounts, account];
        hooks.ok({ type: "forge_account", account });
        return true;
      }
      case "forge_device_flow_start": {
        hooks.ok({
          type: "forge_device_flow",
          flow: {
            flow_id: "flow_mock_1",
            user_code: "7K4Q-2M9P",
            verification_uri: "https://github.com/login/device",
            expires_in: 900,
            interval: 2,
          },
        });
        return true;
      }
      case "forge_device_flow_poll": {
        const account = this.accounts[0];
        hooks.ok({
          type: "forge_device_flow_status",
          status: account
            ? { state: "complete", account }
            : { state: "denied", message: "No account was stored." },
        });
        return true;
      }
      case "forge_detect_repo": {
        hooks.ok({
          type: "forge_repos",
          repos: [{ remote: "origin", repo: REPO, url: "git@github.com:h4rn/api.git" }],
        });
        return true;
      }
      case "forge_list_prs": {
        const request = body as Extract<ClientRequest, { type: "forge_list_prs" }>;
        const prs = filterPrs(SUMMARIES, request.filter).map((summary) => ({
          ...summary,
          repo: request.repo,
          web_url: `${request.repo.base_url}/${request.repo.owner}/${request.repo.repo}/pull/${summary.number}`,
        }));
        hooks.ok({
          type: "forge_prs",
          prs,
          page: request.page,
          has_more: false,
        });
        return true;
      }
      case "forge_get_pr": {
        const request = body as Extract<ClientRequest, { type: "forge_get_pr" }>;
        const summary =
          SUMMARIES.find((item) => item.number === request.number) ??
          (SUMMARIES[0] as ForgePrSummary);
        const pr: ForgePullRequest = {
          summary: { ...summary, repo: request.repo },
          body: "Pins the clock in the test setup so `createPool()` no longer reads `Date.now()` at import time.\n\nAdds a regression test at the 30 s session boundary and closes the pool in `afterAll`.",
          labels: [
            { name: "tests", color: "0e8a16" },
            { name: "auth", color: "1d76db" },
          ],
          mergeable: "mergeable",
          base_sha: "2d6b74d7f8e9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
          additions: 22,
          deletions: 7,
          line_counts_known: true,
          changed_files: 3,
          changed_files_known: true,
          commits: 3,
          created_at: now(-42 * 60_000),
          reviewers: REVIEWS[request.number] ?? [],
          merge_commit_sha: null,
        };
        hooks.ok({ type: "forge_pr", pr });
        return true;
      }
      case "forge_get_pr_diff": {
        const request = body as Extract<ClientRequest, { type: "forge_get_pr_diff" }>;
        const page = Math.max(1, request.page);
        const perPage = Math.max(1, request.per_page ?? 2);
        const start = (page - 1) * perPage;
        const files = FILE_DIFFS.slice(start, start + perPage).map((file) => ({
          ...file,
        }));
        const diff: ForgeDiffPage = {
          repo: request.repo,
          number: request.number,
          files,
          page,
          per_page: perPage,
          has_more: start + perPage < FILE_DIFFS.length,
        };
        hooks.ok({ type: "forge_diff", diff });
        return true;
      }
      case "forge_list_pr_comments": {
        const request = body as Extract<ClientRequest, { type: "forge_list_pr_comments" }>;
        const page: ForgeCommentPage = {
          repo: request.repo,
          number: request.number,
          comments: this.comments.get(request.number) ?? [],
          threads: cloneThreads(this.threads.get(request.number) ?? []),
          reviews: REVIEWS[request.number] ?? [],
          has_more: false,
        };
        hooks.ok({ type: "forge_comments", page });
        return true;
      }
      case "forge_list_checks": {
        const request = body as Extract<ClientRequest, { type: "forge_list_checks" }>;
        const summary =
          SUMMARIES.find((item) => item.number === request.number) ??
          (SUMMARIES[0] as ForgePrSummary);
        hooks.ok({ type: "forge_checks", checks: checksFor(request.repo, summary) });
        return true;
      }
      case "forge_list_pr_commits": {
        const request = body as Extract<ClientRequest, { type: "forge_list_pr_commits" }>;
        hooks.ok({
          type: "forge_commits",
          commits: {
            repo: request.repo,
            number: request.number,
            truncated: false,
            commits: [
              {
                sha: "5d1c0aa83e7f4b6c9d2a1e0f3b4c5d6e7f8a9b0c",
                title: "Pin the clock in the test setup",
                body: "createPool() no longer reads Date.now() at import time.",
                author: "Dev",
                author_login: "dev",
                authored_at: now(-3 * 60 * 60_000),
                web_url: `https://github.com/${request.repo.owner}/${request.repo.repo}/commit/5d1c0aa`,
              },
              {
                sha: "a93be41c07d85f62e19b3c4d5a6f7e8091b2c3d4",
                title: "Add a regression test at the 30 s session boundary",
                body: null,
                author: "Dev",
                author_login: "dev",
                authored_at: now(-2 * 60 * 60_000),
                web_url: `https://github.com/${request.repo.owner}/${request.repo.repo}/commit/a93be41`,
              },
              {
                sha: "f07e92d1a4b3c5d6e7f8091a2b3c4d5e6f708192",
                title: "Close the pool in afterAll",
                body: null,
                author: "Claude Code",
                author_login: null,
                authored_at: now(-9 * 60_000),
                web_url: `https://github.com/${request.repo.owner}/${request.repo.repo}/commit/f07e92d`,
              },
            ],
          },
        });
        return true;
      }
      case "forge_rerun_checks": {
        const request = body as Extract<ClientRequest, { type: "forge_rerun_checks" }>;
        const summary =
          SUMMARIES.find((item) => item.number === request.number) ??
          (SUMMARIES[0] as ForgePrSummary);
        const checks = checksFor(request.repo, summary);
        if (!checks.rerun.supported) {
          hooks.err("unsupported", checks.rerun.reason ?? "Not supported", null);
          return true;
        }
        // The re-run flips the failing check back to running.
        const rerun = {
          ...checks,
          state: "pending" as const,
          failed: 0,
          pending: checks.failed,
          checks: checks.checks.map((check) =>
            check.status === "failure"
              ? { ...check, status: "in_progress" as const, description: null }
              : check,
          ),
        };
        hooks.ok({ type: "forge_checks", checks: rerun });
        return true;
      }
      case "forge_reply": {
        const request = body as Extract<ClientRequest, { type: "forge_reply" }>;
        this.commentSeq += 1;
        const threadId = request.thread_id ?? `rt_${request.number}`;
        const thread = (this.threads.get(request.number) ?? []).find(
          (item) => item.thread_id === threadId,
        );
        const comment: ForgeReviewComment = {
          comment_id: `rc_mock_${this.commentSeq}`,
          thread_id: thread?.thread_id ?? null,
          author: "you",
          body: request.body,
          created_at: now(),
          updated_at: null,
          web_url: null,
          path: thread?.path ?? "",
          line: thread?.line ?? null,
          side: thread?.side ?? "right",
          in_reply_to: request.comment_id,
        };
        if (thread) thread.comments = [...thread.comments, comment];
        hooks.ok({ type: "forge_review_comment_posted", comment });
        return true;
      }
      case "forge_comment": {
        const request = body as Extract<ClientRequest, { type: "forge_comment" }>;
        this.commentSeq += 1;
        const comment: ForgeComment = {
          comment_id: `c_mock_${this.commentSeq}`,
          author: "you",
          body: request.body,
          created_at: now(),
          updated_at: null,
          web_url: null,
        };
        this.comments.set(request.number, [...(this.comments.get(request.number) ?? []), comment]);
        hooks.ok({ type: "forge_comment_posted", comment });
        return true;
      }
      case "forge_open_pr": {
        const request = body as Extract<ClientRequest, { type: "forge_open_pr" }>;
        const repo = request.repo ?? REPO;
        this.openedSeq += 1;
        const summary: ForgePrSummary = {
          repo,
          number: this.openedSeq,
          title: request.title,
          state: "open",
          author: "you",
          head_branch: "h4rn/new-branch",
          base_branch: request.base,
          head_sha: "a".repeat(40),
          updated_at: now(),
          web_url: `${repo.base_url}/${repo.owner}/${repo.repo}/pull/${this.openedSeq}`,
          checks: "pending",
          review: "pending",
          draft: false,
        };
        const pr: ForgePullRequest = {
          summary,
          body: request.body,
          labels: [],
          mergeable: "unknown",
          base_sha: null,
          additions: 0,
          deletions: 0,
          line_counts_known: true,
          changed_files: 0,
          changed_files_known: true,
          commits: 1,
          created_at: now(),
          reviewers: [],
          merge_commit_sha: null,
        };
        hooks.ok({ type: "forge_pr_opened", pr });
        return true;
      }
      case "forge_hand_pr_to_agent": {
        const request = body as Extract<ClientRequest, { type: "forge_hand_pr_to_agent" }>;
        const summary =
          SUMMARIES.find((item) => item.number === request.number) ??
          (SUMMARIES[0] as ForgePrSummary);
        const handoff: ForgeHandoff = {
          thread_id: (request.thread_id ?? "thr_a") as ThreadId,
          turn_id: null,
          context: `Pull request #${request.number}: ${summary.title}\n\nReview the open threads and reply.`,
          pr: { ...summary, repo: request.repo },
        };
        hooks.ok({ type: "forge_handoff", handoff });
        return true;
      }
      default:
        return false;
    }
  }
}

function cloneThreads(threads: ForgeReviewThread[]): ForgeReviewThread[] {
  return threads.map((thread) => ({ ...thread, comments: [...thread.comments] }));
}

function filterPrs(prs: ForgePrSummary[], filter: ForgePrFilter): ForgePrSummary[] {
  switch (filter.type) {
    case "open":
      return prs.filter((pr) => pr.state === "open" || pr.state === "draft");
    case "mine":
      return prs.filter((pr) => pr.author === "you");
    case "review_requested":
      return prs.filter((pr) => pr.number === 479 || pr.number === 476);
    case "by_branch":
      return prs.filter((pr) => pr.head_branch === filter.branch);
  }
}
