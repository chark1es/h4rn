import { CircleCheckIcon, KeyRoundIcon, PlusIcon, TriangleAlertIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
import { useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
import type {
  HarnessKind,
  TextGenerationSettings,
  TextGeneratorDraft,
  TextGeneratorId,
  TextGeneratorKind,
  TitleModelMode,
  TitleModelSetting,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { formatNumber, i18n } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";

import {
  addTextGenerator,
  clearGeneratorSaveError,
  installedTitleAgents,
  loadTextGenerators,
  loadTitleModel,
  removeTextGenerator,
  saveTitleModel,
  testTextGenerator,
  titleModelSummary,
  useTextGeneration,
  validateGeneratorDraft,
} from "./textGenerationStore";

const MODES: TitleModelMode[] = ["off", "coding_agent", "text_generator"];

/**
 * The coding-agent picker has one value the protocol does not: `thread`
 * follows each thread's own agent (a `null` harness).
 */
type AgentChoice = HarnessKind | "thread";
const FOLLOW_THREAD = "thread" as const;

const KINDS: TextGeneratorKind[] = ["open_ai_compatible", "anthropic"];

function useHostName(): string {
  const { t } = useTranslation(["settings", "common"]);
  const name = useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
  );
  return name ?? t("textGeneration.thisHost");
}

function kindLabel(kind: TextGeneratorKind): string {
  return i18n.t(`settings:textGeneration.kinds.${kind}.label`);
}

function kindHint(kind: TextGeneratorKind): string {
  return i18n.t(`settings:textGeneration.kinds.${kind}.hint`);
}

function TestResult({ generatorId }: { generatorId: TextGeneratorId }) {
  const report = useTextGeneration((state) => state.reports[generatorId]);
  const error = useTextGeneration((state) => state.testErrors[generatorId]);
  const testing = useTextGeneration((state) => state.testing[generatorId] === true);
  const { t } = useTranslation(["settings", "common"]);
  if (testing) {
    return (
      <span className="q-faint" style={{ fontSize: 12 }}>
        {t("textGeneration.sendingPrompt")}
      </span>
    );
  }
  if (error) {
    return (
      <span className="q-danger text-xs" role="alert">
        {error.message}
        {error.hint ? ` ${error.hint}` : ""}
      </span>
    );
  }
  if (!report) return null;
  if (report.ok) {
    return (
      <span className="q-ok text-xs">
        <CircleCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
        {report.reply
          ? t("textGeneration.repliedWith", {
              latency: formatNumber(Number(report.latency_ms), {
                style: "unit",
                unit: "millisecond",
                unitDisplay: "short",
              }),
              reply: report.reply,
            })
          : t("textGeneration.replied", {
              latency: formatNumber(Number(report.latency_ms), {
                style: "unit",
                unit: "millisecond",
                unitDisplay: "short",
              }),
            })}
      </span>
    );
  }
  return (
    <span className="q-danger text-xs" role="alert">
      <TriangleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
      {report.error ?? t("textGeneration.noAnswer")}
      {report.hint ? ` ${report.hint}` : ""}
    </span>
  );
}

function GeneratorRow({ id }: { id: TextGeneratorId }) {
  const generator = useTextGeneration((state) =>
    state.generators.find((candidate) => candidate.id === id),
  );
  const testing = useTextGeneration((state) => state.testing[id] === true);
  const host = useHostName();
  const { t } = useTranslation(["settings", "common"]);
  if (!generator) return null;
  return (
    <div className="q-field" style={{ alignItems: "flex-start" }}>
      <div className="lab">
        <div className="t">{generator.name}</div>
        <div className="s">
          {kindLabel(generator.kind)} · {generator.model}
        </div>
        <div className="s q-mono" style={{ overflowWrap: "anywhere" }}>
          {generator.base_url}
        </div>
      </div>
      <div className="ctl" style={{ display: "block" }}>
        <div className="flex gap-2 items-center flex-wrap">
          <span className="q-secret">
            <KeyRoundIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {generator.has_api_key
              ? t("textGeneration.keyStored", { host })
              : t("textGeneration.noKey")}
          </span>
          <button
            type="button"
            className="q-btn secondary"
            disabled={testing}
            onClick={() => void testTextGenerator(generator.id)}
          >
            {testing ? t("textGeneration.testing") : t("textGeneration.test")}
          </button>
          <button
            type="button"
            className="q-btn ghost"
            onClick={() => void removeTextGenerator(generator.id)}
          >
            {t("common:remove")}
          </button>
        </div>
        <div className="mt-1.5">
          <TestResult generatorId={generator.id} />
        </div>
      </div>
    </div>
  );
}

/** The add form, exported so its reset behaviour can be tested. */
export function AddGenerator() {
  const { t } = useTranslation(["settings", "common"]);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<TextGeneratorKind>("open_ai_compatible");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [errors, setErrors] = useState<ReturnType<typeof validateGeneratorDraft>>(null);
  const [saving, setSaving] = useState(false);
  const saveError = useTextGeneration((state) => state.saveError);
  const host = useHostName();

  // Closing the form always forgets the key and everything typed beside it,
  // so a cancelled attempt leaves no secret on screen or in the next draft.
  const reset = () => {
    setName("");
    setKind("open_ai_compatible");
    setBaseUrl("");
    setModel("");
    setApiKey("");
    setErrors(null);
    setSaving(false);
    clearGeneratorSaveError();
  };

  if (!open) {
    return (
      <button type="button" className="q-btn secondary" onClick={() => setOpen(true)}>
        <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        {t("textGeneration.addGenerator")}
      </button>
    );
  }

  const draft: TextGeneratorDraft = {
    name,
    kind,
    base_url: baseUrl,
    model,
    api_key: apiKey.trim() || null,
  };

  return (
    <div className="q-group" style={{ marginTop: 12 }}>
      <h2>{t("textGeneration.newGenerator")}</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("textGeneration.name")}</div>
          {errors?.name ? (
            <div className="s q-danger" role="alert">
              {errors.name}
            </div>
          ) : (
            <div className="s">{t("textGeneration.nameHint")}</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input text-xs"
            value={name}
            aria-label={t("textGeneration.nameLabel")}
            onChange={(event) => setName(event.target.value)}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("textGeneration.kind")}</div>
          <div className="s">{kindHint(kind)}</div>
        </div>
        <div className="ctl">
          <ChipSelect<TextGeneratorKind>
            label={t("textGeneration.kindLabel")}
            value={kind}
            options={KINDS.map((entry) => ({
              value: entry,
              label: kindLabel(entry),
              hint: kindHint(entry),
            }))}
            onChange={setKind}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("forges.add.baseUrl")}</div>
          {errors?.base_url ? (
            <div className="s q-danger" role="alert">
              {errors.base_url}
            </div>
          ) : (
            <div className="s">{t("textGeneration.baseUrlHint")}</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={baseUrl}
            aria-label={t("textGeneration.baseUrlLabel")}
            placeholder={
              kind === "anthropic" ? "https://api.anthropic.com" : "https://api.openai.com/v1"
            }
            onChange={(event) => setBaseUrl(event.target.value)}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("sourceControl.model")}</div>
          {errors?.model ? (
            <div className="s q-danger" role="alert">
              {errors.model}
            </div>
          ) : (
            <div className="s">{t("textGeneration.modelHint")}</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={model}
            aria-label={t("textGeneration.modelLabel")}
            placeholder="gpt-5.4-mini"
            onChange={(event) => setModel(event.target.value)}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("textGeneration.apiKey")}</div>
          <div className="s">{t("textGeneration.apiKeyHint", { host })}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={apiKey}
            aria-label={t("textGeneration.apiKeyLabel")}
            placeholder={t("textGeneration.writeOnly")}
            onChange={(event) => setApiKey(event.target.value)}
          />
        </div>
      </div>
      <div className="flex gap-2 justify-end">
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => {
            reset();
            setOpen(false);
          }}
        >
          {t("common:cancel")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={saving}
          onClick={() => {
            const invalid = validateGeneratorDraft(draft);
            setErrors(invalid);
            if (invalid) return;
            setSaving(true);
            void addTextGenerator(draft).then((generator) => {
              setSaving(false);
              if (!generator) return;
              reset();
              setOpen(false);
            });
          }}
        >
          {saving ? t("common:saving") : t("textGeneration.saveGenerator")}
        </button>
      </div>
      {saveError ? (
        <p className="q-danger text-xs mt-2" role="alert">
          {saveError.message}
          {saveError.hint ? ` ${saveError.hint}` : ""}
        </p>
      ) : null}
    </div>
  );
}

/**
 * Settings → Providers → Text generators.
 *
 * The Providers page owns its one page title, so this renders as a group.
 */
export function TextGeneratorsSettings() {
  const { t } = useTranslation(["settings", "common"]);
  const connection = useApp((state) => state.connection.status);
  const generators = useTextGeneration((state) => state.generators);
  const loaded = useTextGeneration((state) => state.loaded);
  const loading = useTextGeneration((state) => state.loading);
  const error = useTextGeneration((state) => state.error);
  const host = useHostName();

  useEffect(() => {
    if (connection === "online") void loadTextGenerators();
  }, [connection]);

  return (
    <div className="q-group">
      <h2>{t("textGeneration.title")}</h2>
      <p className="desc">{t("textGeneration.lead", { host })}</p>

      {loading && generators.length === 0 ? (
        <div className="q-skel h-15" />
      ) : error && generators.length === 0 ? (
        <div className="q-empty">
          <p className="m-0">{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? t("textGeneration.retryOnline")}
          </p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void loadTextGenerators()}
          >
            {t("common:tryAgain")}
          </button>
        </div>
      ) : generators.length === 0 ? (
        <>
          <p className="desc">{t("textGeneration.empty")}</p>
          {loaded ? <AddGenerator /> : null}
        </>
      ) : (
        <>
          {generators.map((generator) => (
            <GeneratorRow key={generator.id} id={generator.id} />
          ))}
          <AddGenerator />
        </>
      )}
    </div>
  );
}

/**
 * One choice out of three: a radio group, not a tab strip.
 *
 * Arrow keys move the choice and the focus together, Home and End jump to the
 * ends, and on a phone each target is thumb-sized.
 */
export function ModePicker({
  value,
  onChange,
}: {
  value: TitleModelMode;
  onChange: (next: TitleModelMode) => void;
}) {
  const { t } = useTranslation(["settings", "common"]);
  return (
    <SegmentedControl
      className="phone:w-full"
      aria-label={t("textGeneration.titleModelLabel")}
      value={value}
      onValueChange={(next) => {
        if (next) onChange(next as TitleModelMode);
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

function TitleDraftForm({ saved }: { saved: TextGenerationSettings }) {
  const { t } = useTranslation(["settings", "common"]);
  const [draft, setDraft] = useState<TitleModelSetting>(saved.title);
  const generators = useTextGeneration((state) => state.generators);
  const saving = useTextGeneration((state) => state.savingSettings);
  const saveError = useTextGeneration((state) => state.saveError);
  const testing = useTextGeneration((state) =>
    draft.generator_id ? state.testing[draft.generator_id] === true : false,
  );
  useHarnessVersions();
  const harnessRows = useHarnesses((state) => state.rows);
  const agents = useMemo(() => installedTitleAgents(harnessRows), [harnessRows]);
  const generator = generators.find((candidate) => candidate.id === draft.generator_id);

  useEffect(() => {
    setDraft(saved.title);
  }, [saved]);

  const patchSetting = (next: Partial<TitleModelSetting>) => setDraft({ ...draft, ...next });

  const chooseMode = (mode: TitleModelMode) => {
    if (mode === "coding_agent") {
      // No pick means "follow the thread's agent", which is the default.
      patchSetting({ mode, harness: draft.harness ?? null });
      return;
    }
    if (mode === "text_generator") {
      const first = generators[0];
      patchSetting({
        mode,
        generator_id: draft.generator_id ?? first?.id ?? null,
        model: draft.model ?? null,
      });
      return;
    }
    patchSetting({ mode });
  };

  const model = draft.model ?? generator?.model ?? "";
  const canSave =
    draft.mode !== "text_generator" || (draft.generator_id !== null && model.trim().length > 0);
  const changed =
    draft.mode !== saved.title.mode ||
    draft.harness !== saved.title.harness ||
    draft.generator_id !== saved.title.generator_id ||
    draft.model !== saved.title.model;

  return (
    <div className="q-group">
      <h2>{t("textGeneration.titleModel")}</h2>
      <p className="desc">{t("textGeneration.titleModelHint")}</p>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("textGeneration.whoWrites")}</div>
          <div className="s">{titleModelSummary({ title: draft })}</div>
        </div>
        <div className="ctl">
          <ModePicker value={draft.mode} onChange={chooseMode} />
        </div>
      </div>

      {draft.mode === "coding_agent" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("sourceControl.modes.coding_agent")}</div>
            <div className="s">{t("textGeneration.codingAgentHint")}</div>
            {agents.length === 0 ? (
              <div className="s q-faint">{t("sourceControl.noClaudeCode")}</div>
            ) : null}
          </div>
          <div className="ctl">
            <ChipSelect<AgentChoice>
              label={t("textGeneration.codingAgentLabel")}
              value={draft.harness ?? FOLLOW_THREAD}
              options={[
                {
                  value: FOLLOW_THREAD,
                  label: t("sourceControl.followThread"),
                  hint: t("textGeneration.followThreadHint"),
                },
                ...agents.map((agent) => ({
                  value: agent.harness,
                  label: HARNESS_LABELS[agent.harness],
                  hint: agent.installed,
                })),
              ]}
              onChange={(choice) =>
                patchSetting({ harness: choice === FOLLOW_THREAD ? null : choice })
              }
            />
          </div>
        </div>
      ) : null}

      {draft.mode === "text_generator" ? (
        <>
          <div className="q-field">
            <div className="lab">
              <div className="t">{t("textGeneration.generator")}</div>
              <div className="s">
                {generators.length > 0
                  ? t("textGeneration.manageUnderProviders")
                  : t("textGeneration.addUnderProviders")}
              </div>
            </div>
            <div className="ctl">
              <ChipSelect<TextGeneratorId>
                label={t("textGeneration.generatorLabel")}
                value={draft.generator_id ?? ""}
                disabled={generators.length === 0}
                options={generators.map((entry) => ({
                  value: entry.id,
                  label: entry.name,
                  hint: entry.model,
                }))}
                onChange={(generatorId) => {
                  patchSetting({ generator_id: generatorId, model: null });
                }}
              />
            </div>
          </div>
          <div className="q-field">
            <div className="lab">
              <div className="t">{t("sourceControl.model")}</div>
              <div className="s">{t("textGeneration.titleModelDefault")}</div>
            </div>
            <div className="ctl">
              <input
                className="q-input q-mono"
                value={model}
                aria-label={t("textGeneration.titleModelLabel")}
                onChange={(event) => patchSetting({ model: event.target.value })}
              />
              <button
                type="button"
                className="q-btn secondary"
                disabled={!draft.generator_id || testing}
                onClick={() => {
                  if (draft.generator_id) {
                    void testTextGenerator(draft.generator_id, model);
                  }
                }}
              >
                {testing ? t("textGeneration.testing") : t("textGeneration.test")}
              </button>
            </div>
          </div>
          {draft.generator_id ? (
            <div className="q-field" style={{ alignItems: "flex-start" }}>
              <div className="lab" />
              <div className="ctl" style={{ display: "block" }}>
                <TestResult generatorId={draft.generator_id} />
              </div>
            </div>
          ) : null}
        </>
      ) : null}

      <div className="flex gap-2 justify-end">
        <button
          type="button"
          className="q-btn ghost"
          disabled={saving || !changed}
          onClick={() => setDraft(saved.title)}
        >
          {t("common:reset")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={saving || !canSave || !changed}
          onClick={() => {
            void saveTitleModel({
              title: {
                mode: draft.mode,
                harness: draft.mode === "coding_agent" ? (draft.harness ?? null) : null,
                generator_id: draft.mode === "text_generator" ? draft.generator_id : null,
                model:
                  draft.mode === "text_generator" && draft.model?.trim()
                    ? draft.model.trim()
                    : null,
              },
              source_control: saved.source_control,
            });
          }}
        >
          {saving ? t("common:saving") : t("common:save")}
        </button>
      </div>
      {saveError ? (
        <p className="q-danger text-xs mt-2" role="alert">
          {saveError.message}
          {saveError.hint ? ` ${saveError.hint}` : ""}
        </p>
      ) : null}
    </div>
  );
}

/** Settings → Threads. */
export function ThreadSettings({ children }: { children?: ReactNode }) {
  const { t } = useTranslation(["settings", "common"]);
  const connection = useApp((state) => state.connection.status);
  const settings = useTextGeneration((state) => state.settings);
  const loading = useTextGeneration((state) => state.settingsLoading);
  const error = useTextGeneration((state) => state.settingsError);

  useEffect(() => {
    if (connection === "online") {
      void loadTextGenerators();
      void loadTitleModel();
    }
  }, [connection]);

  return (
    <div className="q-form">
      <h1>{t("threads.title")}</h1>
      <p className="lead">{t("threads.lead")}</p>
      {children}
      <h2>{t("threads.titles")}</h2>
      {settings ? (
        <TitleDraftForm saved={settings} />
      ) : loading ? (
        <div className="q-skel h-30" />
      ) : error ? (
        <div className="q-empty">
          <p className="m-0">{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? t("textGeneration.retryOnline")}
          </p>
          <button type="button" className="q-btn secondary" onClick={() => void loadTitleModel()}>
            {t("common:tryAgain")}
          </button>
        </div>
      ) : (
        <p className="q-faint" style={{ fontSize: 12 }}>
          {t("threads.connectForTitles")}
        </p>
      )}
    </div>
  );
}
