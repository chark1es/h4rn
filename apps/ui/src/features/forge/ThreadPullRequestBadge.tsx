import { ExternalLinkIcon, EyeIcon, EyeOffIcon, GitPullRequestIcon, XIcon } from "lucide-react";
import { useState } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
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

/** The quiet header badge and its linked-items popover. */
export function ThreadPullRequestBadge({ threadId }: { threadId: string }) {
  const { pulls } = useThreadPullRequests(threadId, resolveApi);
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
  onUnlink,
  onWatch,
  onUnwatch,
}: {
  pulls: ThreadPullRequest[];
  busy?: string | null;
  onUnlink: (pull: ThreadPullRequest) => void;
  onWatch: (pull: ThreadPullRequest) => void;
  onUnwatch: (pull: ThreadPullRequest) => void;
}) {
  const primary = pulls[0];
  if (!primary) return null;
  const extra = pulls.length - 1;

  return (
    <DropdownMenu>
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
      <DropdownMenuContent align="end" className="w-80">
        <DropdownMenuLabel>Linked pull requests</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <div className="q-pr-links">
          {pulls.map((pull) => {
            const url = pull.summary?.web_url || prWebUrl(pull.repo, pull.number);
            const state = pull.summary ? stateLabel(pull.summary.state).label : "State unavailable";
            return (
              <div className="q-pr-link" key={`${pull.repo.base_url}:${pull.number}`}>
                <div className="q-pr-link-main">
                  <a
                    className="q-pr-link-title"
                    href={url}
                    target="_blank"
                    rel="noopener noreferrer"
                    title={pull.summary?.title ?? `Open ${url}`}
                  >
                    <span className="q-num">#{pull.number}</span>
                    <span className="q-pr-link-name">{pull.summary?.title ?? "Pull request"}</span>
                    <ExternalLinkIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  </a>
                  <div className="q-pr-link-meta">
                    <span className={cn(pull.summary?.state === "closed" && "q-danger")}>
                      {state}
                    </span>
                    {pull.error ? <span className="q-faint">{pull.error}</span> : null}
                    <span className="grow" />
                    <button
                      type="button"
                      className="q-link"
                      disabled={busy !== null}
                      onClick={() => (pull.watching ? onUnwatch(pull) : onWatch(pull))}
                    >
                      {pull.watching ? (
                        <>
                          <EyeIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Stop
                          watching
                        </>
                      ) : (
                        <>
                          <EyeOffIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Watch
                        </>
                      )}
                    </button>
                    <button
                      type="button"
                      className="q-link q-faint"
                      disabled={busy !== null}
                      onClick={() => onUnlink(pull)}
                      title="Unlink this pull request"
                    >
                      <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Unlink
                    </button>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
