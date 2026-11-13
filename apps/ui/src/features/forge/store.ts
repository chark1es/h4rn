import type {
  ClientRequest,
  ForgeAccount,
  ForgeAgentTarget,
  ForgeCapabilityEntry,
  ForgeChecks,
  ForgeChecksState,
  ForgeComment,
  ForgeCommentPage,
  ForgeDeviceFlow,
  ForgeDeviceFlowStatus,
  ForgeDiffPage,
  ForgeFileDiff,
  ForgeHandoff,
  ForgeKind,
  ForgeMergeable,
  ForgePrFilter,
  ForgePrState,
  ForgeReviewState,
  ForgePullRequest,
  ForgeRemote,
  ForgeRepoRef,
  ForgeReviewComment,
  ForgeReviewThread,
  ResponseBody,
} from "@/gen";

/**
 * Typed forge requests for the PR surfaces.
 *
 * The daemon owns every forge call; the UI only names a repo, a filter and a
 * PR number. Keeping the wire details here means the pages never branch on a
 * response tag and the optimistic helpers stay testable without a socket.
 */
export type ForgeRequest = (request: ClientRequest) => Promise<ResponseBody>;

export type ForgeAccounts = {
  accounts: ForgeAccount[];
  capabilities: ForgeCapabilityEntry[];
};

export class ForgeApi {
  constructor(private readonly request: ForgeRequest) {}

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
  ): Promise<ForgePullRequest> {
    const response = await this.call(
      { type: "forge_open_pr", thread_id: threadId, repo, title, body, base },
      "forge_pr_opened",
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

  async addAccount(forge: ForgeKind, baseUrl: string, token: string): Promise<ForgeAccount> {
    const response = await this.call(
      {
        type: "forge_add_account",
        forge,
        base_url: baseUrl,
        token,
        username: null,
        source: "pat",
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

export function accountForRepo(
  accounts: ForgeAccount[],
  repo: ForgeRepoRef,
): ForgeAccount | undefined {
  return accounts.find(
    (account) =>
      account.forge === repo.forge &&
      account.base_url.replace(/\/$/, "") === repo.base_url.replace(/\/$/, ""),
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
