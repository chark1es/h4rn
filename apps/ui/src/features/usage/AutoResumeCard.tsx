import { ChevronDownIcon, ClockIcon, TriangleAlertIcon } from "lucide-react";
import { useEffect } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { FailoverOptions } from "@/gen";
import { useNow } from "@/hooks/use-now";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

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
  // biome-ignore lint/correctness/useExhaustiveDependencies: keyed by the joined ids
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
        return (
          <div
            key={schedule.schedule_id}
            className={cn(compact ? "q-ph-group" : "q-resume")}
            style={compact ? undefined : { marginBottom: 28 }}
          >
            <span className="q-warn">
              <ClockIcon className="q-icon s20" strokeWidth={1.5} aria-hidden />
            </span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontWeight: 600 }}>{thread?.title ?? schedule.thread_id} is paused</div>
              <div className="q-muted" style={{ marginTop: 2, fontSize: compact ? 14 : 13 }}>
                {harness ? harnessLabel(harness) : "The harness"} hit a limit at{" "}
                {absoluteTime(schedule.scheduled_at)}. h4rn resumes the turn at{" "}
                {absoluteTime(schedule.resume_at)} with its saved context.
              </div>
              {schedule.failover ? (
                <div
                  className="q-faint"
                  style={{ marginTop: 6, fontSize: 12, display: "flex", gap: 6 }}
                >
                  <TriangleAlertIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  Failover ready: {schedule.failover.model}
                  {schedule.failover.account ? ` on ${schedule.failover.account}` : ""} ·{" "}
                  {schedule.failover.reason}
                </div>
              ) : null}
              {schedule.last_error ? (
                <div className="q-danger" style={{ marginTop: 6, fontSize: 12 }}>
                  Last attempt failed: {schedule.last_error}
                </div>
              ) : null}
            </div>
            <div className={compact ? undefined : "q-num"} style={{ textAlign: "right" }}>
              <div
                className={compact ? "q-num" : "cd num"}
                style={compact ? { fontSize: 30, lineHeight: "36px", fontWeight: 600 } : undefined}
              >
                {countdownPrecise(schedule.resume_at, now)}
              </div>
              <div className="q-faint" style={{ fontSize: 12 }}>
                until auto-resume
              </div>
            </div>
            <div style={{ display: "flex", gap: compact ? 10 : 6, marginTop: compact ? 12 : 0 }}>
              <FailOverMenu
                compact={compact}
                options={failover[schedule.thread_id]}
                busy={state !== undefined}
                label={state === "failover" ? "Failing over…" : "Fail over…"}
                onChoose={(choice) => void failOver(schedule.thread_id, choice)}
              />
              <button
                type="button"
                className="q-btn secondary"
                style={compact ? { flex: 1 } : undefined}
                disabled={state !== undefined}
                onClick={() => void cancelResume(schedule.thread_id)}
              >
                {state === "cancel" ? "Cancelling…" : compact ? "Cancel" : "Cancel auto-resume"}
              </button>
              <button
                type="button"
                className="q-btn primary"
                style={compact ? { flex: 1 } : undefined}
                disabled={state !== undefined}
                onClick={() => void resumeNow(schedule.thread_id)}
              >
                {state === "resume" ? "Resuming…" : "Resume now"}
              </button>
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
  busy,
  label,
  compact,
  onChoose,
}: {
  options: FailoverOptions | undefined;
  busy: boolean;
  label: string;
  compact: boolean;
  onChoose: (choice: { model: string; account: string | null }) => void;
}) {
  const none = options !== undefined && options.options.length === 0;
  const reason = none ? (options.unavailable ?? "There is no model to fail over to") : undefined;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="q-btn ghost"
          style={compact ? { flex: 1 } : undefined}
          disabled={busy || options === undefined || none}
          title={reason}
        >
          {label}
          <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-[240px] max-w-[360px]">
        {options?.options.map((choice, index) => (
          <DropdownMenuItem
            key={`${choice.model}:${choice.account ?? ""}`}
            onSelect={() => onChoose({ model: choice.model, account: choice.account })}
          >
            <span className="min-w-0 flex-1">
              <span className="block truncate">
                {choice.model}
                {choice.account ? ` on ${choice.account}` : ""}
                {index === 0 ? <span className="text-fg-3"> · default</span> : null}
              </span>
              <span className="block text-[11.5px] text-fg-3">{choice.reason}</span>
            </span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
