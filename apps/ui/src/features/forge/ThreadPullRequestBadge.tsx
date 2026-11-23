import { ExternalLinkIcon, EyeIcon, GitPullRequestIcon, XIcon } from "lucide-react";
import { useState } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { ThreadPullRequest } from "@/gen";
import { getClient, pushToast } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { ForgeApi, linkedPullLabel, linkedPullTone, prWebUrl, stateLabel } from "./store";
import { notifyThreadLinks, useThreadPullRequests } from "./threadLinks";

const resolveApi = () => new ForgeApi((request) => getClient().request(request));

/** Opens a pull request in the browser from a menu row. */
function openPull(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}

/** The quiet header badge and its linked-items popover. */
export function ThreadPullRequestBadge({ threadId }: { threadId: string }) {
  const { pulls, reload } = useThreadPullRequests(threadId, resolveApi);
  const [busy, setBusy] = useState<string | null>(null);

  if (pulls.length === 0) return null;

  const run = async (key: string, action: () => Promise<void>) => {
    setBusy(key);
    try {
      await action();
      notifyThreadLinks(threadId);
    } catch (cause) {
      pushToast({
        title: "The pull request link did not change.",
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
      busy={busy}
      onOpen={reload}
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
    />
  );
}

/** Presentational badge + panel, so markup can be tested without a socket. */
export function LinkedPullRequestsMenu({
  pulls,
  busy = null,
  onOpen,
  onUnlink,
  onWatch,
  onUnwatch,
}: {
  pulls: ThreadPullRequest[];
  busy?: string | null;
  /** Reads the daemon again when the panel opens. */
  onOpen?: () => void;
  onUnlink: (pull: ThreadPullRequest) => void;
  onWatch: (pull: ThreadPullRequest) => void;
  onUnwatch: (pull: ThreadPullRequest) => void;
}) {
  const primary = pulls[0];
  if (!primary) return null;
  const extra = pulls.length - 1;

  return (
    <DropdownMenu onOpenChange={(open) => open && onOpen?.()}>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className={cn("q-badge q-pr-badge", linkedPullTone(primary))}
          aria-label="Linked pull requests"
          title="Linked pull requests"
        >
          <GitPullRequestIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          <span>{linkedPullLabel(primary)}</span>
          {extra > 0 ? <span className="q-pr-more">+{extra}</span> : null}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-80 max-h-80 overflow-y-auto">
        <DropdownMenuLabel>Linked pull requests</DropdownMenuLabel>
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
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
