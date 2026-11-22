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
import { useCallback, useEffect, useMemo, useState } from "react";

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
import { getClient, pushToast } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function describeError(cause: unknown): string {
  if (cause instanceof Error) return cause.message;
  if (typeof cause === "string") return cause;
  if (typeof cause === "object" && cause !== null && "message" in cause) {
    return String((cause as { message: unknown }).message);
  }
  return "The git action did not finish.";
}

const forgeApi = () => new ForgeApi((request) => getClient().request(request));

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
  return (
    <div className="q-git-changes-row">
      <span className="t">
        {label}
        <span className="q-faint"> {count}</span>
      </span>
      {paths.length === 0 ? null : (
        <ul className="q-git-paths" aria-label={`${label} paths`}>
          {paths.map((path) => (
            <li key={path} className="q-mono">
              {path}
            </li>
          ))}
          {truncated ? <li className="q-faint">and {count - paths.length} more</li> : null}
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
  const [status, setStatus] = useState<GitThreadStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [message, setMessage] = useState("");
  const [includeUnstaged, setIncludeUnstaged] = useState(false);
  const [busy, setBusy] = useState<null | "commit" | "push" | "both">(null);
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [hint, setHint] = useState<string | null>(null);
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
      setHint(null);
      setBusy(null);
      setGenerating(false);
      return;
    }
    void refresh();
  }, [open, refresh]);

  const generate = async () => {
    setGenerating(true);
    setError(null);
    setHint(null);
    try {
      const generated = await api.generateCommitMessage(threadId, includeUnstaged, null);
      if (!generated) {
        setHint("Generation is off. Pick a model in Settings → Source control.");
        setMessage("");
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
        title: `Committed ${result.short_commit} (${result.subject}).`,
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
        title: `Pushed ${result.branch} to ${result.upstream}.`,
        variant: "success",
      });
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setBusy(null);
    }
  };

  const commitAndPush = async () => {
    setBusy("both");
    const committed = await commit();
    if (!committed) {
      setBusy(null);
      return;
    }
    await push();
  };

  const canCommit = message.trim().length > 0 && busy === null && !loading;
  const branch = status?.branch ?? summary.branch ?? null;

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-[560px]">
          <DialogHeader>
            <DialogTitle className="q-git-title">
              <GitBranchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {branch ?? "No branch"}
            </DialogTitle>
            <DialogDescription>
              {status?.upstream
                ? `Tracking ${status.upstream}`
                : "This branch has no upstream yet; Push creates one on origin."}
            </DialogDescription>
          </DialogHeader>

          {loading && status === null ? (
            <p className="q-faint" role="status">
              Reading the branch…
            </p>
          ) : null}

          {status ? (
            <div className="q-git-status">
              <div className="q-git-meta">
                {status.operation ? (
                  <span className="q-badge">
                    A {status.operation.replace("_", "-")} is in progress
                  </span>
                ) : null}
                {status.upstream ? (
                  <>
                    <span className="q-mono">
                      <ArrowUpIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      {status.ahead} ahead
                    </span>
                    <span className="q-mono">
                      <ArrowDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      {status.behind} behind
                    </span>
                  </>
                ) : null}
                <span className="grow" />
                <button
                  type="button"
                  className="q-ibtn"
                  aria-label="Refresh git status"
                  title="Refresh"
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
                  label="Staged"
                  count={status.staged.count}
                  paths={status.staged.paths}
                  truncated={status.staged.truncated}
                />
                <ChangeSet
                  label="Unstaged"
                  count={status.unstaged.count}
                  paths={status.unstaged.paths}
                  truncated={status.unstaged.truncated}
                />
                <ChangeSet
                  label="Untracked"
                  count={status.untracked.count}
                  paths={status.untracked.paths}
                  truncated={status.untracked.truncated}
                />
              </div>
            </div>
          ) : null}

          <div className="q-field" style={{ display: "block" }}>
            <div className="t q-git-label">
              <label htmlFor="git-commit-message">Commit message</label>
              <button
                type="button"
                className="q-btn ghost"
                onClick={() => void generate()}
                disabled={generating || loading || busy !== null}
              >
                <SparklesIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                {generating ? "Generating…" : "Generate"}
              </button>
            </div>
            <textarea
              id="git-commit-message"
              className="q-input q-mono"
              rows={4}
              value={message}
              placeholder="Write what changed, and why."
              aria-label="Commit message"
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
            Include unstaged changes
          </label>

          {hint ? (
            <p className="q-faint" role="status">
              {hint}
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
              disabled={busy !== null || loading || !branch}
            >
              <GitPullRequestIcon className="q-icon s14" aria-hidden />
              Create pull request…
            </button>
            <span className="grow" />
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => void push()}
              disabled={busy !== null || loading || !branch}
            >
              {busy === "push" ? "Pushing…" : "Push"}
            </button>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => void commitAndPush()}
              disabled={!canCommit}
            >
              {busy === "both" ? "Committing…" : "Commit & push"}
            </button>
            <button
              type="button"
              className="q-btn primary"
              onClick={() => void commit()}
              disabled={!canCommit}
            >
              <GitCommitHorizontalIcon className="q-icon s14" aria-hidden />
              {busy === "commit" ? "Committing…" : "Commit"}
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
  const api = useMemo(gitActionsApi, []);
  const [title, setTitle] = useState(summary.title);
  const [body, setBody] = useState("");
  const [base, setBase] = useState(status?.default_branch ?? "main");
  const [draft, setDraft] = useState(false);
  const [repo, setRepo] = useState<ForgeRepoRef | null>(null);
  const [repoError, setRepoError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [hint, setHint] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setTitle(summary.title);
    setBody("");
    setBase(status?.default_branch ?? "main");
    setDraft(false);
    setError(null);
    setHint(null);
    setRepo(null);
    setRepoError(null);
    let cancelled = false;
    void (async () => {
      try {
        const remotes = await forgeApi().detectRepo(summary.workspace_path);
        if (cancelled) return;
        const first = remotes[0]?.repo ?? null;
        setRepo(first);
        if (!first) {
          setRepoError("No supported forge remote is configured for this workspace.");
        }
      } catch (cause) {
        if (!cancelled) setRepoError(describeError(cause));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, summary.title, summary.workspace_path, status?.default_branch]);

  const generate = async () => {
    setGenerating(true);
    setError(null);
    setHint(null);
    try {
      const text: PullRequestText = await api.generatePullRequest(
        summary.thread_id,
        base.trim() || null,
      );
      if (!text.title && !text.body) {
        setHint("Generation is off. Pick a model in Settings → Source control.");
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
    if (!repo) return;
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
        title: `Opened ${pr.summary.repo.owner}/${pr.summary.repo.repo}#${pr.summary.number}.`,
        variant: "success",
      });
      onPushed?.();
      onOpenChange(false);
    } catch (cause) {
      setError(describeError(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>Create pull request</DialogTitle>
          <DialogDescription>
            {repo
              ? `${repo.owner}/${repo.repo} on ${forgeLabel(repo.forge)}. The daemon pushes the branch when it needs to.`
              : (repoError ?? "Looking for a forge remote…")}
          </DialogDescription>
        </DialogHeader>

        <div className="q-field" style={{ display: "block" }}>
          <div className="t q-git-label">
            <label htmlFor="git-pr-title">Title</label>
            <button
              type="button"
              className="q-btn ghost"
              onClick={() => void generate()}
              disabled={generating || busy}
            >
              <SparklesIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              {generating ? "Generating…" : "Generate"}
            </button>
          </div>
          <input
            id="git-pr-title"
            className="q-input"
            value={title}
            aria-label="Pull request title"
            onChange={(event) => setTitle(event.target.value)}
          />
        </div>

        <label className="q-field" style={{ display: "block" }}>
          <span className="t">Description</span>
          <textarea
            className="q-input"
            rows={5}
            value={body}
            aria-label="Pull request description"
            onChange={(event) => setBody(event.target.value)}
          />
        </label>

        <div className="q-git-pr-fields">
          <label className="q-field" style={{ display: "block" }}>
            <span className="t">Base branch</span>
            <input
              className="q-input q-mono"
              value={base}
              aria-label="Base branch"
              onChange={(event) => setBase(event.target.value)}
            />
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
            Open as a draft
          </label>
        </div>

        {hint ? (
          <p className="q-faint" role="status">
            {hint}
          </p>
        ) : null}
        {error ? (
          <div className="q-banner err wrap" role="alert">
            {error}
          </div>
        ) : null}

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </button>
          <button
            type="button"
            className="q-btn primary"
            onClick={() => void create()}
            disabled={busy || !repo || !title.trim() || !base.trim()}
          >
            {busy ? "Opening…" : "Open pull request"}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
