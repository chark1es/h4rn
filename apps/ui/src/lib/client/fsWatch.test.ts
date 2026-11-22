import { beforeEach, describe, expect, it, vi } from "vitest";

const requests: unknown[] = [];

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({
    request: async (request: unknown) => {
      requests.push(request);
      return { type: "ack" };
    },
  }),
}));

import { retainWorkspaceWatch, watchedWorkspaces } from "./fsWatch";

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
});
