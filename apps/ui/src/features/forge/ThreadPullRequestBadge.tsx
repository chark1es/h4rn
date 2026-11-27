import { CircleDotIcon, ExternalLinkIcon, EyeIcon, GitPullRequestIcon, XIcon } from "lucide-react";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";

import {
  Menu,
  MenuContent,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuNote,
  MenuSeparator,
  MenuTrigger,
} from "@/components/ui/menu";
import { IssueApi, issueStateLabel } from "@/features/issues/store";
import { notifyThreadIssueLinks, useThreadIssues } from "@/features/issues/threadLinks";
import type { ThreadIssue, ThreadPullRequest } from "@/gen";
import { i18n } from "@/lib/i18n";
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
  return issue.identifier ?? i18n.t("pullRequests:linked.linearIssue");
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
  return issue.state?.issue.title ?? issue.title ?? i18n.t("pullRequests:linked.issueUnavailable");
}

/**
 * A refresh error is the daemon's own words, but an older daemon names the
 * issue by the provider's internal id. Replace it with the name the person
 * knows, so the UUID never appears on screen.
 */
function issueErrorMessage(issue: ThreadIssue): string {
  const error = issue.error ?? "";
  if (issue.scope.tracker !== "linear" || !issue.issue_id) return error;
  const name = issue.identifier ?? i18n.t("pullRequests:linked.linearIssue");
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
  const { t } = useTranslation("pullRequests");
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
        title: t("linked.changeFailed"),
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
  const { t } = useTranslation("pullRequests");
  const primary = pulls[0];
  const primaryIssue = issues[0];
  if (!primary && !primaryIssue) return null;
  const extra = pulls.length + issues.length - 1;

  return (
    <Menu onOpenChange={(open) => open && onOpen?.()}>
      <MenuTrigger
        className={cn("q-badge q-pr-badge", primary ? linkedPullTone(primary) : "")}
        aria-label={t("linked.items")}
        title={t("linked.items")}
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
      </MenuTrigger>
      <MenuContent align="end" className="w-80">
        {pulls.map((pull, index) => {
          const url = pull.summary?.web_url || prWebUrl(pull.repo, pull.number);
          const state = pull.summary ? stateLabel(pull.summary.state) : null;
          const finished = pull.summary?.state === "merged" || pull.summary?.state === "closed";
          return (
            <Fragment key={`${pull.repo.base_url}:${pull.number}`}>
              {index > 0 ? <MenuSeparator /> : null}
              <MenuGroup aria-label={`#${pull.number}`}>
                {index === 0 ? <MenuGroupLabel>{t("linked.items")}</MenuGroupLabel> : null}
                <MenuItem
                  icon={<GitPullRequestIcon />}
                  disabled={busy !== null}
                  onSelect={() => openPull(url)}
                  trailing={
                    <>
                      <span
                        className={cn(
                          state?.tone === "danger" && "text-destructive",
                          state?.tone === "ok" && "text-ok",
                        )}
                      >
                        {state?.label ?? t("linked.stateUnavailable")}
                      </span>
                      <ExternalLinkIcon aria-hidden />
                    </>
                  }
                >
                  <span className="tabular-nums">#{pull.number}</span>{" "}
                  {pull.summary?.title ?? t("linked.pullUnavailable")}
                </MenuItem>
                {pull.error ? <MenuNote tone="danger">{pull.error}</MenuNote> : null}
                {/* A merged or closed PR has nothing left to watch. */}
                {pull.watching || !finished ? (
                  // The eye marks the current state, so only the row that ends a watch has it.
                  <MenuItem
                    icon={pull.watching ? <EyeIcon /> : null}
                    disabled={busy !== null}
                    onSelect={() => (pull.watching ? onUnwatch(pull) : onWatch(pull))}
                  >
                    {pull.watching ? t("linked.stopWatching") : t("linked.watch")}
                  </MenuItem>
                ) : null}
                {pull.watch_notice ? <MenuNote>{pull.watch_notice}</MenuNote> : null}
                <MenuItem icon={<XIcon />} disabled={busy !== null} onSelect={() => onUnlink(pull)}>
                  {t("linked.unlink")}
                </MenuItem>
              </MenuGroup>
            </Fragment>
          );
        })}
        {issues.map((issue, index) => {
          const url = issueUrl(issue);
          const first = pulls.length === 0 && index === 0;
          return (
            <Fragment key={`issue:${issueScopeKey(issue)}`}>
              {first ? null : <MenuSeparator />}
              <MenuGroup aria-label={issueLabel(issue)}>
                {first ? <MenuGroupLabel>{t("linked.items")}</MenuGroupLabel> : null}
                <MenuItem
                  icon={<CircleDotIcon />}
                  disabled={busy !== null || !url}
                  onSelect={() => url && openPull(url)}
                  trailing={<ExternalLinkIcon aria-hidden />}
                >
                  <span className="tabular-nums">{issueLabel(issue)}</span> {issueTitle(issue)}
                </MenuItem>
                {issue.error ? <MenuNote tone="danger">{issueErrorMessage(issue)}</MenuNote> : null}
                {onUnlinkIssue ? (
                  <MenuItem
                    icon={<XIcon />}
                    disabled={busy !== null}
                    onSelect={() => onUnlinkIssue(issue)}
                  >
                    {t("linked.unlinkIssue")}
                  </MenuItem>
                ) : null}
              </MenuGroup>
            </Fragment>
          );
        })}
      </MenuContent>
    </Menu>
  );
}
