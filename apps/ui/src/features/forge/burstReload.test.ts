import { describe, expect, it, vi } from "vitest";

import { createBurstReload } from "./burstReload";

const flushMicrotasks = () => Promise.resolve();

describe("createBurstReload", () => {
  it("schedules one reload for a whole replay burst", async () => {
    const reload = vi.fn();
    const burstReload = createBurstReload(reload);

    // A reconnect replays every missed event in one synchronous burst.
    burstReload();
    burstReload();
    burstReload();

    expect(reload).not.toHaveBeenCalled();
    await flushMicrotasks();
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("reads once for each burst in a separate task", async () => {
    const reload = vi.fn();
    const burstReload = createBurstReload(reload);

    burstReload();
    burstReload();
    await flushMicrotasks();
    expect(reload).toHaveBeenCalledTimes(1);

    // A second reconnect is a new burst, not more of the first.
    burstReload();
    burstReload();
    await flushMicrotasks();
    expect(reload).toHaveBeenCalledTimes(2);
  });
});
