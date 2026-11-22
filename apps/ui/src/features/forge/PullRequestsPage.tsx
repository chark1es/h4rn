import {
  CheckCircle2Icon,
  CircleAlertIcon,
  CircleDashedIcon,
  CircleXIcon,
  ExternalLinkIcon,
  GitPullRequestIcon,
  HandIcon,
  LoaderCircleIcon,
  MessageSquareIcon,
  MessageSquareTextIcon,
  RotateCwIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Markdown } from "@/components/thread/Markdown";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  accountForRepo,
  appendComment,
  checksLabel,
  ForgeApi,
  forgeLabel,
  logCheck,
  mergeableLabel,
  mergeDiffPages,
  parsePatch,
  removeComment,
  replaceComment,
  requiredMark,
  rerunAction,
  reviewChip,
  shortSha,
  stateLabel,
  summaryChecksLabel,
} from "@/features/forge/store";
import type {
  ForgeAccount,
  ForgeCapabilityEntry,
  ForgeChecks,
  ForgeComment,
  ForgeCommentPage,
  ForgeCommits,
  ForgeDiffPage,
  ForgeFileDiff,
  ForgePrFilter,
  ForgePullRequest,
  ForgeRemote,
  ForgeRepoRef,
  ForgeReviewThread,
  ThreadSummary,
} from "@/gen";
import { glyphForThread } from "@/lib/agent";
import { userSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { getClient, navigate, openThread, pushToast, useApp } from "@/lib/store/appStore";

const api = () => new ForgeApi((request) => getClient().request(request));

type FilterKey = "open" | "mine" | "review" | "branch";

const FILTERS: Array<{ key: FilterKey; label: string }> = [
  { key: "open", label: "Open" },
  { key: "mine", label: "Mine" },
  { key: "review", label: "Review" },
];

function filterFor(key: FilterKey, branch: string | null): ForgePrFilter {
  switch (key) {
    case "mine":
      return { type: "mine" };
    case "review":
      return { type: "review_requested" };
    case "branch":
      return branch ? { type: "by_branch", branch } : { type: "open" };
    default:
      return { type: "open" };
  }
}

function repoFromQuery(): ForgeRepoRef | null {
  if (typeof window === "undefined") return null;
  const query = new URLSearchParams(window.location.search);
  const full = query.get("repo");
  if (!full) return null;
  const [owner, repo] = full.split("/");
  const forge = query.get("forge");
  const base = query.get("base");
  if (!owner || !repo || !forge || !base) return null;
  if (
    forge !== "github" &&
    forge !== "gitlab" &&
    forge !== "gitea" &&
    forge !== "forgejo" &&
    forge !== "bitbucket" &&
    forge !== "azure_devops"
  ) {
    return null;
  }
  return { forge, base_url: base.replace(/\/$/, ""), owner, repo };
}

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

function duration(ms: bigint | null): string {
  if (ms === null) return "";
  const seconds = Number(ms) / 1000;
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${String(Math.round(seconds % 60)).padStart(2, "0")}s`;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/** One tab's own load failure, with the rest of the PR still on screen. */
function TabError({
  title,
  message,
  onRetry,
}: {
  title: string;
  message: string;
  onRetry: () => void;
}) {
  return (
    <div className="q-empty" style={{ margin: "18px 0" }}>
      <h3>{title}</h3>
      <p>{message}</p>
      <button type="button" className="q-btn secondary" onClick={onRetry}>
        Retry
      </button>
    </div>
  );
}

/**
 * "View log" and "Re-run failed" under a checks list. View log is a link to
 * the forge's own log page. Re-run is the forge's API where it has one; where
 * it does not, the button stays but is disabled and its tooltip says why.
 */
function ChecksActions({
  checks,
  busy,
  onRerun,
  showLog = true,
}: {
  checks: ForgeChecks;
  busy: boolean;
  onRerun: () => void;
  /** The Checks tab already has a View log on every row. */
  showLog?: boolean;
}) {
  const log = showLog ? logCheck(checks) : null;
  const rerun = rerunAction(checks);
  const blocked = rerun.kind !== "ready";
  return (
    <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
      {log?.details_url ? (
        <a
          className="q-btn secondary"
          href={log.details_url}
          target="_blank"
          rel="noopener noreferrer"
        >
          View log
        </a>
      ) : null}
      <span title={blocked ? rerun.reason : undefined}>
        <button type="button" className="q-btn ghost" disabled={blocked || busy} onClick={onRerun}>
          {busy ? "Re-running…" : "Re-run failed"}
        </button>
      </span>
    </div>
  );
}

function CheckIcon({ status }: { status: string }) {
  if (status === "success")
    return <CheckCircle2Icon className="q-icon s14 q-ok" strokeWidth={1.5} aria-hidden />;
  if (
    status === "failure" ||
    status === "timed_out" ||
    status === "cancelled" ||
    status === "action_required"
  ) {
    return <CircleXIcon className="q-icon s14 q-danger" strokeWidth={1.5} aria-hidden />;
  }
  if (status === "in_progress")
    return <LoaderCircleIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />;
  return <CircleDashedIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />;
}

function PrSkeleton() {
  return (
    <div className="q-page" role="status" aria-busy="true" aria-label="Loading pull requests">
      <div style={{ maxWidth: 860, margin: "0 auto", padding: "24px 32px" }}>
        <div className="q-skel" style={{ width: 220, height: 20, marginBottom: 18 }} />
        {[0, 1, 2, 3].map((row) => (
          <div key={row} className="q-skel" style={{ height: 52, marginBottom: 8 }} />
        ))}
      </div>
    </div>
  );
}

function EmptyForge({ onSettings }: { onSettings: () => void }) {
  return (
    <div className="q-page">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 400 }}>
          <GitPullRequestIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
          <h3>Connect a forge</h3>
          <p>
            Pull requests live on GitHub, GitLab, Gitea, Forgejo, Bitbucket Cloud or Azure DevOps.
            Add an account and the token stays on this host.
          </p>
          <button type="button" className="q-btn primary" onClick={onSettings}>
            Add a forge account
          </button>
        </div>
      </div>
    </div>
  );
}

function NoRepo({ onSettings }: { onSettings: () => void }) {
  return (
    <div className="q-page">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 420 }}>
          <GitPullRequestIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
          <h3>No repository detected</h3>
          <p>
            Open a thread in a git checkout with a GitHub, GitLab, Gitea, Forgejo, Bitbucket Cloud
            or Azure DevOps remote, or add a self-hosted host in Settings.
          </p>
          <button type="button" className="q-btn secondary" onClick={onSettings}>
            Open forge settings
          </button>
        </div>
      </div>
    </div>
  );
}

export function PullRequestsPage() {
  const threads = useApp((state) => state.threads);
  // A cold deep link mounts this page before the socket handshake finishes.
  const connection = useApp((state) => state.connection.status);
  const [accounts, setAccounts] = useState<ForgeAccount[]>([]);
  const [capabilities, setCapabilities] = useState<ForgeCapabilityEntry[]>([]);
  const [loadingAccounts, setLoadingAccounts] = useState(true);
  const [accountsError, setAccountsError] = useState<string | null>(null);
  const [repos, setRepos] = useState<ForgeRemote[]>([]);
  const [repo, setRepo] = useState<ForgeRepoRef | null>(() => repoFromQuery());
  const [detecting, setDetecting] = useState(true);
  const [filter, setFilter] = useState<FilterKey>("open");
  const [page, setPage] = useState(1);
  const [prs, setPrs] = useState<ForgePullRequest["summary"][]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [listLoading, setListLoading] = useState(false);
  const [listError, setListError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [reloadNonce, setReloadNonce] = useState(0);

  const threadList = useMemo(() => Object.values(threads), [threads]);
  const activeThread = useMemo<ThreadSummary | null>(
    () => threadList.find((thread) => thread.workspace_path) ?? null,
    [threadList],
  );
  const threadBranch = activeThread?.branch ?? null;
  const filters = useMemo(
    () => (threadBranch ? [...FILTERS, { key: "branch" as const, label: "This branch" }] : FILTERS),
    [threadBranch],
  );

  const openSettings = () =>
    navigate({ kind: "placeholder", surface: "settings", path: "/settings/forges" });

  useEffect(() => {
    void reloadNonce;
    // The first paint can beat the handshake; wait for it instead of showing
    // "daemon is not connected" as a forge failure.
    if (connection === "connecting") return;
    let cancelled = false;
    setLoadingAccounts(true);
    setAccountsError(null);
    api()
      .accounts()
      .then((result) => {
        if (cancelled) return;
        setAccounts(result.accounts);
        setCapabilities(result.capabilities);
        setLoadingAccounts(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setAccountsError(cause instanceof Error ? cause.message : String(cause));
        setLoadingAccounts(false);
      });
    return () => {
      cancelled = true;
    };
  }, [reloadNonce, connection]);

  // Detect a repository from the thread workspaces unless the URL names one.
  useEffect(() => {
    const fromQuery = repoFromQuery();
    if (fromQuery) {
      setRepo(fromQuery);
      setDetecting(false);
      return;
    }
    if (!accounts.length) {
      setDetecting(false);
      return;
    }
    let cancelled = false;
    setDetecting(true);
    const workspaces = [...new Set(threadList.map((thread) => thread.workspace_path))];
    void (async () => {
      const found: ForgeRemote[] = [];
      for (const workspace of workspaces) {
        try {
          found.push(...(await api().detectRepo(workspace)));
        } catch {
          // A workspace without a forge remote is not an error.
        }
      }
      if (cancelled) return;
      // Two workspaces in the same checkout report the same remote; React and
      // the repository picker want one entry per remote and repository.
      const unique = [
        ...new Map(
          found.map((item) => [
            `${item.remote}:${item.repo.forge}:${item.repo.base_url}:${item.repo.owner}/${item.repo.repo}`,
            item,
          ]),
        ).values(),
      ];
      setRepos(unique);
      setRepo((current) => current ?? unique[0]?.repo ?? null);
      setDetecting(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [accounts, threadList]);

  // The list reloads when the repo, filter or page changes.
  useEffect(() => {
    void reloadNonce;
    if (!repo) {
      setPrs([]);
      setListLoading(false);
      return;
    }
    let cancelled = false;
    setListLoading(true);
    setListError(null);
    api()
      .listPrs(repo, filterFor(filter, threadBranch), page)
      .then((result) => {
        if (cancelled) return;
        setPrs((current) => (page === 1 ? result.prs : [...current, ...result.prs]));
        setHasMore(result.hasMore);
        setListLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setListError(cause instanceof Error ? cause.message : String(cause));
        setListLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, filter, page, reloadNonce, threadBranch]);

  if (loadingAccounts) return <PrSkeleton />;
  if (accountsError) {
    return (
      <main className="q-main" aria-label="Pull requests">
        <div className="q-page">
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 420 }}>
              <CircleAlertIcon className="q-icon s22 q-danger" strokeWidth={1.5} aria-hidden />
              <h3>Forge accounts did not load</h3>
              <p>{accountsError}</p>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => setReloadNonce((value) => value + 1)}
              >
                Retry
              </button>
            </div>
          </div>
        </div>
      </main>
    );
  }
  if (accounts.length === 0) return <EmptyForge onSettings={openSettings} />;

  if (selected !== null && repo) {
    return (
      <PrDetail
        repo={repo}
        number={selected}
        accounts={accounts}
        capabilities={capabilities}
        threads={threadList}
        onBack={() => {
          setSelected(null);
          setReloadNonce((value) => value + 1);
        }}
        onOpenSettings={openSettings}
      />
    );
  }

  if (!repo) {
    if (detecting) return <PrSkeleton />;
    return (
      <main className="q-main" aria-label="Pull requests">
        <header className="q-page-head">
          <h1>Pull requests</h1>
        </header>
        <NoRepo onSettings={openSettings} />
      </main>
    );
  }

  const account = accountForRepo(accounts, repo);

  return (
    <main className="q-main" aria-label="Pull requests">
      <header className="q-page-head q-pr-list-head">
        <h1>Pull requests</h1>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {repo.owner}/{repo.repo}
        </span>
        {repos.length > 1 ? (
          <ChipSelect
            label="Repository"
            value={`${repo.forge}:${repo.base_url}:${repo.owner}/${repo.repo}`}
            options={repos.map((item) => ({
              value: `${item.repo.forge}:${item.repo.base_url}:${item.repo.owner}/${item.repo.repo}`,
              label: `${item.repo.owner}/${item.repo.repo}`,
              hint: item.remote,
            }))}
            onChange={(value) => {
              const next = repos.find(
                (item) =>
                  `${item.repo.forge}:${item.repo.base_url}:${item.repo.owner}/${item.repo.repo}` ===
                  value,
              );
              if (next) {
                setRepo(next.repo);
                setPage(1);
              }
            }}
          />
        ) : null}
        <span className="grow" />
        <div className="q-seg" role="tablist" aria-label="Pull request filters">
          {filters.map((item) => (
            <button
              key={item.key}
              type="button"
              role="tab"
              aria-selected={filter === item.key}
              onClick={() => {
                setFilter(item.key);
                setPage(1);
              }}
            >
              {item.label}
            </button>
          ))}
        </div>
        <OpenPrButton
          repo={repo}
          thread={activeThread}
          account={account}
          onOpened={(pr) => {
            setPrs((current) => [pr.summary, ...current]);
            setSelected(pr.summary.number);
          }}
        />
        <button
          type="button"
          className="q-ibtn"
          aria-label="Refresh pull requests"
          onClick={() => setReloadNonce((value) => value + 1)}
        >
          <RotateCwIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
      </header>
      <div className="q-scroll">
        {listError ? (
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 420 }}>
              <CircleAlertIcon className="q-icon s22 q-danger" strokeWidth={1.5} aria-hidden />
              <h3>The pull requests did not load</h3>
              <p>{listError}</p>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => setReloadNonce((value) => value + 1)}
              >
                Retry
              </button>
            </div>
          </div>
        ) : listLoading && prs.length === 0 ? (
          <div className="q-rows" aria-busy="true">
            {[0, 1, 2, 3].map((row) => (
              <div key={row} className="q-skel" style={{ height: 52, margin: "4px 8px" }} />
            ))}
          </div>
        ) : prs.length === 0 ? (
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 380 }}>
              <GitPullRequestIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
              <h3>
                No{" "}
                {filter === "review"
                  ? "reviews waiting"
                  : filter === "branch"
                    ? "pull requests on this branch"
                    : "pull requests"}
              </h3>
              <p>
                {filter === "mine"
                  ? "Nothing you opened is open on this repository."
                  : filter === "review"
                    ? "No one has asked you to review a pull request."
                    : filter === "branch"
                      ? `No open pull request uses ${threadBranch ?? "this branch"} as its head.`
                      : "This repository has no open pull requests."}
              </p>
            </div>
          </div>
        ) : (
          <div className="q-rows q-pr-rows">
            {prs.map((pr) => {
              const state = stateLabel(pr.state);
              const checks = summaryChecksLabel(pr.checks);
              const review = reviewChip(pr.review);
              return (
                <button
                  key={`${pr.repo.owner}/${pr.repo.repo}#${pr.number}`}
                  type="button"
                  className="q-row"
                  onClick={() => setSelected(pr.number)}
                >
                  <span className="q-row-st" style={{ color: "var(--fg-2)" }}>
                    <GitPullRequestIcon className="q-icon s14" aria-hidden />
                  </span>
                  <span className="q-row-main">
                    <span className="q-row-l1">
                      <span className="q-row-title">{pr.title}</span>
                      <span className="q-row-time">#{pr.number}</span>
                    </span>
                    <span className="q-row-l2">
                      <span className="q-row-meta">
                        {pr.head_branch} → {pr.base_branch}
                      </span>
                      <span className={`q-row-status ${state.tone}`}>{state.label}</span>
                      {checks ? (
                        <span className={`q-row-status ${pr.checks === "failing" ? "danger" : ""}`}>
                          {checks}
                        </span>
                      ) : null}
                      {review ? (
                        <span className={`q-row-status ${review.tone}`}>{review.label}</span>
                      ) : null}
                    </span>
                  </span>
                </button>
              );
            })}
            {hasMore ? (
              <div style={{ padding: "12px 8px" }}>
                <button
                  type="button"
                  className="q-btn secondary"
                  disabled={listLoading}
                  onClick={() => setPage((value) => value + 1)}
                >
                  {listLoading ? "Loading…" : "Load more"}
                </button>
              </div>
            ) : null}
          </div>
        )}
      </div>
    </main>
  );
}

function OpenPrButton({
  repo,
  thread,
  account,
  onOpened,
}: {
  repo: ForgeRepoRef;
  thread: ThreadSummary | null;
  account: ForgeAccount | undefined;
  onOpened: (pr: ForgePullRequest) => void;
}) {
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState(thread?.title ?? "");
  const [body, setBody] = useState("");
  const [base, setBase] = useState("main");
  const [busy, setBusy] = useState(false);

  if (!thread?.branch) {
    return (
      <button
        type="button"
        className="q-btn secondary"
        disabled
        title="Open a thread in this repository first; the thread branch becomes the PR head"
      >
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> Open PR
      </button>
    );
  }
  if (!account) {
    return (
      <button
        type="button"
        className="q-btn secondary"
        disabled
        title={`No ${forgeLabel(repo.forge)} account matches this repository`}
      >
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> Open PR
      </button>
    );
  }

  const submit = async () => {
    setBusy(true);
    try {
      const pr = await api().openPr(thread.thread_id, repo, title, body, base);
      pushToast({
        title: `Opened ${repo.owner}/${repo.repo}#${pr.summary.number}.`,
        variant: "success",
      });
      setOpen(false);
      onOpened(pr);
    } catch (cause) {
      pushToast({
        title: "The pull request was not opened.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <button type="button" className="q-btn secondary" onClick={() => setOpen(true)}>
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> Open PR
      </button>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Open a pull request from {thread.branch}</DialogTitle>
          <DialogDescription>
            The daemon pushes the thread branch when needed and opens the request on{" "}
            {forgeLabel(repo.forge)}.
          </DialogDescription>
        </DialogHeader>
        <label className="q-field" style={{ display: "block" }}>
          <span className="t">Title</span>
          <input
            className="q-input"
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            aria-label="Pull request title"
          />
        </label>
        <label className="q-field" style={{ display: "block" }}>
          <span className="t">Base branch</span>
          <input
            className="q-input"
            value={base}
            onChange={(event) => setBase(event.target.value)}
            aria-label="Base branch"
          />
        </label>
        <label className="q-field" style={{ display: "block" }}>
          <span className="t">Description</span>
          <textarea
            className="q-input"
            rows={4}
            value={body}
            onChange={(event) => setBody(event.target.value)}
            aria-label="Pull request description"
          />
        </label>
        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => setOpen(false)}>
            Cancel
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={busy || title.trim() === "" || base.trim() === ""}
            onClick={() => void submit()}
          >
            {busy ? "Opening…" : "Open pull request"}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Stable ref callback: focuses the reply box the moment the user opens it. */
const focusOnMount = (element: HTMLElement | null) => element?.focus();

/** The diff lines a review thread is anchored to, quoted above its comments. */
function ThreadQuote({ thread, files }: { thread: ForgeReviewThread; files: ForgeFileDiff[] }) {
  const file = files.find((candidate) => candidate.path === thread.path);
  if (!file || thread.line === null) return null;
  const rows = parsePatch(file.patch).filter((row) => row.kind !== "hunk");
  const at = rows.findIndex((row) => (row.newLine ?? row.oldLine) === thread.line);
  if (at < 0) return null;
  const quoted = rows.slice(Math.max(0, at - 1), at + 1);
  return (
    <div className="q-hunk q-quote" role="presentation">
      {quoted.map((row) => (
        <div
          key={`${row.newLine ?? row.oldLine}:${row.kind}`}
          className={row.kind === "add" ? "q-ln a" : row.kind === "del" ? "q-ln d" : "q-ln"}
        >
          <span className="n">{row.newLine ?? row.oldLine ?? ""}</span>
          <span className="sg" aria-hidden>
            {row.kind === "add" ? "+" : row.kind === "del" ? "−" : " "}
          </span>
          <span className="q-cx">{row.text || " "}</span>
        </div>
      ))}
    </div>
  );
}

function PrDetail({
  repo,
  number,
  accounts,
  capabilities,
  threads,
  onBack,
  onOpenSettings,
}: {
  repo: ForgeRepoRef;
  number: number;
  accounts: ForgeAccount[];
  capabilities: ForgeCapabilityEntry[];
  threads: ThreadSummary[];
  onBack: () => void;
  onOpenSettings: () => void;
}) {
  const [pr, setPr] = useState<ForgePullRequest | null>(null);
  const [comments, setComments] = useState<ForgeCommentPage | null>(null);
  const [checks, setChecks] = useState<ForgeChecks | null>(null);
  const [commits, setCommits] = useState<ForgeCommits | null>(null);
  const [rerunBusy, setRerunBusy] = useState(false);
  const [diffFiles, setDiffFiles] = useState<ForgeFileDiff[]>([]);
  const [diffPage, setDiffPage] = useState<ForgeDiffPage | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [commentsError, setCommentsError] = useState<string | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [commitsError, setCommitsError] = useState<string | null>(null);
  const [checksError, setChecksError] = useState<string | null>(null);
  const [tab, setTab] = useState<"conversation" | "commits" | "checks" | "files">("conversation");
  const [reloadNonce, setReloadNonce] = useState(0);
  const [replyDrafts, setReplyDrafts] = useState<Record<string, string>>({});
  const [replyBusy, setReplyBusy] = useState<Record<string, boolean>>({});
  const [replyOpen, setReplyOpen] = useState<Record<string, boolean>>({});
  const [commentDraft, setCommentDraft] = useState("");
  const [commentBusy, setCommentBusy] = useState(false);
  const [handoff, setHandoff] = useState<string | null>(null);
  const [handBusy, setHandBusy] = useState(false);

  const account = accountForRepo(accounts, repo);
  const caps = capabilities.find((entry) => entry.forge === repo.forge)?.capabilities ?? null;
  const hosts = useApp((state) => state.hosts);
  const linkedThread = useMemo(
    () =>
      threads.find((thread) => thread.branch && thread.branch === pr?.summary.head_branch) ??
      threads[0] ??
      null,
    [threads, pr],
  );

  useEffect(() => {
    void reloadNonce;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setCommentsError(null);
    setDiffError(null);
    setCommitsError(null);
    setChecksError(null);
    // The detail gates the page; every tab loads on its own so one failed
    // request (a diff, a comment page) cannot hide the PR header and checks.
    api()
      .getPr(repo, number)
      .then((detail) => {
        if (cancelled) return;
        setPr(detail);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(errorMessage(cause));
        setLoading(false);
      });
    api()
      .comments(repo, number)
      .then((page) => {
        if (cancelled) return;
        setComments(page);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setCommentsError(errorMessage(cause));
      });
    api()
      .checks(repo, number)
      .then((result) => {
        if (cancelled) return;
        setChecks(result);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setChecksError(errorMessage(cause));
      });
    api()
      .getDiff(repo, number, 1)
      .then((diff) => {
        if (cancelled) return;
        setDiffPage(diff);
        setDiffFiles(diff.files);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setDiffError(errorMessage(cause));
      });
    api()
      .commits(repo, number)
      .then((commitList) => {
        if (cancelled) return;
        setCommits(commitList);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setCommitsError(errorMessage(cause));
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, reloadNonce]);

  // Live checks: the daemon broadcasts a transient update when a watched PR's
  // checks change, so the side column never needs to poll.
  useEffect(() => {
    return getClient().onEvent(({ event }) => {
      if (
        event.type === "forge_checks_updated" &&
        event.repo.owner === repo.owner &&
        event.repo.repo === repo.repo &&
        event.number === number
      ) {
        setChecks(event.checks);
      }
    });
  }, [repo, number]);

  const rerunFailed = async () => {
    if (rerunBusy) return;
    setRerunBusy(true);
    try {
      setChecks(await api().rerunChecks(repo, number));
      pushToast({ title: "Re-running the failed checks.", variant: "success" });
    } catch (cause) {
      pushToast({
        title: "Could not re-run the checks.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setRerunBusy(false);
    }
  };

  const loadMoreDiff = async () => {
    if (!diffPage?.has_more || diffLoading) return;
    setDiffLoading(true);
    try {
      const next = await api().getDiff(repo, number, diffPage.page + 1);
      setDiffPage(next);
      setDiffFiles((current) => mergeDiffPages(current, next));
    } catch (cause) {
      pushToast({
        title: "The rest of the diff did not load.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setDiffLoading(false);
    }
  };

  const submitReply = async (thread: ForgeReviewThread) => {
    const body = (replyDrafts[thread.thread_id] ?? "").trim();
    if (!body || replyBusy[thread.thread_id]) return;
    const temporaryId = `tmp_${Date.now()}`;
    const optimistic: ForgeReviewThread = {
      ...thread,
      comments: [
        ...thread.comments,
        {
          comment_id: temporaryId,
          thread_id: thread.thread_id,
          author: "you",
          body,
          created_at: new Date().toISOString(),
          updated_at: null,
          web_url: null,
          path: thread.path,
          line: thread.line,
          side: thread.side,
          in_reply_to: thread.comments.at(-1)?.comment_id ?? null,
        },
      ],
    };
    setComments((current) =>
      current
        ? {
            ...current,
            threads: current.threads.map((item) =>
              item.thread_id === thread.thread_id ? optimistic : item,
            ),
          }
        : current,
    );
    setReplyDrafts((current) => ({ ...current, [thread.thread_id]: "" }));
    setReplyBusy((current) => ({ ...current, [thread.thread_id]: true }));
    try {
      const comment = await api().reply(
        repo,
        number,
        caps?.nested_replies === false ? null : thread.thread_id,
        thread.comments.at(-1)?.comment_id ?? thread.thread_id,
        body,
      );
      setComments((current) =>
        current
          ? {
              ...current,
              threads: current.threads.map((item) =>
                item.thread_id === thread.thread_id
                  ? {
                      ...item,
                      comments: replaceComment(
                        item.comments.filter((candidate) => candidate.comment_id !== temporaryId),
                        temporaryId,
                        comment,
                      ),
                    }
                  : item,
              ),
            }
          : current,
      );
    } catch (cause) {
      setComments((current) =>
        current
          ? {
              ...current,
              threads: current.threads.map((item) =>
                item.thread_id === thread.thread_id
                  ? {
                      ...item,
                      comments: removeComment(item.comments, temporaryId),
                    }
                  : item,
              ),
            }
          : current,
      );
      setReplyDrafts((current) => ({ ...current, [thread.thread_id]: body }));
      pushToast({
        title: "The reply was not posted.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setReplyBusy((current) => ({ ...current, [thread.thread_id]: false }));
    }
  };

  const submitComment = async () => {
    const body = commentDraft.trim();
    if (!body || !comments || commentBusy) return;
    const temporaryId = `tmp_${Date.now()}`;
    const optimistic: ForgeComment = {
      comment_id: temporaryId,
      author: "you",
      body,
      created_at: new Date().toISOString(),
      updated_at: null,
      web_url: null,
    };
    setComments({ ...comments, comments: appendComment(comments.comments, optimistic) });
    setCommentDraft("");
    setCommentBusy(true);
    try {
      const comment = await api().comment(repo, number, body);
      setComments((current) =>
        current
          ? {
              ...current,
              comments: replaceComment(
                current.comments.filter((candidate) => candidate.comment_id !== temporaryId),
                temporaryId,
                comment,
              ),
            }
          : current,
      );
    } catch (cause) {
      setComments((current) =>
        current ? { ...current, comments: removeComment(current.comments, temporaryId) } : current,
      );
      setCommentDraft(body);
      pushToast({
        title: "The comment was not posted.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setCommentBusy(false);
    }
  };

  const handToAgent = async (thread: ForgeReviewThread | null) => {
    if (!linkedThread) {
      pushToast({
        title: "Open a thread first.",
        description: "The agent needs a workspace, harness and model to receive the PR.",
        variant: "error",
      });
      return;
    }
    setHandBusy(true);
    setHandoff(thread?.thread_id ?? "general");
    try {
      const result = await api().handToAgent(repo, number, linkedThread.thread_id, {
        host: linkedThread.host,
        harness: linkedThread.harness,
        model: linkedThread.model,
        workspace_path: linkedThread.workspace_path,
      });
      pushToast({
        title: `Handed #${number} to ${linkedThread.title}.`,
        description: "The agent starts with the PR context and won't push without approval.",
        variant: "success",
      });
      openThread(result.thread_id);
    } catch (cause) {
      pushToast({
        title: "The PR was not handed to an agent.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setHandBusy(false);
      setHandoff(null);
    }
  };

  if (loading) {
    return (
      <main className="q-main" aria-label="Pull request" aria-busy="true">
        <header className="q-page-head" style={{ height: "auto", padding: "14px 16px 14px 32px" }}>
          <div style={{ flex: 1 }}>
            <div className="q-skel" style={{ width: 320, height: 22 }} />
          </div>
        </header>
        <div className="q-page">
          <div className="q-skel" style={{ height: 260, margin: "24px 32px" }} />
        </div>
      </main>
    );
  }

  if (error || !pr) {
    return (
      <main className="q-main" aria-label="Pull request">
        <div className="q-page">
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 420 }}>
              <CircleAlertIcon className="q-icon s22 q-danger" strokeWidth={1.5} aria-hidden />
              <h3>This pull request did not load</h3>
              <p>{error ?? "The forge returned no detail."}</p>
              <div style={{ display: "flex", gap: 6, justifyContent: "center" }}>
                <button type="button" className="q-btn secondary" onClick={onBack}>
                  Back to list
                </button>
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() => setReloadNonce((value) => value + 1)}
                >
                  Retry
                </button>
              </div>
            </div>
          </div>
        </div>
      </main>
    );
  }

  const state = stateLabel(pr.summary.state);
  const mergeable = mergeableLabel(pr.mergeable);
  const unresolved = comments?.threads.filter((thread) => !thread.resolved).length ?? 0;
  const passed = checks?.passed ?? 0;
  const total = checks?.total ?? 0;

  return (
    <main className="q-main" aria-label="Pull request">
      <header className="q-page-head q-pr-head">
        <div style={{ flex: 1, minWidth: 0 }}>
          <nav className="q-crumb" aria-label="Breadcrumb">
            <button type="button" className="q-crumb-link" onClick={onBack}>
              Pull requests
            </button>
            <span className="q-sep" aria-hidden>
              /
            </span>
            <span>
              {repo.owner}/{repo.repo}
            </span>
            <span className="q-sep" aria-hidden>
              /
            </span>
            <span aria-current="page">#{number}</span>
          </nav>
          <h1 style={{ fontSize: 17, lineHeight: "22px" }}>
            {pr.summary.title}{" "}
            <span className="q-faint" style={{ fontWeight: 400 }}>
              #{number}
            </span>
          </h1>
          <div className="q-muted q-pr-meta">
            <span className={`q-badge ${state.tone}`} style={{ height: 18 }}>
              {state.label}
            </span>
            <span className="mono" style={{ fontSize: 11.5 }}>
              {pr.summary.head_branch}
            </span>{" "}
            →{" "}
            <span className="mono" style={{ fontSize: 11.5 }}>
              {pr.summary.base_branch}
            </span>{" "}
            · {pr.commits} commits · opened by {pr.summary.author} {relativeAge(pr.created_at)} ·{" "}
            {repo.owner}/{repo.repo} on {forgeLabel(repo.forge)}
          </div>
        </div>
        <button
          type="button"
          className="q-btn secondary"
          onClick={() => window.open(pr.summary.web_url, "_blank", "noopener")}
        >
          <ExternalLinkIcon className="q-icon s14" aria-hidden /> Open on {forgeLabel(repo.forge)}
        </button>
      </header>

      <div className="q-page">
        <div className="q-pr-cols">
          <div className="q-pr-main">
            <div className="q-tabs" role="tablist">
              <button
                type="button"
                role="tab"
                aria-selected={tab === "conversation"}
                onClick={() => setTab("conversation")}
              >
                Conversation{" "}
                <span className="c">{(comments?.comments.length ?? 0) + unresolved}</span>
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === "commits"}
                onClick={() => setTab("commits")}
              >
                Commits <span className="c">{commits?.commits.length ?? pr.commits}</span>
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === "checks"}
                onClick={() => setTab("checks")}
              >
                Checks <span className="c">{total}</span>
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === "files"}
                onClick={() => setTab("files")}
              >
                Files{" "}
                <span
                  className="c"
                  title={
                    pr.changed_files_known
                      ? undefined
                      : "The forge did not report how many files changed"
                  }
                >
                  {pr.changed_files_known ? pr.changed_files : "—"}
                </span>
              </button>
            </div>

            {tab === "conversation" ? (
              <>
                <div className="q-cmt" style={{ paddingTop: 0 }}>
                  <div className="q-cmt-h">
                    <EntityAvatar
                      seed={userSeed(pr.summary.author)}
                      kind="user"
                      label={pr.summary.author}
                      size={18}
                    />
                    <b>{pr.summary.author}</b> · {relativeAge(pr.created_at)}
                  </div>
                  {pr.body ? (
                    <Markdown text={pr.body} />
                  ) : (
                    <p className="q-faint">No description.</p>
                  )}
                </div>

                {commentsError ? (
                  <TabError
                    title="Comments did not load"
                    message={commentsError}
                    onRetry={() => setReloadNonce((value) => value + 1)}
                  />
                ) : (
                  <>
                    {comments?.comments.map((comment) => (
                      <div className="q-cmt" key={comment.comment_id}>
                        <div className="q-cmt-h">
                          <EntityAvatar
                            seed={userSeed(comment.author)}
                            kind="user"
                            label={comment.author}
                            size={18}
                          />
                          <b>{comment.author}</b> · {relativeAge(comment.created_at)}
                          {comment.comment_id.startsWith("tmp_") ? (
                            <span className="q-faint"> · sending…</span>
                          ) : null}
                        </div>
                        <Markdown text={comment.body} />
                      </div>
                    ))}

                    {comments?.threads.map((thread) => (
                      <div className="q-cmt" key={thread.thread_id}>
                        <div className="q-cmt-h">
                          <MessageSquareIcon className="q-icon s14 q-faint" aria-hidden />
                          <span className="q-mono">
                            {thread.path}
                            {thread.line !== null ? ` · line ${thread.line}` : ""}
                          </span>
                          {thread.resolved ? (
                            <span className="q-badge ok" style={{ height: 18 }}>
                              Resolved
                            </span>
                          ) : (
                            <span className="q-faint">Unresolved</span>
                          )}
                          {thread.outdated ? <span className="q-faint">· outdated</span> : null}
                        </div>
                        <ThreadQuote thread={thread} files={diffFiles} />
                        {thread.comments.map((comment) => (
                          <div key={comment.comment_id} style={{ marginBottom: 10 }}>
                            <div className="q-cmt-h" style={{ marginBottom: 2 }}>
                              <b>{comment.author}</b> · {relativeAge(comment.created_at)}
                              {comment.comment_id.startsWith("tmp_") ? (
                                <span className="q-faint"> · sending…</span>
                              ) : null}
                            </div>
                            <Markdown text={comment.body} />
                          </div>
                        ))}
                        {replyOpen[thread.thread_id] ||
                        (replyDrafts[thread.thread_id] ?? "") !== "" ? (
                          <div className="q-cmt-reply">
                            <textarea
                              className="q-input"
                              rows={2}
                              ref={focusOnMount}
                              placeholder={`Reply to ${thread.comments.at(-1)?.author ?? "review"}`}
                              value={replyDrafts[thread.thread_id] ?? ""}
                              onChange={(event) =>
                                setReplyDrafts((current) => ({
                                  ...current,
                                  [thread.thread_id]: event.target.value,
                                }))
                              }
                              aria-label={`Reply to ${thread.path}`}
                            />
                            <div className="q-cmt-a">
                              <button
                                type="button"
                                className="q-btn primary"
                                disabled={
                                  (replyDrafts[thread.thread_id] ?? "").trim() === "" ||
                                  replyBusy[thread.thread_id]
                                }
                                onClick={() => void submitReply(thread)}
                              >
                                {replyBusy[thread.thread_id] ? "Sending…" : "Send reply"}
                              </button>
                              <button
                                type="button"
                                className="q-btn ghost"
                                onClick={() => {
                                  setReplyOpen((current) => ({
                                    ...current,
                                    [thread.thread_id]: false,
                                  }));
                                  setReplyDrafts((current) => ({
                                    ...current,
                                    [thread.thread_id]: "",
                                  }));
                                }}
                              >
                                Cancel
                              </button>
                            </div>
                          </div>
                        ) : null}
                        <div className="q-cmt-a">
                          <button
                            type="button"
                            className="q-btn secondary"
                            onClick={() =>
                              setReplyOpen((current) => ({ ...current, [thread.thread_id]: true }))
                            }
                          >
                            <MessageSquareIcon className="q-icon s14" aria-hidden />
                            Reply
                          </button>
                          <button
                            type="button"
                            className="q-btn secondary"
                            disabled={handBusy || !linkedThread}
                            title={
                              linkedThread
                                ? `Send to ${linkedThread.title}`
                                : "Open a thread first so the agent has a workspace"
                            }
                            onClick={() => void handToAgent(thread)}
                          >
                            <HandIcon className="q-icon s14" aria-hidden />
                            {handoff === thread.thread_id ? "Handing…" : "Hand to agent"}
                          </button>
                        </div>
                      </div>
                    ))}

                    <div style={{ marginTop: 18 }}>
                      <div className="q-composer">
                        <textarea
                          aria-label={`Comment on #${number}`}
                          placeholder={`Comment on #${number}`}
                          value={commentDraft}
                          onChange={(event) => setCommentDraft(event.target.value)}
                        />
                        <div className="q-comp-f">
                          <span className="grow" />
                          <span className="q-faint" style={{ fontSize: 11.5 }}>
                            {forgeLabel(repo.forge)}
                          </span>
                          <button
                            type="button"
                            className="q-btn primary"
                            disabled={commentDraft.trim() === "" || commentBusy}
                            onClick={() => void submitComment()}
                          >
                            <MessageSquareTextIcon className="q-icon s14" aria-hidden />
                            {commentBusy ? "Posting…" : "Comment"}
                          </button>
                        </div>
                      </div>
                    </div>
                  </>
                )}
              </>
            ) : null}

            {tab === "commits" ? (
              <div>
                {commitsError ? (
                  <TabError
                    title="Commits did not load"
                    message={commitsError}
                    onRetry={() => setReloadNonce((value) => value + 1)}
                  />
                ) : commits && commits.commits.length > 0 ? (
                  <>
                    {commits.commits.map((commit) => (
                      <div className="q-commit" key={commit.sha}>
                        <div className="grow">
                          <div className="t">{commit.title}</div>
                          <div className="s">
                            {commit.author_login ?? commit.author}
                            {commit.authored_at ? ` · ${relativeAge(commit.authored_at)}` : ""}
                          </div>
                        </div>
                        {commit.web_url ? (
                          <a
                            className="q-mono q-commit-sha"
                            href={commit.web_url}
                            target="_blank"
                            rel="noopener noreferrer"
                            title="Open this commit"
                          >
                            {shortSha(commit.sha)}
                          </a>
                        ) : (
                          <span className="q-mono q-faint">{shortSha(commit.sha)}</span>
                        )}
                      </div>
                    ))}
                    {commits.truncated ? (
                      <p className="q-faint" style={{ fontSize: 12, marginTop: 10 }}>
                        Showing the first {commits.commits.length} commits. Open the PR on{" "}
                        {forgeLabel(repo.forge)} for the rest.
                      </p>
                    ) : null}
                  </>
                ) : (
                  <div className="q-empty">
                    <h3>{commits ? "No commits" : "Commits did not load"}</h3>
                    <p>
                      {commits
                        ? "This pull request has no commits."
                        : `${forgeLabel(repo.forge)} did not return the commit list.`}
                    </p>
                  </div>
                )}
              </div>
            ) : null}

            {tab === "checks" ? (
              <div>
                {checksError ? (
                  <TabError
                    title="Checks did not load"
                    message={checksError}
                    onRetry={() => setReloadNonce((value) => value + 1)}
                  />
                ) : checks ? (
                  <>
                    <div className="q-muted" style={{ marginBottom: 10 }}>
                      {checksLabel(checks.state, checks.passed, checks.total)} ·{" "}
                      <span className="q-mono">{checks.head_sha.slice(0, 8)}</span>
                    </div>
                    {checks.checks.map((check) => (
                      <div className="q-chk" key={check.name}>
                        <CheckIcon status={check.status} />
                        <span className="grow">
                          {check.name}
                          {requiredMark(check) ? (
                            <span className="q-faint"> · {requiredMark(check)}</span>
                          ) : null}
                        </span>
                        {check.description ? (
                          <span className="q-faint">{check.description}</span>
                        ) : null}
                        <span className="q-faint num">{duration(check.duration_ms)}</span>
                        {check.details_url ? (
                          <button
                            type="button"
                            className="q-btn ghost"
                            onClick={() =>
                              window.open(check.details_url ?? "", "_blank", "noopener")
                            }
                          >
                            View log
                          </button>
                        ) : null}
                      </div>
                    ))}
                    <ChecksActions
                      checks={checks}
                      busy={rerunBusy}
                      onRerun={rerunFailed}
                      showLog={false}
                    />
                  </>
                ) : (
                  <div className="q-empty">
                    <h3>No checks</h3>
                    <p>This forge reports no checks for the current commit.</p>
                  </div>
                )}
              </div>
            ) : null}

            {tab === "files" ? (
              <div>
                {diffError ? (
                  <TabError
                    title="The diff did not load"
                    message={diffError}
                    onRetry={() => setReloadNonce((value) => value + 1)}
                  />
                ) : null}
                {!diffError
                  ? diffFiles.map((file) => {
                      const { directory, name } = splitPath(file.path);
                      const rows = parsePatch(file.patch);
                      return (
                        <div key={file.path} className="q-dfile">
                          <div className="q-dfile-h">
                            <span className="p q-mono">
                              <span className="q-faint">{directory}</span>
                              {name}
                            </span>
                            <span className="grow" />
                            <span className="q-num q-pm">
                              <span className="q-add">+{file.additions}</span>{" "}
                              <span className="q-del">−{file.deletions}</span>
                            </span>
                          </div>
                          {rows.length > 0 ? (
                            <div
                              className="q-hunk"
                              style={{
                                font: "12px/18px var(--mono)",
                                background: "var(--bg-sunk)",
                              }}
                            >
                              {rows.map((row) =>
                                row.kind === "hunk" ? (
                                  <div className="hh" key={`${file.path}:hunk:${row.text}`}>
                                    {row.text}
                                  </div>
                                ) : (
                                  <div
                                    key={`${file.path}:${row.newLine ?? row.oldLine ?? row.text}`}
                                    className={
                                      row.kind === "add"
                                        ? "q-ln a"
                                        : row.kind === "del"
                                          ? "q-ln d"
                                          : "q-ln"
                                    }
                                  >
                                    <span className="n">{row.newLine ?? row.oldLine ?? ""}</span>
                                    <span className="sg" aria-hidden>
                                      {row.kind === "add" ? "+" : row.kind === "del" ? "−" : " "}
                                    </span>
                                    <span className="q-cx">{row.text || " "}</span>
                                  </div>
                                ),
                              )}
                            </div>
                          ) : (
                            <div className="q-hunk">
                              <div className="hh">
                                {file.binary
                                  ? "Binary file"
                                  : file.truncated
                                    ? "Patch truncated by the forge"
                                    : "No inline patch"}
                              </div>
                            </div>
                          )}
                        </div>
                      );
                    })
                  : null}
                {diffPage?.has_more && !diffError ? (
                  <div style={{ padding: "12px 0" }}>
                    <button
                      type="button"
                      className="q-btn secondary"
                      disabled={diffLoading}
                      onClick={() => void loadMoreDiff()}
                    >
                      {diffLoading ? "Loading…" : "Load more files"}
                    </button>
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>

          <aside className="q-pr-side">
            {mergeable || pr.labels.length > 0 ? (
              <div className="q-side-sec">
                {mergeable ? (
                  <h3>
                    <span>Merge</span>
                    <span className={mergeable.tone === "ok" ? "q-ok" : "q-warn"}>
                      {mergeable.label}
                    </span>
                  </h3>
                ) : null}
                {pr.labels.length > 0 ? (
                  <div className="q-pr-labels">
                    {pr.labels.map((label) => (
                      <span key={label.name} className="q-badge" style={{ height: 18 }}>
                        {label.name}
                      </span>
                    ))}
                  </div>
                ) : null}
              </div>
            ) : null}
            <div className="q-side-sec" hidden={tab === "checks"}>
              <h3>
                <span>Checks</span>
                <span>{checks ? `${passed} of ${total} passed` : "Unknown"}</span>
              </h3>
              {checks && checks.checks.length > 0 ? (
                checks.checks.slice(0, 8).map((check) => (
                  <div className="q-chk" key={check.name}>
                    <CheckIcon status={check.status} />
                    <span className="grow">
                      {check.name}
                      {requiredMark(check) ? (
                        <span className="q-faint"> · {requiredMark(check)}</span>
                      ) : null}
                    </span>
                    <span className="q-faint num">{duration(check.duration_ms)}</span>
                  </div>
                ))
              ) : (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  {checksError ? "Checks did not load." : "No checks reported."}
                </p>
              )}
              {checks && checks.checks.length > 0 ? (
                <ChecksActions checks={checks} busy={rerunBusy} onRerun={rerunFailed} />
              ) : null}
            </div>
            <div className="q-side-sec">
              <h3>
                <span>Reviews</span>
              </h3>
              {pr.reviewers.length === 0 ? (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  No reviews yet.
                </p>
              ) : (
                pr.reviewers.map((review) => (
                  <div className="q-chk" key={review.author}>
                    <EntityAvatar
                      seed={userSeed(review.author)}
                      kind="user"
                      label={review.author}
                      size={16}
                    />
                    <span className="grow">{review.author}</span>
                    <span
                      className={
                        review.state === "changes_requested"
                          ? "q-warn"
                          : review.state === "approved"
                            ? "q-ok"
                            : "q-faint"
                      }
                      style={{ fontSize: 12 }}
                    >
                      {review.state.replace("_", " ")}
                    </span>
                  </div>
                ))
              )}
            </div>
            <div className="q-side-sec" style={{ border: 0 }}>
              <h3>
                <span>Linked thread</span>
              </h3>
              {linkedThread ? (
                <button
                  type="button"
                  className="q-chk"
                  style={{ width: "100%", textAlign: "left" }}
                  onClick={() => openThread(linkedThread.thread_id)}
                >
                  <AgentGlyph
                    glyph={glyphForThread(linkedThread, hosts[linkedThread.host]?.online ?? false)}
                    size={14}
                  />
                  <span className="grow">{linkedThread.title}</span>
                  <span className="q-faint" style={{ fontSize: 12 }}>
                    {[
                      linkedThread.workspace_path.split("/").filter(Boolean).pop(),
                      hosts[linkedThread.host]?.name,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </button>
              ) : (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  Open a thread in this repository to hand reviews to an agent.
                </p>
              )}
              <div style={{ marginTop: 8 }}>
                <button
                  type="button"
                  className="q-btn ghost"
                  disabled={handBusy || !linkedThread}
                  onClick={() => void handToAgent(null)}
                >
                  <HandIcon className="q-icon s14" aria-hidden />
                  {handoff === "general" ? "Handing…" : "Hand PR to agent"}
                </button>
              </div>
            </div>
            {!account ? (
              <div className="q-side-sec" style={{ border: 0 }}>
                <p className="q-faint" style={{ fontSize: 12 }}>
                  No account matches this host.{" "}
                  <button type="button" className="q-link" onClick={onOpenSettings}>
                    Add one
                  </button>
                </p>
              </div>
            ) : null}
          </aside>
        </div>
      </div>
    </main>
  );
}
