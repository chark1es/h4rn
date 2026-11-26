import { forgeLabel } from "@/features/forge/store";
import type {
  ClientRequest,
  ForgeAccount,
  ForgeCapabilities,
  ForgeCapabilityEntry,
  ForgeIssue,
  ForgeIssueCapabilities,
  ForgeIssueComment,
  ForgeIssueCommentPage,
  ForgeIssueFilter,
  ForgeIssueState,
  ForgeIssueSummary,
  ForgeIssueUpdate,
  ForgeRepoRef,
  IssueScope,
  LinearAccount,
  LinearIssue,
  LinearIssueComment,
  LinearIssueSummary,
  LinearProjectTeam,
  LinearTeam,
  ResponseBody,
  ThreadIssue,
} from "@/gen";
import { formatAge, formatDateTime, i18n } from "@/lib/i18n";

export type IssueRequest = (request: ClientRequest) => Promise<ResponseBody>;

/** One row in the Issues list, whichever tracker it came from. */
export type IssueRow = {
  key: string;
  accountId: string;
  scope: IssueScope;
  /** Forge number as a string, or the Linear issue id. */
  id: string;
  number: string;
  title: string;
  state: ForgeIssueState;
  author: string;
  labels: Array<{ name: string; color: string | null }>;
  assignees: string[];
  commentCount: number;
  updatedAt: string;
  webUrl: string;
  tracker: string;
  repository: string | null;
};

/** The tracker an Issues group belongs to. */
export type TrackerGroup = {
  key: string;
  label: string;
  accountId: string;
  kind: "forge" | "linear";
  rows: IssueRow[];
  hasMore: boolean;
  loading: boolean;
  error: string | null;
};

export class IssueApi {
  constructor(private readonly request: IssueRequest) {}

  private async call<K extends ResponseBody["type"]>(
    request: ClientRequest,
    type: K,
  ): Promise<Extract<ResponseBody, { type: K }>> {
    const response = await this.request(request);
    if (response.type !== type) {
      throw new Error(`expected ${type}, got ${response.type}`);
    }
    return response as Extract<ResponseBody, { type: K }>;
  }

  async forgeAccounts(): Promise<{
    accounts: ForgeAccount[];
    capabilities: ForgeCapabilityEntry[];
    linear: ForgeCapabilities | null;
  }> {
    const response = await this.call({ type: "forge_list_accounts" }, "forge_accounts");
    return {
      accounts: response.accounts,
      capabilities: response.capabilities,
      linear: response.linear ?? null,
    };
  }

  async detectRepo(workspace: string): Promise<ForgeRepoRef | null> {
    const response = await this.call({ type: "forge_detect_repo", workspace }, "forge_repos");
    return response.repos[0]?.repo ?? null;
  }

  async listForgeIssues(
    accountId: string,
    repo: ForgeRepoRef,
    filter: ForgeIssueFilter,
    page: number,
    query: string | null,
  ): Promise<{ issues: ForgeIssueSummary[]; page: number; hasMore: boolean; readOnly: boolean }> {
    const response = await this.call(
      { type: "forge_list_issues", account_id: accountId, repo, filter, page, query },
      "forge_issues",
    );
    return {
      issues: response.issues,
      page: response.page,
      hasMore: response.has_more,
      readOnly: response.read_only,
    };
  }

  async getForgeIssue(
    accountId: string,
    repo: ForgeRepoRef,
    number: number,
  ): Promise<{ issue: ForgeIssue; readOnly: boolean }> {
    const response = await this.call(
      { type: "forge_get_issue", account_id: accountId, repo, number },
      "forge_issue",
    );
    return { issue: response.issue, readOnly: response.read_only };
  }

  async forgeIssueComments(
    accountId: string,
    repo: ForgeRepoRef,
    number: number,
    cursor: string | null,
  ): Promise<ForgeIssueCommentPage> {
    const response = await this.call(
      { type: "forge_list_issue_comments", account_id: accountId, repo, number, cursor },
      "forge_issue_comments",
    );
    return response.page;
  }

  async commentForgeIssue(
    accountId: string,
    repo: ForgeRepoRef,
    number: number,
    body: string,
  ): Promise<ForgeIssueComment> {
    const response = await this.call(
      { type: "forge_comment_issue", account_id: accountId, repo, number, body },
      "forge_issue_comment_posted",
    );
    return response.comment;
  }

  async updateForgeIssue(
    accountId: string,
    repo: ForgeRepoRef,
    number: number,
    update: ForgeIssueUpdate,
  ): Promise<{ issue: ForgeIssue; readOnly: boolean }> {
    const response = await this.call(
      { type: "forge_update_issue", account_id: accountId, repo, number, update },
      "forge_issue",
    );
    return { issue: response.issue, readOnly: response.read_only };
  }

  async linearAccounts(): Promise<LinearAccount[]> {
    const response = await this.call({ type: "linear_list_accounts" }, "linear_accounts");
    return response.accounts;
  }

  async connectLinear(apiKey: string): Promise<LinearAccount[]> {
    const response = await this.call(
      { type: "linear_connect", api_key: apiKey },
      "linear_accounts",
    );
    return response.accounts;
  }

  async removeLinear(accountId: string): Promise<void> {
    await this.call({ type: "linear_remove_account", account_id: accountId }, "ack");
  }

  async linearTeams(accountId: string): Promise<LinearTeam[]> {
    const response = await this.call(
      { type: "linear_list_teams", account_id: accountId },
      "linear_teams",
    );
    return response.teams;
  }

  async projectTeams(): Promise<LinearProjectTeam[]> {
    const response = await this.call({ type: "linear_project_teams" }, "linear_project_teams");
    return response.teams;
  }

  async setProjectTeam(
    projectId: string,
    team: LinearProjectTeam | null,
  ): Promise<LinearProjectTeam[]> {
    const response = await this.call(
      { type: "linear_set_project_team", project_id: projectId, team },
      "linear_project_teams",
    );
    return response.teams;
  }

  async listLinearIssues(
    projectId: string,
    filter: ForgeIssueFilter,
    cursor: string | null,
    query: string | null,
  ): Promise<{
    issues: LinearIssueSummary[];
    nextCursor: string | null;
    readOnly: boolean;
  }> {
    const response = await this.call(
      { type: "linear_list_issues", project_id: projectId, filter, cursor, query },
      "linear_issues",
    );
    return {
      issues: response.issues,
      nextCursor: response.next_cursor,
      readOnly: response.read_only,
    };
  }

  async getLinearIssue(
    accountId: string,
    issueId: string,
    commentsAfter: string | null,
  ): Promise<{ issue: LinearIssue; readOnly: boolean }> {
    const response = await this.call(
      {
        type: "linear_get_issue",
        account_id: accountId,
        issue_id: issueId,
        comments_after: commentsAfter,
      },
      "linear_issue",
    );
    return { issue: response.issue, readOnly: response.read_only };
  }

  async commentLinearIssue(
    accountId: string,
    issueId: string,
    body: string,
  ): Promise<LinearIssueComment> {
    const response = await this.call(
      { type: "linear_comment_issue", account_id: accountId, issue_id: issueId, body },
      "linear_issue_comment_posted",
    );
    return response.comment;
  }

  async threadIssues(threadId: string): Promise<ThreadIssue[]> {
    const response = await this.call(
      { type: "list_thread_issues", thread_id: threadId },
      "thread_issues",
    );
    return response.issues;
  }

  async linkIssue(
    threadId: string,
    accountId: string,
    scope: IssueScope,
    issueId: string,
  ): Promise<ThreadIssue> {
    const response = await this.call(
      { type: "link_issue", thread_id: threadId, account_id: accountId, scope, issue_id: issueId },
      "thread_issue",
    );
    return response.issue;
  }

  async unlinkIssue(threadId: string, scope: IssueScope, issueId: string): Promise<void> {
    await this.call({ type: "unlink_issue", thread_id: threadId, scope, issue_id: issueId }, "ack");
  }
}

export function forgeIssueRow(accountId: string, issue: ForgeIssueSummary): IssueRow {
  return {
    key: `forge:${issue.repo.base_url}:${issue.repo.owner}/${issue.repo.repo}#${issue.number}`,
    accountId,
    scope: { tracker: "forge", repo: issue.repo },
    id: String(issue.number),
    number: `#${issue.number}`,
    title: issue.title,
    state: issue.state,
    author: issue.author,
    labels: issue.labels.map((label) => ({ name: label.name, color: label.color })),
    assignees: issue.assignees.map((person) => person.login),
    commentCount: issue.comment_count,
    updatedAt: issue.updated_at ?? "",
    webUrl: issue.web_url,
    tracker: `${issue.repo.owner}/${issue.repo.repo}`,
    repository: `${issue.repo.owner}/${issue.repo.repo}`,
  };
}

/** Linear's completed, canceled and duplicate families are all closed. */
export function linearStateIsClosed(state: LinearIssueSummary["state_type"]): boolean {
  return state === "completed" || state === "canceled" || state === "duplicate";
}

export function linearIssueRow(accountId: string, issue: LinearIssueSummary): IssueRow {
  return {
    key: `linear:${issue.id}`,
    accountId,
    scope: { tracker: "linear", team_id: issue.team_id },
    id: issue.id,
    number: issue.identifier,
    title: issue.title,
    state: linearStateIsClosed(issue.state_type) ? "closed" : "open",
    author: "",
    labels: issue.labels.map((name) => ({ name, color: null })),
    assignees: issue.assignees,
    commentCount: 0,
    updatedAt: issue.updated_at ?? "",
    webUrl: issue.url,
    tracker: issue.team_key,
    repository: null,
  };
}

/** Stable identity used to tell whether this issue is linked to the thread. */
export function issueKey(scope: IssueScope, id: string): string {
  if (scope.tracker === "forge") {
    return `forge:${scope.repo.base_url}:${scope.repo.owner}/${scope.repo.repo}#${id}`;
  }
  return `linear:${id}`;
}

export function issueStateLabel(state: ForgeIssueState): string {
  return state === "closed" ? i18n.t("issues:state.closed") : i18n.t("issues:state.open");
}

export function relativeIssueAge(iso: string | null): string {
  if (!iso) return "";
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return "";
  const now = Date.now();
  const elapsed = Math.max(0, now - then);
  if (elapsed < 60_000) return i18n.t("issues:age.now");
  // Older than a month reads better as the date, in the active language.
  if (elapsed >= 30 * 86_400_000) {
    return formatDateTime(then, { year: "numeric", month: "numeric", day: "numeric" });
  }
  return i18n.t("issues:age.ago", { age: formatAge(then, now) });
}

/**
 * Filters the page offers, in the order the brief names them. The page names
 * each one through `issues:filters.<key>`.
 */
export const ISSUE_FILTERS: ForgeIssueFilter[] = [
  "open",
  "closed",
  "assigned",
  "created",
  "mentioning",
];

/** The daemon's error message, or the raw string, stated plainly. */
export function issueError(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/** One group per tracker account, in account order. */
export function groupLabel(
  kind: "forge" | "linear",
  account: ForgeAccount | LinearAccount,
): string {
  if (kind === "linear") {
    const linear = account as LinearAccount;
    return `Linear · ${linear.name}`;
  }
  const forge = account as ForgeAccount;
  return `${forgeLabel(forge.forge)} · ${forge.username ?? forge.base_url}`;
}

/** A tracker key that never collides between a forge and Linear. */
export function trackerKey(kind: "forge" | "linear", accountId: string): string {
  return `${kind}:${accountId}`;
}

const NO_ISSUE_CAPABILITIES: ForgeIssueCapabilities = {
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
};

/** What the daemon says this connection's issue tracker can do. */
export function issueCapabilitiesFor(
  kind: "forge" | "linear",
  account: ForgeAccount | LinearAccount | undefined,
  entries: ForgeCapabilityEntry[],
  linear: ForgeCapabilities | null | undefined = null,
): ForgeIssueCapabilities {
  if (kind === "linear") {
    // Linear never appears in the forge list; its capabilities arrive in
    // their own field so older clients keep reading a forge-only list.
    return linear?.issues ?? NO_ISSUE_CAPABILITIES;
  }
  if (!account) return NO_ISSUE_CAPABILITIES;
  const forge = (account as ForgeAccount).forge;
  return (
    entries.find((entry) => entry.forge === forge)?.capabilities.issues ?? NO_ISSUE_CAPABILITIES
  );
}
