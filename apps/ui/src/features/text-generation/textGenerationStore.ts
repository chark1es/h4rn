import { useEffect, useSyncExternalStore } from "react";
import type {
  ProtoError,
  TextGenerationSettings,
  TextGenerator,
  TextGeneratorDraft,
  TextGeneratorId,
  TextGeneratorPatch,
  TextGeneratorTestReport,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { HARNESS_LABELS } from "@/lib/entities";
import { i18n } from "@/lib/i18n";
import { getClient, useApp } from "@/lib/store/appStore";

export type GeneratorFieldErrors = Partial<Record<"name" | "base_url" | "model", string>>;

export type TextGenerationState = {
  generators: TextGenerator[];
  /** The list was read, even when the daemon reported none. */
  loaded: boolean;
  loading: boolean;
  error: ProtoError | null;
  settings: TextGenerationSettings | null;
  settingsLoaded: boolean;
  settingsLoading: boolean;
  settingsError: ProtoError | null;
  savingSettings: boolean;
  saveError: ProtoError | null;
  reports: Record<TextGeneratorId, TextGeneratorTestReport>;
  testing: Record<TextGeneratorId, boolean>;
  testErrors: Record<TextGeneratorId, ProtoError | null>;
};

const initialState: TextGenerationState = {
  generators: [],
  loaded: false,
  loading: false,
  error: null,
  settings: null,
  settingsLoaded: false,
  settingsLoading: false,
  settingsError: null,
  savingSettings: false,
  saveError: null,
  reports: {},
  testing: {},
  testErrors: {},
};

type Listener = () => void;

let state = initialState;
const listeners = new Set<Listener>();

function setState(next: TextGenerationState): void {
  state = next;
  for (const listener of listeners) listener();
}

function patch(next: Partial<TextGenerationState>): void {
  setState({ ...state, ...next });
}

export function getTextGenerationState(): TextGenerationState {
  return state;
}

export function useTextGeneration<T>(selector: (value: TextGenerationState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** Exposed for tests; production never calls it. */
export function resetTextGenerationForTests(next: Partial<TextGenerationState> = {}): void {
  setState({ ...initialState, ...next });
}

export async function loadTextGenerators(): Promise<void> {
  patch({ loading: true, error: null });
  try {
    const response = await getClient().request({ type: "list_text_generators" });
    if (response.type !== "text_generators") return;
    patch({ generators: response.generators, loaded: true, loading: false, error: null });
  } catch (error) {
    patch({ loaded: true, loading: false, error: toProtoError(error) });
  }
}

export async function loadTitleModel(): Promise<void> {
  patch({ settingsLoading: true, settingsError: null });
  try {
    const response = await getClient().request({ type: "get_text_generation_settings" });
    if (response.type !== "text_generation_settings") return;
    patch({
      settings: response.settings,
      settingsLoaded: true,
      settingsLoading: false,
      settingsError: null,
    });
  } catch (error) {
    patch({ settingsLoaded: true, settingsLoading: false, settingsError: toProtoError(error) });
  }
}

/** Loads both halves once per mount; the Providers page only needs the list. */
export function useTextGenerationData(options: { settings?: boolean } = {}): void {
  const connection = useApp((app) => app.connection.status);
  useEffect(() => {
    if (connection !== "online") return;
    if (!state.loaded && !state.loading) void loadTextGenerators();
    if (options.settings && !state.settingsLoaded && !state.settingsLoading) void loadTitleModel();
  }, [connection, options.settings]);
}

/** Drops a save error, for example when the add form is closed or reset. */
export function clearGeneratorSaveError(): void {
  patch({ saveError: null });
}

export async function addTextGenerator(draft: TextGeneratorDraft): Promise<TextGenerator | null> {
  const invalid = validateGeneratorDraft(draft);
  if (invalid) return null;
  patch({ saveError: null });
  try {
    const response = await getClient().request({ type: "add_text_generator", generator: draft });
    if (response.type !== "text_generator") return null;
    patch({ generators: [...state.generators, response.generator] });
    return response.generator;
  } catch (error) {
    patch({ saveError: toProtoError(error) });
    return null;
  }
}

export async function updateTextGenerator(
  generatorId: TextGeneratorId,
  patchFields: TextGeneratorPatch,
): Promise<TextGenerator | null> {
  patch({ saveError: null });
  try {
    const response = await getClient().request({
      type: "update_text_generator",
      generator_id: generatorId,
      patch: patchFields,
    });
    if (response.type !== "text_generator") return null;
    patch({
      generators: state.generators.map((generator) =>
        generator.id === response.generator.id ? response.generator : generator,
      ),
    });
    // A key can be set or cleared here; the title picker shows only the name,
    // but the row's "key stored" label must follow the answer.
    return response.generator;
  } catch (error) {
    patch({ saveError: toProtoError(error) });
    return null;
  }
}

export async function removeTextGenerator(generatorId: TextGeneratorId): Promise<boolean> {
  patch({ saveError: null });
  try {
    await getClient().request({ type: "remove_text_generator", generator_id: generatorId });
    patch({ generators: state.generators.filter((generator) => generator.id !== generatorId) });
    // Removing the selected generator turns titles Off on the daemon; read
    // that back instead of guessing.
    void loadTitleModel();
    return true;
  } catch (error) {
    patch({ saveError: toProtoError(error) });
    return false;
  }
}

export async function testTextGenerator(
  generatorId: TextGeneratorId,
  model?: string | null,
): Promise<void> {
  patch({
    testing: { ...state.testing, [generatorId]: true },
    testErrors: { ...state.testErrors, [generatorId]: null },
  });
  try {
    // A provider may spend its whole HTTP timeout answering; the default
    // request budget is shorter than that.
    const response = await getClient().request(
      {
        type: "test_text_generator",
        generator_id: generatorId,
        model: model?.trim() || null,
      },
      { timeoutMs: 40_000 },
    );
    if (response.type !== "text_generator_tested") return;
    patch({
      reports: { ...state.reports, [generatorId]: response.report },
    });
  } catch (error) {
    patch({
      testErrors: { ...state.testErrors, [generatorId]: toProtoError(error) },
    });
  } finally {
    // Every exit clears the spinner, including an early return for a response
    // this client does not understand.
    patch({ testing: { ...state.testing, [generatorId]: false } });
  }
}

export async function saveTitleModel(settings: TextGenerationSettings): Promise<boolean> {
  patch({ savingSettings: true, saveError: null });
  try {
    const response = await getClient().request({
      type: "set_text_generation_settings",
      settings,
    });
    if (response.type !== "text_generation_settings") return false;
    patch({ settings: response.settings, savingSettings: false, saveError: null });
    return true;
  } catch (error) {
    patch({ savingSettings: false, saveError: toProtoError(error) });
    return false;
  }
}

const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "::1", "[::1]"]);

/** Base-URL rule shown in the form: https anywhere, http only on loopback. */
export function baseUrlError(value: string): string | null {
  const trimmed = value.trim();
  const t = i18n.getFixedT(null, "settings", "textGeneration.validate");
  if (!trimmed) return t("baseUrlRequired");
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return t("notUrl");
  }
  if (parsed.username || parsed.password) {
    return t("keyInUrl");
  }
  if (parsed.protocol === "https:") return null;
  if (parsed.protocol !== "http:") return t("useHttps");
  if (!LOOPBACK_HOSTS.has(parsed.hostname.toLowerCase())) {
    return t("httpLoopback");
  }
  return null;
}

/** Checks the add/edit form before it reaches the daemon. */
export function validateGeneratorDraft(draft: TextGeneratorDraft): GeneratorFieldErrors | null {
  const errors: GeneratorFieldErrors = {};
  if (!draft.name.trim()) errors.name = i18n.t("settings:textGeneration.validate.nameRequired");
  if (!draft.model.trim()) errors.model = i18n.t("settings:textGeneration.validate.modelRequired");
  const base = baseUrlError(draft.base_url);
  if (base) errors.base_url = base;
  return Object.keys(errors).length > 0 ? errors : null;
}

/**
 * Installed coding agents h4rn runs for titles.
 *
 * Only Claude Code is here. Codex is not offered as a title agent: its
 * `--disable` flags do not remove the model-visible shell, so a Codex thread
 * keeps its first-line title (or uses a text generator) instead.
 */
export function installedTitleAgents<T extends { harness: string; installed: string | null }>(
  rows: T[],
): Array<{ harness: "claude_code"; installed: string }> {
  return rows
    .filter(
      (row): row is T & { harness: "claude_code"; installed: string } =>
        row.installed !== null && row.harness === "claude_code",
    )
    .map((row) => ({ harness: row.harness, installed: row.installed }));
}

/** The one-line description under the picker. */
export function titleModelSummary(
  settings: { title: TextGenerationSettings["title"] } | null,
): string {
  const t = i18n.getFixedT(null, "settings", "textGeneration.summary");
  if (!settings) return t("reading");
  const title = settings.title;
  if (title.mode === "off") return t("off");
  if (title.mode === "coding_agent") {
    if (title.harness === "claude_code") return t("claudeCode");
    if (title.harness === "codex") return t("codex");
    return title.harness
      ? t("otherHarness", { harness: HARNESS_LABELS[title.harness] })
      : t("followThread");
  }
  return title.model ? t("generatorModel", { model: title.model }) : t("generator");
}
