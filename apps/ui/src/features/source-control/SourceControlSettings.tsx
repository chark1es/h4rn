import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
import { useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
import { LinearSettings } from "@/features/issues/LinearSettings";
import {
  installedTitleAgents,
  saveTitleModel,
  useTextGeneration,
  useTextGenerationData,
} from "@/features/text-generation/textGenerationStore";
import type { HarnessKind, SourceControlModelMode, SourceControlModelSetting } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { i18n } from "@/lib/i18n";

const MODES: SourceControlModelMode[] = ["off", "coding_agent", "text_generator"];

type AgentChoice = HarnessKind | "thread";
const FOLLOW_THREAD = "thread" as const;

/** A radiogroup, matching the title-model picker. */
function ModePicker({
  value,
  onChange,
}: {
  value: SourceControlModelMode;
  onChange: (next: SourceControlModelMode) => void;
}) {
  const { t } = useTranslation(["settings", "common"]);
  return (
    <SegmentedControl
      className="phone:w-full"
      aria-label={t("sourceControl.modeLabel")}
      value={value}
      onValueChange={(next) => {
        if (next) onChange(next as SourceControlModelMode);
      }}
    >
      {MODES.map((mode) => (
        <SegmentedControlItem key={mode} value={mode} className="phone:min-w-0 phone:flex-1">
          {t(`sourceControl.modes.${mode}`)}
        </SegmentedControlItem>
      ))}
    </SegmentedControl>
  );
}

/** The one-line description under the picker. */
export function sourceControlSummary(
  setting: SourceControlModelSetting | null | undefined,
): string {
  const t = i18n.getFixedT(null, "settings", "sourceControl.summary");
  if (!setting) return t("reading");
  if (setting.mode === "off") return t("off");
  if (setting.mode === "coding_agent") {
    if (setting.harness === "claude_code") return t("claudeCode");
    if (setting.harness === "codex") return t("codex");
    return setting.harness
      ? t("otherHarness", { harness: HARNESS_LABELS[setting.harness] })
      : t("followThread");
  }
  return setting.model ? t("generatorModel", { model: setting.model }) : t("generator");
}

/**
 * Settings → Source control: the model that writes commit messages and pull
 * request text, with optional custom instructions.
 *
 * Off starts nothing: Generate leaves the field empty and no model runs.
 */
export function SourceControlSettings() {
  const { t } = useTranslation(["settings", "common"]);
  useTextGenerationData({ settings: true });
  useHarnessVersions();
  const settings = useTextGeneration((state) => state.settings);
  const settingsLoading = useTextGeneration((state) => state.settingsLoading);
  const settingsError = useTextGeneration((state) => state.settingsError);
  const generators = useTextGeneration((state) => state.generators);
  const saving = useTextGeneration((state) => state.savingSettings);
  const saveError = useTextGeneration((state) => state.saveError);
  const harnessRows = useHarnesses((state) => state.rows);
  const agents = useMemo(() => installedTitleAgents(harnessRows), [harnessRows]);
  const [draft, setDraft] = useState<SourceControlModelSetting | null>(
    settings?.source_control ?? null,
  );

  useEffect(() => {
    setDraft(settings?.source_control ?? null);
  }, [settings]);

  const generator = generators.find((candidate) => candidate.id === draft?.generator_id);
  const model = draft?.model ?? generator?.model ?? "";
  const changed =
    draft !== null &&
    settings !== null &&
    (draft.mode !== settings.source_control.mode ||
      draft.harness !== settings.source_control.harness ||
      draft.generator_id !== settings.source_control.generator_id ||
      draft.model !== settings.source_control.model ||
      (draft.instructions ?? "") !== (settings.source_control.instructions ?? ""));
  const canSave =
    draft !== null &&
    (draft.mode !== "text_generator" || (draft.generator_id !== null && model.trim().length > 0));

  const patch = (next: Partial<SourceControlModelSetting>) => {
    if (!draft) return;
    setDraft({ ...draft, ...next });
  };

  const chooseMode = (mode: SourceControlModelMode) => {
    if (!draft) return;
    if (mode === "text_generator") {
      patch({
        mode,
        generator_id: draft.generator_id ?? generators[0]?.id ?? null,
      });
      return;
    }
    patch({ mode });
  };

  const save = async () => {
    if (!settings || !draft) return;
    await saveTitleModel({ ...settings, source_control: draft });
  };

  return (
    <div className="q-form">
      <h1>{t("sourceControl.title")}</h1>
      <p className="lead">{t("sourceControl.lead")}</p>

      {settingsError ? (
        <div className="q-banner err" role="alert">
          {settingsError.message}
        </div>
      ) : null}

      {settingsLoading && !draft ? (
        <p className="q-faint">{t("sourceControl.readingSetting")}</p>
      ) : null}

      {draft ? (
        <>
          <div className="q-group">
            <h2>{t("sourceControl.messages")}</h2>
            <p className="desc">{sourceControlSummary(draft)}</p>
            <div className="q-field">
              <div className="lab">
                <div className="t">{t("sourceControl.whoWrites")}</div>
                <div className="s">{t("sourceControl.whoWritesHint")}</div>
              </div>
              <div className="ctl">
                <ModePicker value={draft.mode} onChange={chooseMode} />
              </div>
            </div>

            {draft.mode === "coding_agent" ? (
              <div className="q-field">
                <div className="lab">
                  <div className="t">{t("sourceControl.modes.coding_agent")}</div>
                  <div className="s">{t("sourceControl.codingAgentHint")}</div>
                  {agents.length === 0 ? (
                    <div className="s q-faint">{t("sourceControl.noClaudeCode")}</div>
                  ) : null}
                </div>
                <div className="ctl">
                  <ChipSelect<AgentChoice>
                    label={t("sourceControl.codingAgentLabel")}
                    value={draft.harness ?? FOLLOW_THREAD}
                    width={220}
                    options={[
                      {
                        value: FOLLOW_THREAD,
                        label: t("sourceControl.followThread"),
                        hint: t("sourceControl.followThreadHint"),
                      },
                      ...agents.map((agent) => ({
                        value: agent.harness,
                        label: HARNESS_LABELS[agent.harness],
                        hint: agent.installed,
                      })),
                    ]}
                    onChange={(choice) =>
                      patch({ harness: choice === FOLLOW_THREAD ? null : choice })
                    }
                  />
                </div>
              </div>
            ) : null}

            {draft.mode === "text_generator" ? (
              <>
                <div className="q-field">
                  <div className="lab">
                    <div className="t">{t("sourceControl.modes.text_generator")}</div>
                    <div className="s">{t("sourceControl.generatorHint")}</div>
                  </div>
                  <div className="ctl">
                    <ChipSelect
                      label={t("sourceControl.modes.text_generator")}
                      value={draft.generator_id ?? ""}
                      width={220}
                      options={generators.map((candidate) => ({
                        value: candidate.id,
                        label: candidate.name,
                        hint: candidate.model,
                      }))}
                      onChange={(generatorId) => patch({ generator_id: generatorId })}
                    />
                  </div>
                </div>
                <div className="q-field">
                  <div className="lab">
                    <div className="t">{t("sourceControl.model")}</div>
                    <div className="s">{t("sourceControl.modelHint")}</div>
                  </div>
                  <div className="ctl">
                    <input
                      className="q-input q-mono"
                      style={{ width: 240 }}
                      aria-label={t("sourceControl.modelLabel")}
                      placeholder={generator?.model ?? t("sourceControl.modelPlaceholder")}
                      value={draft.model ?? ""}
                      onChange={(event) => patch({ model: event.target.value || null })}
                    />
                  </div>
                </div>
                {generators.length === 0 ? (
                  <p className="q-faint">{t("sourceControl.noGenerator")}</p>
                ) : null}
              </>
            ) : null}

            {draft.mode !== "off" ? (
              <label className="q-field q-field-block">
                <span className="t">{t("sourceControl.instructions")}</span>
                <span className="s">{t("sourceControl.instructionsHint")}</span>
                <textarea
                  className="q-input"
                  rows={3}
                  maxLength={2000}
                  value={draft.instructions ?? ""}
                  aria-label={t("sourceControl.instructionsLabel")}
                  placeholder={t("sourceControl.instructionsPlaceholder")}
                  onChange={(event) => patch({ instructions: event.target.value || null })}
                />
              </label>
            ) : null}
          </div>

          {saveError ? (
            <div className="q-banner err" role="alert">
              {saveError.message}
            </div>
          ) : null}

          {settings ? (
            <p className="q-faint" style={{ fontSize: 12 }}>
              {sourceControlSummary(draft ?? settings.source_control)}
            </p>
          ) : null}

          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
            <button
              type="button"
              className="q-btn ghost"
              disabled={!changed}
              onClick={() => setDraft(settings?.source_control ?? draft)}
            >
              {t("common:reset")}
            </button>
            <button
              type="button"
              className="q-btn primary"
              disabled={!changed || !canSave || saving}
              onClick={() => void save()}
            >
              {saving ? t("common:saving") : t("common:save")}
            </button>
          </div>
        </>
      ) : null}
      <LinearSettings />
    </div>
  );
}
