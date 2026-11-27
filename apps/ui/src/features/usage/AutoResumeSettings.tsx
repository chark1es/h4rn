import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Switch } from "@/components/ui/switch";
import type { AutoResumePolicy } from "@/gen";
import { formatDuration, formatNumber } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";
import { loadLimits, setAutoResumePolicy, useUsage } from "./usageStore";

const WAIT_CHOICES: Array<{ amount: number; unit: "minute" | "hour"; ms: number }> = [
  { amount: 30, unit: "minute", ms: 30 * 60_000 },
  { amount: 2, unit: "hour", ms: 2 * 60 * 60_000 },
  { amount: 6, unit: "hour", ms: 6 * 60 * 60_000 },
  { amount: 24, unit: "hour", ms: 24 * 60 * 60_000 },
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
  return <Switch checked={checked} aria-label={label} onCheckedChange={() => onChange(!checked)} />;
}

/**
 * Settings for the daemon's auto-resume policy. The form edits a draft and
 * sends the whole policy; the daemon is the only scheduler.
 */
export function AutoResumeSettings() {
  const { t } = useTranslation(["settings", "common"]);
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
        <h1>{t("resume.title")}</h1>
        <div className="q-skel" style={{ height: 12, width: 240 }} />
        <div className="q-skel" style={{ height: 120, marginTop: 18 }} />
      </div>
    );
  }

  const patch = (next: Partial<AutoResumePolicy>) => setDraft({ ...draft, ...next });

  return (
    <div className="q-form">
      <h1>{t("resume.title")}</h1>
      <p className="lead">{t("resume.lead")}</p>

      <div className="q-group">
        <h2>{t("resume.policy")}</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("resume.automatically")}</div>
            <div className="s">
              {draft.failover ? t("resume.failoverHint") : t("resume.sameModelHint")}
            </div>
          </div>
          <div className="ctl">
            <Toggle
              checked={draft.failover}
              label={t("resume.failoverLabel")}
              onChange={(failover) => patch({ failover })}
            />
            <span className="q-faint" style={{ fontSize: 12 }}>
              {draft.failover ? t("resume.failoverFirst") : t("resume.sameModel")}
            </span>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("resume.longestWait")}</div>
            <div className="s">{t("resume.longestWaitHint")}</div>
          </div>
          <div className="ctl">
            <ChipSelect<string>
              label={t("resume.longestWait")}
              value={String(draft.max_wait_ms)}
              width={180}
              options={WAIT_CHOICES.map((choice) => ({
                value: String(choice.ms),
                label: formatNumber(choice.amount, {
                  style: "unit",
                  unit: choice.unit,
                  unitDisplay: "long",
                }),
              }))}
              onChange={(next) => patch({ max_wait_ms: Number(next) })}
            />
            {!WAIT_CHOICES.some((choice) => choice.ms === draft.max_wait_ms) ? (
              <span className="q-faint" style={{ fontSize: 12 }}>
                {t("resume.current", { duration: formatDuration(draft.max_wait_ms) })}
              </span>
            ) : null}
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("resume.jitter")}</div>
            <div className="s">{t("resume.jitterHint")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              type="number"
              min={0}
              step={5}
              value={Math.round(draft.jitter_ms / 1000)}
              aria-label={t("resume.jitterLabel")}
              onChange={(event) =>
                patch({ jitter_ms: Math.max(0, Number(event.target.value) || 0) * 1000 })
              }
              style={{ maxWidth: 100, fontSize: 12 }}
            />
            <span className="q-faint" style={{ fontSize: 12 }}>
              {t("resume.seconds")}
            </span>
          </div>
        </div>
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">{t("resume.prompt")}</div>
            <div className="s">{t("resume.promptHint")}</div>
          </div>
          <div className="ctl" style={{ display: "block" }}>
            <textarea
              className="q-input"
              value={draft.prompt}
              aria-label={t("resume.prompt")}
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
            {t("resume.observed", { count: limits?.limits.length ?? 0 })}
            {" · "}
            {limits?.policy.failover ? t("resume.failoverOn") : t("resume.failoverOff")}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            onClick={() => limits && setDraft(limits.policy)}
            disabled={saving}
          >
            {t("common:reset")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={saving}
            onClick={() => {
              void setAutoResumePolicy(draft);
            }}
          >
            {saving ? t("common:saving") : t("resume.savePolicy")}
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
