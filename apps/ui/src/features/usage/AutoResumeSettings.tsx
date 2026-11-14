import { useEffect, useState } from "react";
import { ChipSelect } from "@/components/common/ChipSelect";
import type { AutoResumePolicy } from "@/gen";

import { duration } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import { loadLimits, setAutoResumePolicy, useUsage } from "./usageStore";

const WAIT_CHOICES = [
  { label: "30 minutes", ms: 30 * 60_000 },
  { label: "2 hours", ms: 2 * 60 * 60_000 },
  { label: "6 hours", ms: 6 * 60 * 60_000 },
  { label: "24 hours", ms: 24 * 60 * 60_000 },
];

function Toggle({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label: string;
  onChange: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
    />
  );
}

/**
 * Settings for the daemon's auto-resume policy. The form edits a draft and
 * sends the whole policy; the daemon is the only scheduler.
 */
export function AutoResumeSettings() {
  const limits = useUsage((state) => state.limits);
  const saving = useUsage((state) => state.policySaving);
  const error = useUsage((state) => state.policyError);
  const connection = useApp((state) => state.connection.status);
  const [draft, setDraft] = useState<AutoResumePolicy | null>(null);

  useEffect(() => {
    if (connection === "online") void loadLimits();
  }, [connection]);

  useEffect(() => {
    if (limits?.policy && !draft) setDraft(limits.policy);
  }, [limits, draft]);

  if (!draft) {
    return (
      <div className="q-form">
        <h1>Auto-resume</h1>
        <div className="q-skel" style={{ height: 12, width: 240 }} />
        <div className="q-skel" style={{ height: 120, marginTop: 18 }} />
      </div>
    );
  }

  const patch = (next: Partial<AutoResumePolicy>) => setDraft({ ...draft, ...next });

  return (
    <div className="q-form">
      <h1>Auto-resume</h1>
      <p className="lead">
        When a harness reports a limit, h4rn can keep the turn queued and try again when the
        provider resets. The daemon schedules this; closing the app does not cancel it.
      </p>

      <div className="q-group">
        <h2>Policy</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Resume automatically</div>
            <div className="s">
              {draft.failover
                ? "A proxy failover model is preferred when the reset fires."
                : "Retry the same harness and model when the reset fires."}
            </div>
          </div>
          <div className="ctl">
            <Toggle
              checked={draft.failover}
              label="Try a failover model on resume"
              onChange={(failover) => patch({ failover })}
            />
            <span className="q-faint" style={{ fontSize: 12 }}>
              {draft.failover ? "Failover first" : "Same model"}
            </span>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Longest wait</div>
            <div className="s">Unknown reset times are retried within this window</div>
          </div>
          <div className="ctl">
            <ChipSelect<string>
              label="Longest wait"
              value={String(draft.max_wait_ms)}
              width={180}
              options={WAIT_CHOICES.map((choice) => ({
                value: String(choice.ms),
                label: choice.label,
              }))}
              onChange={(next) => patch({ max_wait_ms: Number(next) })}
            />
            {!WAIT_CHOICES.some((choice) => choice.ms === draft.max_wait_ms) ? (
              <span className="q-faint" style={{ fontSize: 12 }}>
                Current: {duration(draft.max_wait_ms)}
              </span>
            ) : null}
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Jitter</div>
            <div className="s">Stops many threads resuming on the same second</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              type="number"
              min={0}
              step={5}
              value={Math.round(draft.jitter_ms / 1000)}
              aria-label="Jitter in seconds"
              onChange={(event) =>
                patch({ jitter_ms: Math.max(0, Number(event.target.value) || 0) * 1000 })
              }
              style={{ maxWidth: 100, fontSize: 12 }}
            />
            <span className="q-faint" style={{ fontSize: 12 }}>
              seconds
            </span>
          </div>
        </div>
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">Resume prompt</div>
            <div className="s">Sent to the session with its saved context</div>
          </div>
          <div className="ctl" style={{ display: "block" }}>
            <textarea
              className="q-input"
              value={draft.prompt}
              aria-label="Resume prompt"
              rows={1}
              ref={(node) => {
                // Grows with its text instead of showing a resize grip.
                if (node) {
                  node.style.height = "auto";
                  node.style.height = `${node.scrollHeight}px`;
                }
              }}
              onChange={(event) => patch({ prompt: event.target.value })}
              style={{
                width: "100%",
                maxWidth: 520,
                fontSize: 12.5,
                padding: 8,
                resize: "none",
                overflow: "hidden",
              }}
            />
          </div>
        </div>
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <span
            className="q-faint"
            style={{ fontSize: 12, marginRight: "auto", alignSelf: "center" }}
          >
            {limits?.limits.length ?? 0} limits observed ·{" "}
            {limits?.policy.failover ? "failover on" : "failover off"}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            onClick={() => limits && setDraft(limits.policy)}
            disabled={saving}
          >
            Reset
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={saving}
            onClick={() => {
              void setAutoResumePolicy(draft);
            }}
          >
            {saving ? "Saving…" : "Save policy"}
          </button>
        </div>
        {error ? (
          <p className="q-danger" style={{ fontSize: 12, marginTop: 8 }}>
            {error.message}
            {error.hint ? ` ${error.hint}` : ""}
          </p>
        ) : null}
      </div>
    </div>
  );
}
