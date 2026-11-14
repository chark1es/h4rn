import { ChevronDownIcon, ClockIcon } from "lucide-react";
import { useEffect } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { FailoverOptions, HarnessKind } from "@/gen";
import { useNow } from "@/hooks/use-now";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import { countdownPrecise } from "./presentation";
import {
  cancelResume,
  failOver,
  getUsageState,
  loadFailoverOptions,
  loadResets,
  resumeNow,
  useUsage,
} from "./usageStore";

/**
 * The auto-resume card. It renders a countdown and two typed actions; the
 * daemon decides whether a resume is still possible.
 */
export function AutoResumeCards({
  compact = false,
  threadId,
}: {
  compact?: boolean;
  /** When set, only this thread's schedule is rendered. */
  threadId?: string;
}) {
  const resets = useUsage((state) => state.resets);
  const pending = useUsage((state) => state.pending);
  const failover = useUsage((state) => state.failover);
  const actionError = useUsage((state) => state.actionError);
  const threads = useApp((state) => state.threads);
  const thread = threadId ? threads[threadId] : undefined;
  const now = useNow(1_000);

  // A thread can open without ever visiting Usage, so the schedule list is
  // fetched lazily as soon as the summary says one is armed.
  useEffect(() => {
    if (!threadId) return;
    if (getUsageState().resets || !thread?.resume_at) return;
    void loadResets();
  }, [threadId, thread?.resume_at]);

  const schedules = (resets?.schedules ?? []).filter(
    (schedule) =>
      (schedule.state === "scheduled" || schedule.state === "waiting") &&
      (threadId === undefined || schedule.thread_id === threadId),
  );
  // Ask the daemon where each paused thread could go as soon as its card
  // shows, so the action can say up front when there is nowhere.
  const pausedIds = schedules.map((schedule) => schedule.thread_id).join(",");
  useEffect(() => {
    for (const id of pausedIds.split(",").filter(Boolean)) {
      if (!getUsageState().failover[id]) void loadFailoverOptions(id);
    }
  }, [pausedIds]);

  if (schedules.length === 0) return null;

  return (
    <>
      {schedules.map((schedule) => {
        const thread = threads[schedule.thread_id];
        const state = pending[schedule.thread_id];
        const harness = thread?.harness;
        const failoverMenu = (
          <FailOverMenu
            compact={compact}
            options={failover[schedule.thread_id]}
            harness={harness}
            busy={state !== undefined}
            label={state === "failover" ? "Failing over…" : "Fail over…"}
            onChoose={(choice) => void failOver(schedule.thread_id, choice)}
          />
        );
        const cancel = (
          <button
            type="button"
            className="q-btn secondary"
            disabled={state !== undefined}
            onClick={() => void cancelResume(schedule.thread_id)}
          >
            {state === "cancel" ? "Cancelling…" : compact ? "Cancel" : "Cancel auto-resume"}
          </button>
        );
        const resume = (
          <button
            type="button"
            className="q-btn primary"
            disabled={state !== undefined}
            onClick={() => void resumeNow(schedule.thread_id)}
          >
            {state === "resume" ? "Resuming…" : "Resume now"}
          </button>
        );
        if (compact) {
          return (
            <div key={schedule.schedule_id} className="q-ph-resume">
              <div className="head">
                <span className="q-warn">
                  <ClockIcon className="q-icon s20" strokeWidth={1.5} aria-hidden />
                </span>
                <div className="grow">
                  <div className="ttl">{thread?.title ?? schedule.thread_id} is paused</div>
                  <div className="q-muted sub">
                    {harness ? harnessLabel(harness) : "The harness"} hit a limit.
                  </div>
                </div>
              </div>
              <div>
                <div className="q-num cd">{countdownPrecise(schedule.resume_at, now)}</div>
                <div className="q-muted sub">
                  Auto-resumes at {absoluteTime(schedule.resume_at)}
                </div>
              </div>
              {schedule.last_error ? (
                <div className="q-danger sub">Last attempt failed: {schedule.last_error}</div>
              ) : null}
              <div className="actions">
                {cancel}
                {resume}
              </div>
              <div className="more">{failoverMenu}</div>
            </div>
          );
        }
        return (
          <div key={schedule.schedule_id} className="q-resume" style={{ marginBottom: 28 }}>
            <span className="q-warn">
              <ClockIcon className="q-icon s20" strokeWidth={1.5} aria-hidden />
            </span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontWeight: 600 }}>{thread?.title ?? schedule.thread_id} is paused</div>
              <div className="q-muted" style={{ marginTop: 2, fontSize: 13 }}>
                {harness ? harnessLabel(harness) : "The harness"} hit a limit at{" "}
                {absoluteTime(schedule.scheduled_at)}. h4rn resumes the turn at{" "}
                {absoluteTime(schedule.resume_at)} with its saved context.
              </div>
              {schedule.failover ? (
                <div className="q-faint" style={{ marginTop: 6, fontSize: 12 }}>
                  Will fail over to <span className="q-mono">{schedule.failover.model}</span>
                  {schedule.failover.account_label ? ` on ${schedule.failover.account_label}` : ""}
                </div>
              ) : null}
              {schedule.last_error ? (
                <div className="q-danger" style={{ marginTop: 6, fontSize: 12 }}>
                  Last attempt failed: {schedule.last_error}
                </div>
              ) : null}
            </div>
            <div className="q-num" style={{ textAlign: "right" }}>
              <div className="cd num">{countdownPrecise(schedule.resume_at, now)}</div>
              <div className="q-faint" style={{ fontSize: 12 }}>
                until auto-resume
              </div>
            </div>
            <div style={{ display: "flex", gap: 6 }}>
              {failoverMenu}
              {cancel}
              {resume}
            </div>
          </div>
        );
      })}
      {actionError ? (
        <p className="q-danger" style={{ fontSize: 12, marginTop: -16, marginBottom: 20 }}>
          {actionError.message}
          {actionError.hint ? ` ${actionError.hint}` : ""}
        </p>
      ) : null}
    </>
  );
}

/**
 * "Fail over…": resume now on another model or account. The daemon picks the
 * default and lists the rest; this only shows them and sends the choice back.
 */
function FailOverMenu({
  options,
  harness,
  busy,
  label,
  compact,
  onChoose,
}: {
  options: FailoverOptions | undefined;
  harness: HarnessKind | undefined;
  busy: boolean;
  label: string;
  compact: boolean;
  onChoose: (choice: { model: string; account: string | null }) => void;
}) {
  const none = options !== undefined && options.options.length === 0;
  const reason = none ? (options.unavailable ?? "There is no model to fail over to") : undefined;
  // One reason shared by every row reads once, as the heading.
  const reasons = new Set(options?.options.map((choice) => choice.reason));
  const perRow = reasons.size > 1;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="q-btn ghost"
          disabled={busy || options === undefined || none}
          title={reason}
        >
          {label}
          <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align={compact ? "center" : "end"}
        className="min-w-[260px] max-w-[360px]"
      >
        <DropdownMenuLabel className="font-normal text-fg-3">
          {harness ? harnessLabel(harness) : "The harness"} hit its limit. Resume now on:
        </DropdownMenuLabel>
        {options?.options.map((choice, index) => (
          <DropdownMenuItem
            key={`${choice.model}:${choice.account ?? ""}`}
            onSelect={() => onChoose({ model: choice.model, account: choice.account })}
          >
            <span className="min-w-0 flex-1">
              <span className="block truncate">
                <span className="q-mono">{choice.model}</span>
                {choice.account_label ? (
                  <span className="text-fg-3"> · {choice.account_label}</span>
                ) : null}
                {index === 0 ? <span className="text-fg-3"> · default</span> : null}
              </span>
              {perRow ? (
                <span className="block text-[11.5px] text-fg-3">{choice.reason}</span>
              ) : null}
            </span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
