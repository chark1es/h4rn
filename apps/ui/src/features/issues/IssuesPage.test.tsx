// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ClientRequest,
  ForgeAccount,
  ForgeCapabilityEntry,
  ForgeIssue,
  ForgeIssueCapabilities,
  ForgeRepoRef,
  ResponseBody,
} from "@/gen";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "acme",
  repo: "widget",
};

function issueCapabilities(
  overrides: Partial<ForgeIssueCapabilities> = {},
): ForgeIssueCapabilities {
  return {
    read: true,
    write: true,
    comments: true,
    labels: true,
    assignees: true,
    close_reason: true,
    search: true,
    assigned_filter: true,
    created_filter: true,
    mentioning_filter: true,
    linked_pull_requests: true,
    ...overrides,
  };
}

function account(overrides: Partial<ForgeAccount> = {}): ForgeAccount {
  return {
    account_id: "acct_1",
    forge: "github",
    base_url: "https://github.com",
    username: "alice",
    display_name: "Alice Example",
    scopes: [],
    source: "pat",
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function capabilityEntry(
  forge: ForgeAccount["forge"],
  issues: ForgeIssueCapabilities,
): ForgeCapabilityEntry {
  return {
    forge,
    capabilities: {
      device_flow: false,
      review_threads: true,
      resolve_threads: true,
      nested_replies: true,
      inline_review_comments: true,
      checks: true,
      pipelines: false,
      diff_pagination: true,
      graphql: forge === "github",
      rerun_checks: false,
      conditional_poll: false,
      issues,
    },
  };
}

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
    accounts: [] as unknown[],
    capabilities: [] as unknown[],
    repos: {} as Record<string, unknown>,
    mappings: [] as unknown[],
    linearAccounts: [] as unknown[],
    linked: false,
    commentsCursor: null as string | null,
    /** Repositories whose first page reports another page. */
    hasMore: {} as Record<string, boolean>,
    /** Blocks the second page until the test resolves it. */
    page2Gate: null as Promise<void> | null,
  };
  const issue = (target: ForgeRepoRef, title = "Dark mode flickers"): ForgeIssue => ({
    summary: {
      repo: target,
      number: 12,
      title,
      state: "open",
      author: "alice",
      labels: [{ name: "bug", color: "d73a4a" }],
      assignees: [{ login: "bob", name: "Bob" }],
      comment_count: 1,
      updated_at: "2026-01-02T00:00:00Z",
      web_url: `${target.base_url}/${target.owner}/${target.repo}/issues/12`,
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
        web_url: `${target.base_url}/${target.owner}/${target.repo}/pull/7`,
      },
    ],
  });
  const defaultRequest = async (request: ClientRequest): Promise<ResponseBody> => {
    switch (request.type) {
      case "forge_list_accounts":
        return {
          type: "forge_accounts",
          accounts: state.accounts as ForgeAccount[],
          capabilities: state.capabilities as ForgeCapabilityEntry[],
        };
      case "linear_list_accounts":
        return {
          type: "linear_accounts",
          accounts: state.linearAccounts as never[],
        };
      case "linear_project_teams":
        return { type: "linear_project_teams", teams: state.mappings as never[] };
      case "linear_list_issues":
        return {
          type: "linear_issues",
          issues:
            request.cursor === null
              ? [
                  {
                    id: "iss_1",
                    identifier: "ENG-1",
                    title: "Rate limit the import",
                    state_name: "In Progress",
                    state_type: "started",
                    team_id: "team_1",
                    team_key: "ENG",
                    assignees: ["Alice"],
                    labels: [],
                    updated_at: "2026-01-02T00:00:00Z",
                    url: "https://linear.app/acme/issue/ENG-1",
                  },
                ]
              : [],
          next_cursor: request.cursor === null ? "cur_1" : null,
          read_only: false,
        };
      case "forge_detect_repo": {
        const detected = state.repos[request.workspace];
        return {
          type: "forge_repos",
          repos: detected
            ? [
                {
                  remote: "origin",
                  repo: detected as ForgeRepoRef,
                  url: "git@github.com:acme/widget.git",
                },
              ]
            : [],
        };
      }
      case "forge_list_issues": {
        if (request.page > 1 && state.page2Gate) await state.page2Gate;
        return {
          type: "forge_issues",
          issues: [
            request.page > 1
              ? {
                  ...issue(request.repo, "Stale page row").summary,
                  number: 99,
                }
              : {
                  ...issue(request.repo).summary,
                  state: request.filter === "closed" ? "closed" : "open",
                },
          ],
          page: request.page,
          has_more: request.page === 1 ? (state.hasMore[request.repo.repo] ?? false) : false,
          read_only: false,
        };
      }
      case "forge_get_issue":
        return { type: "forge_issue", issue: issue(request.repo), read_only: false };
      case "forge_list_issue_comments": {
        const more = request.cursor === null && state.commentsCursor !== null;
        return {
          type: "forge_issue_comments",
          page: {
            repo: request.repo,
            number: request.number,
            comments: [
              request.cursor
                ? {
                    comment_id: "c2",
                    author: "dave",
                    body: "Also on Linux.",
                    created_at: "2026-01-05T00:00:00Z",
                    updated_at: null,
                    web_url: null,
                  }
                : {
                    comment_id: "c1",
                    author: "carol",
                    body: "Reproduced.",
                    created_at: "2026-01-03T00:00:00Z",
                    updated_at: null,
                    web_url: null,
                  },
            ],
            has_more: more,
            next_cursor: more ? state.commentsCursor : null,
          },
        };
      }
      case "forge_update_issue": {
        const next = issue(request.repo, request.update.title ?? "Dark mode flickers");
        next.summary.state = request.update.state ?? "open";
        next.close_reason = request.update.close_reason ?? null;
        return { type: "forge_issue", issue: next, read_only: false };
      }
      case "forge_comment_issue":
        return {
          type: "forge_issue_comment_posted",
          comment: {
            comment_id: "c3",
            author: "alice",
            body: request.body,
            created_at: "2026-01-06T00:00:00Z",
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
                  state: { tracker: "forge", issue: issue(repo).summary },
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
            state: { tracker: "forge", issue: issue(repo).summary },
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
  return { state, defaultRequest, request, issue };
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

function resetTracker(
  options: {
    projects?: typeof mock.state.projects;
    accounts?: ForgeAccount[];
    capabilities?: ForgeCapabilityEntry[];
    repos?: Record<string, ForgeRepoRef>;
    commentsCursor?: string | null;
    mappings?: unknown[];
    linearAccounts?: unknown[];
    hasMore?: Record<string, boolean>;
  } = {},
) {
  mock.state.projects = options.projects ?? [
    {
      id: "proj_1",
      host: "host_local",
      path: "/work/widget",
      name: "widget",
      created_at: "2026-01-01T00:00:00Z",
      git: true,
    },
  ];
  mock.state.accounts = options.accounts ?? [account()];
  mock.state.capabilities = options.capabilities ?? [
    capabilityEntry("github", issueCapabilities()),
  ];
  mock.state.repos = options.repos ?? { "/work/widget": repo };
  mock.state.commentsCursor = options.commentsCursor ?? null;
  mock.state.mappings = options.mappings ?? [];
  mock.state.linearAccounts = options.linearAccounts ?? [];
  mock.state.hasMore = options.hasMore ?? {};
  mock.state.page2Gate = null;
  mock.state.linked = false;
}

beforeEach(() => {
  mock.request.mockReset();
  mock.request.mockImplementation(mock.defaultRequest);
  resetTracker();
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
  it("groups by tracker account and reads the project's repository with that account", async () => {
    render(<IssuesPage />);
    expect(await screen.findByText("GitHub · alice")).toBeTruthy();
    expect(await screen.findByText("#12")).toBeTruthy();
    const list = requestsOfType("forge_list_issues");
    expect(list[0]).toMatchObject({
      filter: "open",
      page: 1,
      query: null,
      account_id: "acct_1",
    });
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
            value.update.title === "Dark mode flickers on load" &&
            value.account_id === "acct_1",
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

  it("loads the next page of comments through the provider continuation", async () => {
    resetTracker({ commentsCursor: "2" });
    await openIssue();
    expect(await screen.findByText("Reproduced.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load more comments" }));
    expect(await screen.findByText("Also on Linux.")).toBeTruthy();
    expect(
      requestsOfType("forge_list_issue_comments").some(
        (value) => value.type === "forge_list_issue_comments" && value.cursor === "2",
      ),
    ).toBe(true);
  });

  it("offers no reason or mentions filter where the tracker cannot honour them", async () => {
    const gitlabRepo: ForgeRepoRef = {
      forge: "gitlab",
      base_url: "https://gitlab.com",
      owner: "acme/widget",
      repo: "widget",
    };
    resetTracker({
      accounts: [
        account({
          account_id: "acct_gl",
          forge: "gitlab",
          base_url: "https://gitlab.com",
          username: "alice",
        }),
      ],
      capabilities: [
        capabilityEntry(
          "gitlab",
          issueCapabilities({ close_reason: false, mentioning_filter: false }),
        ),
      ],
      repos: { "/work/widget": gitlabRepo },
    });
    render(<IssuesPage />);
    await screen.findByText("#12");
    expect(screen.queryByRole("tab", { name: "Mentioning me" })).toBeNull();
    await openIssue();
    expect(screen.queryByLabelText("Close reason")).toBeNull();
  });

  it("closes on GitLab, Gitea and Azure without sending a reason", async () => {
    for (const forge of ["gitlab", "gitea", "azure_devops"] as const) {
      mock.request.mockClear();
      cleanup();
      const base =
        forge === "gitlab"
          ? "https://gitlab.com"
          : forge === "gitea"
            ? "https://gitea.com"
            : "https://dev.azure.com/acme";
      const target: ForgeRepoRef = {
        forge,
        base_url: base,
        owner: forge === "gitlab" ? "acme/widget" : "acme",
        repo: "widget",
      };
      resetTracker({
        accounts: [account({ account_id: `acct_${forge}`, forge, base_url: base })],
        capabilities: [capabilityEntry(forge, issueCapabilities({ close_reason: false }))],
        repos: { "/work/widget": target },
      });
      await openIssue();
      fireEvent.click(screen.getByRole("button", { name: "Close issue" }));
      await waitFor(() => {
        const updates = requestsOfType("forge_update_issue");
        expect(updates).toHaveLength(1);
        expect(updates[0]).toMatchObject({
          type: "forge_update_issue",
          account_id: `acct_${forge}`,
          update: { state: "closed", close_reason: null },
        });
      });
    }
  });

  it("names the issue a GitHub duplicate close duplicates", async () => {
    await openIssue();
    // Radix opens on pointer down, not click.
    fireEvent.pointerDown(screen.getByRole("button", { name: /Close reason/ }), {
      button: 0,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: "Duplicate" }));
    const picker = await screen.findByLabelText("Duplicate of issue number");
    fireEvent.change(picker, { target: { value: "9" } });
    fireEvent.click(screen.getByRole("button", { name: "Close issue" }));
    await waitFor(() => {
      const update = requestsOfType("forge_update_issue")[0];
      expect(update).toMatchObject({
        type: "forge_update_issue",
        update: {
          state: "closed",
          close_reason: "duplicate",
          duplicate_issue_id: 9,
        },
      });
    });
  });

  it("reads two accounts on the same forge through their own account", async () => {
    const secondRepo: ForgeRepoRef = {
      forge: "github",
      base_url: "https://github.example.test",
      owner: "acme",
      repo: "widget",
    };
    resetTracker({
      projects: [
        {
          id: "proj_1",
          host: "host_local",
          path: "/work/widget",
          name: "widget",
          created_at: "2026-01-01T00:00:00Z",
          git: true,
        },
        {
          id: "proj_2",
          host: "host_local",
          path: "/work/enterprise",
          name: "enterprise",
          created_at: "2026-01-01T00:00:00Z",
          git: true,
        },
      ],
      accounts: [
        account(),
        account({
          account_id: "acct_2",
          base_url: "https://github.example.test",
          username: "bob",
        }),
      ],
      capabilities: [capabilityEntry("github", issueCapabilities())],
      repos: { "/work/widget": repo, "/work/enterprise": secondRepo },
    });
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");
    await screen.findByText("GitHub · bob");
    await waitFor(() => {
      const accounts = requestsOfType("forge_list_issues").map((value) =>
        value.type === "forge_list_issues" ? value.account_id : null,
      );
      expect(accounts).toEqual(expect.arrayContaining(["acct_1", "acct_2"]));
    });
  });
});

describe("issue paging", () => {
  it("stores Linear's cursor from page one and uses it for Load more", async () => {
    resetTracker({
      mappings: [
        {
          project_id: "proj_1",
          account_id: "lin_1",
          team_id: "team_1",
          team_key: "ENG",
          team_name: "Engineering",
        },
      ],
      linearAccounts: [
        { account_id: "lin_1", name: "Acme", email: null, created_at: "2026-01-01T00:00:00Z" },
      ],
    });
    render(<IssuesPage />);
    expect(await screen.findByText("Linear · Acme")).toBeTruthy();
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));
    await waitFor(() => {
      expect(
        requestsOfType("linear_list_issues").some(
          (value) => value.type === "linear_list_issues" && value.cursor === "cur_1",
        ),
      ).toBe(true);
    });
    // The exhausted Linear source is skipped, so the button is gone.
    await waitFor(() => expect(screen.queryByRole("button", { name: "Load more" })).toBeNull());
  });

  it("does not page a source that reported no more rows", async () => {
    resetTracker({
      projects: [
        {
          id: "proj_1",
          host: "host_local",
          path: "/work/widget",
          name: "widget",
          created_at: "2026-01-01T00:00:00Z",
          git: true,
        },
        {
          id: "proj_2",
          host: "host_local",
          path: "/work/other",
          name: "other",
          created_at: "2026-01-01T00:00:00Z",
          git: true,
        },
      ],
      repos: {
        "/work/widget": repo,
        "/work/other": { ...repo, repo: "other" },
      },
      hasMore: { widget: false, other: true },
    });
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));
    await waitFor(() => {
      const pages = requestsOfType("forge_list_issues").filter(
        (value) => value.type === "forge_list_issues" && value.page === 2,
      );
      expect(pages).toHaveLength(1);
      expect(pages[0]).toMatchObject({ repo: { repo: "other" } });
    });
  });

  it("drops a page that arrives after the search changed", async () => {
    resetTracker({ hasMore: { widget: true } });
    let release = () => {};
    mock.state.page2Gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    render(<IssuesPage />);
    await screen.findByText("#12");
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    const input = screen.getByRole("textbox", { name: "Search issues" });
    fireEvent.change(input, { target: { value: "flashes" } });
    fireEvent.submit(input.closest("form") as HTMLFormElement);
    await waitFor(() => {
      expect(
        requestsOfType("forge_list_issues").some(
          (value) => value.type === "forge_list_issues" && value.query === "flashes",
        ),
      ).toBe(true);
    });
    release();
    await waitFor(() => expect(screen.queryByText("Stale page row")).toBeNull());
  });
});
