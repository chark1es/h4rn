// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ClientRequest, ForgeChecks, ForgeRepoRef, ResponseBody } from "@/gen";

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
    listeners: [] as Array<(payload: { event: unknown }) => void>,
  };
  const checksPayload = (repo: ForgeRepoRef): ForgeChecks => ({
    repo,
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
  });
  const defaultRequest = async (request: ClientRequest): Promise<ResponseBody> => {
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
        return { type: "forge_checks", checks: checksPayload(request.repo) };
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
  };
  const client = {
    request: vi.fn(defaultRequest),
    onEvent: vi.fn((listener: (payload: { event: unknown }) => void) => {
      state.listeners.push(listener);
      return () => {
        state.listeners = state.listeners.filter((entry) => entry !== listener);
      };
    }),
  };
  return { state, client, defaultRequest, checksPayload };
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
  mock.state.listeners = [];
  mock.client.request.mockReset();
  mock.client.request.mockImplementation(mock.defaultRequest);
  window.history.replaceState({}, "", "/");
});

/** Renders the list, opens #7 and waits for its header. */
async function openPr() {
  render(<PullRequestsPage />);
  fireEvent.click(await screen.findByRole("button", { name: /Fix the widget/ }));
  await screen.findByRole("heading", { name: /Fix the widget/ });
}

function requestsOfType(type: string): number {
  return mock.client.request.mock.calls.filter(([request]) => request.type === type).length;
}

describe("PR detail tabs", () => {
  it("keeps the header and checks when the diff fails, and names the error in Files", async () => {
    await openPr();

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

  it("clears the Checks tab's error when a live update arrives", async () => {
    mock.client.request.mockImplementation(async (request) => {
      if (request.type === "forge_list_checks") {
        throw new Error("GitHub returned HTTP 500: the checks failed");
      }
      return mock.defaultRequest(request);
    });

    await openPr();
    fireEvent.click(screen.getByRole("tab", { name: /Checks/ }));
    expect(await screen.findByText("Checks did not load")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();

    act(() => {
      for (const listener of mock.state.listeners) {
        listener({
          event: {
            type: "forge_checks_updated",
            repo,
            number: 7,
            checks: mock.checksPayload(repo),
          },
        });
      }
    });

    // The pushed update is real data, so the failed load no longer stands.
    expect(screen.queryByText("Checks did not load")).toBeNull();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
    expect(screen.getAllByText("build").length).toBeGreaterThan(0);
    expect(requestsOfType("forge_list_checks")).toBe(1);
  });

  it("re-runs only the tab's own request on Retry and keeps the header", async () => {
    const counts = new Map<string, number>();
    let release: ((body: ResponseBody) => void) | null = null;
    mock.client.request.mockImplementation(async (request) => {
      const count = (counts.get(request.type) ?? 0) + 1;
      counts.set(request.type, count);
      if (request.type === "forge_list_pr_commits") {
        if (count === 1) throw new Error("GitHub returned HTTP 500: the commits failed");
        return new Promise<ResponseBody>((resolve) => {
          release = resolve;
        });
      }
      return mock.defaultRequest(request);
    });

    await openPr();
    fireEvent.click(screen.getByRole("tab", { name: /Commits/ }));
    expect(await screen.findByText("Commits did not load")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));

    // While the tab loads again, the header and the checks stay on screen.
    expect(await screen.findByText("Loading commits…")).toBeTruthy();
    expect(screen.getByRole("heading", { name: /Fix the widget/ })).toBeTruthy();
    expect(screen.getAllByText("1 of 1 passed").length).toBeGreaterThan(0);
    expect(counts.get("forge_get_pr")).toBe(1);
    expect(counts.get("forge_list_checks")).toBe(1);
    expect(counts.get("forge_list_pr_comments")).toBe(1);
    expect(counts.get("forge_get_pr_diff")).toBe(1);

    act(() => {
      release?.({
        type: "forge_commits",
        commits: { repo, number: 7, commits: [], truncated: false },
      });
    });

    expect(await screen.findByText("No commits")).toBeTruthy();
    expect(counts.get("forge_list_pr_commits")).toBe(2);
  });

  it("shows a quiet loading line in each tab while its request is in flight", async () => {
    const pending = new Set([
      "forge_list_pr_comments",
      "forge_list_checks",
      "forge_list_pr_commits",
      "forge_get_pr_diff",
    ]);
    mock.client.request.mockImplementation(async (request) => {
      if (pending.has(request.type)) return new Promise<ResponseBody>(() => undefined);
      return mock.defaultRequest(request);
    });

    await openPr();

    // Conversation: its own comment request is still running.
    expect(await screen.findByText("Loading comments…")).toBeTruthy();
    expect(screen.queryByText("Comments did not load")).toBeNull();
    // The side column says "loading" rather than "No checks reported."
    expect(screen.getAllByText("Loading checks…").length).toBeGreaterThan(0);
    expect(screen.queryByText("No checks reported.")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Commits/ }));
    expect(await screen.findByText("Loading commits…")).toBeTruthy();
    expect(screen.queryByText("Commits did not load")).toBeNull();
    expect(screen.queryByText("No commits")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Checks/ }));
    // The tab and the hidden side column both carry the line; either is the
    // loading state, not a failed load or an empty list.
    expect(screen.getAllByText("Loading checks…").length).toBeGreaterThan(0);
    expect(screen.queryByText("Checks did not load")).toBeNull();
    expect(screen.queryByText("No checks")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Files/ }));
    expect(await screen.findByText("Loading the diff…")).toBeTruthy();
    expect(screen.queryByText("The diff did not load")).toBeNull();
  });
});
