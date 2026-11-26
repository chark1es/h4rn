// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { workingTopLevelThreads } from "./devicePush";

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

function reconciles(invoke: Invoke): string[][] {
  return invoke.mock.calls
    .filter(([command]) => command === "push_reconcile_live")
    .map(([, args]) => (args as { threadIds: string[] }).threadIds);
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

describe("the running threads a Live Update may stay for", () => {
  it("lists only top-level threads that are working", () => {
    expect(
      workingTopLevelThreads({
        th_b: thread("working"),
        th_a: thread("working"),
        th_done: thread("done"),
        th_settled: thread("settled"),
        // A subagent's own turn has no Live Update of its own.
        th_child: thread("working", "th_a"),
      }),
    ).toEqual(["th_a", "th_b"]);
  });
});

describe("stale Live Updates are cleared", () => {
  it("on a cold start, from the first thread list", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();

    // Before the daemon answers, the empty list says nothing: clearing now
    // would remove entries for agents that are still running.
    expect(reconciles(invoke)).toEqual([]);

    // The phone still shows th_stopped from before the app was closed; it was
    // stopped from the desktop meanwhile, so the first list shows it Done.
    receiveThreadList(store, {
      th_stopped: thread("done"),
      th_running: thread("working"),
    });

    expect(reconciles(invoke)).toEqual([["th_running"]]);
  });

  it("after a grouped finish that named no thread", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_one: thread("working"), th_two: thread("working") });
    expect(reconciles(invoke).at(-1)).toEqual(["th_one", "th_two"]);

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

    expect(reconciles(invoke).at(-1)).toEqual([]);
  });

  it("keeps a queued turn that keeps its thread working", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_queue: thread("working") });
    const before = reconciles(invoke).length;

    // A new summary for the same thread, still Working: nothing to clear.
    const state = store.getState();
    store.setState({
      ...state,
      threads: { th_queue: { ...state.threads.th_queue } } as typeof state.threads,
    });

    expect(reconciles(invoke)).toHaveLength(before);
  });

  it("waits while the daemon is out of reach", async () => {
    const { store, startDevicePush } = await freshApp();
    startDevicePush();
    receiveThreadList(store, { th_one: thread("working") });
    const before = reconciles(invoke).length;

    const state = store.getState();
    store.setState({
      ...state,
      connection: { ...state.connection, status: "offline" },
      threads: {} as typeof state.threads,
    });

    expect(reconciles(invoke)).toHaveLength(before);
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
