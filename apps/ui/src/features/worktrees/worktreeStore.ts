import { useEffect, useRef, useSyncExternalStore } from "react";

import type { ProtoError, SkippedWorktree, TrackedWorktree, WorktreeOrigin } from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";

/** What the last clear did; the page words it under the Settled group. */
export type ClearOutcome = { removed: number; skipped: SkippedWorktree[] };

/** The machine and the exact paths a destructive confirmation was opened for. */
export type ClearConfirmation = { client: H4rnClient; paths: string[] };

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
export function captureClear(paths: string[]): ClearConfirmation {
  return { client: getClient(), paths: [...paths] };
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
const ORIGIN_TEXT: Record<WorktreeOrigin, string> = {
  subagent: "Subagent worktree",
  thread: "Thread workspace",
  harness: "Created by an agent",
};

/**
 * Where it came from, who used it, and anything worth knowing before removal.
 *
 * A missing directory means different things by origin: an owned subagent
 * checkout keeps its `h4rn/sa-*` branch, while any other origin is left only
 * as a git registration.
 */
export function worktreeSubline(row: TrackedWorktree): string {
  const users = [...new Set(row.accessed_by.map((access) => access.title).filter(Boolean))];
  const parts = [ORIGIN_TEXT[row.origin]];
  if (users.length > 0) parts.push(users.join(", "));
  if (row.status === "orphaned") parts.push("Left behind after a restart");
  else if (!row.exists)
    parts.push(
      row.origin === "subagent"
        ? "Directory removed, branch kept"
        : "Directory removed, registration left",
    );
  if (row.dirty) parts.push("Uncommitted changes");
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
  ],
  /** `checkout is no longer on <branch>` — force removes the replacement. */
  forceablePrefixes: ["checkout is no longer on"],
  /** `branch <name> has unmerged commits` — the only branch skip force fixes. */
  forceableBranchSuffix: " has unmerged commits",
  /** Force changes nothing here: the entry is in use, gone, or not ours. */
  blockedPrefixes: [
    "in use",
    "not tracked",
    "checkout removed, but branch",
    "contains another worktree at",
    "is the repository's main worktree",
    "is locked",
    "its repository could not be found",
    "git timed out",
    "registration left in place",
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
    reason.startsWith("branch ") && reason.endsWith(WORKTREE_SKIP_REASONS.forceableBranchSuffix)
  );
}

/**
 * What removing one row does, worded for a checkout that is there or gone.
 *
 * "Unmerged work" applies only to owned branches, which are the only branches
 * h4rn deletes; an unowned row can only lose uncommitted changes.
 */
export function removeDescription(row: TrackedWorktree): string {
  const base = row.exists
    ? `The checkout at ${row.path} is deleted from disk. Ignored files such as build output are deleted with it.${
        row.owned && row.branch ? ` The ${row.branch} branch h4rn created is deleted too.` : ""
      }`
    : row.owned && row.branch
      ? `No checkout remains on disk. Its ${row.branch} branch is deleted.`
      : "No checkout remains on disk.";
  const skipped = row.owned
    ? "Uncommitted changes or unmerged work are skipped."
    : "Uncommitted changes are skipped.";
  return `${base} ${skipped}${
    row.origin === "thread" ? " Its thread may still be resumed there." : ""
  }`;
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
 * is refused rather than sent.
 */
export async function clearWorktrees(
  paths: string[],
  force = false,
  expectedClient?: H4rnClient,
): Promise<void> {
  if (state.clearing) return;
  const client = getClient();
  if (
    state.source === null ||
    state.source !== client ||
    (expectedClient !== undefined && client !== expectedClient)
  ) {
    patch({ clearError: "This list belongs to another connection. Refresh it first." });
    return;
  }
  patch({ clearing: true, clearError: null, outcome: null, outcomeSource: null });
  try {
    const response = await client.request(
      // Per-git-call bound is 30 s; a clear of several entries can take longer
      // than the 60 s listing timeout, so it gets a 10x budget.
      { type: "clear_worktrees", paths, force },
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
      outcome: { removed: response.removed.length, skipped: response.skipped },
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
