// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { stoppedThreads } from "./devicePush";

const ANDROID_USER_AGENT = "Mozilla/5.0 (Linux; Android 16; Pixel 9)";

type Invoke = ReturnType<typeof vi.fn>;

/**
 * A fresh copy of the push module and the stores it watches.
 *
 * `startDevicePush` runs once per page, so every case loads its own copy.
 */
async function freshApp() {
  vi.resetModules();
  const appStore = await import("@/lib/store/appStore");
  const devicePush = await import("./devicePush");
  return { store: appStore.store, startDevicePush: devicePush.startDevicePush };
}

type Store = Awaited<ReturnType<typeof freshApp>>["store"];

function thread(tab: string, parent: string | null = null) {
  return { tab, parent_thread_id: parent };
}

/** The daemon's thread list arriving over a live connection. */
function receiveThreadList(store: Store, threads: Record<string, ReturnType<typeof thread>>): void {
  const state = store.getState();
  store.setState({
    ...state,
    connection: { ...state.connection, status: "online" },
    threads: threads as unknown as typeof state.threads,
    lastSyncAt: (state.lastSyncAt ?? 0) + 1,
  });
}

/** The thread lists the shell was told to cancel, in order. */
function cancellations(invoke: Invoke): string[][] {
  return invoke.mock.calls
    .filter(([command]) => command === "push_reconcile_live")
    .map(([, args]) => (args as { stoppedThreadIds: string[] }).stoppedThreadIds);
}

let invoke: Invoke;

beforeEach(() => {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(ANDROID_USER_AGENT);
  invoke = vi.fn(async () => null);
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
});

afterEach(() => {
  window.localStorage.clear();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("the threads whose Live Update has ended", () => {
  it("are the ones this machine lists as not working", () => {
    expect(
      stoppedThreads({
        th_b: thread("done"),
        th_a: thread("settled"),
        th_running: thread("working"),
        th_child: thread("working", "th_a"),
      }),
    ).toEqual(["th_a", "th_b"]);
  });
});

describe("stale Live Updates are cleared", () => {
  it("on a cold start, from the first thread list", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();

    // Before the daemon answers, the empty list says nothing.
    expect(cancellations(invoke)).toEqual([]);

    // The phone still shows th_stopped from before the app was closed; it was
    // stopped from the desktop meanwhile, so the first list shows it Done.
    receiveThreadList(store, {
      th_stopped: thread("done"),
      th_running: thread("working"),
    });

    expect(cancellations(invoke)).toEqual([["th_stopped"]]);
  });

  it("after a grouped finish that named no thread", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_one: thread("working"), th_two: thread("working") });
    expect(cancellations(invoke).at(-1)).toEqual([]);

    // Both finished in one "2 agents finished" alert, which replaced neither
    // thread's ongoing entry.
    const state = store.getState();
    store.setState({
      ...state,
      threads: {
        th_one: { ...state.threads.th_one, tab: "done" },
        th_two: { ...state.threads.th_two, tab: "done" },
      } as typeof state.threads,
    });

    expect(cancellations(invoke).at(-1)).toEqual(["th_one", "th_two"]);
  });

  it("never for a thread this machine does not list", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();

    // th_elsewhere is running on another machine; its Live Update came once,
    // at its turn start, and would never come back if cancelled here.
    receiveThreadList(store, { th_here: thread("working") });
    receiveThreadList(store, { th_here: thread("done") });

    const cancelled = cancellations(invoke).flat();
    expect(cancelled).toContain("th_here");
    expect(cancelled).not.toContain("th_elsewhere");
    // The shell is only ever handed the threads to cancel, never a list of
    // the ones to keep, so it cannot treat a missing id as stopped.
    for (const [, args] of invoke.mock.calls.filter(([c]) => c === "push_reconcile_live")) {
      expect(Object.keys(args as object)).toEqual(["stoppedThreadIds"]);
    }
  });

  it("keeps a queued turn that keeps its thread working", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_queue: thread("working") });
    const before = cancellations(invoke).length;

    // A new summary for the same thread, still Working: nothing to clear.
    const state = store.getState();
    store.setState({
      ...state,
      threads: { th_queue: { ...state.threads.th_queue } } as typeof state.threads,
    });

    expect(cancellations(invoke)).toHaveLength(before);
    expect(cancellations(invoke).flat()).not.toContain("th_queue");
  });

  it("waits while the daemon is out of reach", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_one: thread("working") });
    const before = cancellations(invoke).length;

    const state = store.getState();
    store.setState({
      ...state,
      connection: { ...state.connection, status: "offline" },
      threads: { th_one: thread("done") } as unknown as typeof state.threads,
    });

    expect(cancellations(invoke)).toHaveLength(before);
  });
});

describe("the thread on screen", () => {
  it("is named again when the app comes back, so its Live Update goes away", async () => {
    const { store, startDevicePush } = await freshApp();
    const state = store.getState();
    store.setState({ ...state, ui: { ...state.ui, route: { kind: "thread", threadId: "th_x" } } });
    startDevicePush();
    const named = () =>
      invoke.mock.calls.filter(
        ([command, args]) =>
          command === "push_set_foreground_thread" &&
          (args as { threadId: string | null }).threadId === "th_x",
      ).length;
    expect(named()).toBe(1);

    // In the background, a working frame for th_x posted its Live Update. The
    // route never changed, so only the return to the app can clear it.
    document.dispatchEvent(new Event("visibilitychange"));

    expect(named()).toBe(2);
  });
});
