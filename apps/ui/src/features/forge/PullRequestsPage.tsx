import {
  CheckCircle2Icon,
  CircleAlertIcon,
  CircleDashedIcon,
  CircleXIcon,
  ExternalLinkIcon,
  EyeIcon,
  GitPullRequestIcon,
  HandIcon,
  LoaderCircleIcon,
  MessageSquareIcon,
  MessageSquareTextIcon,
  RotateCwIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Markdown } from "@/components/thread/Markdown";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
import { Tabs, TabsCount, TabsList, TabsTab } from "@/components/ui/tabs";
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
  PrLinkedThread,
  ThreadSummary,
} from "@/gen";
import { glyphForThread } from "@/lib/agent";
import { userSeed } from "@/lib/entities";
import { formatAge, formatList, formatNumber, formatRelativeTime } from "@/lib/i18n";
import { getClient, navigate, openThread, pushToast, useApp } from "@/lib/store/appStore";

const api = () => new ForgeApi((request) => getClient().request(request));

type FilterKey = "open" | "mine" | "review" | "branch";

const FILTERS: FilterKey[] = ["open", "mine", "review"];

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

/** A check's run time in the narrow column: "42s", "3m 5s", "3min 5s". */
function duration(ms: bigint | null): string {
  if (ms === null) return "";
  const unit = (value: number, name: "minute" | "second") =>
    formatNumber(value, { style: "unit", unit: name, unitDisplay: "narrow" });
  const total = Math.round(Number(ms) / 1000);
  if (total < 60) return unit(total, "second");
  return formatList(
    [unit(Math.floor(total / 60), "minute"), unit(total % 60, "second")],
    "unit",
    "narrow",
  );
}

/** "now" under a minute, otherwise the one-unit age ("5m"). */
function isNow(iso: string): boolean {
  const then = Date.parse(iso);
  return Number.isFinite(then) && Date.now() - then < 60_000;
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
  const { t } = useTranslation("common");
  return (
    <div className="q-empty" style={{ margin: "18px 0" }}>
      <h3>{title}</h3>
      <p>{message}</p>
      <button type="button" className="q-btn secondary" onClick={onRetry}>
        {t("retry")}
      </button>
    </div>
  );
}

/** One tab's request still in flight, shown inside the tab. */
function TabLoading({ label }: { label: string }) {
  return (
    <p className="q-faint" role="status" style={{ margin: "12px 0", fontSize: 12.5 }}>
      {label}
    </p>
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
  const { t } = useTranslation("pullRequests");
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
          {t("checks.viewLog")}
        </a>
      ) : null}
      <span title={blocked ? rerun.reason : undefined}>
        <button type="button" className="q-btn ghost" disabled={blocked || busy} onClick={onRerun}>
          {busy ? t("checks.rerunning") : t("checks.rerunFailed")}
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
  const { t } = useTranslation("pullRequests");
  return (
    <div className="q-page" role="status" aria-busy="true" aria-label={t("loading")}>
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
  const { t } = useTranslation("pullRequests");
  return (
    <div className="q-page">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 400 }}>
          <GitPullRequestIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
          <h3>{t("noForge.title")}</h3>
          <p>{t("noForge.body")}</p>
          <button type="button" className="q-btn primary" onClick={onSettings}>
            {t("noForge.action")}
          </button>
        </div>
      </div>
    </div>
  );
}

function NoRepo({ onSettings }: { onSettings: () => void }) {
  const { t } = useTranslation("pullRequests");
  return (
    <div className="q-page">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 420 }}>
          <GitPullRequestIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
          <h3>{t("noRepo.title")}</h3>
          <p>{t("noRepo.body")}</p>
          <button type="button" className="q-btn secondary" onClick={onSettings}>
            {t("noRepo.action")}
          </button>
        </div>
      </div>
    </div>
  );
}

export function PullRequestsPage() {
  const { t } = useTranslation(["pullRequests", "common"]);
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
    () => (threadBranch ? [...FILTERS, "branch" as const] : FILTERS),
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
      <main className="q-main" aria-label={t("title")}>
        <div className="q-page">
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 420 }}>
              <CircleAlertIcon className="q-icon s22 q-danger" strokeWidth={1.5} aria-hidden />
              <h3>{t("accountsFailed")}</h3>
              <p>{accountsError}</p>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => setReloadNonce((value) => value + 1)}
              >
                {t("common:retry")}
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
      <main className="q-main" aria-label={t("title")}>
        <header className="q-page-head">
          <h1>{t("title")}</h1>
        </header>
        <NoRepo onSettings={openSettings} />
      </main>
    );
  }

  const account = accountForRepo(accounts, repo);

  return (
    <main className="q-main" aria-label={t("title")}>
      <header className="q-page-head q-pr-list-head">
        <h1>{t("title")}</h1>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {repo.owner}/{repo.repo}
        </span>
        {repos.length > 1 ? (
          <ChipSelect
            label={t("repository")}
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
        <SegmentedControl
          className="q-pr-filters"
          aria-label={t("filters.label")}
          value={filter}
          onValueChange={(value) => {
            if (!value) return;
            setFilter(value as typeof filter);
            setPage(1);
          }}
        >
          {filters.map((key) => (
            <SegmentedControlItem key={key} value={key}>
              {t(`filters.${key}`)}
            </SegmentedControlItem>
          ))}
        </SegmentedControl>
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
          aria-label={t("refresh")}
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
              <h3>{t("listFailed")}</h3>
              <p>{listError}</p>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => setReloadNonce((value) => value + 1)}
              >
                {t("common:retry")}
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
                {filter === "review"
                  ? t("empty.review")
                  : filter === "branch"
                    ? t("empty.branch")
                    : t("empty.open")}
              </h3>
              <p>
                {filter === "mine"
                  ? t("empty.mineBody")
                  : filter === "review"
                    ? t("empty.reviewBody")
                    : filter === "branch"
                      ? threadBranch
                        ? t("empty.branchBody", { branch: threadBranch })
                        : t("empty.thisBranchBody")
                      : t("empty.openBody")}
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
                  {listLoading ? t("common:loading") : t("loadMore")}
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
  const { t } = useTranslation(["pullRequests", "common"]);
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState(thread?.title ?? "");
  const [body, setBody] = useState("");
  const [base, setBase] = useState("main");
  const [busy, setBusy] = useState(false);

  if (!thread?.branch) {
    return (
      <button type="button" className="q-btn secondary" disabled title={t("open.needsThread")}>
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> {t("open.button")}
      </button>
    );
  }
  if (!account) {
    return (
      <button
        type="button"
        className="q-btn secondary"
        disabled
        title={t("open.noAccount", { forge: forgeLabel(repo.forge) })}
      >
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> {t("open.button")}
      </button>
    );
  }

  const submit = async () => {
    setBusy(true);
    try {
      const pr = await api().openPr(thread.thread_id, repo, title, body, base);
      pushToast({
        title: t("open.opened", { repo: `${repo.owner}/${repo.repo}`, number: pr.summary.number }),
        variant: "success",
      });
      setOpen(false);
      onOpened(pr);
    } catch (cause) {
      pushToast({
        title: t("open.failed"),
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
        <GitPullRequestIcon className="q-icon s14" aria-hidden /> {t("open.button")}
      </button>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t("open.title", { branch: thread.branch })}</DialogTitle>
          <DialogDescription>
            {t("open.description", { forge: forgeLabel(repo.forge) })}
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <label className="q-field" style={{ display: "block" }}>
            <span className="t">{t("open.titleField")}</span>
            <input
              className="q-input"
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              aria-label={t("open.titleLabel")}
            />
          </label>
          <label className="q-field" style={{ display: "block" }}>
            <span className="t">{t("open.base")}</span>
            <input
              className="q-input"
              value={base}
              onChange={(event) => setBase(event.target.value)}
              aria-label={t("open.base")}
            />
          </label>
          <label className="q-field" style={{ display: "block" }}>
            <span className="t">{t("open.descriptionField")}</span>
            <textarea
              className="q-input"
              rows={4}
              value={body}
              onChange={(event) => setBody(event.target.value)}
              aria-label={t("open.descriptionLabel")}
            />
          </label>
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => setOpen(false)}>
            {t("common:cancel")}
          </Button>
          <Button
            disabled={busy || title.trim() === "" || base.trim() === ""}
            onClick={() => void submit()}
          >
            {busy ? t("open.submitting") : t("open.submit")}
          </Button>
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
  const { t } = useTranslation(["pullRequests", "common"]);
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
  const [commentsLoading, setCommentsLoading] = useState(true);
  const [commentsError, setCommentsError] = useState<string | null>(null);
  const [diffInitialLoading, setDiffInitialLoading] = useState(true);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [commitsLoading, setCommitsLoading] = useState(true);
  const [commitsError, setCommitsError] = useState<string | null>(null);
  const [checksLoading, setChecksLoading] = useState(true);
  const [checksError, setChecksError] = useState<string | null>(null);
  // Bumped by every live checks update. A request that started before the
  // update carries an older generation, so its late answer (or failure) must
  // not overwrite the fresher snapshot the daemon pushed.
  const checksGeneration = useRef(0);
  const [tab, setTab] = useState<"conversation" | "commits" | "checks" | "files">("conversation");
  // One nonce per request. A tab's Retry bumps only its own, so the PR header
  // and the checks stay on screen while that tab loads again.
  const [detailNonce, setDetailNonce] = useState(0);
  const [commentsNonce, setCommentsNonce] = useState(0);
  const [checksNonce, setChecksNonce] = useState(0);
  const [commitsNonce, setCommitsNonce] = useState(0);
  const [diffNonce, setDiffNonce] = useState(0);
  const [replyDrafts, setReplyDrafts] = useState<Record<string, string>>({});
  const [replyBusy, setReplyBusy] = useState<Record<string, boolean>>({});
  const [replyOpen, setReplyOpen] = useState<Record<string, boolean>>({});
  const [commentDraft, setCommentDraft] = useState("");
  const [commentBusy, setCommentBusy] = useState(false);
  const [handoff, setHandoff] = useState<string | null>(null);
  const [handBusy, setHandBusy] = useState(false);
  const [linkedThreads, setLinkedThreads] = useState<PrLinkedThread[]>([]);

  const account = accountForRepo(accounts, repo);
  const caps = capabilities.find((entry) => entry.forge === repo.forge)?.capabilities ?? null;
  const hosts = useApp((state) => state.hosts);
  const branchThread = useMemo(
    () =>
      threads.find((thread) => thread.branch && thread.branch === pr?.summary.head_branch) ??
      threads[0] ??
      null,
    [threads, pr],
  );
  // The daemon's links decide which thread gets the PR context; a branch match
  // is only the fallback when no thread has linked it yet.
  const linkedThread = useMemo(() => {
    const first = linkedThreads[0];
    if (!first) return branchThread;
    return threads.find((thread) => thread.thread_id === first.thread_id) ?? branchThread;
  }, [linkedThreads, threads, branchThread]);

  useEffect(() => {
    void detailNonce;
    let cancelled = false;
    setLoading(true);
    setError(null);
    // The detail gates the page. Every tab owns its own request, so one
    // failed request (a diff, a comment page) cannot hide the PR header and
    // the checks.
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
    return () => {
      cancelled = true;
    };
  }, [repo, number, detailNonce]);

  // Linked threads are their own read: a forge failure there must not hide the
  // PR header, and the reverse lookup can lag without the page looking broken.
  useEffect(() => {
    void detailNonce;
    let cancelled = false;
    api()
      .prThreads(repo, number)
      .then((links) => {
        if (!cancelled) setLinkedThreads(links);
      })
      .catch(() => {
        if (!cancelled) setLinkedThreads([]);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, detailNonce]);

  useEffect(() => {
    void commentsNonce;
    let cancelled = false;
    setCommentsLoading(true);
    setCommentsError(null);
    api()
      .comments(repo, number)
      .then((page) => {
        if (cancelled) return;
        setComments(page);
        setCommentsLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setCommentsError(errorMessage(cause));
        setCommentsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, commentsNonce]);

  useEffect(() => {
    void checksNonce;
    let cancelled = false;
    const generation = checksGeneration.current;
    setChecksLoading(true);
    setChecksError(null);
    api()
      .checks(repo, number)
      .then((result) => {
        if (cancelled || generation !== checksGeneration.current) return;
        setChecks(result);
        setChecksLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled || generation !== checksGeneration.current) return;
        setChecksError(errorMessage(cause));
        setChecksLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, checksNonce]);

  useEffect(() => {
    void diffNonce;
    let cancelled = false;
    setDiffInitialLoading(true);
    setDiffError(null);
    api()
      .getDiff(repo, number, 1)
      .then((diff) => {
        if (cancelled) return;
        setDiffPage(diff);
        setDiffFiles(diff.files);
        setDiffInitialLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setDiffError(errorMessage(cause));
        setDiffInitialLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, diffNonce]);

  useEffect(() => {
    void commitsNonce;
    let cancelled = false;
    setCommitsLoading(true);
    setCommitsError(null);
    api()
      .commits(repo, number)
      .then((commitList) => {
        if (cancelled) return;
        setCommits(commitList);
        setCommitsLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setCommitsError(errorMessage(cause));
        setCommitsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo, number, commitsNonce]);

  // Live checks: the daemon broadcasts a transient update when a watched PR's
  // checks change, so the side column never needs to poll. A real update is
  // also what recovers the Checks tab from a failed load.
  useEffect(() => {
    return getClient().onEvent(({ event }) => {
      if (
        event.type === "forge_checks_updated" &&
        event.repo.owner === repo.owner &&
        event.repo.repo === repo.repo &&
        event.number === number
      ) {
        checksGeneration.current += 1;
        setChecks(event.checks);
        setChecksError(null);
        setChecksLoading(false);
      }
    });
  }, [repo, number]);

  const retryPage = () => {
    setDetailNonce((value) => value + 1);
    setCommentsNonce((value) => value + 1);
    setChecksNonce((value) => value + 1);
    setCommitsNonce((value) => value + 1);
    setDiffNonce((value) => value + 1);
  };

  const rerunFailed = async () => {
    if (rerunBusy) return;
    setRerunBusy(true);
    const generation = checksGeneration.current;
    try {
      const result = await api().rerunChecks(repo, number);
      if (generation !== checksGeneration.current) return;
      setChecks(result);
      pushToast({ title: t("toast.rerunStarted"), variant: "success" });
    } catch (cause) {
      if (generation !== checksGeneration.current) return;
      pushToast({
        title: t("toast.rerunFailed"),
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
        title: t("toast.diffMoreFailed"),
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
          author: t("conversation.you"),
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
        title: t("toast.replyFailed"),
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
      author: t("conversation.you"),
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
        title: t("toast.commentFailed"),
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
        title: t("toast.openThreadFirst"),
        description: t("toast.openThreadFirstBody"),
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
        title: t("toast.handed", { number, title: linkedThread.title }),
        description: t("toast.handedBody"),
        variant: "success",
      });
      openThread(result.thread_id);
    } catch (cause) {
      pushToast({
        title: t("toast.handFailed"),
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
      <main className="q-main" aria-label={t("detail.label")} aria-busy="true">
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
      <main className="q-main" aria-label={t("detail.label")}>
        <div className="q-page">
          <div className="q-center-empty">
            <div className="q-empty" style={{ width: 420 }}>
              <CircleAlertIcon className="q-icon s22 q-danger" strokeWidth={1.5} aria-hidden />
              <h3>{t("detail.failed")}</h3>
              <p>{error ?? t("detail.noDetail")}</p>
              <div style={{ display: "flex", gap: 6, justifyContent: "center" }}>
                <button type="button" className="q-btn secondary" onClick={onBack}>
                  {t("detail.back")}
                </button>
                <button type="button" className="q-btn secondary" onClick={retryPage}>
                  {t("common:retry")}
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
    <main className="q-main" aria-label={t("detail.label")}>
      <header className="q-page-head q-pr-head">
        <div style={{ flex: 1, minWidth: 0 }}>
          <nav className="q-crumb" aria-label={t("detail.breadcrumb")}>
            <button type="button" className="q-crumb-link" onClick={onBack}>
              {t("title")}
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
            · {t("detail.commits", { count: pr.commits })} ·{" "}
            {isNow(pr.created_at)
              ? t("detail.openedByNow", { author: pr.summary.author })
              : t("detail.openedBy", {
                  author: pr.summary.author,
                  when: formatRelativeTime(pr.created_at),
                })}{" "}
            ·{" "}
            {t("detail.onForge", {
              repo: `${repo.owner}/${repo.repo}`,
              forge: forgeLabel(repo.forge),
            })}
          </div>
        </div>
        <button
          type="button"
          className="q-btn secondary"
          onClick={() => window.open(pr.summary.web_url, "_blank", "noopener")}
        >
          <ExternalLinkIcon className="q-icon s14" aria-hidden />{" "}
          {t("detail.openOn", { forge: forgeLabel(repo.forge) })}
        </button>
      </header>

      <div className="q-page">
        <div className="q-pr-cols">
          <div className="q-pr-main">
            <Tabs
              className="mb-5 border-b border-border"
              value={tab}
              onValueChange={(value) => {
                if (value) setTab(value as typeof tab);
              }}
            >
              <TabsList aria-label={t("tabs.label")} className="-mb-px gap-5 overflow-x-auto">
                <TabsTab value="conversation">
                  {t("tabs.conversation")}
                  <TabsCount>
                    {formatNumber((comments?.comments.length ?? 0) + unresolved)}
                  </TabsCount>
                </TabsTab>
                <TabsTab value="commits">
                  {t("tabs.commits")}
                  <TabsCount>{formatNumber(commits?.commits.length ?? pr.commits)}</TabsCount>
                </TabsTab>
                <TabsTab value="checks">
                  {t("tabs.checks")}
                  <TabsCount>{formatNumber(total)}</TabsCount>
                </TabsTab>
                <TabsTab value="files">
                  {t("tabs.files")}
                  <TabsCount title={pr.changed_files_known ? undefined : t("tabs.filesUnknown")}>
                    {pr.changed_files_known ? formatNumber(pr.changed_files) : "—"}
                  </TabsCount>
                </TabsTab>
              </TabsList>
            </Tabs>

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
                    <b>{pr.summary.author}</b> · {formatAge(pr.created_at)}
                  </div>
                  {pr.body ? (
                    <Markdown text={pr.body} />
                  ) : (
                    <p className="q-faint">{t("conversation.noDescription")}</p>
                  )}
                </div>

                {commentsLoading ? (
                  <TabLoading label={t("conversation.loading")} />
                ) : commentsError ? (
                  <TabError
                    title={t("conversation.failed")}
                    message={commentsError}
                    onRetry={() => setCommentsNonce((value) => value + 1)}
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
                          <b>{comment.author}</b> · {formatAge(comment.created_at)}
                          {comment.comment_id.startsWith("tmp_") ? (
                            <span className="q-faint"> · {t("conversation.sending")}</span>
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
                            {thread.line !== null
                              ? ` · ${t("conversation.line", { line: thread.line })}`
                              : ""}
                          </span>
                          {thread.resolved ? (
                            <span className="q-badge ok" style={{ height: 18 }}>
                              {t("conversation.resolved")}
                            </span>
                          ) : (
                            <span className="q-faint">{t("conversation.unresolved")}</span>
                          )}
                          {thread.outdated ? (
                            <span className="q-faint">· {t("conversation.outdated")}</span>
                          ) : null}
                        </div>
                        <ThreadQuote thread={thread} files={diffFiles} />
                        {thread.comments.map((comment) => (
                          <div key={comment.comment_id} style={{ marginBottom: 10 }}>
                            <div className="q-cmt-h" style={{ marginBottom: 2 }}>
                              <b>{comment.author}</b> · {formatAge(comment.created_at)}
                              {comment.comment_id.startsWith("tmp_") ? (
                                <span className="q-faint"> · {t("conversation.sending")}</span>
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
                              placeholder={
                                thread.comments.at(-1)?.author
                                  ? t("conversation.replyTo", {
                                      name: thread.comments.at(-1)?.author,
                                    })
                                  : t("conversation.replyToReview")
                              }
                              value={replyDrafts[thread.thread_id] ?? ""}
                              onChange={(event) =>
                                setReplyDrafts((current) => ({
                                  ...current,
                                  [thread.thread_id]: event.target.value,
                                }))
                              }
                              aria-label={t("conversation.replyLabel", { path: thread.path })}
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
                                {replyBusy[thread.thread_id]
                                  ? t("conversation.sendingReply")
                                  : t("conversation.sendReply")}
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
                                {t("common:cancel")}
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
                            {t("conversation.reply")}
                          </button>
                          <button
                            type="button"
                            className="q-btn secondary"
                            disabled={handBusy || !linkedThread}
                            title={
                              linkedThread
                                ? t("conversation.sendTo", { title: linkedThread.title })
                                : t("conversation.needsThread")
                            }
                            onClick={() => void handToAgent(thread)}
                          >
                            <HandIcon className="q-icon s14" aria-hidden />
                            {handoff === thread.thread_id
                              ? t("conversation.handing")
                              : t("conversation.handToAgent")}
                          </button>
                        </div>
                      </div>
                    ))}

                    <div style={{ marginTop: 18 }}>
                      <div className="q-composer">
                        <textarea
                          aria-label={t("conversation.commentOn", { number })}
                          placeholder={t("conversation.commentOn", { number })}
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
                            {commentBusy ? t("conversation.posting") : t("conversation.comment")}
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
                {commitsLoading ? (
                  <TabLoading label={t("commits.loading")} />
                ) : commitsError ? (
                  <TabError
                    title={t("commits.failed")}
                    message={commitsError}
                    onRetry={() => setCommitsNonce((value) => value + 1)}
                  />
                ) : commits && commits.commits.length > 0 ? (
                  <>
                    {commits.commits.map((commit) => (
                      <div className="q-commit" key={commit.sha}>
                        <div className="grow">
                          <div className="t">{commit.title}</div>
                          <div className="s">
                            {commit.author_login ?? commit.author}
                            {commit.authored_at ? ` · ${formatAge(commit.authored_at)}` : ""}
                          </div>
                        </div>
                        {commit.web_url ? (
                          <a
                            className="q-mono q-commit-sha"
                            href={commit.web_url}
                            target="_blank"
                            rel="noopener noreferrer"
                            title={t("commits.openCommit")}
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
                        {t("commits.truncated", {
                          count: commits.commits.length,
                          forge: forgeLabel(repo.forge),
                        })}
                      </p>
                    ) : null}
                  </>
                ) : (
                  <div className="q-empty">
                    <h3>{t("commits.none")}</h3>
                    <p>{t("commits.noneBody")}</p>
                  </div>
                )}
              </div>
            ) : null}

            {tab === "checks" ? (
              <div>
                {checksLoading ? (
                  <TabLoading label={t("checks.loading")} />
                ) : checksError ? (
                  <TabError
                    title={t("checks.failed")}
                    message={checksError}
                    onRetry={() => setChecksNonce((value) => value + 1)}
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
                            {t("checks.viewLog")}
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
                    <h3>{t("checks.none")}</h3>
                    <p>{t("checks.noneBody")}</p>
                  </div>
                )}
              </div>
            ) : null}

            {tab === "files" ? (
              <div>
                {diffInitialLoading ? (
                  <TabLoading label={t("files.loading")} />
                ) : (
                  <>
                    {diffError ? (
                      <TabError
                        title={t("files.failed")}
                        message={diffError}
                        onRetry={() => setDiffNonce((value) => value + 1)}
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
                                        <span className="n">
                                          {row.newLine ?? row.oldLine ?? ""}
                                        </span>
                                        <span className="sg" aria-hidden>
                                          {row.kind === "add"
                                            ? "+"
                                            : row.kind === "del"
                                              ? "−"
                                              : " "}
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
                                      ? t("files.binary")
                                      : file.truncated
                                        ? t("files.truncated")
                                        : t("files.noPatch")}
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
                          {diffLoading ? t("common:loading") : t("files.loadMore")}
                        </button>
                      </div>
                    ) : null}
                  </>
                )}
              </div>
            ) : null}
          </div>

          <aside className="q-pr-side">
            {mergeable || pr.labels.length > 0 ? (
              <div className="q-side-sec">
                {mergeable ? (
                  <h3>
                    <span>{t("side.merge")}</span>
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
                <span>{t("side.checks")}</span>
                <span>
                  {checksLoading
                    ? t("common:loading")
                    : checks
                      ? t("side.passedOf", {
                          passed: formatNumber(passed),
                          total: formatNumber(total),
                        })
                      : t("common:unknown")}
                </span>
              </h3>
              {checksLoading ? (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  {t("checks.loading")}
                </p>
              ) : checks && checks.checks.length > 0 ? (
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
                  {checksError ? t("side.checksFailed") : t("side.noChecks")}
                </p>
              )}
              {checks && checks.checks.length > 0 ? (
                <ChecksActions checks={checks} busy={rerunBusy} onRerun={rerunFailed} />
              ) : null}
            </div>
            <div className="q-side-sec">
              <h3>
                <span>{t("side.reviews")}</span>
              </h3>
              {pr.reviewers.length === 0 ? (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  {t("side.noReviews")}
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
                      {t(`side.reviewState.${review.state}`)}
                    </span>
                  </div>
                ))
              )}
            </div>
            <div className="q-side-sec">
              <h3>
                <span>{t("side.linkedThreads")}</span>
              </h3>
              {linkedThreads.length === 0 ? (
                <p className="q-faint" style={{ fontSize: 12, margin: "4px 0" }}>
                  {t("side.noLinkedThreads")}
                </p>
              ) : (
                linkedThreads.map((link) => {
                  const thread = threads.find((item) => item.thread_id === link.thread_id);
                  return (
                    <button
                      type="button"
                      className="q-chk"
                      key={link.thread_id}
                      style={{ width: "100%", textAlign: "left" }}
                      onClick={() => openThread(link.thread_id)}
                    >
                      {thread ? (
                        <AgentGlyph
                          glyph={glyphForThread(thread, hosts[thread.host]?.online ?? false)}
                          size={14}
                        />
                      ) : null}
                      <span className="grow">{link.title}</span>
                      {link.watching ? (
                        <EyeIcon
                          className="q-icon s12 q-faint"
                          strokeWidth={1.5}
                          aria-label={t("side.watching")}
                        />
                      ) : null}
                    </button>
                  );
                })
              )}
            </div>
            <div className="q-side-sec" style={{ border: 0 }}>
              <h3>
                <span>{t("side.agentThread")}</span>
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
                  {t("side.noAgentThread")}
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
                  {handoff === "general" ? t("conversation.handing") : t("side.handPr")}
                </button>
              </div>
            </div>
            {!account ? (
              <div className="q-side-sec" style={{ border: 0 }}>
                <p className="q-faint" style={{ fontSize: 12 }}>
                  {t("side.noAccount")}{" "}
                  <button type="button" className="q-link" onClick={onOpenSettings}>
                    {t("side.addOne")}
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
