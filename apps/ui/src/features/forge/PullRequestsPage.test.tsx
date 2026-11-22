// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ClientRequest, ForgeRepoRef, ResponseBody } from "@/gen";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "acme",
  repo: "widget",
};

const mock = vi.hoisted(() => {
  const state = {
    threads: {},
    connection: { status: "ready" },
    hosts: {},
    ui: { theme: "light" },
  };
  const client = {
    request: vi.fn(async (request: ClientRequest): Promise<ResponseBody> => {
      switch (request.type) {
        case "forge_list_accounts":
          return {
            type: "forge_accounts",
            accounts: [
              {
                account_id: "acct_1",
                forge: "github",
                base_url: "https://github.com",
                username: "alice",
                display_name: "Alice Example",
                scopes: [],
                source: "pat",
                created_at: "2026-01-01T00:00:00Z",
              },
            ],
            capabilities: [],
          };
        case "forge_list_prs":
          return {
            type: "forge_prs",
            page: 1,
            has_more: false,
            prs: [
              {
                repo: request.repo,
                number: 7,
                title: "Fix the widget",
                state: "open",
                author: "alice",
                head_branch: "alice/fix",
                base_branch: "main",
                head_sha: "abc123",
                updated_at: "2026-01-02T00:00:00Z",
                web_url: "https://github.com/acme/widget/pull/7",
                checks: "passing",
                review: "pending",
                draft: false,
              },
            ],
          };
        case "forge_get_pr":
          return {
            type: "forge_pr",
            pr: {
              summary: {
                repo: request.repo,
                number: 7,
                title: "Fix the widget",
                state: "open",
                author: "alice",
                head_branch: "alice/fix",
                base_branch: "main",
                head_sha: "abc123",
                updated_at: "2026-01-02T00:00:00Z",
                web_url: "https://github.com/acme/widget/pull/7",
                checks: "passing",
                review: "pending",
                draft: false,
              },
              body: "Fixes the widget.",
              labels: [],
              mergeable: "mergeable",
              base_sha: "def456",
              additions: 1,
              deletions: 1,
              changed_files: 1,
              changed_files_known: true,
              line_counts_known: true,
              commits: 1,
              created_at: "2026-01-01T00:00:00Z",
              reviewers: [],
              merge_commit_sha: null,
            },
          };
        case "forge_list_pr_comments":
          return {
            type: "forge_comments",
            page: {
              repo: request.repo,
              number: 7,
              comments: [],
              threads: [],
              reviews: [],
              has_more: false,
            },
          };
        case "forge_list_checks":
          return {
            type: "forge_checks",
            checks: {
              repo: request.repo,
              number: 7,
              head_sha: "abc123",
              state: "passing",
              total: 1,
              passed: 1,
              failed: 0,
              pending: 0,
              checks: [
                {
                  name: "build",
                  status: "success",
                  details_url: null,
                  description: null,
                  started_at: null,
                  completed_at: null,
                  duration_ms: null,
                  required: true,
                },
              ],
              rerun: { supported: false, reason: "not supported here" },
              updated_at: "2026-01-02T00:00:00Z",
            },
          };
        case "forge_list_pr_commits":
          return {
            type: "forge_commits",
            commits: { repo: request.repo, number: 7, commits: [], truncated: false },
          };
        case "forge_get_pr_diff":
          throw new Error("GitHub returned HTTP 500: the diff failed");
        default:
          return { type: "ack" };
      }
    }),
    onEvent: vi.fn(() => () => undefined),
  };
  return { state, client };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getClient: () => mock.client,
  resolvedTheme: () => "light",
  pushToast: vi.fn(),
  navigate: vi.fn(),
  openThread: vi.fn(),
}));

import { PullRequestsPage } from "./PullRequestsPage";

beforeEach(() => {
  window.history.replaceState(
    {},
    "",
    `/?repo=${repo.owner}/${repo.repo}&forge=${repo.forge}&base=${encodeURIComponent(repo.base_url)}`,
  );
});

afterEach(() => {
  cleanup();
  mock.client.request.mockClear();
  window.history.replaceState({}, "", "/");
});

describe("PR detail tabs", () => {
  it("keeps the header and checks when the diff fails, and names the error in Files", async () => {
    render(<PullRequestsPage />);

    fireEvent.click(await screen.findByRole("button", { name: /Fix the widget/ }));

    // The detail loaded, so the header and the checks side column are there.
    expect(await screen.findByRole("heading", { name: /Fix the widget/ })).toBeTruthy();
    expect(screen.getByText("1 of 1 passed")).toBeTruthy();

    fireEvent.click(screen.getByRole("tab", { name: /Files/ }));

    // The diff error stays inside its tab; the header and checks remain.
    expect(await screen.findByText("The diff did not load")).toBeTruthy();
    expect(screen.getByText(/the diff failed/)).toBeTruthy();
    expect(screen.getByRole("heading", { name: /Fix the widget/ })).toBeTruthy();
    expect(screen.getByText("1 of 1 passed")).toBeTruthy();
  });
});
