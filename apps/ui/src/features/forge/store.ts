import { OPEN_PR_TIMEOUT_MS } from "@/features/git-actions/store";
import type {
  ClientRequest,
  ForgeAccount,
  ForgeAgentTarget,
  ForgeCapabilityEntry,
  ForgeChecks,
  ForgeChecksState,
  ForgeComment,
  ForgeCommentPage,
  ForgeCommits,
  ForgeDeviceFlow,
  ForgeDeviceFlowStatus,
  ForgeDiffPage,
  ForgeFileDiff,
  ForgeHandoff,
  ForgeKind,
  ForgeMergeable,
  ForgePrFilter,
  ForgePrState,
  ForgePullRequest,
  ForgeRemote,
  ForgeRepoRef,
  ForgeReviewComment,
  ForgeReviewState,
  ForgeReviewThread,
  PrLinkedThread,
  ResponseBody,
  ThreadPullRequest,
} from "@/gen";

/**
 * Typed forge requests for the PR surfaces.
 *
 * The daemon owns every forge call; the UI only names a repo, a filter and a
 * PR number. Keeping the wire details here means the pages never branch on a
 * response tag and the optimistic helpers stay testable without a socket.
 */
export type ForgeRequest = (
  request: ClientRequest,
  options?: { timeoutMs?: number },
) => Promise<ResponseBody>;

export type ForgeAccounts = {
  accounts: ForgeAccount[];
  capabilities: ForgeCapabilityEntry[];
};

export class ForgeApi {
  constructor(private readonly request: ForgeRequest) {}

  private async call<K extends ResponseBody["type"]>(
    request: ClientRequest,
    type: K,
    options?: { timeoutMs?: number },
  ): Promise<Extract<ResponseBody, { type: K }>> {
    const response = options ? await this.request(request, options) : await this.request(request);
    if (response.type !== type) {
      throw new Error(`expected ${type}, got ${response.type}`);
    }
    return response as Extract<ResponseBody, { type: K }>;
  }

  async accounts(): Promise<ForgeAccounts> {
    const response = await this.call({ type: "forge_list_accounts" }, "forge_accounts");
    return { accounts: response.accounts, capabilities: response.capabilities };
  }

  async detectRepo(workspace: string): Promise<ForgeRemote[]> {
    const response = await this.call({ type: "forge_detect_repo", workspace }, "forge_repos");
    return response.repos;
  }

  async listPrs(
    repo: ForgeRepoRef,
    filter: ForgePrFilter,
    page = 1,
  ): Promise<{ prs: ForgePullRequest["summary"][]; page: number; hasMore: boolean }> {
    const response = await this.call(
      { type: "forge_list_prs", account_id: null, repo, filter, page },
      "forge_prs",
    );
    return { prs: response.prs, page: response.page, hasMore: response.has_more };
  }

  async getPr(repo: ForgeRepoRef, number: number): Promise<ForgePullRequest> {
    const response = await this.call(
      { type: "forge_get_pr", account_id: null, repo, number },
      "forge_pr",
    );
    return response.pr;
  }

  async getDiff(
    repo: ForgeRepoRef,
    number: number,
    page = 1,
    perPage: number | null = null,
  ): Promise<ForgeDiffPage> {
    const response = await this.call(
      {
        type: "forge_get_pr_diff",
        account_id: null,
        repo,
        number,
        page,
        per_page: perPage,
      },
      "forge_diff",
    );
    return response.diff;
  }

  async comments(repo: ForgeRepoRef, number: number): Promise<ForgeCommentPage> {
    const response = await this.call(
      { type: "forge_list_pr_comments", account_id: null, repo, number },
      "forge_comments",
    );
    return response.page;
  }

  async checks(repo: ForgeRepoRef, number: number): Promise<ForgeChecks> {
    const response = await this.call(
      { type: "forge_list_checks", account_id: null, repo, number },
      "forge_checks",
    );
    return response.checks;
  }

  async commits(repo: ForgeRepoRef, number: number): Promise<ForgeCommits> {
    const response = await this.call(
      { type: "forge_list_pr_commits", account_id: null, repo, number },
      "forge_commits",
    );
    return response.commits;
  }

  /** Re-runs the failed checks; the answer is the refreshed checks snapshot. */
  async rerunChecks(repo: ForgeRepoRef, number: number): Promise<ForgeChecks> {
    const response = await this.call(
      { type: "forge_rerun_checks", account_id: null, repo, number },
      "forge_checks",
    );
    return response.checks;
  }

  async reply(
    repo: ForgeRepoRef,
    number: number,
    threadId: string | null,
    commentId: string,
    body: string,
  ): Promise<ForgeReviewComment> {
    const response = await this.call(
      {
        type: "forge_reply",
        account_id: null,
        repo,
        number,
        thread_id: threadId,
        comment_id: commentId,
        body,
      },
      "forge_review_comment_posted",
    );
    return response.comment;
  }

  async comment(repo: ForgeRepoRef, number: number, body: string): Promise<ForgeComment> {
    const response = await this.call(
      { type: "forge_comment", account_id: null, repo, number, body },
      "forge_comment_posted",
    );
    return response.comment;
  }

  async openPr(
    threadId: string,
    repo: ForgeRepoRef | null,
    title: string,
    body: string,
    base: string,
    draft = false,
  ): Promise<ForgePullRequest> {
    // The daemon pushes the branch before it asks the forge, so this waits as
    // long as a push may take rather than timing out while the work continues.
    const response = await this.call(
      { type: "forge_open_pr", thread_id: threadId, repo, title, body, base, draft },
      "forge_pr_opened",
      { timeoutMs: OPEN_PR_TIMEOUT_MS },
    );
    return response.pr;
  }

  async handToAgent(
    repo: ForgeRepoRef,
    number: number,
    threadId: string | null,
    target: ForgeAgentTarget | null,
  ): Promise<ForgeHandoff> {
    const response = await this.call(
      {
        type: "forge_hand_pr_to_agent",
        account_id: null,
        repo,
        number,
        thread_id: threadId,
        target,
      },
      "forge_handoff",
    );
    return response.handoff;
  }

  async addAccount(
    forge: ForgeKind,
    baseUrl: string,
    token: string,
    username: string | null = null,
    repository: ForgeRepoRef | null = null,
  ): Promise<ForgeAccount> {
    const response = await this.call(
      {
        type: "forge_add_account",
        forge,
        base_url: baseUrl,
        token,
        username,
        source: "pat",
        // A repository-scoped Bitbucket access token cannot read `/user`; the
        // daemon validates it against this detected repository instead.
        repository,
      },
      "forge_account",
    );
    return response.account;
  }

  async removeAccount(accountId: string): Promise<void> {
    await this.call({ type: "forge_remove_account", account_id: accountId }, "ack");
  }

  async importCli(forge: ForgeKind, baseUrl: string | null): Promise<ForgeAccount> {
    const response = await this.call(
      { type: "forge_import_cli", forge, base_url: baseUrl },
      "forge_account",
    );
    return response.account;
  }

  async startDeviceFlow(baseUrl: string | null): Promise<ForgeDeviceFlow> {
    const response = await this.call(
      { type: "forge_device_flow_start", base_url: baseUrl },
      "forge_device_flow",
    );
    return response.flow;
  }

  async pollDeviceFlow(flowId: string): Promise<ForgeDeviceFlowStatus> {
    const response = await this.call(
      { type: "forge_device_flow_poll", flow_id: flowId },
      "forge_device_flow_status",
    );
    return response.status;
  }

  async threadPullRequests(threadId: string): Promise<ThreadPullRequest[]> {
    const response = await this.call(
      { type: "list_thread_pull_requests", thread_id: threadId },
      "thread_pull_requests",
    );
    return response.pulls;
  }

  async prThreads(repo: ForgeRepoRef, number: number): Promise<PrLinkedThread[]> {
    const response = await this.call({ type: "list_pr_threads", repo, number }, "pr_threads");
    return response.threads;
  }

  async linkPullRequest(
    threadId: string,
    repo: ForgeRepoRef,
    number: number,
    accountId: string | null = null,
  ): Promise<ThreadPullRequest> {
    const response = await this.call(
      { type: "link_pull_request", thread_id: threadId, account_id: accountId, repo, number },
      "thread_pull_request",
    );
    return response.pull;
  }

  async unlinkPullRequest(threadId: string, repo: ForgeRepoRef, number: number): Promise<void> {
    await this.call({ type: "unlink_pull_request", thread_id: threadId, repo, number }, "ack");
  }

  async watchPullRequest(threadId: string, repo: ForgeRepoRef, number: number): Promise<void> {
    await this.call({ type: "watch_pull_request", thread_id: threadId, repo, number }, "ack");
  }

  async unwatchPullRequest(threadId: string, repo: ForgeRepoRef, number: number): Promise<void> {
    await this.call({ type: "unwatch_pull_request", thread_id: threadId, repo, number }, "ack");
  }
}

/** Appends an optimistic comment unless its id is already present. */
export function appendComment<T extends { comment_id: string }>(items: T[], item: T): T[] {
  return items.some((candidate) => candidate.comment_id === item.comment_id)
    ? items
    : [...items, item];
}

/** Replaces a temporary id with the daemon's authoritative comment. */
export function replaceComment<T extends { comment_id: string }>(
  items: T[],
  temporaryId: string,
  item: T,
): T[] {
  return items.map((candidate) => (candidate.comment_id === temporaryId ? item : candidate));
}

/** Removes a failed optimistic comment, or every copy of one id. */
export function removeComment<T extends { comment_id: string }>(
  items: T[],
  commentId: string,
): T[] {
  return items.filter((candidate) => candidate.comment_id !== commentId);
}

/** Adds one diff page without duplicating a file already loaded. */
export function mergeDiffPages(existing: ForgeFileDiff[], page: ForgeDiffPage): ForgeFileDiff[] {
  const seen = new Set(existing.map((file) => file.path));
  return [...existing, ...page.files.filter((file) => !seen.has(file.path))];
}

/**
 * Whether two account base URLs name the same forge account.
 *
 * Mirrors the daemon's `base_urls_match`: trailing slashes and host case are
 * ignored, and an Azure DevOps legacy organization is the same account with or
 * without the historical `DefaultCollection` segment
 * (`https://acme.visualstudio.com` ≡
 * `https://acme.visualstudio.com/DefaultCollection`). Without this, a checkout
 * using the alias would not find its account and Open PR stays disabled.
 */
export function baseUrlsMatch(forge: ForgeKind, a: string, b: string): boolean {
  return normalizeBaseUrl(forge, a).toLowerCase() === normalizeBaseUrl(forge, b).toLowerCase();
}

function normalizeBaseUrl(forge: ForgeKind, value: string): string {
  const trimmed = value.trim().replace(/\/+$/, "");
  if (forge !== "azure_devops") return trimmed;
  const suffix = "/defaultcollection";
  if (!trimmed.toLowerCase().endsWith(suffix)) return trimmed;
  const base = trimmed.slice(0, -suffix.length);
  try {
    if (new URL(base).hostname.toLowerCase().endsWith(".visualstudio.com")) return base;
  } catch {
    // Not a URL: compare it exactly rather than guessing at an alias.
  }
  return trimmed;
}

export function accountForRepo(
  accounts: ForgeAccount[],
  repo: ForgeRepoRef,
): ForgeAccount | undefined {
  return accountsForRepo(accounts, repo)[0];
}

/**
 * Every account that can read `repo`.
 *
 * More than one means the person must choose: two accounts on the same host
 * are both valid credentials, and the daemon refuses to guess when neither is
 * named.
 */
export function accountsForRepo(accounts: ForgeAccount[], repo: ForgeRepoRef): ForgeAccount[] {
  return accounts.filter(
    (account) =>
      account.forge === repo.forge && baseUrlsMatch(account.forge, account.base_url, repo.base_url),
  );
}

export function forgeLabel(forge: ForgeKind): string {
  switch (forge) {
    case "github":
      return "GitHub";
    case "gitlab":
      return "GitLab";
    case "gitea":
      return "Gitea";
    case "forgejo":
      return "Forgejo";
    case "bitbucket":
      return "Bitbucket Cloud";
    case "azure_devops":
      return "Azure DevOps";
  }
}

export function stateLabel(state: ForgePrState): {
  label: string;
  tone: "ok" | "warn" | "danger" | "";
} {
  switch (state) {
    case "open":
      return { label: "Open", tone: "ok" };
    case "draft":
      return { label: "Draft", tone: "" };
    case "merged":
      return { label: "Merged", tone: "ok" };
    case "closed":
      return { label: "Closed", tone: "danger" };
  }
}

export function checksLabel(state: ForgeChecksState, passed: number, total: number): string {
  switch (state) {
    case "passing":
      return `${passed} of ${total} checks passed`;
    case "failing":
      return `${total - passed} failing`;
    case "pending":
      return "Checks running";
    case "unknown":
      return "No checks";
  }
}

/**
 * A list row's check state, without per-check counts.
 *
 * The list endpoint only reports the aggregate, so saying "0 of 0 checks
 * passed" would be a lie; `null` means the row says nothing.
 */
export function summaryChecksLabel(state: ForgeChecksState): string | null {
  switch (state) {
    case "passing":
      return "All checks passed";
    case "failing":
      return "Checks failing";
    case "pending":
      return "Checks running";
    case "unknown":
      return null;
  }
}

/** The list row's review chip, or `null` when there is nothing to say. */
export function reviewChip(state: ForgeReviewState): {
  label: string;
  tone: "ok" | "warn" | "danger" | "you" | "";
} | null {
  switch (state) {
    case "changes_requested":
      return { label: "Changes requested", tone: "warn" };
    case "approved":
      return { label: "Approved", tone: "ok" };
    case "pending":
      return { label: "Review requested", tone: "you" };
    case "commented":
      return { label: "Review comments", tone: "" };
    case "dismissed":
      return null;
  }
}

/** The mergeability line for the detail header. */
export function mergeableLabel(state: ForgeMergeable): {
  label: string;
  tone: "ok" | "warn" | "danger" | "";
} | null {
  switch (state) {
    case "mergeable":
      return { label: "No conflicts", tone: "ok" };
    case "conflicting":
      return { label: "Merge conflicts", tone: "danger" };
    case "unknown":
      return null;
  }
}

export type PatchRow = {
  kind: "hunk" | "add" | "del" | "context";
  text: string;
  oldLine: number | null;
  newLine: number | null;
};

/**
 * A forgiving unified-patch parser for the provider's `patch` string.
 *
 * Forges disagree about the envelope (some include `diff --git`, some start at
 * the hunk); only the hunk headers, signs and content matter to the view.
 */
export function parsePatch(patch: string | null): PatchRow[] {
  if (!patch) return [];
  const rows: PatchRow[] = [];
  let oldLine = 0;
  let newLine = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("@@")) {
      const match = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      oldLine = Number(match?.[1] ?? 0);
      newLine = Number(match?.[2] ?? 0);
      rows.push({ kind: "hunk", text: line, oldLine: null, newLine: null });
      continue;
    }
    if (
      line.startsWith("diff --git") ||
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ") ||
      line.startsWith("new file") ||
      line.startsWith("deleted file") ||
      line.startsWith("similarity index") ||
      line.startsWith("rename ")
    ) {
      continue;
    }
    if (line.startsWith("+")) {
      rows.push({ kind: "add", text: line.slice(1), oldLine: null, newLine: newLine++ });
    } else if (line.startsWith("-")) {
      rows.push({ kind: "del", text: line.slice(1), oldLine: oldLine++, newLine: null });
    } else if (line.startsWith(" ")) {
      rows.push({ kind: "context", text: line.slice(1), oldLine: oldLine++, newLine: newLine++ });
    } else if (line === "\\ No newline at end of file") {
    } else if (line !== "") {
      rows.push({ kind: "context", text: line, oldLine: oldLine++, newLine: newLine++ });
    }
  }
  return rows;
}

/** Finds the inline thread anchored to a file and line, when one exists. */
export function threadForLine(
  threads: ForgeReviewThread[],
  path: string,
  line: number | null,
): ForgeReviewThread | undefined {
  return threads.find((thread) => thread.path === path && (line === null || thread.line === line));
}

/** What the "Re-run failed" action should do for one checks snapshot. */
export type RerunAction =
  | { kind: "ready" }
  | { kind: "nothing"; reason: string }
  | { kind: "unsupported"; reason: string };

/**
 * The daemon says whether the forge can re-run checks and why not; this only
 * adds "there is nothing failed to re-run".
 */
export function rerunAction(checks: ForgeChecks): RerunAction {
  if (!checks.rerun.supported) {
    return {
      kind: "unsupported",
      reason: checks.rerun.reason ?? "This forge cannot re-run checks",
    };
  }
  if (checks.failed === 0) return { kind: "nothing", reason: "No checks failed" };
  return { kind: "ready" };
}

/** The check "View log" opens: the first failing one with a link, else any link. */
export function logCheck(checks: ForgeChecks): ForgeChecks["checks"][number] | null {
  const linked = checks.checks.filter((check) => check.details_url);
  return linked.find((check) => check.status === "failure") ?? linked[0] ?? null;
}

/** The faint "required" mark after a check name, or `null` when unknown or optional. */
export function requiredMark(check: ForgeChecks["checks"][number]): string | null {
  return check.required === true ? "required" : null;
}

/** First 7 characters of a commit id, the way forges abbreviate it. */
export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

/** The browser URL of one pull request or merge request. */
export function prWebUrl(repo: ForgeRepoRef, number: number): string {
  const root = repo.base_url.replace(/\/$/, "");
  const base =
    repo.forge === "azure_devops"
      ? `${root}/${repo.owner}/_git/${repo.repo}`
      : `${root}/${repo.owner}/${repo.repo}`;
  switch (repo.forge) {
    case "github":
      return `${base}/pull/${number}`;
    case "gitlab":
      return `${base}/-/merge_requests/${number}`;
    case "gitea":
    case "forgejo":
      return `${base}/pulls/${number}`;
    case "bitbucket":
      return `${base}/pull-requests/${number}`;
    case "azure_devops":
      return `${base}/pullrequest/${number}`;
  }
}

/** A pull request named by a pasted URL. */
export type LinkedPullReference = { repo: ForgeRepoRef; number: number };

function publicForgeForHost(host: string): ForgeKind | null {
  if (host === "bitbucket.org") return "bitbucket";
  if (host === "dev.azure.com" || host.endsWith(".visualstudio.com")) return "azure_devops";
  switch (host) {
    case "github.com":
      return "github";
    case "gitlab.com":
      return "gitlab";
    case "gitea.com":
      return "gitea";
    case "codeberg.org":
      return "forgejo";
    default:
      return null;
  }
}

/**
 * The workspace repository a URL belongs to, when its host is already known.
 *
 * Azure DevOps needs the organization path to match as well, because every
 * organization shares `dev.azure.com`.
 */
function detectedRepo(known: ForgeRepoRef[], url: URL, host: string): ForgeRepoRef | undefined {
  return known.find((repo) => {
    try {
      const base = new URL(repo.base_url);
      if (base.hostname.toLowerCase() !== host) return false;
      if (repo.forge !== "azure_devops") return true;
      const prefix = base.pathname.replace(/\/$/, "").toLowerCase();
      return prefix === "" || url.pathname.toLowerCase().startsWith(`${prefix}/`);
    } catch {
      return false;
    }
  });
}

/**
 * Parses a forge pull request URL.
 *
 * Public hosts are recognised without configuration. A custom host works when
 * the workspace's detected repository names it, so an ssh alias or a
 * self-hosted forge is never guessed. Unknown hosts return `null` rather than
 * pointing a link at the wrong server.
 */
export function parsePullRequestUrl(
  value: string,
  known: ForgeRepoRef[] = [],
): LinkedPullReference | null {
  let url: URL;
  try {
    url = new URL(value.trim());
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;

  const host = url.hostname.toLowerCase();
  const origin = `${url.protocol}//${url.host}`;
  const detected = detectedRepo(known, url, host);
  const forge = detected?.forge ?? publicForgeForHost(host);
  if (!forge) return null;

  const segments = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  const baseUrl = (detected?.base_url ?? origin).replace(/\/$/, "");

  if (forge === "azure_devops") {
    const git = segments.indexOf("_git");
    if (git < 1 || segments[git + 2] !== "pullrequest") return null;
    const repo = segments[git + 1];
    const owner = segments[git - 1];
    const number = Number(segments[git + 3]);
    if (!owner || !repo || !Number.isInteger(number) || number <= 0) return null;
    const organization = segments.slice(0, git - 1).join("/");
    const base = detected ? baseUrl : organization ? `${origin}/${organization}` : origin;
    return { repo: { forge, base_url: base, owner, repo }, number };
  }

  const marker =
    forge === "gitlab"
      ? "merge_requests"
      : forge === "github"
        ? "pull"
        : forge === "bitbucket"
          ? "pull-requests"
          : "pulls";
  const index = segments.indexOf(marker);
  if (index < 2) return null;

  if (forge === "gitlab") {
    if (segments[index - 1] !== "-" || index < 3) return null;
    const owner = segments.slice(0, index - 2).join("/");
    const repo = segments[index - 2];
    const number = Number(segments[index + 1]);
    if (!owner || !repo || !Number.isInteger(number) || number <= 0) return null;
    return { repo: { forge, base_url: baseUrl, owner, repo }, number };
  }

  if (index !== 2) return null;
  const [owner, repo] = segments;
  const number = Number(segments[index + 1]);
  if (!owner || !repo || !Number.isInteger(number) || number <= 0) return null;
  return { repo: { forge, base_url: baseUrl, owner, repo }, number };
}

/** `#7 Open` for a linked PR, or `#7` when the last read failed. */
export function linkedPullLabel(pull: ThreadPullRequest): string {
  if (!pull.summary) return `#${pull.number}`;
  return `#${pull.number} ${stateLabel(pull.summary.state).label}`;
}

/** A badge tone: only a closed PR earns colour; open and merged stay quiet. */
export function linkedPullTone(pull: ThreadPullRequest): string {
  switch (pull.summary?.state) {
    case "closed":
      return "danger";
    case "merged":
      return "ok";
    default:
      return "";
  }
}
