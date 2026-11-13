import { ClockIcon, MoreHorizontalIcon, PinIcon } from "lucide-react";
import type { MouseEvent } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Badge } from "@/components/ui/badge";
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
import { harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { cueNumber, relativeAge } from "@/lib/format";
import { cn } from "@/lib/utils";

type ThreadRowProps = {
  thread: ThreadSummary;
  host: HostSummary | undefined;
  index: number;
  selected: boolean;
  now: number;
  onOpen: () => void;
  onAcknowledge: () => void;
  onSettle: () => void;
  onReopen: () => void;
  onPin: () => void;
  onSnooze: (until: string | null) => void;
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

export function ThreadRow({
  thread,
  host,
  index,
  selected,
  now,
  onOpen,
  onAcknowledge,
  onSettle,
  onReopen,
  onPin,
  onSnooze,
}: ThreadRowProps) {
  const hostOnline = host?.online ?? false;
  const glyph = glyphForThread(thread, hostOnline);
  const actionable =
    thread.agent_state === "awaiting_approval" ||
    thread.needs_attention ||
    thread.agent_state === "error";
  const isDone = thread.tab === "done";
  const isSettled = thread.tab === "settled";

  const stop = (event: MouseEvent) => event.stopPropagation();

  return (
    <div
      role="option"
      aria-selected={selected}
      tabIndex={-1}
      data-thread-id={thread.thread_id}
      className={cn(
        "group/row relative flex h-16 w-full items-center gap-2 border-b border-line/70 pr-2 pl-3 text-left transition-colors duration-[var(--dur-instant)] hover:bg-paper md:h-12",
        selected && "bg-paper",
      )}
      onClick={onOpen}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          onOpen();
        }
      }}
    >
      {selected ? <span aria-hidden className="absolute inset-y-0 left-0 w-0.5 bg-cue" /> : null}

      <span className="w-5 shrink-0 text-right font-mono text-micro tabular text-ink-faint">
        {cueNumber(index)}
      </span>

      <span className="flex min-w-0 flex-1 flex-col justify-center gap-0.5 overflow-hidden">
        <span className="flex min-w-0 items-center gap-1.5">
          <span
            className={cn(
              "truncate text-compact text-ink",
              thread.unread ? "font-semibold" : "font-medium",
            )}
          >
            {thread.title}
          </span>
          {thread.pinned ? (
            <PinIcon className="size-3 shrink-0 text-ink-faint" aria-label="Pinned to Working" />
          ) : null}
        </span>

        <span className="flex min-w-0 items-center gap-1.5 overflow-hidden text-micro text-ink-muted">
          <EntityAvatar
            seed={machineSeed(thread.host)}
            kind="machine"
            label={host?.name ?? thread.host}
            size={14}
          />
          <EntityAvatar
            seed={harnessSeed(thread.host, thread.harness)}
            kind="harness"
            label={harnessLabel(thread.harness)}
            size={14}
          />
          <span className={cn("flex min-w-0 items-center gap-1", actionable && "lg:hidden")}>
            <AgentGlyph glyph={glyph} size={14} />
            <span className={cn("truncate", actionable && "text-cue-ink")}>{glyph.label}</span>
          </span>
          {thread.agent_state === "rate_limited" && !actionable ? (
            <ClockIcon className="size-3 shrink-0 text-ink-faint" aria-label="Limited" />
          ) : null}
          <span className="ml-auto shrink-0 font-mono tabular text-ink-faint">
            {relativeAge(thread.last_activity, now)}
          </span>
        </span>
      </span>

      {actionable && !isSettled ? (
        <Badge variant="cue" className="hidden shrink-0 lg:inline-flex">
          {thread.agent_state === "awaiting_approval"
            ? "Approval"
            : thread.agent_state === "error"
              ? "Failed"
              : agentStateLabel(thread.agent_state)}
        </Badge>
      ) : null}
      {isDone && thread.unread ? (
        <Badge variant="standby" className="hidden shrink-0 lg:inline-flex">
          Unread
        </Badge>
      ) : null}

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={`Actions for ${thread.title}`}
            className="flex size-9 shrink-0 items-center justify-center rounded-sm text-ink-faint opacity-0 transition-opacity duration-[var(--dur-instant)] hover:bg-panel hover:text-ink focus-visible:opacity-100 focus-visible:ring-2 focus-visible:ring-ring group-hover/row:opacity-100 md:size-7"
            onClick={stop}
          >
            <MoreHorizontalIcon className="size-4" />
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
              <span className="ml-auto text-micro text-ink-faint">A</span>
            </DropdownMenuItem>
          ) : null}
          {isSettled ? <DropdownMenuItem onSelect={onReopen}>Reopen</DropdownMenuItem> : null}
          {!isSettled ? <DropdownMenuItem onSelect={onSettle}>Settle now</DropdownMenuItem> : null}
          <DropdownMenuItem onSelect={onPin}>
            {thread.pinned ? "Unpin from Working" : "Keep in Working"}
            <span className="ml-auto text-micro text-ink-faint">P</span>
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
