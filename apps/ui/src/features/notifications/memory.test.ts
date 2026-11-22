import { describe, expect, it } from "vitest";

import { ERROR_MEMORY_MS, NotificationMemory } from "./memory";

describe("notifier memory", () => {
  it("explains a failed turn with the error that preceded it", () => {
    const memory = new NotificationMemory();
    memory.rememberError("thr_1", "the provider rejected the model", 1_000);
    memory.completeTurn("thr_1", "failed", 1_100);
    expect(memory.lastTurn("thr_1")).toEqual({
      status: "failed",
      error: "the provider rejected the model",
    });
  });

  it("forgets an older error when the thread starts a new turn", () => {
    const memory = new NotificationMemory();
    memory.rememberError("thr_1", "the provider rejected the model", 1_000);
    memory.completeTurn("thr_1", "failed", 1_100);
    memory.startTurn("thr_1");

    memory.completeTurn("thr_1", "failed", 2_000);
    expect(memory.lastTurn("thr_1")).toEqual({ status: "failed", error: null });
  });

  it("stops using an error after the memory window", () => {
    const memory = new NotificationMemory();
    memory.rememberError("thr_1", "old failure", 1_000);
    expect(memory.lastError("thr_1", 1_000 + ERROR_MEMORY_MS)).toBe("old failure");
    expect(memory.lastError("thr_1", 1_001 + ERROR_MEMORY_MS)).toBeNull();
  });

  it("only suppresses the Done repeat for the same turn", () => {
    const memory = new NotificationMemory();
    memory.announceFailure("thr_1");
    expect(memory.failureAnnounced("thr_1")).toBe(true);
    memory.startTurn("thr_1");
    expect(memory.failureAnnounced("thr_1")).toBe(false);
  });

  it("remembers a live completion until it is announced", () => {
    const memory = new NotificationMemory();
    memory.completeTurn("thr_1", "completed", 1_000);
    expect(memory.hasLiveCompletion("thr_1")).toBe(true);
    memory.announceFinished("thr_1");
    expect(memory.hasLiveCompletion("thr_1")).toBe(false);
  });

  it("does not treat a replayed completion as a live announcement", () => {
    const memory = new NotificationMemory();
    memory.completeTurn("thr_1", "completed", 1_000, false);
    expect(memory.lastTurn("thr_1")).toEqual({ status: "completed", error: null });
    expect(memory.hasLiveCompletion("thr_1")).toBe(false);
  });

  it("forgets a live completion when a new turn starts", () => {
    const memory = new NotificationMemory();
    memory.completeTurn("thr_1", "completed", 1_000);
    memory.startTurn("thr_1");
    expect(memory.hasLiveCompletion("thr_1")).toBe(false);
  });

  it("forgets every live completion when the baseline is re-seeded", () => {
    const memory = new NotificationMemory();
    memory.completeTurn("thr_1", "completed", 1_000);
    memory.completeTurn("thr_2", "completed", 1_000);
    memory.clearCompletions();
    expect(memory.hasLiveCompletion("thr_1")).toBe(false);
    expect(memory.hasLiveCompletion("thr_2")).toBe(false);
    // The wording for a later turn survives; only the permission to notify
    // from a pre-baseline completion is gone.
    expect(memory.lastTurn("thr_1")).toEqual({ status: "completed", error: null });
  });
});
