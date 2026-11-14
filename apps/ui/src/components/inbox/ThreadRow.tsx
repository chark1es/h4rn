import { MoreHorizontalIcon, PlayIcon } from "lucide-react";
import type { MouseEvent } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HostSummary, ThreadSummary } from "@/gen";
import { agentStateLabel, glyphForThread } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime, relativeAge, tokensPerSecond } from "@/lib/format";
import { cn } from "@/lib/utils";

type ThreadRowProps = {
  thread: ThreadSummary;
  host: HostSummary | undefined;
  index: number;
  selected: boolean;
  tabStop?: boolean;
  resumeAt?: string | null;
  now: number;
  compact?: boolean;
  onOpen: () => void;
  onAcknowledge: () => void;
  onSettle: () => void;
  onReopen: () => void;
  onPin: () => void;
  onSnooze: (until: string | null) => void;
  onFocus?: () => void;
};

function snoozeUntil(hours: number): string {
  return new Date(Date.now() + hours * 3_600_000).toISOString();
}

function tomorrowMorning(): string {
  const date = new Date();
  date.setDate(date.getDate() + 1);
  date.setHours(9, 0, 0, 0);
  return date.toISOString();
}

function statusFor(thread: ThreadSummary): { text: string; tone: string } {
  switch (thread.agent_state) {
    case "awaiting_approval":
      return { text: "Needs approval", tone: "you" };
    case "error":
      return { text: "Failed", tone: "danger" };
    case "unreachable":
      return { text: "Host unreachable", tone: "danger" };
    case "queued":
      return { text: "Queued", tone: "" };
    case "scheduled":
      return { text: "Auto-resume scheduled", tone: "warn" };
    case "rate_limited":
      return thread.needs_attention
        ? { text: "Limited · needs you", tone: "warn" }
        : { text: "Limited", tone: "warn" };
    case "done":
      if (thread.tab === "settled") return { text: "Settled", tone: "" };
      return thread.unread ? { text: "Unread", tone: "" } : { text: "Done", tone: "" };
    case "idle":
      return { text: thread.tab === "settled" ? "Settled" : "Idle", tone: "" };
    case "running_tool": {
      const rate = tokensPerSecond(thread.usage.tps);
      return { text: rate === "—" ? "Running" : `Running · ${rate} tok/s`, tone: "" };
    }
    default:
      return { text: agentStateLabel(thread.agent_state), tone: "" };
  }
}

export function ThreadRow({
  thread,
  host,
  selected,
  tabStop = false,
  resumeAt = null,
  now,
  compact = false,
  onOpen,
  onAcknowledge,
  onSettle,
  onReopen,
  onPin,
  onSnooze,
  onFocus,
}: ThreadRowProps) {
  const hostOnline = host?.online ?? false;
  const glyph = glyphForThread(thread, hostOnline);
  const status = statusFor(thread);
  const limited = thread.agent_state === "scheduled" || thread.agent_state === "rate_limited";
  const statusText = resumeAt && limited ? `Resumes ${absoluteTime(resumeAt)}` : status.text;
  const isDone = thread.tab === "done";
  const isSettled = thread.tab === "settled";
  const needsResume = isDone && thread.needs_attention;

  const stop = (event: MouseEvent) => event.stopPropagation();

  return (
    <div
      role="option"
      aria-selected={selected}
      tabIndex={tabStop ? 0 : -1}
      data-thread-id={thread.thread_id}
      className={cn("q-row", selected && "sel", isSettled && "dim")}
      onClick={onOpen}
      onFocus={onFocus}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          onOpen();
        }
      }}
    >
      <span className="q-row-st">
        <AgentGlyph glyph={glyph} size={14} />
      </span>
      <span className="q-row-main">
        <span className="q-row-l1">
          <span className="q-row-title">{thread.title}</span>
          <span className="q-row-time q-num">{relativeAge(thread.last_activity, now)}</span>
        </span>
        <span className="q-row-l2">
          <span className="q-row-meta">
            {compact
              ? harnessLabel(thread.harness)
              : `${host?.name ?? thread.host} · ${harnessLabel(thread.harness)}`}
          </span>
          <span className={cn("q-row-status", status.tone)}>{statusText}</span>
        </span>
        {isDone ? (
          <span className="q-row-act">
            {needsResume ? (
              <>
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={(event) => {
                    stop(event);
                    onReopen();
                  }}
                >
                  <PlayIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  Resume now
                </button>
                <button
                  type="button"
                  className="q-btn ghost"
                  onClick={(event) => {
                    stop(event);
                    onAcknowledge();
                  }}
                >
                  Acknowledge
                </button>
              </>
            ) : (
              <button
                type="button"
                className="q-btn secondary"
                onClick={(event) => {
                  stop(event);
                  onAcknowledge();
                }}
              >
                Acknowledge <span className="q-kbd">A</span>
              </button>
            )}
          </span>
        ) : null}
      </span>

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={`Actions for ${thread.title}`}
            className="q-ibtn q-row-more"
            tabIndex={-1}
            onClick={stop}
          >
            <MoreHorizontalIcon className="q-icon" strokeWidth={1.5} />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-52" onClick={stop}>
          <DropdownMenuLabel>
            {harnessLabel(thread.harness)} on {host?.name ?? thread.host}
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          {isDone ? (
            <DropdownMenuItem onSelect={onAcknowledge}>
              Acknowledge
              <span className="ml-auto text-small text-fg-3">A</span>
            </DropdownMenuItem>
          ) : null}
          {isSettled ? <DropdownMenuItem onSelect={onReopen}>Reopen</DropdownMenuItem> : null}
          {!isSettled ? <DropdownMenuItem onSelect={onSettle}>Settle now</DropdownMenuItem> : null}
          <DropdownMenuItem onSelect={onPin}>
            {thread.pinned ? "Unpin from Working" : "Keep in Working"}
            <span className="ml-auto text-small text-fg-3">P</span>
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem onSelect={() => onSnooze(snoozeUntil(1))}>
            Snooze for 1 h
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={() => onSnooze(snoozeUntil(3))}>
            Snooze for 3 h
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={() => onSnooze(tomorrowMorning())}>
            Snooze until tomorrow
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
