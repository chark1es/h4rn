// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
import { setLocalePreference } from "@/lib/i18n";

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
    /** Blocks one repository's first page, for a superseded listing. */
    page1Gate: null as { repo: string; account?: string; promise: Promise<void> } | null,
    /** Blocks a Linear listing's first page, for a superseded team. */
    linearPageGate: null as Promise<void> | null,
    /** A request type that fails, as a tracker that is briefly unreachable. */
    fail: null as string | null,
    /** Details that differ from the list row. */
    detailLabels: null as { name: string; color: string | null }[] | null,
    detailAssignees: null as { login: string; name: string | null }[] | null,
    /** Pull requests the detail reports, when not the fixture's one. */
    detailPulls: null as ForgeIssue["linked_pull_requests"] | null,
    /** Simulates an issue leaving the list after a write. */
    hideRow: false,
    /** Makes the next non-first page fail once, for a temporary tracker error. */
    failPage2: false,
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
    if (state.fail === request.type) {
      throw new Error(`${request.type} is briefly unavailable`);
    }
    switch (request.type) {
      case "forge_list_accounts":
        return {
          type: "forge_accounts",
          accounts: state.accounts as ForgeAccount[],
          capabilities: state.capabilities as ForgeCapabilityEntry[],
          // An older daemon omits the field rather than sending null.
          ...(state.linearCapabilities ? { linear: state.linearCapabilities } : {}),
        };
      case "linear_list_accounts":
        return {
          type: "linear_accounts",
          accounts: state.linearAccounts as never[],
        };
      case "linear_project_teams":
        return { type: "linear_project_teams", teams: state.mappings as never[] };
      case "linear_list_issues": {
        // The daemon resolves the project's team from its mapping, so the
        // response follows whatever team is mapped when the read lands.
        const mapped = (
          state.mappings as { team_id?: string; team_key?: string; team_name?: string }[]
        )[0];
        const teamId = mapped?.team_id ?? "team_1";
        const teamKey = mapped?.team_key ?? "ENG";
        if (request.cursor === null && state.linearPageGate) await state.linearPageGate;
        if (request.cursor !== null && state.page2Gate) await state.page2Gate;
        return {
          type: "linear_issues",
          issues:
            request.cursor === null
              ? [
                  {
                    id: `issue_${teamId}`,
                    identifier: teamId === "team_1" ? "ENG-1" : "OPS-2",
                    title: teamId === "team_1" ? "Rate limit the import" : "Operations queue",
                    state_name: "In Progress",
                    state_type: "started",
                    team_id: teamId,
                    team_key: teamKey,
                    assignees: ["Alice"],
                    labels: [],
                    updated_at: "2026-01-02T00:00:00Z",
                    url: `https://linear.app/acme/issue/${teamId === "team_1" ? "ENG-1" : "OPS-2"}`,
                  },
                ]
              : [
                  {
                    id: "iss_stale",
                    identifier: "ENG-99",
                    title: "Stale Linear page row",
                    state_name: "In Progress",
                    state_type: "started",
                    team_id: "team_1",
                    team_key: "ENG",
                    assignees: ["Alice"],
                    labels: [],
                    updated_at: "2026-01-02T00:00:00Z",
                    url: "https://linear.app/acme/issue/ENG-99",
                  },
                ],
          next_cursor: request.cursor === null ? "cur_1" : null,
          read_only: false,
        };
      }
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
        if (
          request.page === 1 &&
          state.page1Gate &&
          state.page1Gate.repo === request.repo.repo &&
          (!state.page1Gate.account || state.page1Gate.account === request.account_id)
        ) {
          await state.page1Gate.promise;
        }
        if (request.page > 1 && state.page2Gate) await state.page2Gate;
        if (request.page > 1 && state.failPage2) {
          throw new Error("the tracker is briefly unavailable");
        }
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
        if (state.detailPulls) detail.linked_pull_requests = state.detailPulls;
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
                  identifier: null,
                  title: "Dark mode flickers",
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
            identifier: null,
            title: "Dark mode flickers",
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
    fail?: string | null;
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
  mock.state.page1Gate = null;
  mock.state.linearPageGate = null;
  mock.state.connection = { status: "ready" };
  mock.state.fail = options.fail ?? null;
  mock.state.linked = false;
  mock.state.detailLabels = options.detailLabels ?? null;
  mock.state.detailAssignees = options.detailAssignees ?? null;
  mock.state.detailPulls = null;
  mock.state.hideRow = false;
  mock.state.failPage2 = false;
  mock.state.closed = false;
  mock.state.readOnly = options.readOnly ?? false;
}

beforeEach(() => {
  // Hidden projects and account choices are stored per browser; a test that
  // hides one must not change what the next test sees.
  window.localStorage.clear();
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
    fireEvent.click(screen.getByRole("radio", { name: "Closed" }));
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

  it("keeps the comment draft after a title edit is saved", async () => {
    await openIssue();
    const comment = screen.getByLabelText("New comment");
    fireEvent.change(comment, { target: { value: "Still here after the edit" } });
    const title = screen.getByLabelText("Issue title");
    fireEvent.change(title, { target: { value: "Dark mode flickers on load" } });
    fireEvent.blur(title);
    await waitFor(() => expect(requestsOfType("forge_update_issue")).toHaveLength(1));
    // The pane stayed mounted, so the draft the person typed is untouched;
    // only the list behind it was refreshed.
    const draft = await screen.findByLabelText("New comment");
    expect((draft as HTMLTextAreaElement).value).toBe("Still here after the edit");
  });

  it("names a project whose repository no connected account serves", async () => {
    resetTracker({
      accounts: [
        account({
          account_id: "acct_gl",
          forge: "gitlab",
          base_url: "https://gitlab.com",
          username: "alice",
        }),
      ],
    });
    render(<IssuesPage />);
    expect(await screen.findByText(/No GitHub account matches this repository/)).toBeTruthy();
    expect(requestsOfType("forge_list_issues")).toHaveLength(0);
  });

  it("reads no Linear source when the accounts response has no linear field", async () => {
    resetTracker({
      linearCapabilities: null,
      // A tracker that cannot answer the involvement filters must not offer
      // them, so the assertion below is about the missing Linear field rather
      // than GitHub's own capabilities.
      capabilities: [capabilityEntry("github", issueCapabilities({ mentioning_filter: false }))],
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
    // The forge group proves accounts, repositories and mappings all
    // resolved; asserting before that only tested the loading state.
    expect(await screen.findByText("GitHub · alice")).toBeTruthy();
    // No Linear group, row or comment box: without the capability field there
    // is no Linear source to read, so nothing Linear can be opened.
    expect(screen.queryByText("Linear · Acme")).toBeNull();
    expect(screen.queryByText("ENG-1")).toBeNull();
    expect(screen.queryByLabelText("New comment")).toBeNull();
    expect(screen.queryByRole("radio", { name: "Mentioning me" })).toBeNull();
    expect(mock.request.mock.calls.map(([value]) => (value as ClientRequest).type)).not.toContain(
      "linear_list_issues",
    );
  });

  it("keeps the forge groups when only the Linear account list fails", async () => {
    resetTracker({
      fail: "linear_list_accounts",
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
    expect(await screen.findByText("GitHub · alice")).toBeTruthy();
    expect(await screen.findByText("linear_list_accounts is briefly unavailable")).toBeTruthy();
    // The forge list did load, so its projects are not "unmatched".
    expect(screen.queryByText(/No GitHub account matches this repository/)).toBeNull();
    expect(screen.queryByText("Linear · Acme")).toBeNull();
  });

  it("does not claim no account matches when the forge account load fails", async () => {
    resetTracker({ fail: "forge_list_accounts" });
    render(<IssuesPage />);
    expect(await screen.findByText("forge_list_accounts is briefly unavailable")).toBeTruthy();
    // Repository detection republishes the listing, which must not clear the
    // discovery error with its own (empty) state.
    await waitFor(() => expect(requestsOfType("forge_detect_repo")).toHaveLength(1));
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(screen.getByText("forge_list_accounts is briefly unavailable")).toBeTruthy();
    expect(screen.queryByText(/account matches this repository/)).toBeNull();
    expect(requestsOfType("forge_list_issues")).toHaveLength(0);
  });

  it("drives Linear's comment box and filters from the accounts response", async () => {
    resetTracker({
      linearCapabilities: linearCapabilities({ mentioning_filter: true }),
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
    expect(screen.getByRole("radio", { name: "Mentioning me" })).toBeTruthy();
    fireEvent.click(await screen.findByRole("button", { name: /Rate limit the import/ }));
    expect(await screen.findByLabelText("New comment")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Comment" })).toBeTruthy();
  });

  it("hides Linear's comment box and mention filter when the field says so", async () => {
    resetTracker({
      linearCapabilities: linearCapabilities({ comments: false, mentioning_filter: false }),
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
    expect(screen.queryByRole("radio", { name: "Mentioning me" })).toBeNull();
    fireEvent.click(await screen.findByRole("button", { name: /Rate limit the import/ }));
    expect(await screen.findByRole("button", { name: /Link to this thread/ })).toBeTruthy();
    expect(screen.queryByLabelText("New comment")).toBeNull();
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
    expect(screen.getByRole("radio", { name: "Open" })).toBeTruthy();
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
    expect(screen.queryByRole("radio", { name: "Mentioning me" })).toBeNull();
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
    fireEvent.click(screen.getByRole("radio", { name: "Closed" }));
    await waitFor(() =>
      expect(
        requestsOfType("forge_list_issues").some(
          (value) => value.type === "forge_list_issues" && value.filter === "closed",
        ),
      ).toBe(true),
    );
    expect(await screen.findByText("#12")).toBeTruthy();
    fireEvent.click(screen.getByRole("radio", { name: "Closed" }));
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

  it("lists two linked pull requests that share a number", async () => {
    mock.state.detailPulls = [
      {
        number: 7,
        title: "Fix flicker in the API",
        state: "open",
        web_url: "https://github.com/acme/api/pull/7",
      },
      {
        number: 7,
        title: "Fix flicker in the web client",
        state: "open",
        web_url: "https://github.com/acme/web/pull/7",
      },
    ];
    const duplicateKey = vi.spyOn(console, "error").mockImplementation(() => undefined);
    await openIssue();
    expect(await screen.findByText("Fix flicker in the API")).toBeTruthy();
    expect(screen.getByText("Fix flicker in the web client")).toBeTruthy();
    // Keying by the provider number would collapse or duplicate the row.
    expect(duplicateKey.mock.calls.some((call) => String(call[0]).includes("same key"))).toBe(
      false,
    );
    duplicateKey.mockRestore();
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

  it("shows Bitbucket's one note when no account serves the project", async () => {
    const bitbucketRepo: ForgeRepoRef = {
      forge: "bitbucket",
      base_url: "https://bitbucket.org",
      owner: "acme",
      repo: "widget",
    };
    resetTracker({
      accounts: [],
      capabilities: [capabilityEntry("bitbucket", issueCapabilities({ read: false }))],
      repos: { "/work/widget": bitbucketRepo },
    });
    render(<IssuesPage />);
    expect(await screen.findByText(/Bitbucket Cloud has no issue API/)).toBeTruthy();
    // The removed tracker is already explained; "no account matches" would
    // be a second, misleading note for the same project.
    expect(screen.queryByText(/No Bitbucket account matches this repository/)).toBeNull();
    expect(screen.queryByText(/account matches this repository/)).toBeNull();
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

  it("keeps Load more when a page fails temporarily", async () => {
    resetTracker({ hasMore: { widget: true } });
    mock.state.failPage2 = true;
    render(<IssuesPage />);
    await screen.findByText("#12");
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    expect(await screen.findByText("the tracker is briefly unavailable")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Load more" })).toBeTruthy();

    // The same page can be tried again once the tracker answers.
    mock.state.failPage2 = false;
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    expect(await screen.findByText("Stale page row")).toBeTruthy();
  });

  it("drops a page that arrives after its project is hidden", async () => {
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
      hasMore: { widget: true, other: true },
    });
    let release = () => {};
    mock.state.page2Gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    render(<IssuesPage />);
    await screen.findAllByText("#12");
    const before = requestsOfType("forge_list_issues").filter(
      (value) => value.type === "forge_list_issues" && value.page === 1,
    ).length;
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));

    // Hold the relisting that follows hiding the project, so the delayed page
    // lands first.
    let releaseList = () => {};
    mock.state.page1Gate = {
      repo: "other",
      promise: new Promise<void>((resolve) => {
        releaseList = resolve;
      }),
    };
    fireEvent.pointerDown(screen.getByRole("button", { name: "Projects" }), {
      button: 0,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitemcheckbox", { name: "widget" }));
    // It must not add the hidden project's rows back.
    await waitFor(() => {
      const now = requestsOfType("forge_list_issues").filter(
        (value) => value.type === "forge_list_issues" && value.page === 1,
      ).length;
      expect(now).toBeGreaterThan(before);
    });
    release();
    // The relisting is still held, so the hidden project's delayed page has
    // landed and there has been time to render it.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(screen.queryByText("Stale page row")).toBeNull();
    releaseList();
    await waitFor(() => expect(screen.queryByText("Reading trackers…")).toBeNull());
  });

  it("drops a page two that was in flight when the account changed", async () => {
    resetTracker({
      accounts: [account(), account({ account_id: "acct_2", username: "bob" })],
      hasMore: { widget: true },
    });
    let releasePage = () => {};
    mock.state.page2Gate = new Promise<void>((resolve) => {
      releasePage = resolve;
    });
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));

    // Hold the new account's relisting, so the stale page would stay on
    // screen if it were spliced into the old group.
    let releaseList = () => {};
    mock.state.page1Gate = {
      repo: "widget",
      account: "acct_2",
      promise: new Promise<void>((resolve) => {
        releaseList = resolve;
      }),
    };
    fireEvent.pointerDown(screen.getByRole("button", { name: "Projects" }), {
      button: 0,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitemradio", { name: "bob" }));
    await waitFor(() =>
      expect(
        requestsOfType("forge_list_issues").some(
          (value) => value.type === "forge_list_issues" && value.account_id === "acct_2",
        ),
      ).toBe(true),
    );

    releasePage();
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(screen.queryByText("Stale page row")).toBeNull();

    releaseList();
    await waitFor(() => expect(screen.queryByText("GitHub · alice")).toBeNull());
  });

  it("drops a page that arrives after the Linear team changed", async () => {
    const mapping = (team_id: string, team_key: string, team_name: string) => ({
      project_id: "proj_1",
      account_id: "lin_1",
      team_id,
      team_key,
      team_name,
    });
    resetTracker({
      mappings: [mapping("team_1", "ENG", "Engineering")],
      linearAccounts: [
        { account_id: "lin_1", name: "Acme", email: null, created_at: "2026-01-01T00:00:00Z" },
      ],
    });
    let releasePage = () => {};
    mock.state.page2Gate = new Promise<void>((resolve) => {
      releasePage = resolve;
    });
    const { rerender } = render(<IssuesPage />);
    expect(await screen.findByText("Linear · Acme")).toBeTruthy();
    expect(await screen.findByText("ENG-1")).toBeTruthy();
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));

    // The team was changed in Settings. The page learns of it on the next
    // start of the connection, and the new team's first page is held so the
    // old team's page lands while the listing is still being replaced.
    let releaseList = () => {};
    mock.state.linearPageGate = new Promise<void>((resolve) => {
      releaseList = resolve;
    });
    mock.state.mappings = [mapping("team_2", "OPS", "Operations")];
    mock.state.connection = { status: "connecting" };
    rerender(<IssuesPage />);
    mock.state.connection = { status: "ready" };
    rerender(<IssuesPage />);
    await waitFor(() => expect(requestsOfType("linear_list_issues").length).toBeGreaterThan(3));

    releasePage();
    await new Promise((resolve) => setTimeout(resolve, 50));
    // The old team's page belongs to a listing that no longer exists.
    expect(screen.queryByText("Stale Linear page row")).toBeNull();

    releaseList();
    expect(await screen.findByText("OPS-2")).toBeTruthy();
    expect(screen.queryByText("ENG-1")).toBeNull();
  });

  it("does not page a group that a new listing replaced", async () => {
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
      repos: {
        "/work/widget": repo,
        "/work/enterprise": {
          ...repo,
          base_url: "https://github.example.test",
          repo: "enterprise",
        },
      },
      hasMore: { widget: true, enterprise: true },
    });
    render(<IssuesPage />);
    await screen.findByText("GitHub · alice");
    await screen.findByText("GitHub · bob");
    const firstPages = requestsOfType("forge_list_issues").filter(
      (value) => value.type === "forge_list_issues" && value.page === 1,
    ).length;

    // Hold the relisting that hiding the project starts, so the superseded
    // group is still on screen when the test clicks it.
    let releaseList = () => {};
    mock.state.page1Gate = {
      repo: "enterprise",
      promise: new Promise<void>((resolve) => {
        releaseList = resolve;
      }),
    };
    fireEvent.pointerDown(screen.getByRole("button", { name: "Projects" }), {
      button: 0,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitemcheckbox", { name: "widget" }));
    await waitFor(() => {
      const now = requestsOfType("forge_list_issues").filter(
        (value) => value.type === "forge_list_issues" && value.page === 1,
      ).length;
      expect(now).toBeGreaterThan(firstPages);
    });
    // An open menu marks the page behind it aria-hidden; close it so the
    // groups are part of the accessibility tree again.
    fireEvent.keyDown(document.body, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());

    const hiddenGroup = screen.getByRole("region", { name: "GitHub · alice" });
    const more = within(hiddenGroup).getByRole("button", { name: "Load more" });
    expect((more as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(more);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(
      requestsOfType("forge_list_issues").filter(
        (value) => value.type === "forge_list_issues" && value.page === 2,
      ),
    ).toHaveLength(0);

    releaseList();
    await waitFor(() => expect(screen.queryByText("Reading trackers…")).toBeNull());
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

describe("the phone layout", () => {
  it("lets the issue filters wrap with thumb-sized targets", () => {
    const source = readFileSync("src/features/issues/IssuesPage.tsx", "utf8");
    // Five involvement filters do not fit across a 390 px screen in one row.
    expect(source).toMatch(/max-\[560px\]:flex-wrap/);
    expect(source).toMatch(/max-\[560px\]:\[&>\[data-slot=segmented-control-item\]\]:min-h-11/);
  });
});

describe("the Issues page in another language", () => {
  afterEach(async () => {
    await setLocalePreference("en");
  });

  it("words the page and the issue pane in Japanese, and keeps tracker text as sent", async () => {
    resetTracker({
      detailLabels: [
        { name: "bug", color: null },
        { name: "frontend", color: null },
      ],
      detailAssignees: [
        { login: "alice", name: null },
        { login: "bob", name: null },
      ],
    });
    await setLocalePreference("ja");
    render(<IssuesPage />);
    expect(await screen.findByRole("heading", { name: "Issue", level: 1 })).toBeTruthy();
    expect(screen.getByRole("radio", { name: "クローズ" })).toBeTruthy();
    expect(screen.getByPlaceholderText("トラッカーを検索")).toBeTruthy();

    fireEvent.click(await screen.findByRole("button", { name: /Dark mode flickers/ }));
    const title = await screen.findByLabelText("Issue のタイトル");
    // The issue's own title is the tracker's, never translated.
    expect((title as HTMLInputElement).value).toBe("Dark mode flickers");
    expect(screen.getByText("オープン", { selector: ".q-issue-state" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Issue をクローズ" })).toBeTruthy();
    // The read-only list of assignees joins the Japanese way; the editable
    // field keeps the comma-separated text it is saved from.
    expect(screen.getByText("alice、bob")).toBeTruthy();
    expect((screen.getByDisplayValue("alice, bob") as HTMLInputElement).value).toBe("alice, bob");
    expect(screen.getByDisplayValue("bug, frontend")).toBeTruthy();
  });

  async function openJapaneseIssueWithLabels() {
    resetTracker({
      detailLabels: [
        { name: "bug", color: null },
        { name: "frontend", color: null },
      ],
      detailAssignees: [
        { login: "alice", name: null },
        { login: "bob", name: null },
      ],
    });
    await setLocalePreference("ja");
    render(<IssuesPage />);
    fireEvent.click(await screen.findByRole("button", { name: /Dark mode flickers/ }));
    // The fetched detail has arrived once its assignees are listed.
    await screen.findByText("alice、bob");
  }

  it("sends nothing when Labels and Assignees lose focus unedited in Japanese", async () => {
    await openJapaneseIssueWithLabels();
    // The fields hold the comma-separated text they are saved from, so an
    // untouched field round-trips to the same list and asks for no update.
    fireEvent.blur(screen.getByLabelText("ラベル"));
    fireEvent.blur(screen.getByLabelText("担当者"));
    // A title edit after the blurs proves the page had time to send anything.
    const title = screen.getByLabelText("Issue のタイトル");
    fireEvent.change(title, { target: { value: "Dark mode flickers on load" } });
    fireEvent.blur(title);
    await waitFor(() => expect(requestsOfType("forge_update_issue")).toHaveLength(1));
    expect(requestsOfType("forge_update_issue")[0]).toMatchObject({
      update: { title: "Dark mode flickers on load" },
    });
  });

  it("saves edited Labels and Assignees as plain lists in Japanese", async () => {
    await openJapaneseIssueWithLabels();
    const labels = screen.getByLabelText("ラベル");
    fireEvent.change(labels, { target: { value: "bug, frontend, ui" } });
    fireEvent.blur(labels);
    await waitFor(() => expect(requestsOfType("forge_update_issue")).toHaveLength(1));
    expect(requestsOfType("forge_update_issue")[0]).toMatchObject({
      update: { labels: ["bug", "frontend", "ui"] },
    });
    const assignees = screen.getByLabelText("担当者");
    fireEvent.change(assignees, { target: { value: "alice, bob, carol" } });
    fireEvent.blur(assignees);
    await waitFor(() => expect(requestsOfType("forge_update_issue")).toHaveLength(2));
    expect(requestsOfType("forge_update_issue")[1]).toMatchObject({
      update: { assignees: ["alice", "bob", "carol"] },
    });
  });

  it("names an unmatched project in Spanish with its name in bold", async () => {
    resetTracker({
      accounts: [
        account({
          account_id: "acct_gl",
          forge: "gitlab",
          base_url: "https://gitlab.com",
          username: "alice",
        }),
      ],
    });
    await setLocalePreference("es");
    render(<IssuesPage />);
    const note = await screen.findByText(/ninguna cuenta de GitHub coincide con este repositorio/);
    expect(note.querySelector("strong")?.textContent).toBe("widget");
  });
});
