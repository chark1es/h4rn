import { beforeEach, describe, expect, it, vi } from "vitest";

const { appState } = vi.hoisted(() => ({
  appState: {
    current: null as unknown,
    changeListeners: new Set<(client: unknown) => void>(),
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => appState.current,
  onClientChange: (listener: (client: unknown) => void) => {
    appState.changeListeners.add(listener);
    return () => appState.changeListeners.delete(listener);
  },
}));

import { retainWorkspaceWatch, watchedWorkspaces } from "./fsWatch";

type ReadyListener = (info: { reconnected: boolean }) => void;

function makeClient() {
  const requests: unknown[] = [];
  const readyListeners = new Set<ReadyListener>();
  return {
    requests,
    request: async (request: unknown) => {
      requests.push(request);
      return { type: "ack" };
    },
    onReady: (listener: ReadyListener) => {
      readyListeners.add(listener);
      return () => readyListeners.delete(listener);
    },
    ready: (reconnected: boolean) => {
      for (const listener of readyListeners) listener({ reconnected });
    },
    listenerCount: () => readyListeners.size,
  };
}

type FakeClient = ReturnType<typeof makeClient>;

function ready(client: FakeClient, reconnected: boolean): void {
  client.ready(reconnected);
}

/** Replaces the store's client the way a sign-in or re-pair would. */
function replaceClient(): FakeClient {
  const client = makeClient();
  appState.current = client;
  for (const listener of appState.changeListeners) listener(client);
  return client;
}

describe("workspace watch refcount", () => {
  let client: FakeClient;

  beforeEach(() => {
    appState.current = makeClient();
    client = appState.current as FakeClient;
    client.requests.length = 0;
  });

  it("watches once for two views and unwatches after the last release", () => {
    const ide = retainWorkspaceWatch("host_1", "/work");
    const preview = retainWorkspaceWatch("host_1", "/work");
    expect(client.requests).toEqual([
      { type: "fs_watch", host: "host_1", workspace: "/work", paths: [] },
    ]);

    ide();
    expect(client.requests).toHaveLength(1);

    preview();
    expect(client.requests.at(-1)).toEqual({ type: "fs_unwatch", workspace: "/work" });
  });

  it("is safe to release twice and to watch different workspaces", () => {
    const first = retainWorkspaceWatch("host_1", "/a");
    const second = retainWorkspaceWatch("host_1", "/b");
    first();
    first();
    expect(
      client.requests.filter((entry) => (entry as { type: string }).type === "fs_unwatch"),
    ).toEqual([{ type: "fs_unwatch", workspace: "/a" }]);
    second();
    expect(watchedWorkspaces()).toEqual([]);
  });

  it("starts a fresh watch after everyone left", () => {
    retainWorkspaceWatch("host_1", "/work")();
    retainWorkspaceWatch("host_1", "/work")();
    expect(client.requests.map((entry) => (entry as { type: string }).type)).toEqual([
      "fs_watch",
      "fs_unwatch",
      "fs_watch",
      "fs_unwatch",
    ]);
  });

  it("re-sends held watches after a reconnect, and only then", () => {
    const release = retainWorkspaceWatch("host_1", "/work");
    client.requests.length = 0;

    ready(client, false);
    expect(client.requests).toEqual([]);

    ready(client, true);
    expect(client.requests).toEqual([
      { type: "fs_watch", host: "host_1", workspace: "/work", paths: [] },
    ]);

    release();
    client.requests.length = 0;
    ready(client, true);
    expect(client.requests).toEqual([]);
  });

  it("re-sends held watches on a replacement client", () => {
    const first = retainWorkspaceWatch("host_1", "/work");
    const replacement = replaceClient();
    expect(replacement.requests).toEqual([]);

    // A replacement client has never been sent the watch, so its first ready
    // notice counts even though the transport reports no reconnect.
    ready(replacement, false);
    expect(replacement.requests).toEqual([
      { type: "fs_watch", host: "host_1", workspace: "/work", paths: [] },
    ]);

    // A late notice on the replaced client must not send anything.
    client.requests.length = 0;
    ready(client, true);
    expect(client.requests).toEqual([]);

    // The held watch is released on the connection that carries it.
    first();
    expect(replacement.requests.at(-1)).toEqual({ type: "fs_unwatch", workspace: "/work" });
  });

  it("sends one watch per workspace after a client replacement", () => {
    const ide = retainWorkspaceWatch("host_1", "/work");
    const preview = retainWorkspaceWatch("host_1", "/work");
    const replacement = replaceClient();

    ready(replacement, false);
    expect(replacement.requests).toEqual([
      { type: "fs_watch", host: "host_1", workspace: "/work", paths: [] },
    ]);

    ide();
    preview();
    expect(watchedWorkspaces()).toEqual([]);
  });

  it("subscribes to the ready notice once per client", () => {
    const first = retainWorkspaceWatch("host_1", "/a");
    const second = retainWorkspaceWatch("host_1", "/b");
    expect(client.listenerCount()).toBe(1);
    first();
    second();

    const replacement = replaceClient();
    retainWorkspaceWatch("host_1", "/c")();
    expect(replacement.listenerCount()).toBe(1);
    expect(client.listenerCount()).toBe(0);
  });
});
