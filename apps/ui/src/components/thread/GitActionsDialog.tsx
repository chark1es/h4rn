import {
  ArrowDownIcon,
  ArrowUpIcon,
  CheckIcon,
  GitBranchIcon,
  GitCommitHorizontalIcon,
  GitPullRequestIcon,
  RefreshCwIcon,
  SparklesIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ForgeApi, forgeLabel } from "@/features/forge/store";
import { gitActionsApi, type PullRequestText } from "@/features/git-actions/store";
import type { ForgeRepoRef, GitThreadStatus, ThreadSummary } from "@/gen";
import { i18n } from "@/lib/i18n";
import { getClient, pushToast } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function describeError(cause: unknown): string {
  if (cause instanceof Error) return cause.message;
  if (typeof cause === "string") return cause;
  if (typeof cause === "object" && cause !== null && "message" in cause) {
    return String((cause as { message: unknown }).message);
  }
  return i18n.t("dialogs:git.failed");
}

const forgeApi = () => new ForgeApi((request, options) => getClient().request(request, options));

/** One group of changes: its exact count and the paths the daemon sent. */
function ChangeSet({
  label,
  count,
  paths,
  truncated,
}: {
  label: string;
  count: number;
  paths: string[];
  truncated: boolean;
}) {
  const { t } = useTranslation("dialogs");
  return (
    <div className="q-git-changes-row">
      <span className="t">
        {label}
        <span className="q-faint"> {count}</span>
      </span>
      {paths.length === 0 ? null : (
        <ul className="q-git-paths" aria-label={t("git.paths", { label })}>
          {paths.map((path) => (
            <li key={path} className="q-mono">
              {path}
            </li>
          ))}
          {truncated ? (
            <li className="q-faint">{t("git.more", { count: count - paths.length })}</li>
          ) : null}
        </ul>
      )}
    </div>
  );
}

/**
 * A quiet panel for the thread's branch: what changed, a commit message with
 * Generate, and push. Nothing here invents state: every count and path comes
 * from git on the host, and every failure shows git's or the forge's message.
 */
export function GitActionsDialog({
  summary,
  open,
  onOpenChange,
}: {
  summary: ThreadSummary;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation(["dialogs", "common"]);
  const [status, setStatus] = useState<GitThreadStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [message, setMessage] = useState("");
  const [includeUnstaged, setIncludeUnstaged] = useState(false);
  const [busy, setBusy] = useState<null | "commit" | "push" | "both">(null);
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** Generate answered that generation is off in Settings. */
  const [generationOff, setGenerationOff] = useState(false);
  const [prOpen, setPrOpen] = useState(false);

  const api = useMemo(gitActionsApi, []);
  const threadId = summary.thread_id;

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setStatus(await api.status(threadId));
      setError(null);
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setLoading(false);
    }
  }, [api, threadId]);

  useEffect(() => {
    if (!open) {
      setMessage("");
      setIncludeUnstaged(false);
      setError(null);
      setGenerationOff(false);
      setBusy(null);
      setGenerating(false);
      return;
    }
    void refresh();
  }, [open, refresh]);

  const generate = async () => {
    setGenerating(true);
    setError(null);
    setGenerationOff(false);
    try {
      const generated = await api.generateCommitMessage(threadId, includeUnstaged, null);
      if (!generated) {
        // Off never touches what the person typed; it only says why nothing
        // was written.
        setGenerationOff(true);
      } else {
        setMessage(generated);
      }
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setGenerating(false);
    }
  };

  const commit = async (): Promise<boolean> => {
    setBusy("commit");
    setError(null);
    try {
      const result = await api.commit(threadId, message, includeUnstaged, null);
      setStatus(result.status);
      setMessage("");
      pushToast({
        title: t("git.committed", { commit: result.short_commit, subject: result.subject }),
        variant: "success",
      });
      return true;
    } catch (cause) {
      setError(describeError(cause));
      return false;
    } finally {
      setBusy(null);
    }
  };

  const push = async () => {
    setBusy("push");
    setError(null);
    try {
      const result = await api.push(threadId, true);
      setStatus(result.status);
      pushToast({
        title: t("git.pushed", { branch: result.branch, upstream: result.upstream }),
        variant: "success",
      });
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setBusy(null);
    }
  };

  const commitAndPush = async () => {
    // One busy state for the whole pair: the button that started it says so
    // until both the commit and the push are done.
    setBusy("both");
    setError(null);
    try {
      const committed = await api.commit(threadId, message, includeUnstaged, null);
      setStatus(committed.status);
      setMessage("");
      pushToast({
        title: t("git.committed", { commit: committed.short_commit, subject: committed.subject }),
        variant: "success",
      });
      const pushed = await api.push(threadId, true);
      setStatus(pushed.status);
      pushToast({
        title: t("git.pushed", { branch: pushed.branch, upstream: pushed.upstream }),
        variant: "success",
      });
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setBusy(null);
    }
  };

  // Once the daemon has answered, its status wins: a detached HEAD it reports
  // is a real state, not a reason to fall back to the thread's old branch.
  const detached = status !== null && status.branch === null;
  const branch = status ? status.branch : (summary.branch ?? null);
  const canPush = busy === null && !loading && !detached && branch !== null;
  const canCommit = message.trim().length > 0 && busy === null && !loading && !detached;

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-[560px]">
          <DialogHeader>
            <DialogTitle className="q-git-title">
              <GitBranchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {branch ?? t("git.noBranch")}
            </DialogTitle>
            <DialogDescription>
              {detached
                ? t("git.detached")
                : status?.upstream
                  ? t("git.tracking", { upstream: status.upstream })
                  : t("git.noUpstream")}
            </DialogDescription>
          </DialogHeader>

          {loading && status === null ? (
            <p className="q-faint" role="status">
              {t("git.reading")}
            </p>
          ) : null}

          {status ? (
            <div className="q-git-status">
              <div className="q-git-meta">
                {status.operation ? (
                  <span className="q-badge">
                    {t("git.operation", { operation: status.operation.replace("_", "-") })}
                  </span>
                ) : null}
                {status.upstream ? (
                  <>
                    <span className="q-mono">
                      <ArrowUpIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      {t("git.ahead", { count: status.ahead })}
                    </span>
                    <span className="q-mono">
                      <ArrowDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      {t("git.behind", { count: status.behind })}
                    </span>
                  </>
                ) : null}
                <span className="grow" />
                <button
                  type="button"
                  className="q-ibtn"
                  aria-label={t("git.refresh")}
                  title={t("common:refresh")}
                  onClick={() => void refresh()}
                  disabled={loading}
                >
                  <RefreshCwIcon
                    className={cn("q-icon s14", loading && "animate-spin")}
                    strokeWidth={1.5}
                    aria-hidden
                  />
                </button>
              </div>
              <div className="q-git-changes">
                <ChangeSet
                  label={t("git.staged")}
                  count={status.staged.count}
                  paths={status.staged.paths}
                  truncated={status.staged.truncated}
                />
                <ChangeSet
                  label={t("git.unstaged")}
                  count={status.unstaged.count}
                  paths={status.unstaged.paths}
                  truncated={status.unstaged.truncated}
                />
                <ChangeSet
                  label={t("git.untracked")}
                  count={status.untracked.count}
                  paths={status.untracked.paths}
                  truncated={status.untracked.truncated}
                />
              </div>
            </div>
          ) : null}

          <div className="q-field q-field-block">
            <div className="t q-git-label">
              <label htmlFor="git-commit-message">{t("git.commitMessage")}</label>
              <button
                type="button"
                className="q-btn ghost"
                onClick={() => void generate()}
                disabled={generating || loading || busy !== null}
              >
                <SparklesIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                {generating ? t("git.generating") : t("git.generate")}
              </button>
            </div>
            <textarea
              id="git-commit-message"
              className="q-input q-mono"
              rows={4}
              value={message}
              placeholder={t("git.messagePlaceholder")}
              aria-label={t("git.commitMessage")}
              onChange={(event) => setMessage(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                  event.preventDefault();
                  if (canCommit) void commit();
                }
              }}
            />
          </div>

          <label className="q-addp-check">
            <input
              type="checkbox"
              className="sr-only"
              checked={includeUnstaged}
              onChange={(event) => setIncludeUnstaged(event.target.checked)}
            />
            <span className={cn("q-check", includeUnstaged && "on")} aria-hidden>
              {includeUnstaged ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
            </span>
            {t("git.includeUnstaged")}
          </label>

          {generationOff ? (
            <p className="q-faint" role="status">
              {t("git.generationOff")}
            </p>
          ) : null}
          {error ? (
            <div className="q-banner err wrap" role="alert">
              {error}
            </div>
          ) : null}

          <DialogFooter className="q-git-footer">
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => setPrOpen(true)}
              disabled={!canPush}
            >
              <GitPullRequestIcon className="q-icon s14" aria-hidden />
              {t("git.createPr")}
            </button>
            <span className="grow" />
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => void push()}
              disabled={!canPush}
            >
              {busy === "push" ? t("git.pushing") : t("git.push")}
            </button>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => void commitAndPush()}
              disabled={!canCommit}
            >
              {busy === "both" ? t("git.committingAndPushing") : t("git.commitAndPush")}
            </button>
            <button
              type="button"
              className="q-btn primary"
              onClick={() => void commit()}
              disabled={!canCommit}
            >
              <GitCommitHorizontalIcon className="q-icon s14" aria-hidden />
              {busy === "commit" ? t("git.committing") : t("git.commit")}
            </button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <CreatePullRequestDialog
        summary={summary}
        status={status}
        open={prOpen}
        onOpenChange={setPrOpen}
        onPushed={refresh}
      />
    </>
  );
}

/**
 * The pull request dialog: title, body, base and draft. Generate fills the two
 * text fields and never opens the request; only the person's click does.
 */
export function CreatePullRequestDialog({
  summary,
  status,
  open,
  onOpenChange,
  onPushed,
}: {
  summary: ThreadSummary;
  status: GitThreadStatus | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onPushed?: () => void;
}) {
  const { t } = useTranslation(["dialogs", "common"]);
  const api = useMemo(gitActionsApi, []);
  // Only a fresh open seeds the fields. A title that changes, or a status that
  // arrives, must not rewrite text the person is editing.
  const opening = useRef({
    title: summary.title,
    base: status?.default_branch ?? "",
  });
  opening.current = {
    title: summary.title,
    base: status?.default_branch ?? "",
  };
  const [title, setTitle] = useState(opening.current.title);
  const [body, setBody] = useState("");
  const [base, setBase] = useState(opening.current.base);
  const [draft, setDraft] = useState(false);
  const [repo, setRepo] = useState<ForgeRepoRef | null>(null);
  const [repoError, setRepoError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const inFlight = useRef(false);
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [generationOff, setGenerationOff] = useState(false);
  /** No supported forge remote was found for the workspace. */
  const [noRemote, setNoRemote] = useState(false);

  useEffect(() => {
    if (!open) return;
    setTitle(opening.current.title);
    setBody("");
    setBase(opening.current.base);
    setDraft(false);
    setError(null);
    setGenerationOff(false);
    setRepo(null);
    setRepoError(null);
    setNoRemote(false);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    void (async () => {
      try {
        const remotes = await forgeApi().detectRepo(summary.workspace_path);
        if (cancelled) return;
        const first = remotes[0]?.repo ?? null;
        setRepo(first);
        if (!first) {
          setNoRemote(true);
        }
      } catch (cause) {
        if (!cancelled) setRepoError(describeError(cause));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, summary.workspace_path]);

  const generate = async () => {
    setGenerating(true);
    setError(null);
    setGenerationOff(false);
    try {
      const text: PullRequestText = await api.generatePullRequest(
        summary.thread_id,
        base.trim() || null,
      );
      if (!text.title && !text.body) {
        setGenerationOff(true);
      } else {
        setTitle(text.title || title);
        setBody(text.body);
      }
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setGenerating(false);
    }
  };

  const create = async () => {
    if (!repo || inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    try {
      const pr = await forgeApi().openPr(
        summary.thread_id,
        repo,
        title.trim(),
        body,
        base.trim(),
        draft,
      );
      pushToast({
        title: t("pullRequest.opened", {
          ref: `${pr.summary.repo.owner}/${pr.summary.repo.repo}#${pr.summary.number}`,
        }),
        variant: "success",
      });
      onPushed?.();
      onOpenChange(false);
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>{t("pullRequest.title")}</DialogTitle>
          <DialogDescription>
            {repo
              ? t("pullRequest.repo", {
                  repo: `${repo.owner}/${repo.repo}`,
                  forge: forgeLabel(repo.forge),
                })
              : (repoError ?? (noRemote ? t("pullRequest.noRemote") : t("pullRequest.looking")))}
          </DialogDescription>
        </DialogHeader>

        <div className="q-field q-field-block">
          <div className="t q-git-label">
            <label htmlFor="git-pr-title">{t("pullRequest.titleField")}</label>
            <button
              type="button"
              className="q-btn ghost"
              onClick={() => void generate()}
              disabled={generating || busy}
            >
              <SparklesIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              {generating ? t("git.generating") : t("git.generate")}
            </button>
          </div>
          <input
            id="git-pr-title"
            className="q-input"
            value={title}
            aria-label={t("pullRequest.titleLabel")}
            onChange={(event) => setTitle(event.target.value)}
          />
        </div>

        <label className="q-field q-field-block">
          <span className="t">{t("pullRequest.description")}</span>
          <textarea
            className="q-input"
            rows={5}
            value={body}
            aria-label={t("pullRequest.descriptionLabel")}
            onChange={(event) => setBody(event.target.value)}
          />
        </label>

        <div className="q-git-pr-fields">
          <label className="q-field q-field-block">
            <span className="t">{t("pullRequest.base")}</span>
            <input
              className="q-input q-mono"
              value={base}
              aria-label={t("pullRequest.base")}
              onChange={(event) => setBase(event.target.value)}
            />
            <span className="s">
              {status !== null && !status.default_branch
                ? t("pullRequest.baseNoDefault")
                : t("pullRequest.baseHint")}
            </span>
          </label>
          <label className="q-addp-check q-git-draft">
            <input
              type="checkbox"
              className="sr-only"
              checked={draft}
              onChange={(event) => setDraft(event.target.checked)}
            />
            <span className={cn("q-check", draft && "on")} aria-hidden>
              {draft ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
            </span>
            {t("pullRequest.draft")}
          </label>
        </div>

        {generationOff ? (
          <p className="q-faint" role="status">
            {t("git.generationOff")}
          </p>
        ) : null}
        {error ? (
          <div className="q-banner err wrap" role="alert">
            {error}
          </div>
        ) : null}

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => onOpenChange(false)}>
            {t("common:cancel")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            onClick={() => void create()}
            disabled={busy || !repo || !title.trim() || !base.trim()}
          >
            {busy ? t("pullRequest.opening") : t("pullRequest.open")}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
