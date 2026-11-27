import { GitBranchIcon, MoreHorizontalIcon, PlayIcon } from "lucide-react";
import type { MouseEvent } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Kbd } from "@/components/ui/kbd";
import { openGuestShare } from "@/features/guest/guestStore";
import type { HostSummary, ThreadSummary } from "@/gen";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime, tokensPerSecond } from "@/lib/format";
import { formatAge, formatNumber } from "@/lib/i18n";
import type enSidebar from "@/lib/i18n/locales/en/sidebar.json";
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

type StatusKey = keyof typeof enSidebar.row.status;

/** The row's status as a catalogue key and tone; the row words it. */
function statusFor(thread: ThreadSummary): { key: StatusKey; tone: string; rate?: string } {
  const critical =
    thread.agent_state === "awaiting_approval" ||
    thread.agent_state === "error" ||
    thread.agent_state === "unreachable";
  if ((thread.pending_guest_requests ?? 0) > 0 && !critical) {
    return { key: "request", tone: "you" };
  }
  switch (thread.agent_state) {
    case "awaiting_approval":
      return { key: "needsApproval", tone: "you" };
    case "error":
      return { key: "failed", tone: "danger" };
    case "unreachable":
      return { key: "unreachable", tone: "danger" };
    case "queued":
      return { key: "queued", tone: "" };
    case "scheduled":
      return { key: "scheduled", tone: "warn" };
    case "rate_limited":
      return thread.needs_attention
        ? { key: "limitedNeedsYou", tone: "warn" }
        : { key: "limited", tone: "warn" };
    case "done":
      if (thread.tab === "settled") return { key: "settled", tone: "" };
      return thread.unread ? { key: "unread", tone: "" } : { key: "done", tone: "" };
    case "idle":
      return { key: thread.tab === "settled" ? "settled" : "idle", tone: "" };
    case "running_tool": {
      const rate = tokensPerSecond(thread.usage.tps);
      return rate === "—"
        ? { key: "running", tone: "" }
        : { key: "runningRate", tone: "", rate: formatNumber(Number(rate)) };
    }
    case "thinking":
      return { key: "thinking", tone: "" };
    case "searching":
      return { key: "searching", tone: "" };
    case "reading":
      return { key: "reading", tone: "" };
    case "editing":
      return { key: "editing", tone: "" };
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
  const { t } = useTranslation("sidebar");
  const hostOnline = host?.online ?? false;
  const glyph = glyphForThread(thread, hostOnline);
  const status = statusFor(thread);
  const limited = thread.agent_state === "scheduled" || thread.agent_state === "rate_limited";
  const statusText =
    resumeAt && limited
      ? t("row.resumes", { time: absoluteTime(resumeAt) })
      : t(`row.status.${status.key}`, { rate: status.rate });
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
          <span className="q-row-time q-num">{formatAge(thread.last_activity, now)}</span>
        </span>
        <span className="q-row-l2">
          <span className="q-row-meta">
            {compact
              ? harnessLabel(thread.harness)
              : `${host?.name ?? thread.host} · ${harnessLabel(thread.harness)}`}
          </span>
          {thread.branch ? (
            <span className="q-row-branch" title={thread.branch}>
              <GitBranchIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              <span className="q-mono">{thread.branch}</span>
            </span>
          ) : null}
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
                  {t("row.resumeNow")}
                </button>
                <button
                  type="button"
                  className="q-btn ghost"
                  onClick={(event) => {
                    stop(event);
                    onAcknowledge();
                  }}
                >
                  {t("row.acknowledge")}
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
                {t("row.acknowledge")} <Kbd>A</Kbd>
              </button>
            )}
          </span>
        ) : null}
      </span>

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={t("row.actionsFor", { title: thread.title })}
            className="q-ibtn q-row-more"
            tabIndex={-1}
            onClick={stop}
          >
            <MoreHorizontalIcon className="q-icon" strokeWidth={1.5} />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-52" onClick={stop}>
          <DropdownMenuLabel>
            {t("row.harnessOn", {
              harness: harnessLabel(thread.harness),
              host: host?.name ?? thread.host,
            })}
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          {isDone ? (
            <DropdownMenuItem onSelect={onAcknowledge}>
              {t("row.acknowledge")}
              <span className="ml-auto text-small text-fg-3">A</span>
            </DropdownMenuItem>
          ) : null}
          {isSettled ? (
            <DropdownMenuItem onSelect={onReopen}>{t("row.reopen")}</DropdownMenuItem>
          ) : null}
          {!isSettled ? (
            <DropdownMenuItem onSelect={onSettle}>{t("row.settleNow")}</DropdownMenuItem>
          ) : null}
          {!isSettled ? (
            <DropdownMenuItem onSelect={() => openGuestShare(thread.thread_id)}>
              {t("row.share")}
            </DropdownMenuItem>
          ) : null}
          <DropdownMenuItem onSelect={onPin}>
            {thread.pinned ? t("row.unpin") : t("row.keep")}
            <span className="ml-auto text-small text-fg-3">P</span>
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem onSelect={() => onSnooze(snoozeUntil(1))}>
            {t("row.snoozeHours", { count: 1 })}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={() => onSnooze(snoozeUntil(3))}>
            {t("row.snoozeHours", { count: 3 })}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={() => onSnooze(tomorrowMorning())}>
            {t("row.snoozeTomorrow")}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
