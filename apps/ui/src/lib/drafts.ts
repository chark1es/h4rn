import type { HostId, ThreadId } from "@/gen";

/**
 * Unsent composer text, kept on this device across reloads and restarts.
 *
 * Drafts are keyed by host and target: thread ids only have to be unique on
 * one host, and the new-thread box belongs to a folder on a host. File bytes
 * are never stored; a thread draft is the text alone.
 *
 * Writes are debounced, flushed on `pagehide` and when the document becomes
 * hidden, and every storage call is guarded so a locked-down webview keeps
 * working with drafts in memory only.
 */

export const DRAFTS_STORAGE_KEY = "h4rn.drafts";
export const DRAFT_WRITE_DELAY_MS = 300;
export const MAX_STORED_DRAFTS = 200;

export type DraftStorage = {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
};

export type DraftStoreOptions = {
  delayMs?: number;
  maxDrafts?: number;
  now?: () => number;
};

type DraftScope = "thread" | "new-thread";

type StoredDraft = {
  scope: DraftScope;
  host: string;
  target: string;
  text: string;
  updatedAt: number;
};

type StoredDrafts = {
  version: 1;
  drafts: StoredDraft[];
};

export type DraftStore = ReturnType<typeof createDraftStore>;

function draftKey(scope: DraftScope, host: string, target: string): string {
  return `${scope}\u0000${host}\u0000${target}`;
}

function isStoredDraft(value: unknown): value is StoredDraft {
  if (!value || typeof value !== "object") return false;
  const entry = value as Record<string, unknown>;
  return (
    (entry.scope === "thread" || entry.scope === "new-thread") &&
    typeof entry.host === "string" &&
    typeof entry.target === "string" &&
    typeof entry.text === "string" &&
    entry.text.length > 0 &&
    typeof entry.updatedAt === "number" &&
    Number.isFinite(entry.updatedAt)
  );
}

function parseStoredDrafts(raw: string | null): StoredDraft[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object") return [];
    const record = parsed as Record<string, unknown>;
    if (record.version !== 1 || !Array.isArray(record.drafts)) return [];
    const byKey = new Map<string, StoredDraft>();
    for (const entry of record.drafts) {
      if (!isStoredDraft(entry)) continue;
      const key = draftKey(entry.scope, entry.host, entry.target);
      const previous = byKey.get(key);
      if (!previous || entry.updatedAt >= previous.updatedAt) byKey.set(key, entry);
    }
    return [...byKey.values()];
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

export function createDraftStore(storage: DraftStorage | null, options: DraftStoreOptions = {}) {
  const delayMs = options.delayMs ?? DRAFT_WRITE_DELAY_MS;
  const maxDrafts = options.maxDrafts ?? MAX_STORED_DRAFTS;
  const now = options.now ?? Date.now;

  let entries: StoredDraft[] = [];
  let loaded = false;
  let dirty = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  /** Oldest first, so the cap always drops the least recently touched draft. */
  function trim(): boolean {
    if (entries.length <= maxDrafts) return false;
    entries = [...entries].sort((a, b) => a.updatedAt - b.updatedAt).slice(-maxDrafts);
    return true;
  }

  function writeNow(): void {
    if (!dirty) return;
    if (!storage) {
      dirty = false;
      return;
    }
    const payload: StoredDrafts = { version: 1, drafts: entries };
    try {
      storage.setItem(DRAFTS_STORAGE_KEY, JSON.stringify(payload));
      dirty = false;
    } catch {
      // Keep the draft in memory and retry on the next write or flush.
    }
  }

  function flush(): void {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    writeNow();
  }

  function schedule(): void {
    dirty = true;
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      writeNow();
    }, delayMs);
  }

  function load(): void {
    if (loaded) return;
    loaded = true;
    if (!storage) return;
    let raw: string | null = null;
    try {
      raw = storage.getItem(DRAFTS_STORAGE_KEY);
    } catch {
      return;
    }
    entries = parseStoredDrafts(raw);
    if (trim()) {
      // A store left over the cap by an older build is rewritten at once.
      dirty = true;
      flush();
    }
  }

  function indexOf(scope: DraftScope, host: string, target: string): number {
    return entries.findIndex(
      (entry) => entry.scope === scope && entry.host === host && entry.target === target,
    );
  }

  function read(scope: DraftScope, host: string, target: string): string {
    load();
    const index = indexOf(scope, host, target);
    return index >= 0 ? (entries[index]?.text ?? "") : "";
  }

  function write(scope: DraftScope, host: string, target: string, text: string): void {
    load();
    const index = indexOf(scope, host, target);
    if (text.length === 0) {
      if (index < 0) return;
      entries.splice(index, 1);
    } else if (index >= 0) {
      entries[index] = { scope, host, target, text, updatedAt: now() };
    } else {
      entries.push({ scope, host, target, text, updatedAt: now() });
    }
    trim();
    schedule();
  }

  function remove(scope: DraftScope, host: string, target: string): void {
    load();
    const index = indexOf(scope, host, target);
    if (index < 0) return;
    entries.splice(index, 1);
    // Cancel a pending write so a debounced older value cannot bring it back.
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    dirty = true;
    flush();
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

  function readNewThread(host: HostId, workspace: string): string {
    return read("new-thread", host, workspace.trim());
  }

  function writeNewThread(host: HostId, workspace: string, text: string): void {
    write("new-thread", host, workspace.trim(), text);
  }

  function clearNewThread(host: HostId, workspace: string): void {
    remove("new-thread", host, workspace.trim());
  }

  /** Drops thread drafts whose thread is gone; new-thread drafts are left alone. */
  function pruneThreads(threads: ReadonlyArray<{ host: HostId; threadId: ThreadId }>): void {
    load();
    const live = new Set(threads.map((thread) => draftKey("thread", thread.host, thread.threadId)));
    const next = entries.filter(
      (entry) =>
        entry.scope !== "thread" || live.has(draftKey(entry.scope, entry.host, entry.target)),
    );
    if (next.length === entries.length) return;
    entries = next;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    dirty = true;
    flush();
  }

  return {
    readThreadDraft: readThread,
    writeThreadDraft: writeThread,
    clearThreadDraft: clearThread,
    readNewThreadDraft: readNewThread,
    writeNewThreadDraft: writeNewThread,
    clearNewThreadDraft: clearNewThread,
    pruneThreadDrafts: pruneThreads,
    flush,
  };
}

const browserDrafts = createDraftStore(readBrowserStorage());

export const readThreadDraft = browserDrafts.readThreadDraft;
export const writeThreadDraft = browserDrafts.writeThreadDraft;
export const clearThreadDraft = browserDrafts.clearThreadDraft;
export const readNewThreadDraft = browserDrafts.readNewThreadDraft;
export const writeNewThreadDraft = browserDrafts.writeNewThreadDraft;
export const clearNewThreadDraft = browserDrafts.clearNewThreadDraft;
export const pruneThreadDrafts = browserDrafts.pruneThreadDrafts;
export const flushDrafts = browserDrafts.flush;

if (typeof window !== "undefined") {
  window.addEventListener("pagehide", () => flushDrafts());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flushDrafts();
  });
}
