import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  type ActiveRemote,
  clearActiveRemote,
  pathsFor,
  readActiveRemote,
  writeActiveRemote,
} from "./active";

function memoryStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key) => map.get(key) ?? null,
    key: (index) => [...map.keys()][index] ?? null,
    removeItem: (key) => {
      map.delete(key);
    },
    setItem: (key, value) => {
      map.set(key, value);
    },
  };
}

const ACTIVE: ActiveRemote = {
  connectUrl: "https://connect.example",
  machineId: "mac_1",
  machineName: "atlas",
  daemonNoiseKey: "ab".repeat(32),
  addresses: ["100.101.5.4:47800", "atlas.tailnet.ts.net", "relay:https://connect.example"],
  online: true,
  relay: true,
};

beforeEach(() => {
  vi.stubGlobal("window", { localStorage: memoryStorage() });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("active remote descriptor", () => {
  it("round-trips through localStorage without a token", () => {
    writeActiveRemote(ACTIVE);
    expect(readActiveRemote()).toEqual(ACTIVE);
    expect(window.localStorage.getItem("h4rn.remote.active")).not.toContain("token");
    clearActiveRemote();
    expect(readActiveRemote()).toBeNull();
  });

  it("fills defaults and rejects incomplete records", () => {
    window.localStorage.setItem(
      "h4rn.remote.active",
      JSON.stringify({ connectUrl: "https://c", machineId: "m", daemonNoiseKey: "k" }),
    );
    expect(readActiveRemote()).toMatchObject({
      machineName: "m",
      addresses: [],
      online: true,
      relay: true,
    });

    window.localStorage.setItem("h4rn.remote.active", JSON.stringify({ machineId: "m" }));
    expect(readActiveRemote()).toBeNull();

    window.localStorage.setItem("h4rn.remote.active", "{not json");
    expect(readActiveRemote()).toBeNull();
  });

  it("treats a locked storage as absent", () => {
    vi.stubGlobal("window", {
      get localStorage(): Storage {
        throw new Error("denied");
      },
    });
    expect(readActiveRemote()).toBeNull();
    expect(() => writeActiveRemote(ACTIVE)).not.toThrow();
  });
});

describe("pathsFor", () => {
  it("orders dialable candidates before the relay fallback", () => {
    expect(pathsFor(ACTIVE)).toEqual([
      { kind: "tailscale", endpoint: "ws://100.101.5.4:47800/secure" },
      { kind: "tailscale", endpoint: "ws://atlas.tailnet.ts.net/secure" },
      { kind: "relay", endpoint: "relay" },
    ]);
  });

  it("dedupes equivalent endpoints and skips empty entries", () => {
    const paths = pathsFor({
      ...ACTIVE,
      addresses: ["192.168.1.8:47800", "ws://192.168.1.8:47800/secure", "  "],
      relay: false,
    });
    expect(paths).toEqual([{ kind: "direct", endpoint: "ws://192.168.1.8:47800/secure" }]);
  });

  it("adds the relay path when a relay marker is present, even without the flag", () => {
    const paths = pathsFor({ ...ACTIVE, relay: false, addresses: ["relay:https://c"] });
    expect(paths).toEqual([{ kind: "relay", endpoint: "relay" }]);
  });

  it("skips candidates that are not URLs", () => {
    const paths = pathsFor({
      ...ACTIVE,
      addresses: ["http://[broken"],
      relay: false,
    });
    expect(paths).toEqual([]);
  });
});
