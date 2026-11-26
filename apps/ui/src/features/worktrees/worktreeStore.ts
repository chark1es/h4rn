import { useEffect, useRef, useSyncExternalStore } from "react";

import type {
  ProtoError,
  SkippedWorktree,
  TrackedWorktree,
  WorktreeOrigin,
  WorktreeSettings,
} from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { toProtoError } from "@/lib/client/errors";
import { formatList, i18n } from "@/lib/i18n";
import { getClient } from "@/lib/store/appStore";

/**
 * What the last clear did; the page words it under the Settled group. `bulk`
 * records that it was a Clear all, so a forced retry of its skips keeps the
 * same exclusions.
 */
export type ClearOutcome = { removed: number; skipped: SkippedWorktree[]; bulk: boolean };

/**
 * The machine and the exact paths a destructive confirmation was opened for,
 * and whether it is a Clear all (`bulk`): the daemon then never removes a
 * thread workspace among those paths, as for a clear with no paths.
 */
export type ClearConfirmation = { client: H4rnClient; paths: string[]; bulk: boolean };

export type WorktreeState = {
  rows: TrackedWorktree[];
  /** The client that produced `rows`; a rebuilt client makes them stale. */
  source: H4rnClient | null;
  loaded: boolean;
  loading: boolean;
  clearing: boolean;
  error: ProtoError | null;
  outcome: ClearOutcome | null;
  /** The client whose request produced `outcome`; never offer it on another. */
  outcomeSource: H4rnClient | null;
  /** Set when a clear request itself failed, as opposed to skipping entries. */
  clearError: string | null;
  /** This machine's defaults for new worktrees, once read. */
  settings: WorktreeSettings | null;
  /** The client that produced `settings`; null until loaded. */
  settingsSource: H4rnClient | null;
  settingsLoading: boolean;
  settingsSaving: boolean;
  settingsError: ProtoError | null;
};

const initialState: WorktreeState = {
  rows: [],
  source: null,
  loaded: false,
  loading: false,
  clearing: false,
  error: null,
  outcome: null,
  outcomeSource: null,
  clearError: null,
  settings: null,
  settingsSource: null,
  settingsLoading: false,
  settingsSaving: false,
  settingsError: null,
};

type Listener = () => void;

let state = initialState;
const listeners = new Set<Listener>();

function patch(next: Partial<WorktreeState>): void {
  state = { ...state, ...next };
  for (const listener of listeners) listener();
}

export function getWorktreeState(): WorktreeState {
  return state;
}

export function useWorktrees<T>(selector: (value: WorktreeState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/**
 * Captures the machine and the paths a confirmation will act on.
 *
 * Taken when the dialog opens, so a refresh behind it cannot change what
 * Confirm removes, and a machine switch makes Confirm refuse instead.
 */
export function captureClear(paths: string[], bulk = false): ClearConfirmation {
  return { client: getClient(), paths: [...paths], bulk };
}

/** The last clear's result, only while it came from the client in use now. */
export function worktreeOutcomeForClient(state: WorktreeState): ClearOutcome | null {
  return state.outcomeSource !== null && state.outcomeSource === getClient() ? state.outcome : null;
}

/**
 * Whether the loaded rows came from the client this session is talking to now.
 *
 * `restartConnection` throws the old client away (a machine switch, sign-in or
 * pairing), so rows loaded through it describe another host. Clearing through
 * them would remove checkouts on the wrong machine.
 */
export function worktreeClientCurrent(source: H4rnClient | null): boolean {
  return source !== null && source === getClient();
}

/** Worktrees something is using right now. */
export function inUseWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return rows.filter((row) => row.status === "active");
}

/** Worktrees with nothing running in them: settled, or left behind. */
export function settledWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return rows.filter((row) => row.status !== "active");
}

/** The origin in words. */
const ORIGIN_KEY = {
  subagent: "settings:worktrees.origin.subagent",
  thread: "settings:worktrees.origin.thread",
  harness: "settings:worktrees.origin.harness",
} as const satisfies Record<WorktreeOrigin, string>;

/**
 * Where it came from, who used it, and anything worth knowing before removal.
 *
 * A missing directory means different things by origin: an owned subagent
 * checkout keeps its `h4rn/sa-*` branch, while any other origin is left only
 * as a git registration. The daemon reports no branch when it knows the branch
 * is gone and only the registration remains, so a branch is never promised.
 */
export function worktreeSubline(row: TrackedWorktree): string {
  const users = [...new Set(row.accessed_by.map((access) => access.title).filter(Boolean))];
  const parts: string[] = [i18n.t(ORIGIN_KEY[row.origin])];
  if (users.length > 0) parts.push(formatList(users, "conjunction", "narrow"));
  if (row.status === "orphaned") parts.push(i18n.t("settings:worktrees.orphaned"));
  else if (!row.exists)
    parts.push(
      row.origin === "subagent" && row.branch
        ? i18n.t("settings:worktrees.removedBranchKept")
        : i18n.t("settings:worktrees.removedRegistrationLeft"),
    );
  if (row.dirty) parts.push(i18n.t("settings:worktrees.uncommitted"));
  return parts.join(" · ");
}

/**
 * What "Clear all" removes. Thread workspaces belong to whoever made them and
 * their thread may resume there, so the daemon only removes one by path.
 */
export function clearableWorktrees(rows: TrackedWorktree[]): TrackedWorktree[] {
  return settledWorktrees(rows).filter((row) => row.origin !== "thread");
}

/**
 * The daemon's skip reasons, grouped by whether "Remove anyway" can act on
 * them. Keep this in step with `crates/h4rn-daemon/src/worktrees/mod.rs`.
 */
export const WORKTREE_SKIP_REASONS = {
  /** Force removes the checkout despite the work it holds. */
  forceable: [
    "has uncommitted changes",
    "could not check for uncommitted changes",
    "detached HEAD has commits on no branch",
    "contains submodules",
    "could not read the registration's HEAD",
    "has files hidden from git status",
    "has a git operation in progress",
    "has staged changes",
    "the registration could no longer be located",
  ],
  /** `checkout is no longer on <branch>` — force removes the replacement. */
  forceablePrefixes: ["checkout is no longer on"],
  /**
   * `branch <name> has unmerged commits` and `branch <name> changed during the
   * clear` — the only branch skips force fixes.
   */
  forceableBranchSuffixes: [" has unmerged commits", " changed during the clear"],
  /** Force changes nothing here: the entry is in use, gone, or not ours. */
  blockedPrefixes: [
    "in use",
    "not tracked",
    "checkout removed, but branch",
    "contains another worktree at",
    "contains another git checkout at",
    // The daemon requires the nested scan to finish even on a forced clear.
    "could not check for nested checkouts",
    "is the repository's main worktree",
    "is locked",
    "its repository could not be found",
    "git timed out",
    "registration left in place",
    "the checkout was recreated during the clear",
    "not a registered worktree",
    "is a thread workspace",
    "has an enabled scheduled task",
    "could not read every worktree registration",
  ],
  /** `branch <name> is a symbolic ref` — the ref is kept, target included. */
  blockedSuffixes: ["is a symbolic ref", "was left in place"],
} as const;

/** Whether a forced clear could remove an entry the daemon skipped. */
export function forceCanFix(reason: string): boolean {
  if (WORKTREE_SKIP_REASONS.blockedPrefixes.some((prefix) => reason.startsWith(prefix))) {
    return false;
  }
  if (WORKTREE_SKIP_REASONS.blockedSuffixes.some((suffix) => reason.endsWith(suffix))) {
    return false;
  }
  if ((WORKTREE_SKIP_REASONS.forceable as readonly string[]).includes(reason)) return true;
  if (WORKTREE_SKIP_REASONS.forceablePrefixes.some((prefix) => reason.startsWith(prefix))) {
    return true;
  }
  return (
    reason.startsWith("branch ") &&
    WORKTREE_SKIP_REASONS.forceableBranchSuffixes.some((suffix) => reason.endsWith(suffix))
  );
}

/**
 * What removing one row does, worded for a checkout that is there or gone.
 *
 * "Unmerged work" applies only to owned branches, which are the only branches
 * h4rn deletes; an unowned row can only lose uncommitted changes.
 */
export function removeDescription(row: TrackedWorktree): string {
  const sentences: string[] = [];
  if (row.exists) {
    sentences.push(i18n.t("settings:worktrees.remove.checkoutDeleted", { path: row.path }));
    if (row.owned && row.branch) {
      sentences.push(i18n.t("settings:worktrees.remove.ownedBranchToo", { branch: row.branch }));
    }
  } else if (row.owned && row.branch) {
    sentences.push(i18n.t("settings:worktrees.remove.goneBranchDeleted", { branch: row.branch }));
  } else if (row.branch) {
    sentences.push(i18n.t("settings:worktrees.remove.gone"));
  } else {
    sentences.push(i18n.t("settings:worktrees.remove.goneRegistration"));
  }
  sentences.push(
    row.owned
      ? i18n.t("settings:worktrees.remove.skipsUnmerged")
      : i18n.t("settings:worktrees.remove.skipsUncommitted"),
  );
  if (row.origin === "thread") sentences.push(i18n.t("settings:worktrees.remove.threadResume"));
  return joinSentences(sentences);
}

/**
 * What "Remove anyway" discards, for its confirmation. `threadNote` is added
 * when thread workspaces are among the rows.
 */
export function forceRemoveDescription(threadNote: string): string {
  return joinSentences([i18n.t("settings:worktrees.force.description"), threadNote]);
}

/**
 * Sentences in one paragraph. Latin scripts separate them with a space;
 * Chinese and Japanese full stops already end the sentence.
 */
export function joinSentences(sentences: string[]): string {
  const separator = i18n.t("settings:worktrees.sentenceSeparator");
  return sentences
    .map((sentence) => sentence.trim())
    .filter(Boolean)
    .join(separator);
}

/**
 * What Clear all does, for its confirmation.
 *
 * The daemon decides which worktrees a thread is working in by path, at the
 * moment it clears, so the copy always says they are kept; `keptThreads` adds
 * the count of thread workspaces already left out of the rows shown.
 */
export function clearAllDescription(keptThreads: number): string {
  return joinSentences([
    i18n.t("settings:worktrees.clearAll.description"),
    keptThreads > 0 ? i18n.t("settings:worktrees.clearAll.kept", { count: keptThreads }) : "",
  ]);
}

/** The loaded page's layout: a notice above the groups, or the empty copy. */
export type WorktreePage =
  | { kind: "loading" }
  | { kind: "groups"; notice: string | null; empty: boolean };

/**
 * A failed read keeps whatever rows are already loaded and shows the error as
 * a notice above them, never in their place. The empty copy only shows after a
 * successful read, so an error is not mistaken for "nothing here".
 */
export function worktreePage(
  loaded: boolean,
  rows: TrackedWorktree[],
  error: ProtoError | null,
): WorktreePage {
  if (!loaded) return { kind: "loading" };
  return {
    kind: "groups",
    notice: error?.message ?? null,
    empty: rows.length === 0 && error === null,
  };
}

export async function loadWorktrees(): Promise<void> {
  const client = getClient();
  patch({ loading: true, error: null });
  try {
    const response = await client.request({ type: "list_worktrees" }, { timeoutMs: 60_000 });
    // A machine switch rebuilt the client while the request was in flight.
    // These rows are another host's.
    if (getClient() !== client) {
      patch({ loading: false });
      return;
    }
    if (response.type !== "worktrees") {
      // Rows loaded from another machine must never be shown as this one's.
      patch({
        loaded: true,
        loading: false,
        source: client,
        ...(state.source === client ? {} : { rows: [] }),
      });
      return;
    }
    patch({
      rows: response.worktrees,
      loaded: true,
      loading: false,
      error: null,
      source: client,
    });
  } catch (error) {
    if (getClient() !== client) {
      patch({ loading: false });
      return;
    }
    // A failed first read on a new machine keeps no rows from the old one.
    patch({
      loaded: true,
      loading: false,
      error: toProtoError(error),
      source: client,
      ...(state.source === client ? {} : { rows: [] }),
    });
  }
}

/**
 * Clears the given explicit paths. Entries the daemon leaves in place come back
 * in `outcome.skipped` so the page can offer a forced retry.
 *
 * `expectedClient` comes from a confirmation captured when its dialog opened:
 * if the machine changed since, the paths describe another host and the clear
 * is refused rather than sent. `bulk` marks a Clear all (or the forced retry
 * of its skips): the daemon applies the Clear all exclusions to the paths, so
 * a row that became a thread workspace behind the dialog is left alone.
 */
export async function clearWorktrees(
  paths: string[],
  force = false,
  expectedClient?: H4rnClient,
  bulk = false,
): Promise<void> {
  if (state.clearing) return;
  const client = getClient();
  if (
    state.source === null ||
    state.source !== client ||
    (expectedClient !== undefined && client !== expectedClient)
  ) {
    patch({ clearError: i18n.t("settings:worktrees.otherConnection.list") });
    return;
  }
  patch({ clearing: true, clearError: null, outcome: null, outcomeSource: null });
  try {
    const response = await client.request(
      // Per-git-call bound is 30 s; a clear of several entries can take longer
      // than the 60 s listing timeout, so it gets a 10x budget.
      { type: "clear_worktrees", paths, force, bulk },
      { timeoutMs: 600_000 },
    );
    if (getClient() !== client) {
      patch({ clearing: false });
      return;
    }
    if (response.type !== "worktrees_cleared") {
      patch({ clearing: false });
      return;
    }
    patch({
      rows: response.worktrees,
      loaded: true,
      clearing: false,
      error: null,
      outcome: { removed: response.removed.length, skipped: response.skipped, bulk },
      outcomeSource: client,
    });
  } catch (error) {
    if (getClient() !== client) {
      patch({ clearing: false });
      return;
    }
    const proto = toProtoError(error);
    patch({
      clearing: false,
      clearError: proto.hint ? `${proto.message} ${proto.hint}` : proto.message,
    });
  }
}

export function dismissWorktreeOutcome(): void {
  patch({ outcome: null, outcomeSource: null, clearError: null });
}

/** Reads this machine's defaults for new thread worktrees. */
export async function loadWorktreeSettings(): Promise<void> {
  const client = getClient();
  patch({ settingsLoading: true, settingsError: null });
  try {
    const response = await client.request({ type: "get_worktree_settings" }, { timeoutMs: 15_000 });
    // A machine switch rebuilt the client; these settings describe another
    // host.
    if (getClient() !== client) {
      patch({ settingsLoading: false });
      return;
    }
    if (response.type !== "worktree_settings") {
      patch({
        settingsLoading: false,
        ...(state.settingsSource === client ? {} : { settings: null }),
      });
      return;
    }
    patch({
      settings: response.settings,
      settingsSource: client,
      settingsLoading: false,
      settingsError: null,
    });
  } catch (error) {
    if (getClient() !== client) {
      patch({ settingsLoading: false });
      return;
    }
    patch({
      settingsLoading: false,
      settingsError: toProtoError(error),
      ...(state.settingsSource === client ? {} : { settings: null }),
    });
  }
}

/**
 * Saves this machine's defaults. Returns whether the daemon accepted them;
 * a rejection is kept in `settingsError` so the page can word it.
 */
export async function saveWorktreeSettings(
  settings: WorktreeSettings,
  expectedClient?: H4rnClient,
): Promise<boolean> {
  const client = getClient();
  if (
    state.settingsSource === null ||
    state.settingsSource !== client ||
    (expectedClient !== undefined && expectedClient !== client)
  ) {
    patch({
      settingsError: {
        code: "conflict",
        message: i18n.t("settings:worktrees.otherConnection.settings"),
        hint: null,
      },
    });
    return false;
  }
  patch({ settingsSaving: true, settingsError: null });
  try {
    const response = await client.request(
      { type: "set_worktree_settings", settings },
      { timeoutMs: 15_000 },
    );
    if (getClient() !== client) {
      patch({ settingsSaving: false });
      return false;
    }
    if (response.type !== "worktree_settings") {
      patch({ settingsSaving: false });
      return false;
    }
    patch({ settings: response.settings, settingsSaving: false, settingsError: null });
    return true;
  } catch (error) {
    if (getClient() !== client) {
      patch({ settingsSaving: false });
      return false;
    }
    patch({ settingsSaving: false, settingsError: toProtoError(error) });
    return false;
  }
}

/**
 * Whether this effect run should refetch: the connection just came back (its
 * rows may be hours old) or the stored rows came from a replaced client.
 */
export function shouldReloadWorktrees(
  online: boolean,
  wasOnline: boolean,
  current: boolean,
): boolean {
  return online && (!wasOnline || !current);
}

/**
 * Reads the list on mount, again each time the connection returns, and again
 * when the stored rows came from a client that has since been replaced.
 */
export function useWorktreeList(online: boolean): void {
  const current = useWorktrees((value) => worktreeClientCurrent(value.source));
  // Starts as `false`, so mounting while online is treated as a transition and
  // always reloads; cached rows keep rendering until the fresh list lands.
  const wasOnline = useRef(false);
  useEffect(() => {
    const reconnect = shouldReloadWorktrees(online, wasOnline.current, current);
    wasOnline.current = online;
    if (reconnect) void loadWorktrees();
  }, [online, current]);
}

export function resetWorktreeStoreForTests(): void {
  state = initialState;
  for (const listener of listeners) listener();
}

/**
 * Splits a path for middle truncation: the head may be cut with an ellipsis,
 * the tail keeps the last two folders. Indices are UTF-16 offsets (what
 * `slice` uses), so a character outside the BMP is never split; `/` and `\`
 * both count, and a UNC `\\server\share` prefix is never cut into.
 */
export function splitMiddlePath(shown: string): [string, string] {
  const separators: number[] = [];
  for (let index = 0; index < shown.length; index += 1) {
    if (shown[index] === "/" || shown[index] === "\\") separators.push(index);
  }
  // A path's root prefix is never cut: `\\?\UNC\server\share`,
  // `\\server\share`, `\\?\C:` / `\\.\C:`. The earliest cut is the
  // separator that ends it.
  const rootEnd = shown.startsWith("\\\\?\\UNC\\")
    ? separators[5]
    : shown.startsWith("\\\\?\\") || shown.startsWith("\\\\.\\")
      ? separators[3]
      : shown.startsWith("\\\\")
        ? separators[3]
        : 1;
  const floor = rootEnd ?? Number.POSITIVE_INFINITY;
  const cut = separators.length >= 2 ? (separators.at(-2) ?? -1) : -1;
  if (cut < floor) return [shown, ""];
  return [shown.slice(0, cut), shown.slice(cut)];
}
