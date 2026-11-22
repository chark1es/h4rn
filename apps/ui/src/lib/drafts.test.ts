import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createDraftStore,
  DRAFTS_STORAGE_KEY,
  type DraftStorage,
  MAX_STORED_DRAFTS,
} from "./drafts";

type MemoryStorage = DraftStorage & {
  data: Map<string, string>;
};

function memoryStorage(initial?: string): MemoryStorage {
  const data = new Map<string, string>();
  if (initial !== undefined) data.set(DRAFTS_STORAGE_KEY, initial);
  return {
    data,
    getItem: (key) => data.get(key) ?? null,
    setItem: vi.fn((key: string, value: string) => {
      data.set(key, value);
    }),
  };
}

function payload(storage: DraftStorage): { version: number; drafts: unknown[] } {
  const raw = storage.getItem(DRAFTS_STORAGE_KEY);
  if (!raw) return { version: 0, drafts: [] };
  return JSON.parse(raw) as { version: number; drafts: unknown[] };
}

describe("draft store", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("persists a draft after the debounce, keyed by host and thread", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage, { now: () => 1 });

    store.writeThreadDraft("host_a", "thread_1", "hello");
    expect(storage.getItem(DRAFTS_STORAGE_KEY)).toBeNull();

    vi.advanceTimersByTime(299);
    expect(storage.getItem(DRAFTS_STORAGE_KEY)).toBeNull();
    vi.advanceTimersByTime(1);

    expect(payload(storage).drafts).toEqual([
      { scope: "thread", host: "host_a", target: "thread_1", text: "hello", updatedAt: 1 },
    ]);
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
    expect(storage.getItem(DRAFTS_STORAGE_KEY)).toContain("hello");
  });

  it("flushes immediately and cancels the pending timer", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "hello");
    store.flush();
    expect(storage.getItem(DRAFTS_STORAGE_KEY)).toContain("hello");

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
    store.clearThreadDraft("host_a", "thread_1");
    vi.advanceTimersByTime(300);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    expect(payload(storage).drafts).toEqual([]);
  });

  it("drops drafts for threads that no longer exist and keeps new-thread drafts", () => {
    const storage = memoryStorage();
    const store = createDraftStore(storage);

    store.writeThreadDraft("host_a", "thread_1", "live");
    store.writeThreadDraft("host_a", "thread_2", "deleted");
    store.writeThreadDraft("host_b", "thread_2", "also deleted");
    store.writeNewThreadDraft("host_a", "~/code", "new thread text");
    store.flush();

    store.pruneThreadDrafts([{ host: "host_a", threadId: "thread_1" }]);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("live");
    expect(store.readThreadDraft("host_a", "thread_2")).toBe("");
    expect(store.readThreadDraft("host_b", "thread_2")).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("new thread text");
  });

  it("caps the store at the oldest-first limit", () => {
    const storage = memoryStorage();
    let time = 0;
    const store = createDraftStore(storage, { maxDrafts: 3, now: () => ++time });

    store.writeThreadDraft("host_a", "t1", "one");
    store.writeThreadDraft("host_a", "t2", "two");
    store.writeThreadDraft("host_a", "t3", "three");
    store.writeThreadDraft("host_a", "t4", "four");
    store.flush();

    expect(payload(storage).drafts).toHaveLength(3);
    expect(store.readThreadDraft("host_a", "t1")).toBe("");
    expect(store.readThreadDraft("host_a", "t2")).toBe("two");
    expect(store.readThreadDraft("host_a", "t3")).toBe("three");
    expect(store.readThreadDraft("host_a", "t4")).toBe("four");
  });

  it("trims an over-cap store when it loads", () => {
    const drafts = Array.from({ length: MAX_STORED_DRAFTS + 5 }, (_, index) => ({
      scope: "thread",
      host: "host_a",
      target: `t${index}`,
      text: `draft ${index}`,
      updatedAt: index,
    }));
    const storage = memoryStorage(JSON.stringify({ version: 1, drafts }));
    const store = createDraftStore(storage);

    expect(store.readThreadDraft("host_a", "t0")).toBe("");
    expect(payload(storage).drafts).toHaveLength(MAX_STORED_DRAFTS);
  });

  it("keeps working when storage throws", () => {
    const storage: DraftStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
    };
    const store = createDraftStore(storage);

    expect(() => store.writeThreadDraft("host_a", "thread_1", "still here")).not.toThrow();
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("still here");
    expect(() => store.flush()).not.toThrow();
  });

  it("ignores corrupt storage and starts empty", () => {
    const storage = memoryStorage("not json");
    const store = createDraftStore(storage);

    expect(store.readThreadDraft("host_a", "thread_1")).toBe("");
    store.writeThreadDraft("host_a", "thread_1", "fresh");
    store.flush();
    expect(store.readThreadDraft("host_a", "thread_1")).toBe("fresh");
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
    expect(payload(storage).drafts).toEqual([]);
  });
});
