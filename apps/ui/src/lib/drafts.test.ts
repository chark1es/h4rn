import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createDraftStore,
  DRAFT_KEY_PREFIX,
  type DraftStorage,
  draftStorageKey,
  installDraftFlushHandlers,
  LEGACY_DRAFTS_STORAGE_KEY,
  MAX_STORED_DRAFTS,
} from "./drafts";

type MemoryStorage = DraftStorage & {
  data: Map<string, string>;
};

function memoryStorage(initial?: Record<string, string>, unreadable?: Set<string>): MemoryStorage {
  const data = new Map<string, string>(Object.entries(initial ?? {}));
  return {
    data,
    getItem: (key) => {
      if (unreadable?.has(key)) throw new Error("blocked");
      return data.get(key) ?? null;
    },
    setItem: vi.fn((key: string, value: string) => {
      data.set(key, value);
    }),
    removeItem: vi.fn((key: string) => {
      data.delete(key);
    }),
    key: (index: number) => [...data.keys()][index] ?? null,
    get length() {
      return data.size;
    },
  };
}

function threadKey(host: string, threadId: string): string {
  return draftStorageKey("thread", host, threadId);
}

function storedText(storage: MemoryStorage, key: string): string | null {
  const raw = storage.getItem(key);
  return raw === null ? null : (JSON.parse(raw) as { text: string }).text;
}

describe("draft store", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("persists a draft after the debounce, under its own key", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage, { now: () => 1 });

    store.writeThreadDraft("host_a", "thread_1", "hello");
    expect(storage.data.size).toBe(0);

    vi.advanceTimersByTime(299);
    expect(storage.data.size).toBe(0);
    vi.advanceTimersByTime(1);

    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("hello");
    expect(storage.getItem(LEGACY_DRAFTS_STORAGE_KEY)).toBeNull();
  });

  it("debounces a burst of edits into one write", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "h");
    vi.advanceTimersByTime(200);
    store.writeThreadDraft("host_a", "thread_1", "he");
    vi.advanceTimersByTime(200);
    store.writeThreadDraft("host_a", "thread_1", "hello");
    vi.advanceTimersByTime(299);
    expect(storage.setItem).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);

    expect(storage.setItem).toHaveBeenCalledTimes(1);
    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("hello");
  });

  it("flushes immediately and cancels the pending timer", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "hello");
    store.flush();
    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("hello");

    vi.advanceTimersByTime(300);
    expect(storage.setItem).toHaveBeenCalledTimes(1);
  });

  it("keeps the same thread id on two hosts apart", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "on a");
    store.writeThreadDraft("host_b", "thread_1", "on b");
    store.flush();

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("on a");
    expect(store.readThreadDraft("host_b", "thread_1")).toBe("on b");
  });

  it("clears a draft without a pending write resurrecting it", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "send me");
    store.flush();
    store.writeThreadDraft("host_a", "thread_1", "newer but unsent");
    store.clearThreadDraft("host_a", "thread_1");
    vi.advanceTimersByTime(300);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    expect(storage.data.size).toBe(0);
  });

  it("drops drafts older than 30 days at load, and keeps the rest", () => {
    const day = 24 * 60 * 60 * 1000;
    const storage = memoryStorage({
      [threadKey("host_a", "old_thread")]: JSON.stringify({ text: "old", updatedAt: 0 }),
      [draftStorageKey("new-thread", "host_a", "~/code")]: JSON.stringify({
        text: "old prompt",
        updatedAt: 0,
      }),
      [threadKey("host_a", "recent_thread")]: JSON.stringify({
        text: "recent",
        updatedAt: 29 * day,
      }),
    });
    const store = createDraftStore(storage, { now: () => 31 * day });

    expect(store.readThreadDraft("host_a", "recent_thread")).toBe("recent");
    expect(store.readThreadDraft("host_a", "old_thread")).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("");
    expect(storage.data.size).toBe(1);
  });

  it("expires drafts by age, not by which threads a daemon lists", () => {
    const day = 24 * 60 * 60 * 1000;
    const storage = memoryStorage({
      [threadKey("wsl_host", "child_thread")]: JSON.stringify({
        text: "child draft",
        updatedAt: 2 * day,
      }),
      [threadKey("same_id_host", "shared_thread")]: JSON.stringify({
        text: "other machine",
        updatedAt: 2 * day,
      }),
    });
    const store = createDraftStore(storage, { now: () => 3 * day });

    expect(store.readThreadDraft("wsl_host", "child_thread")).toBe("child draft");
    expect(store.readThreadDraft("same_id_host", "shared_thread")).toBe("other machine");
  });

  it("clears a draft only while it is still the revision that was submitted", () => {
    const storage = memoryStorage();
    let time = 100;
    const store = createDraftStore(storage, { now: () => time });
    store.writeThreadDraft("host_a", "thread_1", "submitted");
    store.flush();
    const submitted = store.readThreadDraftRevision("host_a", "thread_1");

    time = 200;
    store.writeThreadDraft("host_a", "thread_1", "newer from another tab");
    store.flush();

    expect(store.clearThreadDraftIfRevision("host_a", "thread_1", submitted)).toBe(false);
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("newer from another tab");

    const newer = store.readThreadDraftRevision("host_a", "thread_1");
    expect(store.clearThreadDraftIfRevision("host_a", "thread_1", newer)).toBe(true);
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
  });

  it("keeps another tab's newer draft when a clear would flush an older write", () => {
    const storage = memoryStorage();
    const tabA = createDraftStore(storage, { now: () => 5, nonce: "tab-a" });
    const tabB = createDraftStore(storage, { now: () => 5, nonce: "tab-b" });

    // A's message starts before its debounce fires.
    const submitted = tabA.writeThreadDraft("host_a", "thread_1", "sent from A");
    // B saves newer text for the same slot while the send waits.
    tabB.writeThreadDraft("host_a", "thread_1", "newer from B");
    tabB.flush();

    expect(tabA.clearThreadDraftIfRevision("host_a", "thread_1", submitted)).toBe(false);
    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("newer from B");

    // The queued write is dropped, not flushed over the newer text later.
    tabA.flush();
    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("newer from B");
    expect(tabB.readThreadDraft("host_a", "thread_1")).toBe("newer from B");
  });

  it("does not let a debounced write replace a revision it never saw", () => {
    const storage = memoryStorage();
    const tabA = createDraftStore(storage, { now: () => 5, nonce: "tab-a" });
    const tabB = createDraftStore(storage, { now: () => 5, nonce: "tab-b" });

    tabA.writeThreadDraft("host_a", "thread_1", "older from A");
    tabB.writeThreadDraft("host_a", "thread_1", "newer from B");
    tabB.flush();

    tabA.flush();

    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("newer from B");
    expect(tabA.readThreadDraft("host_a", "thread_1")).toBe("newer from B");
  });

  it("reads the text and its revision together", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage, { now: () => 42, nonce: "tab-a" });
    store.writeThreadDraft("host_a", "thread_1", "loaded");
    store.flush();

    const fresh = createDraftStore(storage, { now: () => 42 });
    expect(fresh.readThreadDraftEntry("host_a", "thread_1")).toEqual({
      text: "loaded",
      revision: { updatedAt: 42, nonce: "tab-a" },
    });
  });

  it("gives every write its own revision, even across tabs in one millisecond", () => {
    const storage = memoryStorage();
    const tabA = createDraftStore(storage, { now: () => 5, nonce: "tab-a" });
    const tabB = createDraftStore(storage, { now: () => 5, nonce: "tab-b" });

    const first = tabA.writeThreadDraft("host_a", "thread_1", "first");
    tabA.flush();
    const second = tabB.writeThreadDraft("host_a", "thread_1", "second");
    tabB.flush();

    expect(first?.nonce).toBe("tab-a");
    expect(second?.nonce).toBe("tab-b");
    expect(second?.updatedAt).toBeGreaterThan(first?.updatedAt ?? 0);
    expect(tabA.readThreadDraftRevision("host_a", "thread_1")).toEqual(second);
    expect(tabA.readThreadDraft("host_a", "thread_1")).toBe("second");
  });

  it("advances the revision within the same millisecond", () => {
    const store = createDraftStore(memoryStorage(), { now: () => 7, nonce: "tab-a" });
    const first = store.writeThreadDraft("host_a", "thread_1", "one");
    const second = store.writeThreadDraft("host_a", "thread_1", "one more");

    expect(second?.updatedAt).toBeGreaterThan(first?.updatedAt ?? 0);
    expect(second?.nonce).toBe("tab-a");
  });

  it("compares both halves of a revision before clearing", () => {
    const store = createDraftStore(memoryStorage(), { now: () => 10, nonce: "tab-a" });
    store.writeThreadDraft("host_a", "thread_1", "one");
    store.flush();

    expect(
      store.clearThreadDraftIfRevision("host_a", "thread_1", { updatedAt: 10, nonce: "tab-b" }),
    ).toBe(false);
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("one");
  });

  it("retries a failed removal only while the stored revision still matches", () => {
    const key = threadKey("host_a", "thread_1");
    const data = new Map<string, string>();
    let failRemove = false;
    const storage: DraftStorage = {
      getItem: (current) => data.get(current) ?? null,
      setItem: (current, value) => {
        data.set(current, value);
      },
      removeItem: (current) => {
        if (failRemove) throw new Error("blocked");
        data.delete(current);
      },
      key: (index) => [...data.keys()][index] ?? null,
      get length() {
        return data.size;
      },
    };
    const store = createDraftStore(storage, { nonce: "tab-a" });
    store.writeThreadDraft("host_a", "thread_1", "sent");
    store.flush();
    const revision = store.readThreadDraftRevision("host_a", "thread_1");

    failRemove = true;
    expect(store.clearThreadDraftIfRevision("host_a", "thread_1", revision)).toBe(true);
    expect(data.has(key)).toBe(true);

    // Another tab saves newer text before the removal can be retried.
    data.set(key, JSON.stringify({ text: "newer", updatedAt: 9_999_999_999, nonce: "tab-b" }));
    failRemove = false;
    store.flush();

    expect(data.has(key)).toBe(true);
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("newer");
  });

  it("clears only the submitted folder's new-thread draft", () => {
    const storage = memoryStorage();
    let time = 100;
    const store = createDraftStore(storage, { now: () => time });
    store.writeNewThreadDraft("host_a", "~/code/api", "api work");
    store.flush();
    const submitted = store.readNewThreadDraftRevision("host_a", "~/code/api");

    time = 200;
    store.writeNewThreadDraft("host_a", "~/code/web", "web work");
    store.flush();

    expect(store.clearNewThreadDraftIfRevision("host_a", "~/code/api", submitted)).toBe(true);
    expect(store.readNewThreadDraft("host_a", "~/code/api")).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code/web")).toBe("web work");
  });

  it("caps the store by removing the oldest draft keys", () => {
    const storage = memoryStorage();
    let time = 0;
    const store = createDraftStore(storage, { maxDrafts: 3, now: () => ++time });

    store.writeThreadDraft("host_a", "t1", "one");
    store.writeThreadDraft("host_a", "t2", "two");
    store.writeThreadDraft("host_a", "t3", "three");
    store.writeThreadDraft("host_a", "t4", "four");
    store.flush();

    const keys = [...storage.data.keys()].filter((key) => key.startsWith(DRAFT_KEY_PREFIX));
    expect(keys).toHaveLength(3);
    expect(store.readThreadDraft("host_a", "t1")).toBe("");
    expect(store.readThreadDraft("host_a", "t2")).toBe("two");
    expect(store.readThreadDraft("host_a", "t3")).toBe("three");
    expect(store.readThreadDraft("host_a", "t4")).toBe("four");
  });

  it("trims an over-cap store when it loads", () => {
    const base = 1_700_000_000_000;
    const initial: Record<string, string> = {};
    for (let index = 0; index < MAX_STORED_DRAFTS + 5; index += 1) {
      initial[threadKey("host_a", `t${index}`)] = JSON.stringify({
        text: `draft ${index}`,
        updatedAt: base + index,
      });
    }
    const storage = memoryStorage(initial);
    const store = createDraftStore(storage, { now: () => base + MAX_STORED_DRAFTS + 10 });

    expect(store.readThreadDraft("host_a", "t0")).toBe("");
    expect([...storage.data.keys()].filter((key) => key.startsWith(DRAFT_KEY_PREFIX))).toHaveLength(
      MAX_STORED_DRAFTS,
    );
  });

  it("keeps working when storage throws", () => {
    const storage: DraftStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
      removeItem: () => {
        throw new Error("blocked");
      },
      key: () => {
        throw new Error("blocked");
      },
      get length(): number {
        throw new Error("blocked");
      },
    };
    const store = createDraftStore(storage);

    expect(() => store.writeThreadDraft("host_a", "thread_1", "still here")).not.toThrow();
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("still here");
    expect(() => store.flush()).not.toThrow();
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("still here");
  });

  it("ignores corrupt values, and drops them from storage", () => {
    const key = threadKey("host_a", "thread_1");
    const storage = memoryStorage({ [key]: "not json" });
    const store = createDraftStore(storage);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    expect(storage.data.has(key)).toBe(false);
    store.writeThreadDraft("host_a", "thread_1", "fresh");
    store.flush();
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("fresh");
  });

  it("does not evict a draft whose age it could not read", () => {
    const base = 1_000;
    const unreadable = threadKey("host_a", "unreadable");
    const storage = memoryStorage(
      {
        [unreadable]: JSON.stringify({ text: "keep", updatedAt: base }),
        [threadKey("host_a", "old")]: JSON.stringify({ text: "old", updatedAt: base + 1 }),
        [threadKey("host_a", "new")]: JSON.stringify({ text: "new", updatedAt: base + 2 }),
      },
      new Set([unreadable]),
    );
    createDraftStore(storage, { maxDrafts: 2, now: () => base + 100 }).readThreadDraft(
      "host_a",
      "new",
    );

    // Unknown age means it is not treated as the oldest; the oldest readable
    // draft goes instead.
    expect(storage.data.has(unreadable)).toBe(true);
    expect(storage.data.has(threadKey("host_a", "old"))).toBe(false);
    expect(storage.data.has(threadKey("host_a", "new"))).toBe(true);
  });

  it("works without storage, keeping drafts in memory", () => {
    const store = createDraftStore(null);

    store.writeThreadDraft("host_a", "thread_1", "memory only");
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("memory only");
    expect(() => store.flush()).not.toThrow();
  });

  it("removes a draft when its text becomes empty", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "hello");
    store.flush();
    store.writeThreadDraft("host_a", "thread_1", "");
    store.flush();

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    expect(storage.data.size).toBe(0);
  });

  it("does not resurrect an older stored draft when the box is emptied", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "first");
    store.flush();
    store.writeThreadDraft("host_a", "thread_1", "first and more");
    store.writeThreadDraft("host_a", "thread_1", "");
    vi.advanceTimersByTime(300);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    expect(storage.data.size).toBe(0);
  });

  it("migrates the old single-blob key, then deletes it", () => {
    const now = Date.now();
    const legacy = JSON.stringify({
      version: 1,
      drafts: [
        {
          scope: "thread",
          host: "host_a",
          target: "thread_1",
          text: "old draft",
          updatedAt: now - 1000,
        },
        {
          scope: "new-thread",
          host: "host_a",
          target: "~/code",
          text: "old prompt",
          updatedAt: now - 500,
        },
      ],
    });
    const storage = memoryStorage({ [LEGACY_DRAFTS_STORAGE_KEY]: legacy });
    const store = createDraftStore(storage);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("old draft");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("old prompt");
    expect(storage.getItem(LEGACY_DRAFTS_STORAGE_KEY)).toBeNull();
    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("old draft");
  });

  it("does not overwrite a newer per-key draft during migration", () => {
    const key = threadKey("host_a", "thread_1");
    const now = Date.now();
    const storage = memoryStorage({
      [LEGACY_DRAFTS_STORAGE_KEY]: JSON.stringify({
        version: 1,
        drafts: [
          {
            scope: "thread",
            host: "host_a",
            target: "thread_1",
            text: "older",
            updatedAt: now - 2000,
          },
        ],
      }),
      [key]: JSON.stringify({ text: "newer", updatedAt: now - 1000 }),
    });
    const store = createDraftStore(storage);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("newer");
  });

  it("lets two tabs write their own drafts without erasing each other", () => {
    const storage = memoryStorage();
    const tabA = createDraftStore(storage, { now: () => 1 });
    const tabB = createDraftStore(storage, { now: () => 2 });

    tabA.writeThreadDraft("host_a", "thread_1", "tab A");
    tabA.flush();
    tabB.writeThreadDraft("host_a", "thread_2", "tab B");
    tabB.flush();

    expect(storedText(storage, threadKey("host_a", "thread_1"))).toBe("tab A");
    expect(storedText(storage, threadKey("host_a", "thread_2"))).toBe("tab B");
    // Each tab reads through to storage, so both see both drafts.
    expect(tabB.readThreadDraft("host_a", "thread_1")).toBe("tab A");
    expect(tabA.readThreadDraft("host_a", "thread_2")).toBe("tab B");
  });

  it("flushes only the keys this tab changed", () => {
    const storage = memoryStorage();
    const tabA = createDraftStore(storage);
    tabA.writeThreadDraft("host_a", "thread_1", "tab A");
    tabA.flush();
    vi.mocked(storage.setItem).mockClear();

    const tabB = createDraftStore(storage);
    tabB.writeThreadDraft("host_a", "thread_2", "tab B");
    tabB.flush();

    expect(storage.setItem).toHaveBeenCalledTimes(1);
    expect(storage.setItem).toHaveBeenCalledWith(
      threadKey("host_a", "thread_2"),
      expect.stringContaining("tab B"),
    );
  });

  it("adopts another tab's change unless this tab has an unsaved edit", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);
    const key = threadKey("host_a", "thread_1");

    store.writeThreadDraft("host_a", "thread_1", "local saved");
    store.flush();

    const remote = JSON.stringify({ text: "from the other tab", updatedAt: 4 });
    storage.data.set(key, remote);

    expect(store.applyStorageEvent(key, remote)?.text).toBe("from the other tab");
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("from the other tab");

    store.writeThreadDraft("host_a", "thread_1", "local, being typed");
    expect(store.applyStorageEvent(key, JSON.stringify({ text: "remote", updatedAt: 5 }))).toBe(
      null,
    );
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("local, being typed");
  });

  it("removes a corrupt value reported by another tab", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);
    const key = threadKey("host_a", "thread_1");
    storage.data.set(key, "not json");

    const change = store.applyStorageEvent(key, "not json");

    expect(change).toEqual({
      scope: "thread",
      host: "host_a",
      target: "thread_1",
      text: "",
      revision: null,
    });
    expect(storage.data.has(key)).toBe(false);
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
  });

  it("clears the open composer when another tab removes the draft", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);
    store.writeThreadDraft("host_a", "thread_1", "saved");
    store.flush();

    storage.data.delete(threadKey("host_a", "thread_1"));
    const change = store.applyStorageEvent(threadKey("host_a", "thread_1"), null);

    expect(change?.text).toBe("");
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
  });

  it("remembers the last folder per host", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeLastWorkspace("host_a", "  ~/code/api  ");
    store.writeLastWorkspace("host_b", "~/work");

    expect(store.readLastWorkspace("host_a")).toBe("~/code/api");
    expect(store.readLastWorkspace("host_b")).toBe("~/work");
    expect(store.readLastWorkspace("host_c")).toBe("");
  });
});

describe("draft flush wiring", () => {
  function fakeTarget() {
    const listeners = new Map<string, Set<() => void>>();
    return {
      addEventListener: vi.fn((type: string, listener: () => void) => {
        const set = listeners.get(type) ?? new Set();
        set.add(listener);
        listeners.set(type, set);
      }),
      removeEventListener: vi.fn((type: string, listener: () => void) => {
        listeners.get(type)?.delete(listener);
      }),
      fire(type: string) {
        for (const listener of [...(listeners.get(type) ?? [])]) listener();
      },
      count(type: string) {
        return listeners.get(type)?.size ?? 0;
      },
    };
  }

  it("flushes on pagehide and on visibilitychange only when hidden", () => {
    const target = fakeTarget();
    const flush = vi.fn();
    let hidden = false;
    installDraftFlushHandlers(target, () => hidden, flush);

    target.fire("visibilitychange");
    expect(flush).not.toHaveBeenCalled();

    hidden = true;
    target.fire("visibilitychange");
    expect(flush).toHaveBeenCalledTimes(1);

    target.fire("pagehide");
    expect(flush).toHaveBeenCalledTimes(2);
  });

  it("removes both listeners when torn down", () => {
    const target = fakeTarget();
    const stop = installDraftFlushHandlers(target, () => true, vi.fn());
    expect(target.count("pagehide")).toBe(1);
    expect(target.count("visibilitychange")).toBe(1);

    stop();

    expect(target.count("pagehide")).toBe(0);
    expect(target.count("visibilitychange")).toBe(0);
  });
});
