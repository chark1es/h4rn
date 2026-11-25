import { CircleDotIcon, ExternalLinkIcon, EyeIcon, GitPullRequestIcon, XIcon } from "lucide-react";
import { useState } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { IssueApi, issueStateLabel } from "@/features/issues/store";
import { notifyThreadIssueLinks, useThreadIssues } from "@/features/issues/threadLinks";
import type { ThreadIssue, ThreadPullRequest } from "@/gen";
import { getClient, pushToast } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { ForgeApi, linkedPullLabel, linkedPullTone, prWebUrl, stateLabel } from "./store";
import { notifyThreadLinks, useThreadPullRequests } from "./threadLinks";

const resolveApi = () => new ForgeApi((request) => getClient().request(request));
const resolveIssueApi = () => new IssueApi((request) => getClient().request(request));

/** Opens a pull request or issue in the browser from a menu row. */
function openPull(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}

/** The scope that tells two same-numbered issues apart. */
function issueScopeLabel(issue: ThreadIssue): string {
  if (issue.scope.tracker === "forge") {
    const { owner, repo } = issue.scope.repo;
    return `${owner}/${repo}#${issue.issue_id}`;
  }
  const state = issue.state;
  if (state?.tracker === "linear") {
    // Name the team the way Linear shows it (`ENG`), not its internal UUID.
    return `${state.issue.identifier} (${state.issue.team_key})`;
  }
  // A failed refresh empties the state. The identifier stored at link time
  // still names the issue; a link made before it was stored gets a label
  // instead of the provider's internal id.
  return issue.identifier ?? "Linear issue";
}

/**
 * The row's identity comes from what the link stored (the team and the issue
 * id), never from the live read: a failed refresh empties the state, and a key
 * that followed it would remount the row and drop its open menu.
 */
function issueScopeKey(issue: ThreadIssue): string {
  if (issue.scope.tracker === "forge") {
    const { base_url, owner, repo } = issue.scope.repo;
    return `forge:${base_url}:${owner}/${repo}#${issue.issue_id}`;
  }
  return `linear:${issue.scope.team_id}:${issue.issue_id}`;
}

function issueLabel(issue: ThreadIssue): string {
  const state = issue.state;
  if (!state) return issueScopeLabel(issue);
  if (state.tracker === "forge") {
    return `${issueScopeLabel(issue)} ${issueStateLabel(state.issue.state)}`;
  }
  return `${issueScopeLabel(issue)} ${state.issue.state_name}`;
}

function issueTitle(issue: ThreadIssue): string {
  return issue.state?.issue.title ?? issue.title ?? "Issue unavailable";
}

/**
 * A refresh error is the daemon's own words, but an older daemon names the
 * issue by the provider's internal id. Replace it with the name the person
 * knows, so the UUID never appears on screen.
 */
function issueErrorMessage(issue: ThreadIssue): string {
  const error = issue.error ?? "";
  if (issue.scope.tracker !== "linear" || !issue.issue_id) return error;
  const name = issue.identifier ?? "Linear issue";
  // An older daemon may have echoed the id in either case.
  const id = issue.issue_id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return error
    .replace(new RegExp(`Linear issue ${id}`, "gi"), () => name)
    .replace(new RegExp(id, "gi"), () => name);
}

function issueUrl(issue: ThreadIssue): string {
  const state = issue.state;
  if (!state) return "";
  return state.tracker === "forge" ? state.issue.web_url : state.issue.url;
}

/** The quiet header badge and its linked-items popover. */
export function ThreadPullRequestBadge({ threadId }: { threadId: string }) {
  const { pulls, reload } = useThreadPullRequests(threadId, resolveApi);
  const { issues, reload: reloadIssues } = useThreadIssues(threadId, resolveIssueApi);
  const [busy, setBusy] = useState<string | null>(null);

  if (pulls.length === 0 && issues.length === 0) return null;

  const run = async (key: string, action: () => Promise<void>) => {
    setBusy(key);
    try {
      await action();
      notifyThreadLinks(threadId);
      notifyThreadIssueLinks(threadId);
    } catch (cause) {
      pushToast({
        title: "The linked item did not change.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(null);
    }
  };

  return (
    <LinkedPullRequestsMenu
      pulls={pulls}
      issues={issues}
      busy={busy}
      onOpen={() => {
        reload();
        reloadIssues();
      }}
      onUnlink={(pull) =>
        run(`unlink:${pull.number}`, () =>
          resolveApi().unlinkPullRequest(threadId, pull.repo, pull.number),
        )
      }
      onWatch={(pull) =>
        run(`watch:${pull.number}`, () =>
          resolveApi().watchPullRequest(threadId, pull.repo, pull.number),
        )
      }
      onUnwatch={(pull) =>
        run(`unwatch:${pull.number}`, () =>
          resolveApi().unwatchPullRequest(threadId, pull.repo, pull.number),
        )
      }
      onUnlinkIssue={(issue) =>
        run(`unlink-issue:${issue.issue_id}`, () =>
          resolveIssueApi().unlinkIssue(threadId, issue.scope, issue.issue_id),
        )
      }
    />
  );
}

/** Presentational badge + panel, so markup can be tested without a socket. */
export function LinkedPullRequestsMenu({
  pulls,
  issues = [],
  busy = null,
  onOpen,
  onUnlink,
  onWatch,
  onUnwatch,
  onUnlinkIssue,
}: {
  pulls: ThreadPullRequest[];
  issues?: ThreadIssue[];
  busy?: string | null;
  /** Reads the daemon again when the panel opens. */
  onOpen?: () => void;
  onUnlink: (pull: ThreadPullRequest) => void;
  onWatch: (pull: ThreadPullRequest) => void;
  onUnwatch: (pull: ThreadPullRequest) => void;
  onUnlinkIssue?: (issue: ThreadIssue) => void;
}) {
  const primary = pulls[0];
  const primaryIssue = issues[0];
  if (!primary && !primaryIssue) return null;
  const extra = pulls.length + issues.length - 1;

  return (
    <DropdownMenu onOpenChange={(open) => open && onOpen?.()}>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className={cn("q-badge q-pr-badge", primary ? linkedPullTone(primary) : "")}
          aria-label="Linked items"
          title="Linked items"
        >
          {primary ? (
            <GitPullRequestIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          ) : (
            <CircleDotIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          )}
          <span>
            {primary ? linkedPullLabel(primary) : primaryIssue ? issueLabel(primaryIssue) : ""}
          </span>
          {extra > 0 ? <span className="q-pr-more">+{extra}</span> : null}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-80 max-h-80 overflow-y-auto">
        <DropdownMenuLabel>Linked items</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {pulls.map((pull, index) => {
          const url = pull.summary?.web_url || prWebUrl(pull.repo, pull.number);
          const state = pull.summary ? stateLabel(pull.summary.state) : null;
          const finished = pull.summary?.state === "merged" || pull.summary?.state === "closed";
          return (
            <div className="q-pr-links" key={`${pull.repo.base_url}:${pull.number}`}>
              {index > 0 ? <DropdownMenuSeparator /> : null}
              <DropdownMenuItem
                className="q-pr-row"
                disabled={busy !== null}
                onSelect={() => openPull(url)}
              >
                <GitPullRequestIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                <span className="q-num">#{pull.number}</span>
                <span className="q-pr-link-name">
                  {pull.summary?.title ?? "Pull request unavailable"}
                </span>
                <span
                  className={cn(
                    "q-pr-row-state",
                    state?.tone === "danger" && "q-danger",
                    state?.tone === "ok" && "q-ok",
                  )}
                >
                  {state?.label ?? "State unavailable"}
                </span>
                <ExternalLinkIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              </DropdownMenuItem>
              {pull.error ? <p className="q-pr-note q-danger">{pull.error}</p> : null}
              {/* A merged or closed PR has nothing left to watch. */}
              {pull.watching || !finished ? (
                <DropdownMenuItem
                  disabled={busy !== null}
                  onSelect={() => (pull.watching ? onUnwatch(pull) : onWatch(pull))}
                >
                  {/* The eye marks the current state; the action row that ends a
                    watch is the only place it appears. */}
                  {pull.watching ? (
                    <EyeIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  ) : null}
                  {pull.watching ? "Stop watching" : "Watch for changes"}
                </DropdownMenuItem>
              ) : null}
              {pull.watch_notice ? <p className="q-pr-note">{pull.watch_notice}</p> : null}
              <DropdownMenuItem disabled={busy !== null} onSelect={() => onUnlink(pull)}>
                <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                Unlink
              </DropdownMenuItem>
            </div>
          );
        })}
        {issues.map((issue, index) => {
          const url = issueUrl(issue);
          return (
            <div className="q-pr-links" key={`issue:${issueScopeKey(issue)}`}>
              {pulls.length > 0 || index > 0 ? <DropdownMenuSeparator /> : null}
              <DropdownMenuItem
                className="q-pr-row"
                disabled={busy !== null || !url}
                onSelect={() => url && openPull(url)}
              >
                <CircleDotIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                <span className="q-num">{issueLabel(issue)}</span>
                <span className="q-pr-link-name">{issueTitle(issue)}</span>
                <ExternalLinkIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              </DropdownMenuItem>
              {issue.error ? (
                <p className="q-pr-note q-danger">{issueErrorMessage(issue)}</p>
              ) : null}
              {onUnlinkIssue ? (
                <DropdownMenuItem disabled={busy !== null} onSelect={() => onUnlinkIssue(issue)}>
                  <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  Unlink issue
                </DropdownMenuItem>
              ) : null}
            </div>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
