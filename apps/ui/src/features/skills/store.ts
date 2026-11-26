/**
 * The Skills surface state.
 *
 * The daemon owns the registry, the managed copies and sync. This module sends
 * the `skills_*` requests, keeps the last inventory and applies the transient
 * `SkillsChanged` / `SyncStatus` events. Per-harness enable switches are
 * optimistic and roll back with an explanation when the daemon refuses.
 */
import { useSyncExternalStore } from "react";
import type {
  ClientRequest,
  Event,
  HarnessKind,
  HostId,
  ProtoError,
  ResponseBody,
  SkillConflict,
  SkillConflictResolution,
  SkillDiff,
  SkillImportCandidate,
  SkillImportPreview,
  SkillInventory,
  SkillSource,
  SkillSummary,
  SkillSyncStatus,
} from "@/gen";
import { clientProto, toProtoError } from "@/lib/client/errors";
import { i18n } from "@/lib/i18n";
import { joinSentences } from "@/lib/i18n/sentences";
import { getClient, getState, pushToast } from "@/lib/store/appStore";

export type SkillsState = {
  inventory: SkillInventory | null;
  loading: boolean;
  refreshing: boolean;
  /** Why the inventory could not load, kept as sent so a client error relabels. */
  error: ProtoError | null;
  /** Which request failed, worded by the page as `library:errors.actions.<kind>`. */
  errorAction: SkillsAction | null;
  conflicts: SkillConflict[];
  preview: SkillImportPreview | null;
  previewing: boolean;
  diff: SkillDiff | null;
  sync: SkillSyncStatus | null;
  syncing: boolean;
  checkedAt: number | null;
  pending: Record<string, boolean>;
};

/** A request whose failure the Skills page shows in its error banner. */
export type SkillsAction = "listSkills";

const initialState: SkillsState = {
  inventory: null,
  loading: false,
  refreshing: false,
  error: null,
  errorAction: null,
  conflicts: [],
  preview: null,
  previewing: false,
  diff: null,
  sync: null,
  syncing: false,
  checkedAt: null,
  pending: {},
};

let state: SkillsState = initialState;
const listeners = new Set<() => void>();

function set(patch: Partial<SkillsState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export function getSkillsState(): SkillsState {
  return state;
}

type SkillsRequest = Extract<ClientRequest, { type: `skills_${string}` }>;

/** Test seam: answer `skills_*` requests without a socket. */
let sendImpl: ((request: SkillsRequest) => Promise<ResponseBody>) | null = null;

export function setSkillsRequesterForTests(
  next: ((request: SkillsRequest) => Promise<ResponseBody>) | null,
): void {
  sendImpl = next;
}

async function send(request: SkillsRequest): Promise<ResponseBody> {
  if (sendImpl) return sendImpl(request);
  return (await getClient().request(request as never)) as unknown as ResponseBody;
}

export function resetSkillsStore(): void {
  state = initialState;
  for (const listener of listeners) listener();
}

export function useSkills<T>(selector: (value: SkillsState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** The host id the daemon reports for its own machine. */
export function skillsHost(): HostId | null {
  return getState().localHostId;
}

function fail(action: SkillsAction, error: unknown): ProtoError {
  const proto = toProtoError(error);
  set({ loading: false, refreshing: false, previewing: false, error: proto, errorAction: action });
  return proto;
}

function toggleFailedTitle(enabled: boolean, skill: string | undefined, harness: string): string {
  if (skill) {
    return i18n.t(enabled ? "shell:skills.enableFailed" : "shell:skills.disableFailed", {
      skill,
      harness,
    });
  }
  return i18n.t(
    enabled ? "shell:skills.enableFailedUnnamed" : "shell:skills.disableFailedUnnamed",
    {
      harness,
    },
  );
}

/** Loads the inventory and unresolved conflicts. */
export async function loadSkills(options: { force?: boolean } = {}): Promise<void> {
  const host = skillsHost();
  if (!host) {
    set({
      loading: false,
      error: clientProto("unsupported", "noHost"),
      errorAction: null,
    });
    return;
  }
  if (state.loading || (state.refreshing && !options.force)) return;
  set({
    loading: state.inventory === null,
    refreshing: state.inventory !== null,
    error: null,
    errorAction: null,
  });
  try {
    const [inventory, conflicts] = await Promise.all([
      send({ type: "skills_list", host }),
      send({ type: "skills_conflicts", host }).catch(() => ({
        type: "skill_conflicts" as const,
        conflicts: [],
      })),
    ]);
    if (inventory.type !== "skills") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${inventory.type}\`, not the skill inventory.`);
    }
    set({
      inventory: inventory.inventory,
      conflicts: conflicts.type === "skill_conflicts" ? conflicts.conflicts : [],
      loading: false,
      refreshing: false,
      error: null,
      errorAction: null,
      checkedAt: Date.now(),
    });
  } catch (error) {
    fail("listSkills", error);
  }
}

function replaceSkill(skill: SkillSummary): SkillInventory | null {
  if (!state.inventory) return null;
  const exists = state.inventory.skills.some((entry) => entry.id === skill.id);
  const skills = exists
    ? state.inventory.skills.map((entry) => (entry.id === skill.id ? skill : entry))
    : [...state.inventory.skills, skill];
  skills.sort((left, right) => left.name.localeCompare(right.name));
  return { ...state.inventory, skills };
}

/** Adds a skill from a folder, a pinned git commit or a harness directory. */
export async function addSkill(input: {
  source: SkillSource;
  harnesses: HarnessKind[];
  name?: string | null;
}): Promise<SkillSummary | null> {
  const host = skillsHost();
  if (!host) return null;
  try {
    const response = await send({
      type: "skills_add",
      host,
      source: input.source,
      harnesses: input.harnesses,
      name: input.name ?? null,
    });
    if (response.type !== "skill_added") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the added skill.`);
    }
    set({ inventory: replaceSkill(response.skill), checkedAt: Date.now() });
    await loadSkills({ force: true });
    return response.skill;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:skills.addFailed"),
      description: joinSentences([
        proto.message,
        proto.hint ?? i18n.t("shell:skills.checkSkillFile"),
      ]),
      variant: "error",
    });
    return null;
  }
}

/** Replaces a skill's current version from a source. */
export async function updateSkill(skillId: string, source: SkillSource): Promise<boolean> {
  const host = skillsHost();
  if (!host) return false;
  try {
    const response = await send({ type: "skills_update", host, skill_id: skillId, source });
    if (response.type !== "skill_added") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the updated skill.`);
    }
    set({ inventory: replaceSkill(response.skill), checkedAt: Date.now() });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:skills.updateFailed"),
      description: joinSentences([proto.message, proto.hint ?? i18n.t("shell:skills.unchanged")]),
      variant: "error",
    });
    return false;
  }
}

/** Removes the registry row and every h4rn-managed copy. */
export async function removeSkill(skillId: string): Promise<boolean> {
  const host = skillsHost();
  if (!host || !state.inventory) return false;
  const before = state.inventory;
  set({
    inventory: {
      ...before,
      skills: before.skills.filter((skill) => skill.id !== skillId),
    },
  });
  try {
    await send({ type: "skills_remove", host, skill_id: skillId });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ inventory: before });
    pushToast({
      title: i18n.t("shell:skills.removeFailed"),
      description: joinSentences([proto.message, i18n.t("shell:skills.stillRegistered")]),
      variant: "error",
    });
    return false;
  }
}

/**
 * Enables or disables one harness on this machine.
 *
 * The switch and the installed flag move immediately; the daemon response (or
 * a rejection) is authoritative, so a failure restores the whole row.
 */
export async function setSkillEnabled(
  skillId: string,
  harness: HarnessKind,
  enabled: boolean,
): Promise<boolean> {
  const host = skillsHost();
  if (!host || !state.inventory) return false;
  const before = state.inventory;
  const optimism = state.inventory.skills.map((skill) => {
    if (skill.id !== skillId) return skill;
    return {
      ...skill,
      installs: skill.installs.map((install) =>
        install.harness === harness
          ? {
              ...install,
              enabled,
              installed: enabled ? true : install.installed,
              managed: enabled ? true : install.managed,
            }
          : install,
      ),
    };
  });
  const key = `${skillId}:${harness}`;
  set({
    inventory: { ...state.inventory, skills: optimism },
    pending: { ...state.pending, [key]: true },
  });
  try {
    const response = await send({
      type: "skills_set_enabled",
      host,
      skill_id: skillId,
      harness,
      enabled,
    });
    if (response.type !== "skill_added") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not the updated skill.`);
    }
    set({ inventory: replaceSkill(response.skill), checkedAt: Date.now() });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    set({ inventory: before });
    const skill = before.skills.find((entry) => entry.id === skillId);
    pushToast({
      title: toggleFailedTitle(enabled, skill?.name, harnessLabel(harness)),
      description: joinSentences([proto.message, i18n.t("shell:skills.switchBack"), proto.hint]),
      variant: "error",
    });
    return false;
  } finally {
    const pending = { ...state.pending };
    delete pending[key];
    set({ pending });
  }
}

/**
 * Scans every harness directory; writes nothing. `quiet` skips the error
 * toast; `onError` receives the reason instead, so a caller can show it inline.
 */
export async function previewSkillImport(
  options: { quiet?: boolean; onError?: (error: ProtoError) => void } = {},
): Promise<SkillImportPreview | null> {
  const host = skillsHost();
  if (!host) {
    options.onError?.(clientProto("unsupported", "noHost"));
    return null;
  }
  set({ previewing: true });
  try {
    const response = await send({ type: "skills_import_preview", host });
    if (response.type !== "skills_import_preview") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not an import preview.`);
    }
    set({
      preview: response.preview,
      previewing: false,
    });
    return response.preview;
  } catch (error) {
    const proto = toProtoError(error);
    set({ previewing: false });
    options.onError?.(proto);
    if (options.quiet) return null;
    pushToast({
      title: i18n.t("shell:skills.scanFailed"),
      description: joinSentences([
        proto.message,
        proto.hint ?? i18n.t("shell:skills.checkDirectories"),
      ]),
      variant: "error",
    });
    return null;
  }
}

/**
 * Imports the selected candidates from the last preview. With `manage`, h4rn
 * also takes over each matching harness folder so sync keeps it current.
 */
export async function commitSkillImport(
  candidateIds: string[],
  options: { manage?: boolean } = {},
): Promise<boolean> {
  const host = skillsHost();
  if (!host || candidateIds.length === 0) return false;
  try {
    await send({
      type: "skills_import_commit",
      host,
      candidate_ids: candidateIds,
      manage: options.manage ?? false,
    });
    set({ preview: null });
    await loadSkills({ force: true });
    await previewSkillImport({ quiet: true });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:skills.importFailed"),
      description: joinSentences([proto.message, proto.hint ?? i18n.t("shell:skills.rescan")]),
      variant: "error",
    });
    return false;
  }
}

/** Settles one import or sync conflict. */
export async function resolveSkillConflict(
  conflictId: string,
  resolution: SkillConflictResolution,
): Promise<boolean> {
  const host = skillsHost();
  if (!host) return false;
  // The conflict leaves the list once settled; name it before that.
  const name = state.conflicts.find((conflict) => conflict.id === conflictId)?.name ?? conflictId;
  try {
    await send({ type: "skills_resolve_conflict", host, conflict_id: conflictId, resolution });
    set({ conflicts: state.conflicts.filter((conflict) => conflict.id !== conflictId) });
    await loadSkills({ force: true });
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:skills.resolveFailed", { name }),
      description: joinSentences([proto.message, i18n.t("shell:skills.stillOpen")]),
      variant: "error",
    });
    return false;
  }
}

/** Runs one push/pull cycle. The daemon returns a status even when offline. */
export async function syncSkillsNow(): Promise<SkillSyncStatus | null> {
  set({ syncing: true });
  try {
    const response = await send({ type: "skills_sync_now" });
    if (response.type !== "skill_sync_status") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not a sync status.`);
    }
    set({ sync: response.status, syncing: false, checkedAt: Date.now() });
    await loadSkills({ force: true });
    return response.status;
  } catch (error) {
    const proto = toProtoError(error);
    set({ syncing: false });
    pushToast({
      title: i18n.t("shell:skills.syncFailed"),
      description: joinSentences([
        proto.message,
        i18n.t("shell:skills.localStillWork"),
        proto.hint,
      ]),
      variant: "error",
    });
    return null;
  }
}

/** Compares two stored versions of one skill by path. */
export async function diffSkill(
  skillId: string,
  fromVersion: string | null,
  toVersion: string | null,
): Promise<SkillDiff | null> {
  const host = skillsHost();
  if (!host) return null;
  try {
    const response = await send({
      type: "skills_diff",
      host,
      skill_id: skillId,
      from_version: fromVersion,
      to_version: toVersion,
    });
    if (response.type !== "skill_diff") {
      // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
      throw new Error(`The daemon answered \`${response.type}\`, not a diff.`);
    }
    set({ diff: response.diff });
    return response.diff;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("shell:skills.compareFailed"),
      description: joinSentences([proto.message, i18n.t("shell:skills.pickVersions")]),
      variant: "error",
    });
    return null;
  }
}

export function clearSkillDiff(): void {
  set({ diff: null });
}

// ---------------------------------------------------------------------------
// Transient daemon events
// ---------------------------------------------------------------------------

let unsubscribe: (() => void) | null = null;

export function ensureSkillEvents(): void {
  if (unsubscribe) return;
  unsubscribe = getClient().onEvent(({ event }) => applySkillEvent(event));
}

export function stopSkillEvents(): void {
  unsubscribe?.();
  unsubscribe = null;
}

/** Applies one event; exported for tests. */
export function applySkillEvent(event: Event): void {
  if (event.type === "skills_changed") {
    void loadSkills({ force: true });
    return;
  }
  if (event.type === "sync_status") {
    set({ sync: event });
    if (event.conflicts > 0) void loadSkills({ force: true });
  }
}

function harnessLabel(harness: HarnessKind): string {
  switch (harness) {
    case "claude_code":
      return "Claude Code";
    case "codex":
      return "Codex";
    case "opencode":
      return "OpenCode";
    case "antigravity":
      return "Antigravity";
    case "acp":
      return "ACP agent";
  }
}

/** A scanned folder h4rn could bring in or take over: not managed, no conflict. */
export function isUnmanagedCandidate(candidate: SkillImportCandidate): boolean {
  return !candidate.managed && !candidate.conflict;
}
