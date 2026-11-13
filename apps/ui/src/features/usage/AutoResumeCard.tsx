import { ClockIcon, TriangleAlertIcon } from "lucide-react";
import { useNow } from "@/hooks/use-now";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { countdownPrecise } from "./presentation";
import { cancelResume, resumeNow, useUsage } from "./usageStore";

/**
 * The auto-resume card. It renders a countdown and two typed actions; the
 * daemon decides whether a resume is still possible.
 */
export function AutoResumeCards({ compact = false }: { compact?: boolean }) {
  const resets = useUsage((state) => state.resets);
  const pending = useUsage((state) => state.pending);
  const actionError = useUsage((state) => state.actionError);
  const threads = useApp((state) => state.threads);
  const now = useNow(1_000);

  const schedules = (resets?.schedules ?? []).filter(
    (schedule) => schedule.state === "scheduled" || schedule.state === "waiting",
  );
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
              <div style={{ fontWeight: 600 }}>{thread?.title ?? schedule.thread_id}</div>
              <div className="q-muted" style={{ marginTop: 2, fontSize: compact ? 14 : 13 }}>
                {harness ? harnessLabel(harness) : "The harness"} hit a limit at{" "}
                {absoluteTime(schedule.scheduled_at)}. h4rn resumes the turn at{" "}
                {absoluteTime(schedule.resume_at)} with its saved context.
              </div>
              {schedule.reason ? (
                <div className="q-faint" style={{ marginTop: 4, fontSize: 12 }}>
                  {schedule.reason}
                </div>
              ) : null}
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
              <div className={compact ? "q-num" : "cd num"}>
                {countdownPrecise(schedule.resume_at, now)}
              </div>
              <div className="q-faint" style={{ fontSize: 12 }}>
                until auto-resume
              </div>
            </div>
            <div style={{ display: "flex", gap: 6 }}>
              <button
                type="button"
                className="q-btn secondary"
                disabled={state !== undefined}
                onClick={() => void cancelResume(schedule.thread_id)}
              >
                {state === "cancel" ? "Cancelling…" : "Cancel"}
              </button>
              <button
                type="button"
                className="q-btn primary"
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
