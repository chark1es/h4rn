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

export const DRAFT_KEY_PREFIX = "h4rn.draft.v2.";
export const DRAFT_WRITE_DELAY_MS = 300;
export const MAX_STORED_DRAFTS = 200;
/** An unsent draft older than this is dropped when the app loads. */
export const DRAFT_MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000;
export const DEFAULT_NEW_THREAD_WORKSPACE = "~/code";

const LAST_WORKSPACE_KEY_PREFIX = "h4rn.newThread.workspace.";

export type DraftScope = "thread" | "new-thread";

export type DraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem" | "key" | "length">;

export type DraftStoreOptions = {
  delayMs?: number;
  maxDrafts?: number;
  maxAgeMs?: number;
  now?: () => number;
  /** Identifies this tab inside a revision; generated once per store by default. */
  nonce?: string;
  /**
   * Called when a queued write loses to newer text another tab saved. The
   * open composer uses it to show that text instead of the edit it lost.
   */
  onAdopt?: (change: DraftChange) => void;
};

/**
 * Optimistic threads exist only while the daemon starts them. Their ids must
 * never reach storage: another session could reuse the same temporary id.
 */
export const LOCAL_THREAD_PREFIX = "local-thread-";

export function isLocalThreadId(threadId: string): boolean {
  return threadId.startsWith(LOCAL_THREAD_PREFIX);
}

/**
 * Identifies one saved draft: when it was written, and which tab wrote it.
 * Two tabs can write in the same millisecond, so the timestamp alone is not
 * unique; both halves are compared.
 */
export type DraftRevision = {
  updatedAt: number;
  nonce: string;
};

/** A value as stored: the text, its revision and when it was last touched. */
export type StoredDraft = DraftRevision & {
  text: string;
};

export type DraftTarget = {
  scope: DraftScope;
  host: string;
  target: string;
};

/** One saved draft as this tab sees it. */
export type DraftEntry = {
  text: string;
  revision: DraftRevision | null;
};

export type DraftChange = DraftTarget & DraftEntry;

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
    // Values written before revisions carried a nonce are still drafts.
    const nonce = typeof record.nonce === "string" ? record.nonce : "";
    return { text: record.text, updatedAt: record.updatedAt, nonce };
  } catch {
    return null;
  }
}

export function revisionOf(value: StoredDraft): DraftRevision {
  return { updatedAt: value.updatedAt, nonce: value.nonce };
}

export function sameRevision(a: DraftRevision | null, b: DraftRevision | null): boolean {
  if (a === null || b === null) return a === b;
  return a.updatedAt === b.updatedAt && a.nonce === b.nonce;
}

/**
 * Whether `a` was written after `b`. Timestamps are monotonic per key, so a
 * larger one is newer; at the same millisecond a different tab wrote it.
 */
function isNewerRevision(a: DraftRevision, b: DraftRevision | null): boolean {
  if (b === null) return true;
  if (a.updatedAt !== b.updatedAt) return a.updatedAt > b.updatedAt;
  return a.nonce !== b.nonce;
}

/**
 * Whether `a` is definitely later than `b`. Two revisions written in the same
 * millisecond by different tabs have no order, so neither wins by comparison;
 * on a tie the flush that runs later lands and the other tab re-renders to it.
 * Both edits do not survive.
 */
function isStrictlyNewerRevision(a: DraftRevision, b: DraftRevision): boolean {
  return a.updatedAt > b.updatedAt;
}

function randomNonce(): string {
  try {
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
  } catch {
    // Fall through to a non-cryptographic id; it only has to be unique.
  }
  return `${Math.random().toString(36).slice(2, 10)}${Math.random().toString(36).slice(2, 10)}`;
}

function readBrowserStorage(): DraftStorage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage;
  } catch {
    return null;
  }
}

/** What a key currently holds, with the read failures storage cannot report. */
type StoredRead =
  | { state: "ok"; value: StoredDraft }
  | { state: "missing" }
  | { state: "corrupt" }
  | { state: "unreadable" };

function readStored(storage: DraftStorage, key: string): StoredRead {
  let raw: string | null;
  try {
    raw = storage.getItem(key);
  } catch {
    return { state: "unreadable" };
  }
  if (raw === null) return { state: "missing" };
  const value = parseDraftValue(raw);
  return value ? { state: "ok", value } : { state: "corrupt" };
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
  const maxAgeMs = options.maxAgeMs ?? DRAFT_MAX_AGE_MS;
  const now = options.now ?? Date.now;
  const nonce = options.nonce ?? randomNonce();

  /** Last value this tab knows per key; the fallback when storage is unreadable. */
  const memory = new Map<string, StoredDraft>();
  /** Keys changed by this tab and not yet written to storage. */
  const pendingWrites = new Map<string, StoredDraft>();
  /**
   * Pending writes storage refused (a full quota, typically). They hold the
   * text in memory but do not reserve room under the cap: nothing was stored.
   */
  const failedWrites = new Set<string>();
  /** Keys this tab removed, with the revision it meant to remove. */
  const pendingRemoves = new Map<string, DraftRevision>();
  let loaded = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  /** A key that does not parse is not a draft; drop it from storage too. */
  function forgetCorrupt(key: string): void {
    memory.delete(key);
    pendingWrites.delete(key);
    failedWrites.delete(key);
    try {
      storage?.removeItem(key);
    } catch {
      // The next load tries again; nothing usable is lost either way.
    }
  }

  /** Tell the open composer that newer text replaced a queued edit. */
  function adopted(key: string, value: StoredDraft | null): void {
    if (!options.onAdopt) return;
    const target = parseDraftStorageKey(key);
    if (!target) return;
    options.onAdopt({
      ...target,
      text: value?.text ?? "",
      revision: value ? revisionOf(value) : null,
    });
  }

  function dropBehindCap(): void {
    if (!storage) {
      if (memory.size <= maxDrafts) return;
      const oldest = [...memory.entries()].sort((a, b) => a[1].updatedAt - b[1].updatedAt);
      for (const [key] of oldest.slice(0, memory.size - maxDrafts)) memory.delete(key);
      return;
    }
    const entries: { key: string; updatedAt: number }[] = [];
    let unreadable = 0;
    for (const key of storageDraftKeys(storage)) {
      // Writes still pending will add keys of their own; reserve room for
      // them rather than deleting a draft the user just typed.
      if (pendingWrites.has(key)) continue;
      const read = readStored(storage, key);
      if (read.state === "unreadable") {
        // Its age is unknown, so it is never treated as the oldest. It still
        // counts against the cap, so the store cannot grow without bound.
        unreadable += 1;
        continue;
      }
      if (read.state === "corrupt") {
        forgetCorrupt(key);
        continue;
      }
      if (read.state === "missing") continue;
      entries.push({ key, updatedAt: read.value.updatedAt });
    }
    // A write storage refused is not taking up a slot: only queued writes
    // that can still land reserve room, so a full quota cannot evict a stored
    // draft for text that never made it into storage.
    let reserving = 0;
    for (const key of pendingWrites.keys()) {
      if (!failedWrites.has(key)) reserving += 1;
    }
    const allowed = Math.max(0, maxDrafts - reserving - unreadable);
    if (entries.length <= allowed) return;
    entries.sort((a, b) => a.updatedAt - b.updatedAt);
    for (const entry of entries.slice(0, entries.length - allowed)) {
      try {
        storage.removeItem(entry.key);
      } catch {
        // Leave it; the next flush tries again.
      }
      memory.delete(entry.key);
      pendingRemoves.delete(entry.key);
    }
  }

  /**
   * Forgets drafts nobody has touched in a month. Drafts are kept on this
   * device only, so there is no inventory to compare against: a thread may be
   * gone while a host (a WSL box, a child thread, a second machine with the
   * same host id) still needs the text, and deleting by inventory lost it.
   */
  function dropExpired(): void {
    const cutoff = now() - maxAgeMs;
    const keys = new Set<string>(memory.keys());
    if (storage) for (const key of storageDraftKeys(storage)) keys.add(key);
    for (const key of keys) {
      if (pendingWrites.has(key)) continue;
      let updatedAt = memory.get(key)?.updatedAt;
      if (storage) {
        const read = readStored(storage, key);
        // A storage throw means we cannot tell; leave the draft alone.
        if (read.state === "unreadable") continue;
        if (read.state === "corrupt") {
          forgetCorrupt(key);
          continue;
        }
        if (read.state === "missing") {
          memory.delete(key);
          continue;
        }
        updatedAt = read.value.updatedAt;
      }
      if (updatedAt === undefined || updatedAt === null) continue;
      if (updatedAt >= cutoff) continue;
      if (storage) {
        try {
          storage.removeItem(key);
        } catch {
          // If it cannot be removed now, the next load tries again.
          continue;
        }
      }
      memory.delete(key);
      pendingWrites.delete(key);
    }
  }

  /** Temporary ids never belong in storage; drop any a development build left. */
  function dropPersistedTemporaryIds(): void {
    if (!storage) return;
    for (const key of storageDraftKeys(storage)) {
      const target = parseDraftStorageKey(key);
      if (target?.scope !== "thread" || !isLocalThreadId(target.target)) continue;
      memory.delete(key);
      pendingWrites.delete(key);
      failedWrites.delete(key);
      try {
        storage.removeItem(key);
      } catch {
        // The next load tries again; the value is unreadable to us either way.
      }
    }
  }

  function load(): void {
    if (loaded) return;
    loaded = true;
    dropPersistedTemporaryIds();
    dropExpired();
    dropBehindCap();
  }

  function writeNow(): void {
    if (!storage) {
      pendingWrites.clear();
      pendingRemoves.clear();
      failedWrites.clear();
      dropBehindCap();
      return;
    }
    for (const [key, value] of [...pendingWrites]) {
      const read = readStored(storage, key);
      if (read.state === "ok") {
        // Storage may hold text written after this edit was queued. Compare
        // the two revisions rather than the queued write's base: a queued edit
        // that really is later must land instead of being dropped, and one
        // that is definitely older must give way to the other tab's text.
        const storedRevision = revisionOf(read.value);
        if (isStrictlyNewerRevision(storedRevision, revisionOf(value))) {
          memory.set(key, read.value);
          pendingWrites.delete(key);
          failedWrites.delete(key);
          adopted(key, read.value);
          continue;
        }
      }
      try {
        storage.setItem(key, JSON.stringify(value));
        memory.set(key, value);
        pendingWrites.delete(key);
        failedWrites.delete(key);
      } catch {
        // The value stays in memory and pending; it just must not hold a slot
        // in the cap, because storage never received it.
        failedWrites.add(key);
      }
    }
    for (const [key, expected] of [...pendingRemoves]) {
      const read = readStored(storage, key);
      // Cannot tell what is there; keep the removal pending.
      if (read.state === "unreadable") continue;
      if (read.state === "corrupt") {
        forgetCorrupt(key);
        pendingRemoves.delete(key);
        continue;
      }
      if (read.state === "ok" && !sameRevision(revisionOf(read.value), expected)) {
        // Another tab saved newer text after this removal was queued. That
        // text is not what was sent, so the removal is dropped.
        pendingRemoves.delete(key);
        continue;
      }
      try {
        storage.removeItem(key);
        memory.delete(key);
        pendingRemoves.delete(key);
        failedWrites.delete(key);
      } catch {
        // Keep it pending; the next flush retries while the revision matches.
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

  /** The text and revision for a slot, reading through to storage when it can. */
  function entry(scope: DraftScope, host: string, target: string): DraftEntry {
    load();
    const key = draftStorageKey(scope, host, target);
    if (pendingRemoves.has(key)) return { text: "", revision: null };
    if (scope === "thread" && isLocalThreadId(target)) {
      // Temporary ids are memory-only, so a missing storage key says nothing.
      const known = memory.get(key);
      return { text: known?.text ?? "", revision: known ? revisionOf(known) : null };
    }
    const pending = pendingWrites.get(key);
    if (pending) return { text: pending.text, revision: revisionOf(pending) };
    if (!storage) {
      const known = memory.get(key);
      return { text: known?.text ?? "", revision: known ? revisionOf(known) : null };
    }
    const read = readStored(storage, key);
    if (read.state === "unreadable") {
      const known = memory.get(key);
      return { text: known?.text ?? "", revision: known ? revisionOf(known) : null };
    }
    if (read.state === "corrupt") {
      forgetCorrupt(key);
      return { text: "", revision: null };
    }
    if (read.state === "missing") {
      memory.delete(key);
      return { text: "", revision: null };
    }
    memory.set(key, read.value);
    return { text: read.value.text, revision: revisionOf(read.value) };
  }

  /** The revision storage holds now, or null when it holds nothing usable. */
  function storedRevision(key: string): DraftRevision | null {
    if (!storage) return null;
    const read = readStored(storage, key);
    if (read.state === "ok") return revisionOf(read.value);
    if (read.state === "corrupt") forgetCorrupt(key);
    return null;
  }

  function write(
    scope: DraftScope,
    host: string,
    target: string,
    text: string,
  ): DraftRevision | null {
    load();
    const key = draftStorageKey(scope, host, target);
    if (text.length === 0) {
      // Emptied: remove the slot. This persists any pending text first, so
      // the removal is guarded against the revision storage really holds.
      remove(scope, host, target);
      return null;
    }
    if (scope === "thread" && isLocalThreadId(target)) {
      // The daemon is still starting this thread. Keep the text for the
      // composer, but never write a temporary id to storage: another session
      // could hand the same id to an unrelated thread later.
      const previous = memory.get(key)?.updatedAt ?? 0;
      const value: StoredDraft = {
        text,
        updatedAt: Math.max(now(), previous + 1),
        nonce,
      };
      memory.set(key, value);
      pendingWrites.delete(key);
      failedWrites.delete(key);
      pendingRemoves.delete(key);
      return revisionOf(value);
    }
    // A revision must change with every write, even two in the same
    // millisecond or two tabs writing at once, so a finish cannot clear a
    // draft saved just after Start. The nonce keeps two tabs apart.
    const previous = Math.max(
      memory.get(key)?.updatedAt ?? 0,
      pendingWrites.get(key)?.updatedAt ?? 0,
    );
    const base = storedRevision(key);
    const value: StoredDraft = {
      text,
      updatedAt: Math.max(now(), (base?.updatedAt ?? 0) + 1, previous + 1),
      nonce,
    };
    memory.set(key, value);
    pendingWrites.set(key, value);
    pendingRemoves.delete(key);
    schedule();
    return revisionOf(value);
  }

  /**
   * Removes a draft only while it is still the one that was submitted. A
   * message takes a moment to send, and another tab (or a folder switch while
   * Start waits) can save newer text in the meantime; that text must survive.
   */
  function removeIfRevision(
    scope: DraftScope,
    host: string,
    target: string,
    expected: DraftRevision | null,
  ): boolean {
    const key = draftStorageKey(scope, host, target);
    if (scope === "thread" && isLocalThreadId(target)) {
      const current = entry(scope, host, target).revision;
      if (!sameRevision(current, expected)) return false;
      if (current === null) return true;
      memory.delete(key);
      pendingWrites.delete(key);
      failedWrites.delete(key);
      pendingRemoves.delete(key);
      return true;
    }
    if (storage) {
      const read = readStored(storage, key);
      // Storage may already hold another tab's newer text. Flushing this
      // tab's queued write first would replace it, so drop that write
      // instead and clear nothing.
      if (read.state === "ok" && isNewerRevision(revisionOf(read.value), expected)) {
        const pending = pendingWrites.get(key);
        if (pending !== undefined && sameRevision(revisionOf(pending), expected)) {
          pendingWrites.delete(key);
          memory.set(key, read.value);
        }
        return false;
      }
    }
    // Write any pending text first: it is the revision this clear is about,
    // and the guard compares what storage holds. A pending write would leave
    // the older stored revision in place and make the guard fail.
    flush();
    const current = entry(scope, host, target).revision;
    if (!sameRevision(current, expected)) return false;
    if (current === null) return true;
    memory.delete(key);
    pendingWrites.delete(key);
    pendingRemoves.set(key, current);
    flush();
    return true;
  }

  function remove(scope: DraftScope, host: string, target: string): void {
    removeIfRevision(scope, host, target, entry(scope, host, target).revision);
  }

  function readThreadEntry(host: HostId, threadId: ThreadId): DraftEntry {
    return entry("thread", host, threadId);
  }

  function readThread(host: HostId, threadId: ThreadId): string {
    return readThreadEntry(host, threadId).text;
  }

  function writeThread(host: HostId, threadId: ThreadId, text: string): DraftRevision | null {
    return write("thread", host, threadId, text);
  }

  /**
   * The daemon named the optimistic thread. Carry whatever is still in its
   * box to the real id, and write it now: the user may reload before the
   * debounce fires.
   */
  function moveThreadDraft(host: HostId, fromThreadId: ThreadId, toThreadId: ThreadId): void {
    if (fromThreadId === toThreadId) return;
    load();
    const fromKey = draftStorageKey("thread", host, fromThreadId);
    const text = entry("thread", host, fromThreadId).text;
    memory.delete(fromKey);
    pendingWrites.delete(fromKey);
    failedWrites.delete(fromKey);
    pendingRemoves.delete(fromKey);
    if (text.length === 0) return;
    write("thread", host, toThreadId, text);
    flush();
  }

  function clearThread(host: HostId, threadId: ThreadId): void {
    remove("thread", host, threadId);
  }

  function threadRevision(host: HostId, threadId: ThreadId): DraftRevision | null {
    return readThreadEntry(host, threadId).revision;
  }

  function clearThreadIfRevision(
    host: HostId,
    threadId: ThreadId,
    expected: DraftRevision | null,
  ): boolean {
    return removeIfRevision("thread", host, threadId, expected);
  }

  function readNewThreadEntry(host: HostId, workspace: string): DraftEntry {
    return entry("new-thread", host, normalizeWorkspace(workspace));
  }

  function readNewThread(host: HostId, workspace: string): string {
    return readNewThreadEntry(host, workspace).text;
  }

  function writeNewThread(host: HostId, workspace: string, text: string): DraftRevision | null {
    return write("new-thread", host, normalizeWorkspace(workspace), text);
  }

  function clearNewThread(host: HostId, workspace: string): void {
    remove("new-thread", host, normalizeWorkspace(workspace));
  }

  function newThreadRevision(host: HostId, workspace: string): DraftRevision | null {
    return readNewThreadEntry(host, workspace).revision;
  }

  function clearNewThreadIfRevision(
    host: HostId,
    workspace: string,
    expected: DraftRevision | null,
  ): boolean {
    return removeIfRevision("new-thread", host, normalizeWorkspace(workspace), expected);
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
   * A `storage` event means another tab changed a draft. Adopt it only when
   * this tab has no pending write for the same key; otherwise the local edit
   * is the newer intent and is left alone.
   */
  function applyStorageEvent(key: string | null, newValue: string | null): DraftChange | null {
    if (key === null) return null;
    const target = parseDraftStorageKey(key);
    if (!target) return null;
    if (pendingWrites.has(key) || pendingRemoves.has(key)) return null;
    if (newValue === null) {
      memory.delete(key);
      return { ...target, text: "", revision: null };
    }
    const value = parseDraftValue(newValue);
    if (!value) {
      // The event may be stale: another tab can have repaired this key between
      // the bad write and this delivery. Re-read before deleting anything; a
      // valid value now on disk is what the composer should show.
      if (storage) {
        const current = readStored(storage, key);
        if (current.state === "ok") {
          memory.set(key, current.value);
          return { ...target, text: current.value.text, revision: revisionOf(current.value) };
        }
        if (current.state === "missing") memory.delete(key);
        if (current.state === "corrupt") forgetCorrupt(key);
        if (current.state === "unreadable") return null;
      } else {
        memory.delete(key);
      }
      return { ...target, text: "", revision: null };
    }
    memory.set(key, value);
    return { ...target, text: value.text, revision: revisionOf(value) };
  }

  return {
    readThreadDraft: readThread,
    readThreadDraftEntry: readThreadEntry,
    writeThreadDraft: writeThread,
    moveThreadDraft,
    clearThreadDraft: clearThread,
    readThreadDraftRevision: threadRevision,
    clearThreadDraftIfRevision: clearThreadIfRevision,
    readNewThreadDraft: readNewThread,
    readNewThreadDraftEntry: readNewThreadEntry,
    writeNewThreadDraft: writeNewThread,
    clearNewThreadDraft: clearNewThread,
    readNewThreadDraftRevision: newThreadRevision,
    clearNewThreadDraftIfRevision: clearNewThreadIfRevision,
    readLastWorkspace,
    writeLastWorkspace,
    applyStorageEvent,
    flush,
  };
}

const draftChangeListeners = new Set<(change: DraftChange) => void>();

function emitDraftChange(change: DraftChange): void {
  for (const listener of draftChangeListeners) listener(change);
}

/** Notifies the open composer when another tab changes one of its drafts. */
export function subscribeDraftChanges(listener: (change: DraftChange) => void): () => void {
  draftChangeListeners.add(listener);
  return () => {
    draftChangeListeners.delete(listener);
  };
}

export const drafts = createDraftStore(readBrowserStorage(), { onAdopt: emitDraftChange });

export const readThreadDraft = drafts.readThreadDraft;
export const readThreadDraftEntry = drafts.readThreadDraftEntry;
export const writeThreadDraft = drafts.writeThreadDraft;
export const moveLocalThreadDraft = drafts.moveThreadDraft;
export const clearThreadDraftIfRevision = drafts.clearThreadDraftIfRevision;
export const readNewThreadDraft = drafts.readNewThreadDraft;
export const readNewThreadDraftEntry = drafts.readNewThreadDraftEntry;
export const writeNewThreadDraft = drafts.writeNewThreadDraft;
export const flushDrafts = drafts.flush;

if (typeof window !== "undefined") {
  window.addEventListener("storage", (event) => {
    const change = drafts.applyStorageEvent(event.key, event.newValue);
    if (!change) return;
    emitDraftChange(change);
  });
  installDraftFlushHandlers(
    window,
    () => typeof document !== "undefined" && document.visibilityState === "hidden",
    () => flushDrafts(),
  );
}
