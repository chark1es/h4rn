import { beforeEach, describe, expect, it, vi } from "vitest";

const { client, requests, readyListeners } = vi.hoisted(() => {
  const requests: unknown[] = [];
  const readyListeners = new Set<(info: { reconnected: boolean }) => void>();
  const client = {
    request: async (request: unknown) => {
      requests.push(request);
      return { type: "ack" };
    },
    onReady: (listener: (info: { reconnected: boolean }) => void) => {
      readyListeners.add(listener);
      return () => readyListeners.delete(listener);
    },
  };
  return { client, requests, readyListeners };
});

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => client,
}));

import { retainWorkspaceWatch, watchedWorkspaces } from "./fsWatch";

function ready(reconnected: boolean): void {
  for (const listener of readyListeners) listener({ reconnected });
}

describe("workspace watch refcount", () => {
  beforeEach(() => {
    requests.length = 0;
  });

  it("watches once for two views and unwatches after the last release", () => {
    const ide = retainWorkspaceWatch("host_1", "/work");
    const preview = retainWorkspaceWatch("host_1", "/work");
    expect(requests).toEqual([{ type: "fs_watch", host: "host_1", workspace: "/work", paths: [] }]);

    ide();
    expect(requests).toHaveLength(1);

    preview();
    expect(requests.at(-1)).toEqual({ type: "fs_unwatch", workspace: "/work" });
  });

  it("is safe to release twice and to watch different workspaces", () => {
    const first = retainWorkspaceWatch("host_1", "/a");
    const second = retainWorkspaceWatch("host_1", "/b");
    first();
    first();
    expect(requests.filter((entry) => (entry as { type: string }).type === "fs_unwatch")).toEqual([
      { type: "fs_unwatch", workspace: "/a" },
    ]);
    second();
    expect(watchedWorkspaces()).toEqual([]);
  });

  it("starts a fresh watch after everyone left", () => {
    retainWorkspaceWatch("host_1", "/work")();
    retainWorkspaceWatch("host_1", "/work")();
    expect(requests.map((entry) => (entry as { type: string }).type)).toEqual([
      "fs_watch",
      "fs_unwatch",
      "fs_watch",
      "fs_unwatch",
    ]);
  });

  it("re-sends held watches after a reconnect, and only then", () => {
    const release = retainWorkspaceWatch("host_1", "/work");
    requests.length = 0;

    ready(false);
    expect(requests).toEqual([]);

    ready(true);
    expect(requests).toEqual([{ type: "fs_watch", host: "host_1", workspace: "/work", paths: [] }]);

    release();
    requests.length = 0;
    ready(true);
    expect(requests).toEqual([]);
  });

  it("subscribes to the ready notice once per client", () => {
    const first = retainWorkspaceWatch("host_1", "/a");
    const second = retainWorkspaceWatch("host_1", "/b");
    expect(readyListeners.size).toBe(1);
    first();
    second();
  });
});
