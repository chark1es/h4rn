import { GitPullRequestIcon } from "lucide-react";
import { useEffect, useState } from "react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import type { ForgePrSummary, ForgeRemote, ThreadSummary } from "@/gen";
import { getClient, pushToast } from "@/lib/store/appStore";

import { ForgeApi, parsePullRequestUrl, prWebUrl, stateLabel } from "./store";
import { notifyThreadLinks } from "./threadLinks";

const defaultApi = () => new ForgeApi((request) => getClient().request(request));

/**
 * "Link pull request…" from the command palette.
 *
 * Pasting a URL is the exact path (any supported host, including a custom one
 * the workspace already knows); picking from the repository's open PRs is the
 * shortcut. Nothing is listed when the workspace has no forge remote.
 */
export function LinkPullRequestDialog({
  open,
  thread,
  onClose,
  api = defaultApi,
}: {
  open: boolean;
  thread: ThreadSummary | null;
  onClose: () => void;
  api?: () => ForgeApi;
}) {
  const [url, setUrl] = useState("");
  const [repos, setRepos] = useState<ForgeRemote[]>([]);
  const [candidates, setCandidates] = useState<ForgePrSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const workspace = thread?.workspace_path ?? null;

  useEffect(() => {
    if (!open || !workspace) return;
    let cancelled = false;
    setUrl("");
    setError(null);
    setRepos([]);
    setCandidates([]);
    setLoading(true);
    const client = api();
    client
      .detectRepo(workspace)
      .then(async (found) => {
        if (cancelled) return;
        setRepos(found);
        const repo = found[0]?.repo;
        if (!repo) return;
        const page = await client.listPrs(repo, { type: "open" }, 1);
        if (!cancelled) setCandidates(page.prs.slice(0, 8));
      })
      .catch((cause: unknown) => {
        if (!cancelled) setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, workspace, api]);

  if (!thread) return null;

  const link = async (repo: ForgeRemote["repo"], number: number) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await api().linkPullRequest(thread.thread_id, repo, number);
      notifyThreadLinks(thread.thread_id);
      pushToast({ title: `Linked #${number} to this thread.`, variant: "success" });
      onClose();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  const submitUrl = () => {
    const reference = parsePullRequestUrl(
      url,
      repos.map((entry) => entry.repo),
    );
    if (!reference) {
      setError(
        "That is not a GitHub, GitLab, Gitea or Forgejo pull request link. Paste the page URL, including the number.",
      );
      return;
    }
    void link(reference.repo, reference.number);
  };

  const knownRepo = repos[0]?.repo;

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>Link pull request</DialogTitle>
          <DialogDescription>
            {knownRepo
              ? `Paste a link, or pick an open pull request from ${knownRepo.owner}/${knownRepo.repo}.`
              : "Paste a pull request link. This workspace has no forge remote, so there is no repository to pick from."}
          </DialogDescription>
        </DialogHeader>

        <div className="q-pr-link-form">
          <Input
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") submitUrl();
            }}
            placeholder="https://github.com/owner/repo/pull/12"
            aria-label="Pull request URL"
            autoFocus
          />
          <button
            type="button"
            className="q-btn primary"
            disabled={busy || url.trim().length === 0}
            onClick={submitUrl}
          >
            Link
          </button>
        </div>

        {error ? (
          <p className="q-field-error" role="alert">
            {error}
          </p>
        ) : null}

        {loading ? (
          <p className="q-faint" style={{ fontSize: 12 }}>
            Reading the repository…
          </p>
        ) : candidates.length > 0 ? (
          <ul className="q-pr-picks" aria-label="Open pull requests">
            {candidates.map((pr) => (
              <li key={pr.number}>
                <button
                  type="button"
                  className="q-pr-pick"
                  disabled={busy}
                  onClick={() => void link(pr.repo, pr.number)}
                  title={prWebUrl(pr.repo, pr.number)}
                >
                  <GitPullRequestIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  <span className="q-num">#{pr.number}</span>
                  <span className="q-pr-pick-title">{pr.title}</span>
                  <span className="q-faint">{stateLabel(pr.state).label}</span>
                </button>
              </li>
            ))}
          </ul>
        ) : knownRepo && !loading && !error ? (
          <p className="q-faint" style={{ fontSize: 12 }}>
            No open pull requests in {knownRepo.owner}/{knownRepo.repo}.
          </p>
        ) : null}

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={onClose}>
            Cancel
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
