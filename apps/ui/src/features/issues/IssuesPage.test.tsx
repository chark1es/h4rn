// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ClientRequest,
  ForgeAccount,
  ForgeCapabilities,
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
  forge: ForgeCapabilityEntry["forge"],
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

/** Linear's capabilities arrive in the response's own `linear` field. */
function linearCapabilities(overrides: Partial<ForgeIssueCapabilities> = {}): ForgeCapabilities {
  return {
    device_flow: false,
    review_threads: false,
    resolve_threads: false,
    nested_replies: false,
    inline_review_comments: false,
    checks: false,
    pipelines: false,
    diff_pagination: false,
    graphql: true,
    rerun_checks: false,
    conditional_poll: false,
    issues: issueCapabilities({
      write: false,
      labels: false,
      assignees: false,
      close_reason: false,
      mentioning_filter: false,
      ...overrides,
    }),
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
    linearCapabilities: null as ForgeCapabilities | null,
    repos: {} as Record<string, unknown>,
    mappings: [] as unknown[],
    linearAccounts: [] as unknown[],
    linked: false,
    commentsCursor: null as string | null,
    /** Repositories whose first page reports another page. */
    hasMore: {} as Record<string, boolean>,
    /** Blocks the second page until the test resolves it. */
    page2Gate: null as Promise<void> | null,
    /** Details that differ from the list row. */
    detailLabels: null as { name: string; color: string | null }[] | null,
    detailAssignees: null as { login: string; name: string | null }[] | null,
    /** Simulates an issue leaving the list after a write. */
    hideRow: false,
    /** The tracker's state after the last update. */
    closed: false,
    /** Whether the read reported a repository no project owns. */
    readOnly: false,
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
          linear: state.linearCapabilities,
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
          issues:
            state.hideRow && request.page === 1
              ? []
              : [
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
          read_only: state.readOnly,
        };
      }
      case "forge_get_issue": {
        const detail = issue(request.repo);
        detail.summary.state = state.closed ? "closed" : "open";
        if (state.detailLabels) detail.summary.labels = state.detailLabels;
        if (state.detailAssignees) detail.summary.assignees = state.detailAssignees;
        return { type: "forge_issue", issue: detail, read_only: state.readOnly };
      }
      case "linear_get_issue": {
        const after = request.comments_after;
        return {
          type: "linear_issue",
          issue: {
            summary: {
              id: request.issue_id,
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
            description: "Imports hammer the API.",
            created_at: "2026-01-01T00:00:00Z",
            comments: after
              ? [
                  {
                    id: "lc_2",
                    author: "Dave",
                    body: "Second page comment.",
                    created_at: "2026-01-04T00:00:00Z",
                    updated_at: null,
                  },
                ]
              : [
                  {
                    id: "lc_1",
                    author: "Carol",
                    body: "First page comment.",
                    created_at: "2026-01-03T00:00:00Z",
                    updated_at: null,
                  },
                ],
            comments_next_cursor: after ? null : "lc_2",
            linked_pull_requests: [],
          },
          read_only: state.readOnly,
        };
      }
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
        state.closed = next.summary.state === "closed";
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
import { issueCapabilitiesFor, linearIssueRow } from "./store";

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
    detailLabels?: { name: string; color: string | null }[];
    detailAssignees?: { login: string; name: string | null }[];
    readOnly?: boolean;
    linearCapabilities?: ForgeCapabilities | null;
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
  mock.state.linearCapabilities =
    "linearCapabilities" in options ? (options.linearCapabilities ?? null) : linearCapabilities();
  mock.state.repos = options.repos ?? { "/work/widget": repo };
  mock.state.commentsCursor = options.commentsCursor ?? null;
  mock.state.mappings = options.mappings ?? [];
  mock.state.linearAccounts = options.linearAccounts ?? [];
  mock.state.hasMore = options.hasMore ?? {};
  mock.state.page2Gate = null;
  mock.state.linked = false;
  mock.state.detailLabels = options.detailLabels ?? null;
  mock.state.detailAssignees = options.detailAssignees ?? null;
  mock.state.hideRow = false;
  mock.state.closed = false;
  mock.state.readOnly = options.readOnly ?? false;
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

  it("marks an issue read-only when no project owns the repository", async () => {
    resetTracker({ readOnly: true });
    render(<IssuesPage />);
    fireEvent.click(await screen.findByRole("button", { name: /Dark mode flickers/ }));
    expect(
      await screen.findByText(
        "This project does not own the repository, so the issue is read-only.",
      ),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Issue title")).toBeNull();
    expect(screen.queryByRole("button", { name: "Comment" })).toBeNull();
  });

  it("says a Linear issue is read-only because its team is not on a project", async () => {
    resetTracker({
      readOnly: true,
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
    fireEvent.click(await screen.findByRole("button", { name: /Rate limit the import/ }));
    expect(
      await screen.findByText(
        "This Linear team is not attached to a project, so its issues are read-only. Choose the team for a project in Settings → Source control.",
      ),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Issue title")).toBeNull();
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

describe("accounts, filters and detail", () => {
  it("reads one project through the same-host account the person picks", async () => {
    resetTracker({
      accounts: [
        account(),
        account({ account_id: "acct_2", username: "bob", display_name: "Bob Example" }),
      ],
    });
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");

    fireEvent.pointerDown(screen.getByRole("button", { name: "Projects" }), {
      button: 0,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitemradio", { name: "bob" }));
    expect(await screen.findByText("GitHub · bob")).toBeTruthy();
    await waitFor(() => {
      const list = requestsOfType("forge_list_issues");
      expect(list[list.length - 1]).toMatchObject({
        type: "forge_list_issues",
        account_id: "acct_2",
      });
    });

    fireEvent.click(await screen.findByRole("button", { name: /Dark mode flickers/ }));
    await screen.findByLabelText("Issue title");
    expect(requestsOfType("forge_get_issue")[0]).toMatchObject({ account_id: "acct_2" });
    expect(requestsOfType("forge_list_issue_comments")[0]).toMatchObject({
      account_id: "acct_2",
    });

    const comment = screen.getByLabelText("New comment");
    fireEvent.change(comment, { target: { value: "On it." } });
    fireEvent.click(screen.getByRole("button", { name: "Comment" }));
    await waitFor(() =>
      expect(requestsOfType("forge_comment_issue")[0]).toMatchObject({ account_id: "acct_2" }),
    );

    const title = screen.getByLabelText("Issue title");
    fireEvent.change(title, { target: { value: "Dark mode flickers on load" } });
    fireEvent.blur(title);
    await waitFor(() =>
      expect(requestsOfType("forge_update_issue")[0]).toMatchObject({ account_id: "acct_2" }),
    );

    fireEvent.click(await screen.findByRole("button", { name: /Link to this thread/ }));
    await waitFor(() =>
      expect(requestsOfType("link_issue")[0]).toMatchObject({ account_id: "acct_2" }),
    );
  });

  it("does not clear the list when the same filter is chosen again", async () => {
    render(<IssuesPage />);
    await screen.findByText("#12");
    fireEvent.click(screen.getByRole("tab", { name: "Closed" }));
    await waitFor(() =>
      expect(
        requestsOfType("forge_list_issues").some(
          (value) => value.type === "forge_list_issues" && value.filter === "closed",
        ),
      ).toBe(true),
    );
    expect(await screen.findByText("#12")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Closed" }));
    expect(screen.getByText("#12")).toBeTruthy();
    expect(
      requestsOfType("forge_list_issues").filter(
        (value) => value.type === "forge_list_issues" && value.filter === "closed",
      ),
    ).toHaveLength(1);
  });

  it("keeps comment and reopen actions after the issue leaves the list", async () => {
    await openIssue();
    mock.state.hideRow = true;
    fireEvent.click(screen.getByRole("button", { name: "Close issue" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Reopen issue" })).toBeTruthy());
    expect(screen.getByLabelText("New comment")).toBeTruthy();
  });

  it("renders labels and assignees from the fetched detail", async () => {
    resetTracker({
      detailLabels: [{ name: "detail-only-label", color: null }],
      detailAssignees: [{ login: "carol-detail", name: null }],
    });
    await openIssue();
    expect(await screen.findByText("detail-only-label")).toBeTruthy();
    expect(screen.getByText("carol-detail")).toBeTruthy();
    expect(screen.queryByText("d73a4a")).toBeNull();
  });

  it("names Bitbucket Cloud's removed tracker per project and reads nothing", async () => {
    const bitbucketRepo: ForgeRepoRef = {
      forge: "bitbucket",
      base_url: "https://bitbucket.org",
      owner: "acme",
      repo: "widget",
    };
    resetTracker({
      accounts: [
        account({ account_id: "acct_bb", forge: "bitbucket", base_url: bitbucketRepo.base_url }),
      ],
      capabilities: [
        capabilityEntry("bitbucket", {
          ...issueCapabilities(),
          read: false,
          write: false,
          comments: false,
          labels: false,
          assignees: false,
          close_reason: false,
          search: false,
          assigned_filter: false,
          created_filter: false,
          mentioning_filter: false,
          linked_pull_requests: false,
        }),
      ],
      repos: { "/work/widget": bitbucketRepo },
    });
    render(<IssuesPage />);
    expect(await screen.findByText(/Bitbucket Cloud has no issue API/)).toBeTruthy();
    expect(screen.getByText(/pull requests only/)).toBeTruthy();
    expect(requestsOfType("forge_list_issues")).toHaveLength(0);
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

  it("pages a Linear discussion with comments_after and keeps both pages", async () => {
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
    fireEvent.click(await screen.findByRole("button", { name: /Rate limit the import/ }));
    expect(await screen.findByText("First page comment.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load more comments" }));
    expect(await screen.findByText("Second page comment.")).toBeTruthy();
    expect(screen.getByText("First page comment.")).toBeTruthy();
    expect(
      requestsOfType("linear_get_issue").some(
        (value) => value.type === "linear_get_issue" && value.comments_after === "lc_2",
      ),
    ).toBe(true);
  });
});

describe("Linear capabilities", () => {
  it("comes from the daemon's linear field, not the UI's own defaults", () => {
    const caps = issueCapabilitiesFor(
      "linear",
      undefined,
      [capabilityEntry("github", issueCapabilities())],
      linearCapabilities({ linked_pull_requests: false }),
    );
    expect(caps.read).toBe(true);
    expect(caps.comments).toBe(true);
    expect(caps.linked_pull_requests).toBe(false);
  });

  it("offers nothing for Linear when the daemon sends no linear field", () => {
    expect(issueCapabilitiesFor("linear", undefined, [], null).read).toBe(false);
  });
});

describe("Linear state families", () => {
  it("shows a duplicate as closed", () => {
    const row = linearIssueRow("lin_1", {
      id: "iss_dup",
      identifier: "ENG-9",
      title: "Replaced by ENG-1",
      state_name: "Duplicate",
      state_type: "duplicate",
      team_id: "team_1",
      team_key: "ENG",
      assignees: [],
      labels: [],
      updated_at: "2026-01-02T00:00:00Z",
      url: "https://linear.app/acme/issue/ENG-9",
    });
    expect(row.state).toBe("closed");
  });
});
