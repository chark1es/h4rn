// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ClientRequest, ForgeIssue, ForgeRepoRef, ResponseBody } from "@/gen";

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
    ui: { selectedThreadId: "th_1" },
    projects: [
      {
        id: "proj_1",
        host: "host_local",
        path: "/work/widget",
        name: "widget",
        created_at: "2026-01-01T00:00:00Z",
        git: true,
      },
    ],
    linked: false,
  };
  const issue = (title = "Dark mode flickers"): ForgeIssue => ({
    summary: {
      repo,
      number: 12,
      title,
      state: "open",
      author: "alice",
      labels: [{ name: "bug", color: "d73a4a" }],
      assignees: [{ login: "bob", name: "Bob" }],
      comment_count: 1,
      updated_at: "2026-01-02T00:00:00Z",
      web_url: "https://github.com/acme/widget/issues/12",
    },
    body: "The list flashes.",
    created_at: "2026-01-01T00:00:00Z",
    closed_at: null,
    close_reason: null,
    linked_pull_requests: [
      {
        number: 7,
        title: "Fix flicker",
        state: "open",
        web_url: "https://github.com/acme/widget/pull/7",
      },
    ],
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
      case "linear_list_accounts":
        return { type: "linear_accounts", accounts: [] };
      case "linear_project_teams":
        return { type: "linear_project_teams", teams: [] };
      case "forge_detect_repo":
        return {
          type: "forge_repos",
          repos: [{ remote: "origin", repo, url: "git@github.com:acme/widget.git" }],
        };
      case "forge_list_issues":
        return {
          type: "forge_issues",
          issues: [
            {
              ...issue().summary,
              state: request.filter === "closed" ? "closed" : "open",
            },
          ],
          page: request.page,
          has_more: false,
          read_only: false,
        };
      case "forge_get_issue":
        return { type: "forge_issue", issue: issue(), read_only: false };
      case "forge_list_issue_comments":
        return {
          type: "forge_issue_comments",
          page: {
            repo,
            number: 12,
            comments: [
              {
                comment_id: "c1",
                author: "carol",
                body: "Reproduced.",
                created_at: "2026-01-03T00:00:00Z",
                updated_at: null,
                web_url: null,
              },
            ],
            has_more: false,
          },
        };
      case "forge_update_issue":
        return {
          type: "forge_issue",
          issue: issue(request.update.title ?? "Dark mode flickers"),
          read_only: false,
        };
      case "forge_comment_issue":
        return {
          type: "forge_issue_comment_posted",
          comment: {
            comment_id: "c2",
            author: "alice",
            body: request.body,
            created_at: "2026-01-04T00:00:00Z",
            updated_at: null,
            web_url: null,
          },
        };
      case "list_thread_issues":
        return {
          type: "thread_issues",
          thread_id: request.thread_id,
          issues: state.linked
            ? [
                {
                  scope: { tracker: "forge", repo },
                  issue_id: "12",
                  account_id: "acct_1",
                  state: { tracker: "forge", issue: issue().summary },
                  error: null,
                  linked_at: "2026-01-05T00:00:00Z",
                },
              ]
            : [],
        };
      case "link_issue":
        state.linked = true;
        return {
          type: "thread_issue",
          issue: {
            scope: request.scope,
            issue_id: request.issue_id,
            account_id: "acct_1",
            state: { tracker: "forge", issue: issue().summary },
            error: null,
            linked_at: "2026-01-05T00:00:00Z",
          },
        };
      case "unlink_issue":
        state.linked = false;
        return { type: "ack" };
      default:
        return { type: "ack" };
    }
  };
  const request = vi.fn(defaultRequest);
  return { state, defaultRequest, request };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getClient: () => ({ request: mock.request }),
  pushToast: vi.fn(),
  navigate: vi.fn(),
}));

vi.mock("@/features/projects/store", () => ({
  useProjects: (selector: (state: { projects: unknown[] }) => unknown) => selector(mock.state),
}));

import { IssuesPage } from "./IssuesPage";

beforeEach(() => {
  mock.request.mockReset();
  mock.request.mockImplementation(mock.defaultRequest);
  mock.state.linked = false;
});

afterEach(() => {
  cleanup();
});

function requestsOfType(type: string): ClientRequest[] {
  return mock.request.mock.calls
    .map(([value]) => value as ClientRequest)
    .filter((value) => value.type === type);
}

async function openIssue() {
  render(<IssuesPage />);
  fireEvent.click(await screen.findByRole("button", { name: /Dark mode flickers/ }));
  await screen.findByLabelText("Issue title");
}

describe("the Issues page", () => {
  it("groups by tracker account and reads the project's repository", async () => {
    render(<IssuesPage />);
    expect(await screen.findByText("GitHub · alice")).toBeTruthy();
    expect(await screen.findByText("#12")).toBeTruthy();
    const list = requestsOfType("forge_list_issues");
    expect(list[0]).toMatchObject({ filter: "open", page: 1, query: null });
  });

  it("asks the tracker again when the filter changes", async () => {
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");
    fireEvent.click(screen.getByRole("tab", { name: "Closed" }));
    await waitFor(() => {
      expect(
        requestsOfType("forge_list_issues").some(
          (value) => value.type === "forge_list_issues" && value.filter === "closed",
        ),
      ).toBe(true);
    });
  });

  it("opens the issue beside the list and saves an edited title", async () => {
    await openIssue();
    expect(await screen.findByText("Reproduced.")).toBeTruthy();
    const title = screen.getByLabelText("Issue title");
    fireEvent.change(title, { target: { value: "Dark mode flickers on load" } });
    fireEvent.blur(title);
    await waitFor(() => {
      expect(
        requestsOfType("forge_update_issue").some(
          (value) =>
            value.type === "forge_update_issue" &&
            value.update.title === "Dark mode flickers on load",
        ),
      ).toBe(true);
    });
  });

  it("links the issue to the open thread and unlinks it again", async () => {
    await openIssue();
    fireEvent.click(screen.getByRole("button", { name: /Link to this thread/ }));
    await waitFor(() => expect(requestsOfType("link_issue")).toHaveLength(1));
    expect(await screen.findByRole("button", { name: /Unlink/ })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Unlink/ }));
    await waitFor(() => expect(requestsOfType("unlink_issue")).toHaveLength(1));
  });

  it("keeps rows and actions keyboard reachable", async () => {
    render(<IssuesPage />);
    const row = await screen.findByRole("button", { name: /Dark mode flickers/ });
    row.focus();
    expect(document.activeElement).toBe(row);
    expect(row.tagName).toBe("BUTTON");
    expect(screen.getByRole("tab", { name: "Open" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Projects" })).toBeTruthy();
  });
});
