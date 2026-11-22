import { useEffect, useMemo, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
import {
  installedTitleAgents,
  saveTitleModel,
  titleModelSummary,
  useTextGeneration,
  useTextGenerationData,
} from "@/features/text-generation/textGenerationStore";
import type { HarnessKind, SourceControlModelMode, SourceControlModelSetting } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";

const MODES: Array<{ value: SourceControlModelMode; label: string }> = [
  { value: "off", label: "Off" },
  { value: "coding_agent", label: "Coding agent" },
  { value: "text_generator", label: "Text generator" },
];

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
  return (
    <div
      className="q-seg inline q-seg-choice"
      role="radiogroup"
      aria-label="Commit and pull request model"
    >
      {MODES.map((mode) => (
        // biome-ignore lint/a11y/useSemanticElements: a segmented radio group, not a form radio
        <button
          key={mode.value}
          type="button"
          role="radio"
          aria-checked={value === mode.value}
          onClick={() => onChange(mode.value)}
        >
          {mode.label}
        </button>
      ))}
    </div>
  );
}

/** The one-line description under the picker. */
export function sourceControlSummary(
  setting: SourceControlModelSetting | null | undefined,
): string {
  if (!setting) return "Reading the message model…";
  if (setting.mode === "off") return "Commit and pull request text is written by you.";
  if (setting.mode === "coding_agent") {
    if (setting.harness === "claude_code") return "Messages are written by Claude Code.";
    if (setting.harness === "codex") {
      return "Codex is not used for messages; pick Claude Code or a text generator.";
    }
    return setting.harness
      ? `${HARNESS_LABELS[setting.harness]} is not used for messages.`
      : "Claude Code threads use their own agent; other threads use a text generator or write it themselves.";
  }
  return setting.model
    ? `Messages are written by a text generator with ${setting.model}.`
    : "Messages are written by the selected text generator.";
}

/**
 * Settings → Source control: the model that writes commit messages and pull
 * request text, with optional custom instructions.
 *
 * Off starts nothing: Generate leaves the field empty and no model runs.
 */
export function SourceControlSettings() {
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
      <h1>Source control</h1>
      <p className="lead">
        Commit messages and pull request text can be written by a model when you ask for them. A
        request you write yourself is never replaced, and Generate never commits or opens anything.
      </p>

      {settingsError ? (
        <div className="q-banner err" role="alert">
          {settingsError.message}
        </div>
      ) : null}

      {settingsLoading && !draft ? <p className="q-faint">Reading the setting…</p> : null}

      {draft ? (
        <>
          <div className="q-group">
            <h2>Commit &amp; PR messages</h2>
            <p className="desc">{sourceControlSummary(draft)}</p>
            <div className="q-field">
              <div className="lab">
                <div className="t">Who writes messages</div>
                <div className="s">
                  Off leaves the fields empty and starts nothing. A coding agent runs once with no
                  tools; a text generator is the model you configured under Providers.
                </div>
              </div>
              <div className="ctl">
                <ModePicker value={draft.mode} onChange={chooseMode} />
              </div>
            </div>

            {draft.mode === "coding_agent" ? (
              <div className="q-field">
                <div className="lab">
                  <div className="t">Coding agent</div>
                  <div className="s">
                    Follow the thread's agent uses Claude Code for Claude Code threads; other
                    threads keep their own model choice.
                  </div>
                  {agents.length === 0 ? (
                    <div className="s q-faint">No Claude Code is installed on this host yet.</div>
                  ) : null}
                </div>
                <div className="ctl">
                  <ChipSelect<AgentChoice>
                    label="Coding agent for messages"
                    value={draft.harness ?? FOLLOW_THREAD}
                    width={220}
                    options={[
                      {
                        value: FOLLOW_THREAD,
                        label: "Follow the thread's agent",
                        hint: "Claude Code threads use their own",
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
                    <div className="t">Text generator</div>
                    <div className="s">The provider is configured under Providers.</div>
                  </div>
                  <div className="ctl">
                    <ChipSelect
                      label="Text generator"
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
                    <div className="t">Model</div>
                    <div className="s">Leave empty to use the generator's own model.</div>
                  </div>
                  <div className="ctl">
                    <input
                      className="q-input q-mono"
                      style={{ width: 240 }}
                      aria-label="Model for commit and pull request messages"
                      placeholder={generator?.model ?? "model name"}
                      value={draft.model ?? ""}
                      onChange={(event) => patch({ model: event.target.value || null })}
                    />
                  </div>
                </div>
                {generators.length === 0 ? (
                  <p className="q-faint">
                    No text generator is configured yet. Add one under Providers.
                  </p>
                ) : null}
              </>
            ) : null}

            {draft.mode !== "off" ? (
              <label className="q-field" style={{ display: "block" }}>
                <span className="t">Custom instructions</span>
                <span className="s">
                  Optional. Appended to the trusted instruction for every commit message and pull
                  request. Never spliced into the repository text.
                </span>
                <textarea
                  className="q-input"
                  rows={3}
                  maxLength={2000}
                  value={draft.instructions ?? ""}
                  aria-label="Custom instructions for commit and pull request messages"
                  placeholder="For example: use the imperative mood, and mention the user-visible change."
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
              {titleModelSummary(settings)}
            </p>
          ) : null}

          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
            <button
              type="button"
              className="q-btn ghost"
              disabled={!changed}
              onClick={() => setDraft(settings?.source_control ?? draft)}
            >
              Reset
            </button>
            <button
              type="button"
              className="q-btn primary"
              disabled={!changed || !canSave || saving}
              onClick={() => void save()}
            >
              {saving ? "Saving…" : "Save"}
            </button>
          </div>
        </>
      ) : null}
    </div>
  );
}
