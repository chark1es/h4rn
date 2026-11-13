import { RefreshCwIcon } from "lucide-react";

import { Kbd } from "@/components/common/Kbd";
import { cost, latency, tokens } from "@/lib/format";
import { getClient, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const STATUS_WORD = {
  connecting: "Connecting",
  online: "Online",
  degraded: "Reconnecting",
  offline: "Offline",
} as const;

export function StatusBar() {
  const connection = useApp((state) => state.connection);
  const route = useApp((state) => state.ui.route);
  const threadId = route.kind === "thread" ? route.threadId : null;
  const summary = useApp((state) => (threadId ? state.threads[threadId] : undefined));
  const log = useApp((state) => (threadId ? state.logs[threadId] : undefined));
  const sequenceHint = useApp((state) => state.ui.sequenceHint);
  const serverVersion = useApp((state) => state.serverVersion);

  const cueCount = log?.turns.length ?? 0;
  const currentCue = cueCount > 0 ? cueCount : null;

  return (
    <footer className="flex h-6 shrink-0 items-center gap-3 bg-stage px-3 text-micro text-stage-ink-muted">
      <span className="flex items-center gap-1.5 text-stage-ink">
        <span
          aria-hidden
          className={cn(
            "size-1.5 rounded-full",
            connection.status === "online"
              ? "bg-standby"
              : connection.status === "degraded"
                ? "bg-cue"
                : connection.status === "offline"
                  ? "bg-strike"
                  : "bg-stage-ink-muted",
          )}
        />
        {STATUS_WORD[connection.status]}
        <span className="text-stage-ink-muted">· {connection.transport}</span>
      </span>

      {connection.latencyMs !== null ? (
        <span className="font-mono tabular">{latency(connection.latencyMs)}</span>
      ) : null}

      {currentCue ? (
        <span className="font-mono tabular">
          cue {currentCue.toString().padStart(2, "0")}/{cueCount.toString().padStart(2, "0")}
        </span>
      ) : null}

      {summary ? (
        <span className="hidden items-center gap-2 font-mono tabular sm:flex">
          <span>{tokens(summary.usage.input_tokens + summary.usage.output_tokens)} tok</span>
          <span aria-hidden>·</span>
          <span>{cost(summary.usage.cost_micros)}</span>
        </span>
      ) : null}

      {sequenceHint ? (
        <span className="flex items-center gap-1 text-cue">
          <Kbd className="border-cue/30 bg-cue/10 text-cue">{sequenceHint}</Kbd>
          <span>waiting for the next key</span>
        </span>
      ) : null}

      {connection.status !== "online" && connection.error ? (
        <span
          className="hidden max-w-[380px] truncate text-cue lg:inline"
          title={connection.error.message}
        >
          {connection.error.message}
        </span>
      ) : null}

      <span className="ml-auto flex items-center gap-3">
        {connection.status === "offline" ? (
          <button
            type="button"
            className="flex items-center gap-1 text-cue hover:text-stage-ink"
            onClick={() => getClient().reconnectNow()}
          >
            <RefreshCwIcon className="size-3" aria-hidden />
            Reconnect
          </button>
        ) : null}
        <span className="hidden font-mono tabular sm:inline">direct · 127.0.0.1:47800</span>
        <span className="font-mono tabular">{serverVersion ? `v${serverVersion}` : "—"}</span>
      </span>
    </footer>
  );
}
