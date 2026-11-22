import { CircleCheckIcon, KeyRoundIcon, PlusIcon, TriangleAlertIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
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

const MODES: Array<{ value: TitleModelMode; label: string }> = [
  { value: "off", label: "Off" },
  { value: "coding_agent", label: "Coding agent" },
  { value: "text_generator", label: "Text generator" },
];

/**
 * The coding-agent picker has one value the protocol does not: `thread`
 * follows each thread's own agent (a `null` harness).
 */
type AgentChoice = HarnessKind | "thread";
const FOLLOW_THREAD = "thread" as const;

const KINDS: Array<{ value: TextGeneratorKind; label: string; hint: string }> = [
  {
    value: "open_ai_compatible",
    label: "OpenAI-compatible",
    hint: "OpenAI, OpenRouter, Ollama, LM Studio",
  },
  { value: "anthropic", label: "Anthropic", hint: "api.anthropic.com" },
];

function useHostName(): string {
  return useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? "this host") : "this host",
  );
}

function kindLabel(kind: TextGeneratorKind): string {
  return KINDS.find((entry) => entry.value === kind)?.label ?? kind;
}

function TestResult({ generatorId }: { generatorId: TextGeneratorId }) {
  const report = useTextGeneration((state) => state.reports[generatorId]);
  const error = useTextGeneration((state) => state.testErrors[generatorId]);
  const testing = useTextGeneration((state) => state.testing[generatorId] === true);
  if (testing) {
    return (
      <span className="q-faint" style={{ fontSize: 12 }}>
        Sending a tiny prompt…
      </span>
    );
  }
  if (error) {
    return (
      <span className="q-danger" role="alert" style={{ fontSize: 12 }}>
        {error.message}
        {error.hint ? ` ${error.hint}` : ""}
      </span>
    );
  }
  if (!report) return null;
  if (report.ok) {
    return (
      <span className="q-ok" style={{ fontSize: 12 }}>
        <CircleCheckIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Replied in{" "}
        {Number(report.latency_ms)} ms
        {report.reply ? `: “${report.reply}”` : "."}
      </span>
    );
  }
  return (
    <span className="q-danger" role="alert" style={{ fontSize: 12 }}>
      <TriangleAlertIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />{" "}
      {report.error ?? "The generator did not answer."}
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
        <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
          <span className="q-secret">
            <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            {generator.has_api_key ? `Key stored on ${host}` : "No key"}
          </span>
          <button
            type="button"
            className="q-btn secondary"
            disabled={testing}
            onClick={() => void testTextGenerator(generator.id)}
          >
            {testing ? "Testing…" : "Test"}
          </button>
          <button
            type="button"
            className="q-btn ghost"
            onClick={() => void removeTextGenerator(generator.id)}
          >
            Remove
          </button>
        </div>
        <div style={{ marginTop: 6 }}>
          <TestResult generatorId={generator.id} />
        </div>
      </div>
    </div>
  );
}

/** The add form, exported so its reset behaviour can be tested. */
export function AddGenerator() {
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
        Add generator
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
      <h2>New generator</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">Name</div>
          {errors?.name ? (
            <div className="s q-danger" role="alert">
              {errors.name}
            </div>
          ) : (
            <div className="s">Shown in the title-model picker</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input"
            value={name}
            aria-label="Generator name"
            onChange={(event) => setName(event.target.value)}
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Kind</div>
          <div className="s">{KINDS.find((entry) => entry.value === kind)?.hint}</div>
        </div>
        <div className="ctl">
          <ChipSelect<TextGeneratorKind>
            label="Generator kind"
            value={kind}
            width={190}
            options={KINDS.map((entry) => ({
              value: entry.value,
              label: entry.label,
              hint: entry.hint,
            }))}
            onChange={setKind}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Base URL</div>
          {errors?.base_url ? (
            <div className="s q-danger" role="alert">
              {errors.base_url}
            </div>
          ) : (
            <div className="s">https, or http on this machine only</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={baseUrl}
            aria-label="Generator base URL"
            placeholder={
              kind === "anthropic" ? "https://api.anthropic.com" : "https://api.openai.com/v1"
            }
            onChange={(event) => setBaseUrl(event.target.value)}
            style={{ maxWidth: 340, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Model</div>
          {errors?.model ? (
            <div className="s q-danger" role="alert">
              {errors.model}
            </div>
          ) : (
            <div className="s">The model that writes titles</div>
          )}
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={model}
            aria-label="Generator model"
            placeholder="gpt-5.4-mini"
            onChange={(event) => setModel(event.target.value)}
            style={{ maxWidth: 260, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">API key</div>
          <div className="s">Stored on {host}; optional for a local server</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={apiKey}
            aria-label="Generator API key"
            placeholder="Write-only"
            onChange={(event) => setApiKey(event.target.value)}
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => {
            reset();
            setOpen(false);
          }}
        >
          Cancel
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
          {saving ? "Saving…" : "Save generator"}
        </button>
      </div>
      {saveError ? (
        <p className="q-danger" role="alert" style={{ fontSize: 12, marginTop: 8 }}>
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
      <h2>Text generators</h2>
      <p className="desc">
        A model API that only writes text. h4rn can use one for thread titles instead of starting a
        coding agent. Keys stay on {host}; this app only sees whether one exists.
      </p>

      {loading && generators.length === 0 ? (
        <div className="q-skel" style={{ height: 60 }} />
      ) : error && generators.length === 0 ? (
        <div className="q-empty">
          <p style={{ margin: 0 }}>{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? "Retry when the daemon is online."}
          </p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void loadTextGenerators()}
          >
            Try again
          </button>
        </div>
      ) : generators.length === 0 ? (
        <>
          <p className="desc">
            None yet. Titles can still be written by an installed coding agent; add a generator for
            a faster, cheaper model.
          </p>
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
  const buttons = useRef<Array<HTMLButtonElement | null>>([]);
  const select = (index: number) => {
    const next = (index + MODES.length) % MODES.length;
    const mode = MODES[next];
    if (!mode) return;
    onChange(mode.value);
    buttons.current[next]?.focus();
  };
  return (
    <div className="q-seg inline q-seg-choice" role="radiogroup" aria-label="Title model">
      {MODES.map((mode, index) => (
        // biome-ignore lint/a11y/useSemanticElements: a segmented radio group, not a form radio
        <button
          key={mode.value}
          ref={(element) => {
            buttons.current[index] = element;
          }}
          type="button"
          role="radio"
          aria-checked={value === mode.value}
          tabIndex={value === mode.value ? 0 : -1}
          onClick={() => onChange(mode.value)}
          onKeyDown={(event) => {
            if (event.key === "ArrowRight" || event.key === "ArrowDown") {
              event.preventDefault();
              select(index + 1);
            } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
              event.preventDefault();
              select(index - 1);
            } else if (event.key === "Home") {
              event.preventDefault();
              select(0);
            } else if (event.key === "End") {
              event.preventDefault();
              select(MODES.length - 1);
            }
          }}
        >
          {mode.label}
        </button>
      ))}
    </div>
  );
}

function TitleDraftForm({ saved }: { saved: TextGenerationSettings }) {
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
      <h2>Title model</h2>
      <p className="desc">
        A new thread shows the first line of your first prompt immediately. The model then writes a
        shorter title and replaces it. A title you rename yourself is never replaced.
      </p>
      <div className="q-field">
        <div className="lab">
          <div className="t">Who writes titles</div>
          <div className="s">{titleModelSummary({ title: draft })}</div>
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
              Each thread's own agent runs once in an empty folder with no tools, then exits. Pick
              one agent only to hand every thread's title to it.
            </div>
          </div>
          <div className="ctl">
            <ChipSelect<AgentChoice>
              label="Coding agent for titles"
              value={draft.harness ?? FOLLOW_THREAD}
              width={220}
              options={[
                {
                  value: FOLLOW_THREAD,
                  label: "Follow the thread's agent",
                  hint: "Claude Code and Codex threads use their own",
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
          {agents.length === 0 ? (
            <div className="s q-faint">No Claude Code or Codex is installed on this host yet.</div>
          ) : null}
        </div>
      ) : null}

      {draft.mode === "text_generator" ? (
        <>
          <div className="q-field">
            <div className="lab">
              <div className="t">Generator</div>
              <div className="s">
                {generators.length > 0
                  ? "Add or remove generators under Providers."
                  : "Add a generator under Providers first."}
              </div>
            </div>
            <div className="ctl">
              <ChipSelect<TextGeneratorId>
                label="Text generator for titles"
                value={draft.generator_id ?? ""}
                width={200}
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
              <div className="t">Model</div>
              <div className="s">Defaults to the generator's model</div>
            </div>
            <div className="ctl">
              <input
                className="q-input q-mono"
                value={model}
                aria-label="Title model"
                onChange={(event) => patchSetting({ model: event.target.value })}
                style={{ maxWidth: 260, fontSize: 12 }}
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
                {testing ? "Testing…" : "Test"}
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

      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <button
          type="button"
          className="q-btn ghost"
          disabled={saving || !changed}
          onClick={() => setDraft(saved.title)}
        >
          Reset
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
            });
          }}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </div>
      {saveError ? (
        <p className="q-danger" role="alert" style={{ fontSize: 12, marginTop: 8 }}>
          {saveError.message}
          {saveError.hint ? ` ${saveError.hint}` : ""}
        </p>
      ) : null}
    </div>
  );
}

/** Settings → Threads. */
export function ThreadSettings() {
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
      <h1>Threads</h1>
      <p className="lead">
        How h4rn names new threads, and where that title comes from. The first prompt is always
        accepted before any title is written.
      </p>
      {settings ? (
        <TitleDraftForm saved={settings} />
      ) : loading ? (
        <div className="q-skel" style={{ height: 120 }} />
      ) : error ? (
        <div className="q-empty">
          <p style={{ margin: 0 }}>{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? "Retry when the daemon is online."}
          </p>
          <button type="button" className="q-btn secondary" onClick={() => void loadTitleModel()}>
            Try again
          </button>
        </div>
      ) : (
        <p className="q-faint" style={{ fontSize: 12 }}>
          Connect to the daemon to read the title model.
        </p>
      )}
    </div>
  );
}
