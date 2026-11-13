import { CircleAlertIcon, InboxIcon, RefreshCwIcon, ServerIcon } from "lucide-react";
import { useEffect } from "react";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EmptyState } from "@/components/common/EmptyState";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import {
  ensureThread,
  interruptThread,
  navigate,
  requestComposerFocus,
  retryThread,
  useApp,
} from "@/lib/store/appStore";
import { Composer } from "./Composer";
import { StandbyBar } from "./StandbyBar";
import { ThreadHeader } from "./ThreadHeader";
import { ThreadStream, ThreadStreamSkeleton } from "./ThreadStream";

function HeaderSkeleton() {
  return (
    <div className="flex h-14 shrink-0 items-center gap-3 border-b border-line px-4">
      <Skeleton className="h-4 w-56" />
      <Skeleton className="ml-auto h-3 w-24" />
    </div>
  );
}

function WaitingForTurn({
  threadId,
  host,
  harness,
}: {
  threadId: string;
  host: string;
  harness: string;
}) {
  const glyph = glyphForAgentState("thinking");
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="mx-auto w-full max-w-[760px] flex-1 px-4 py-6">
        <div className="flex items-center gap-2">
          <AgentGlyph glyph={glyph} size={16} />
          <p className="text-micro text-ink-muted">
            Waiting for {harness} on {host}…
          </p>
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto h-7"
            onClick={() => void interruptThread(threadId)}
          >
            Cancel
          </Button>
        </div>
        <div className="mt-4 space-y-2" aria-hidden>
          <Skeleton className="h-3 w-10/12" />
          <Skeleton className="h-3 w-8/12" />
          <Skeleton className="h-3 w-5/12" />
        </div>
        <p className="mt-4 text-micro text-ink-faint">
          The thread exists on the daemon. Nothing has streamed yet; the first token will replace
          this outline.
        </p>
      </div>
    </div>
  );
}

export function ThreadPane({ threadId }: { threadId: string }) {
  const summary = useApp((state) => state.threads[threadId]);
  const host = useApp((state) => state.hosts[summary?.host ?? ""]);
  const log = useApp((state) => state.logs[threadId]);
  const threadError = useApp((state) => state.threadErrors[threadId]);

  useEffect(() => {
    ensureThread(threadId);
  }, [threadId]);

  if (!summary && !log && !threadError) {
    return (
      <main className="flex h-full min-h-0 flex-1 flex-col bg-paper" aria-busy="true">
        <HeaderSkeleton />
        <ThreadStreamSkeleton />
        <div className="border-t border-line px-4 py-3">
          <Skeleton className="h-9 w-full" />
        </div>
      </main>
    );
  }

  if (!summary) {
    return (
      <main className="flex h-full min-h-0 flex-1 flex-col bg-paper">
        <div className="flex h-14 items-center border-b border-line px-4">
          <h2 className="text-title text-ink">Thread</h2>
        </div>
        <EmptyState
          icon={CircleAlertIcon}
          title="This thread couldn’t be loaded."
          description={
            threadError
              ? `${threadError.message} The run may still be active on its host.`
              : "It may have been settled, or the daemon restarted. The run may still be active on its host."
          }
          action={
            <>
              <Button onClick={() => retryThread(threadId)}>
                <RefreshCwIcon aria-hidden />
                Retry
              </Button>
              <Button variant="secondary" onClick={() => navigate({ kind: "hosts" })}>
                <ServerIcon aria-hidden />
                Open hosts
              </Button>
            </>
          }
        />
      </main>
    );
  }

  const hostOnline = host?.online ?? false;
  const turns = log?.turns ?? [];
  const pending = log?.pending ?? [];
  const loaded = log?.loaded ?? false;
  const waiting =
    loaded && turns.length === 0 && pending.length === 0 && summary.agent_state !== "done";

  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-paper" aria-label={summary.title}>
      <ThreadHeader summary={summary} host={host} onBack={() => navigate({ kind: "inbox" })} />

      {!hostOnline && summary.tab === "working" ? (
        <div className="shrink-0 border-b border-strike/30 bg-strike-tint/50 px-4 py-1.5 text-micro text-strike-ink">
          <span className="font-medium">{host?.name ?? summary.host} is unreachable.</span> The turn
          may still be running. The cached stream is read-only until the host returns.
        </div>
      ) : null}

      {threadError && log?.loaded ? (
        <div className="shrink-0 border-b border-strike/30 bg-strike-tint/50 px-4 py-1.5 text-micro text-strike-ink">
          <span className="font-medium">The stream may be stale.</span> {threadError.message}{" "}
          <button
            type="button"
            className="font-medium underline underline-offset-2"
            onClick={() => retryThread(threadId)}
          >
            Retry
          </button>
        </div>
      ) : null}

      {!loaded && threadError && !log?.loading ? (
        <EmptyState
          icon={CircleAlertIcon}
          title="The stream couldn’t load."
          description={`${threadError.message} ${threadError.hint ?? "Check the daemon connection, then retry."}`}
          action={
            <Button onClick={() => retryThread(threadId)}>
              <RefreshCwIcon aria-hidden />
              Retry
            </Button>
          }
          compact
          className="flex-1"
        />
      ) : !loaded ? (
        <ThreadStreamSkeleton />
      ) : waiting ? (
        <WaitingForTurn
          threadId={threadId}
          host={host?.name ?? summary.host}
          harness={harnessLabel(summary.harness)}
        />
      ) : turns.length === 0 ? (
        <EmptyState
          icon={InboxIcon}
          title="Nothing has streamed yet."
          description={`The thread is ${summary.agent_state === "done" ? "finished" : "idle"} on ${host?.name ?? summary.host}. Reply below to start the next turn.`}
          compact
          className="flex-1"
        />
      ) : (
        <ThreadStream
          threadId={threadId}
          summary={summary}
          turns={turns}
          pending={pending}
          onRetry={requestComposerFocus}
        />
      )}

      <StandbyBar threadId={threadId} />
      <Composer summary={summary} />
    </main>
  );
}
