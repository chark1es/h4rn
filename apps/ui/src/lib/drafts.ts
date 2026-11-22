import type { HostId, ThreadId } from "@/gen";

/**
 * Unsent composer text, kept on this device across reloads and restarts.
 *
 * Drafts are keyed by host and target: thread ids only have to be unique on
 * one host, and the new-thread box belongs to a folder on a host. File bytes
 * are never stored; a thread draft is the text alone.
 *
 * Every draft has its own localStorage key, so two tabs (or two windows on
 * different threads) flush only the keys they changed and never erase each
 * other's work. Writes are debounced, flushed on `pagehide` and when the
 * document becomes hidden, and every storage call is guarded so a locked-down
 * webview keeps working with drafts in memory only.
 */

/** Drafts written by builds that kept one JSON blob; migrated on first load. */
export const LEGACY_DRAFTS_STORAGE_KEY = "h4rn.drafts";
export const DRAFT_KEY_PREFIX = "h4rn.draft.v2.";
export const DRAFT_WRITE_DELAY_MS = 300;
export const MAX_STORED_DRAFTS = 200;
export const DEFAULT_NEW_THREAD_WORKSPACE = "~/code";

const LAST_WORKSPACE_KEY_PREFIX = "h4rn.newThread.workspace.";

export type DraftScope = "thread" | "new-thread";

export type DraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem" | "key" | "length">;

export type DraftStoreOptions = {
  delayMs?: number;
  maxDrafts?: number;
  now?: () => number;
};

/** A value as stored: the text and when it was last touched. */
export type StoredDraft = {
  text: string;
  updatedAt: number;
};

export type DraftTarget = {
  scope: DraftScope;
  host: string;
  target: string;
};

export type DraftChange = DraftTarget & { text: string };

type LegacyDraft = DraftTarget & StoredDraft;

export type DraftStore = ReturnType<typeof createDraftStore>;

export function normalizeWorkspace(workspace: string): string {
  return workspace.trim();
}

function encodePart(value: string): string {
  // A dot separates the parts of a key, so encode the dots inside a part too.
  return encodeURIComponent(value).replace(/\./g, "%2E");
}

export function draftStorageKey(scope: DraftScope, host: string, target: string): string {
  return `${DRAFT_KEY_PREFIX}${scope}.${encodePart(host)}.${encodePart(target)}`;
}

export function parseDraftStorageKey(key: string): DraftTarget | null {
  if (!key.startsWith(DRAFT_KEY_PREFIX)) return null;
  const parts = key.slice(DRAFT_KEY_PREFIX.length).split(".");
  if (parts.length !== 3) return null;
  const [scope, host, target] = parts;
  if (scope !== "thread" && scope !== "new-thread") return null;
  if (host === undefined || target === undefined) return null;
  try {
    return { scope, host: decodeURIComponent(host), target: decodeURIComponent(target) };
  } catch {
    return null;
  }
}

export function parseDraftValue(raw: string | null): StoredDraft | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object") return null;
    const record = parsed as Record<string, unknown>;
    if (typeof record.text !== "string" || record.text.length === 0) return null;
    if (typeof record.updatedAt !== "number" || !Number.isFinite(record.updatedAt)) return null;
    return { text: record.text, updatedAt: record.updatedAt };
  } catch {
    return null;
  }
}

function parseLegacyDrafts(raw: string | null): LegacyDraft[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object") return [];
    const record = parsed as Record<string, unknown>;
    if (record.version !== 1 || !Array.isArray(record.drafts)) return [];
    const drafts: LegacyDraft[] = [];
    for (const value of record.drafts) {
      if (!value || typeof value !== "object") continue;
      const entry = value as Record<string, unknown>;
      if (entry.scope !== "thread" && entry.scope !== "new-thread") continue;
      if (typeof entry.host !== "string" || typeof entry.target !== "string") continue;
      if (typeof entry.text !== "string" || entry.text.length === 0) continue;
      if (typeof entry.updatedAt !== "number" || !Number.isFinite(entry.updatedAt)) continue;
      drafts.push({
        scope: entry.scope,
        host: entry.host,
        target: entry.target,
        text: entry.text,
        updatedAt: entry.updatedAt,
      });
    }
    return drafts;
  } catch {
    return [];
  }
}

function readBrowserStorage(): DraftStorage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage;
  } catch {
    return null;
  }
}

/**
 * The storage interface cannot report a failed read, so `undefined` means
 * "storage threw" and `null` means "no value". Reads fall back to the values
 * this tab already knows when storage throws.
 */
function readStoredDraft(storage: DraftStorage, key: string): StoredDraft | null | undefined {
  try {
    return parseDraftValue(storage.getItem(key));
  } catch {
    return undefined;
  }
}

function storageDraftKeys(storage: DraftStorage): string[] {
  try {
    const keys: string[] = [];
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (key?.startsWith(DRAFT_KEY_PREFIX)) keys.push(key);
    }
    return keys;
  } catch {
    return [];
  }
}

function lastWorkspaceStorageKey(host: string): string {
  return `${LAST_WORKSPACE_KEY_PREFIX}${encodePart(host)}`;
}

/**
 * Flushes drafts when the page is about to be hidden. Pulled out of the module
 * wiring so the listener behaviour can be tested without a browser.
 */
export function installDraftFlushHandlers(
  target: {
    addEventListener(type: string, listener: () => void): void;
    removeEventListener(type: string, listener: () => void): void;
  },
  isHidden: () => boolean,
  flush: () => void,
): () => void {
  const onPageHide = () => flush();
  const onVisibilityChange = () => {
    if (isHidden()) flush();
  };
  target.addEventListener("pagehide", onPageHide);
  target.addEventListener("visibilitychange", onVisibilityChange);
  return () => {
    target.removeEventListener("pagehide", onPageHide);
    target.removeEventListener("visibilitychange", onVisibilityChange);
  };
}

export function createDraftStore(storage: DraftStorage | null, options: DraftStoreOptions = {}) {
  const delayMs = options.delayMs ?? DRAFT_WRITE_DELAY_MS;
  const maxDrafts = options.maxDrafts ?? MAX_STORED_DRAFTS;
  const now = options.now ?? Date.now;

  /** Last value this tab knows per key; the fallback when storage is unreadable. */
  const memory = new Map<string, StoredDraft>();
  /** Keys changed by this tab and not yet written to storage. */
  const pendingWrites = new Map<string, StoredDraft>();
  const pendingRemoves = new Set<string>();
  let loaded = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  function dropBehindCap(): void {
    const keys = storage ? storageDraftKeys(storage) : [];
    if (storage) {
      // Writes still pending will add keys of their own; reserve room for them
      // rather than deleting a draft the user just typed.
      const storedKeys = keys.filter((key) => !pendingWrites.has(key));
      const allowed = Math.max(0, maxDrafts - pendingWrites.size);
      if (storedKeys.length <= allowed) return;
      const entries = storedKeys
        .map((key) => ({ key, updatedAt: readStoredDraft(storage, key)?.updatedAt ?? 0 }))
        .sort((a, b) => a.updatedAt - b.updatedAt);
      for (const entry of entries.slice(0, entries.length - allowed)) {
        try {
          storage.removeItem(entry.key);
        } catch {
          // Leave it; the next flush tries again.
        }
        memory.delete(entry.key);
        pendingRemoves.delete(entry.key);
      }
      return;
    }
    if (memory.size <= maxDrafts) return;
    const oldest = [...memory.entries()].sort((a, b) => a[1].updatedAt - b[1].updatedAt);
    for (const [key] of oldest.slice(0, memory.size - maxDrafts)) memory.delete(key);
  }

  /** Moves drafts from the single JSON blob older builds wrote, then removes it. */
  function migrateLegacy(): void {
    if (!storage) return;
    let raw: string | null = null;
    try {
      raw = storage.getItem(LEGACY_DRAFTS_STORAGE_KEY);
    } catch {
      return;
    }
    if (!raw) return;
    for (const draft of parseLegacyDrafts(raw)) {
      const key = draftStorageKey(draft.scope, draft.host, draft.target);
      const existing = readStoredDraft(storage, key);
      if (existing && existing.updatedAt >= draft.updatedAt) continue;
      const value: StoredDraft = { text: draft.text, updatedAt: draft.updatedAt };
      try {
        storage.setItem(key, JSON.stringify(value));
        memory.set(key, value);
      } catch {
        // Keep what we can; the legacy blob is only removed once it is handled.
        return;
      }
    }
    try {
      storage.removeItem(LEGACY_DRAFTS_STORAGE_KEY);
    } catch {
      // It will be migrated again next time; no draft is lost either way.
    }
  }

  function load(): void {
    if (loaded) return;
    loaded = true;
    migrateLegacy();
    dropBehindCap();
  }

  function writeNow(): void {
    if (!storage) {
      pendingWrites.clear();
      pendingRemoves.clear();
      dropBehindCap();
      return;
    }
    for (const [key, value] of [...pendingWrites]) {
      try {
        storage.setItem(key, JSON.stringify(value));
        memory.set(key, value);
        pendingWrites.delete(key);
      } catch {
        // Keep the draft pending and retry on the next write or flush.
      }
    }
    for (const key of [...pendingRemoves]) {
      try {
        storage.removeItem(key);
        memory.delete(key);
        pendingRemoves.delete(key);
      } catch {
        // Keep it pending so a retry cannot resurrect the removed text.
      }
    }
    dropBehindCap();
  }

  function flush(): void {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    writeNow();
  }

  function schedule(): void {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      writeNow();
    }, delayMs);
  }

  function read(scope: DraftScope, host: string, target: string): string {
    load();
    const key = draftStorageKey(scope, host, target);
    if (pendingRemoves.has(key)) return "";
    const pending = pendingWrites.get(key);
    if (pending) return pending.text;
    if (!storage) return memory.get(key)?.text ?? "";
    const stored = readStoredDraft(storage, key);
    if (stored === undefined) return memory.get(key)?.text ?? "";
    if (stored === null) {
      memory.delete(key);
      return "";
    }
    memory.set(key, stored);
    return stored.text;
  }

  function write(scope: DraftScope, host: string, target: string, text: string): void {
    load();
    const key = draftStorageKey(scope, host, target);
    if (text.length === 0) {
      memory.delete(key);
      pendingWrites.delete(key);
      pendingRemoves.add(key);
    } else {
      const value = { text, updatedAt: now() };
      memory.set(key, value);
      pendingWrites.set(key, value);
      pendingRemoves.delete(key);
    }
    schedule();
  }

  function hasStoredDraft(scope: DraftScope, host: string, target: string): boolean {
    const key = draftStorageKey(scope, host, target);
    if (memory.has(key) || pendingWrites.has(key)) return true;
    if (!storage) return false;
    return readStoredDraft(storage, key) !== null;
  }

  function remove(scope: DraftScope, host: string, target: string): void {
    load();
    if (!hasStoredDraft(scope, host, target)) return;
    const key = draftStorageKey(scope, host, target);
    memory.delete(key);
    pendingWrites.delete(key);
    pendingRemoves.add(key);
    // Cancel a pending write so a debounced older value cannot bring it back.
    flush();
  }

  function hasPending(scope: DraftScope, host: string, target: string): boolean {
    const key = draftStorageKey(scope, host, target);
    return pendingWrites.has(key) || pendingRemoves.has(key);
  }

  function readThread(host: HostId, threadId: ThreadId): string {
    return read("thread", host, threadId);
  }

  function writeThread(host: HostId, threadId: ThreadId, text: string): void {
    write("thread", host, threadId, text);
  }

  function clearThread(host: HostId, threadId: ThreadId): void {
    remove("thread", host, threadId);
  }

  function hasPendingThread(host: HostId, threadId: ThreadId): boolean {
    return hasPending("thread", host, threadId);
  }

  function readNewThread(host: HostId, workspace: string): string {
    return read("new-thread", host, normalizeWorkspace(workspace));
  }

  function writeNewThread(host: HostId, workspace: string, text: string): void {
    write("new-thread", host, normalizeWorkspace(workspace), text);
  }

  function clearNewThread(host: HostId, workspace: string): void {
    remove("new-thread", host, normalizeWorkspace(workspace));
  }

  function readLastWorkspace(host: HostId): string {
    if (!storage) return "";
    try {
      return storage.getItem(lastWorkspaceStorageKey(host)) ?? "";
    } catch {
      return "";
    }
  }

  function writeLastWorkspace(host: HostId, workspace: string): void {
    if (!storage) return;
    const value = normalizeWorkspace(workspace);
    try {
      if (value.length === 0) storage.removeItem(lastWorkspaceStorageKey(host));
      else storage.setItem(lastWorkspaceStorageKey(host), value);
    } catch {
      // Remembering the folder is a convenience; the dialog still works.
    }
  }

  /**
   * Drops thread drafts whose thread is gone. Only hosts this daemon reported
   * are considered: a draft from another machine or a signed-out account is
   * not ours to delete. New-thread drafts are left alone.
   */
  function pruneThreads(
    threads: ReadonlyArray<{ host: HostId; threadId: ThreadId }>,
    hosts: ReadonlyArray<string> | ReadonlySet<string>,
  ): void {
    load();
    const hostScope = hosts instanceof Set ? hosts : new Set(hosts);
    if (hostScope.size === 0) return;
    const live = new Set(
      threads.map((thread) => draftStorageKey("thread", thread.host, thread.threadId)),
    );
    const keys = new Set<string>([
      ...(storage ? storageDraftKeys(storage) : []),
      ...memory.keys(),
      ...pendingWrites.keys(),
      ...pendingRemoves,
    ]);
    let changed = false;
    for (const key of keys) {
      const target = parseDraftStorageKey(key);
      if (target?.scope !== "thread") continue;
      if (!hostScope.has(target.host)) continue;
      if (live.has(key)) continue;
      memory.delete(key);
      pendingWrites.delete(key);
      pendingRemoves.add(key);
      changed = true;
    }
    if (changed) flush();
  }

  /**
   * A `storage` event means another tab changed a draft. Adopt it only when
   * this tab has no pending write for the same key; otherwise the local edit
   * is the newer intent and is left alone.
   */
  function applyStorageEvent(key: string | null, newValue: string | null): DraftChange | null {
    if (key === null) return null;
    const target = parseDraftStorageKey(key);
    if (!target) return null;
    if (pendingWrites.has(key) || pendingRemoves.has(key)) return null;
    const value = parseDraftValue(newValue);
    if (value) memory.set(key, value);
    else memory.delete(key);
    return { ...target, text: value?.text ?? "" };
  }

  return {
    readThreadDraft: readThread,
    writeThreadDraft: writeThread,
    clearThreadDraft: clearThread,
    hasPendingThreadDraft: hasPendingThread,
    readNewThreadDraft: readNewThread,
    writeNewThreadDraft: writeNewThread,
    clearNewThreadDraft: clearNewThread,
    readLastWorkspace,
    writeLastWorkspace,
    pruneThreadDrafts: pruneThreads,
    applyStorageEvent,
    flush,
  };
}

export const drafts = createDraftStore(readBrowserStorage());

export const readThreadDraft = drafts.readThreadDraft;
export const writeThreadDraft = drafts.writeThreadDraft;
export const clearThreadDraft = drafts.clearThreadDraft;
export const hasPendingThreadDraft = drafts.hasPendingThreadDraft;
export const readNewThreadDraft = drafts.readNewThreadDraft;
export const writeNewThreadDraft = drafts.writeNewThreadDraft;
export const clearNewThreadDraft = drafts.clearNewThreadDraft;
export const readLastWorkspace = drafts.readLastWorkspace;
export const writeLastWorkspace = drafts.writeLastWorkspace;
export const pruneThreadDrafts = drafts.pruneThreadDrafts;
export const flushDrafts = drafts.flush;

const draftChangeListeners = new Set<(change: DraftChange) => void>();

/** Notifies the open composer when another tab changes one of its drafts. */
export function subscribeDraftChanges(listener: (change: DraftChange) => void): () => void {
  draftChangeListeners.add(listener);
  return () => {
    draftChangeListeners.delete(listener);
  };
}

if (typeof window !== "undefined") {
  window.addEventListener("storage", (event) => {
    const change = drafts.applyStorageEvent(event.key, event.newValue);
    if (!change) return;
    for (const listener of draftChangeListeners) listener(change);
  });
  installDraftFlushHandlers(
    window,
    () => typeof document !== "undefined" && document.visibilityState === "hidden",
    () => flushDrafts(),
  );
}
