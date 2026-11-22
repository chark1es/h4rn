import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const host = vi.hoisted(() => ({
  restarts: 0,
}));

vi.mock("@/lib/store/appStore", () => ({
  pushToast: vi.fn(),
  navigate: vi.fn(),
  getState: () => ({ connection: { status: "offline" } }),
  restartConnection: () => {
    host.restarts += 1;
  },
}));

import { readBootConfig } from "@/lib/client/config";

import { type ActiveRemote, readActiveRemote, writeActiveRemote } from "./active";
import { getRemoteState, signOut } from "./remoteStore";
import { setConnectClient } from "./session";

const ACTIVE: ActiveRemote = {
  connectUrl: "https://connect.example",
  machineId: "mac_1",
  machineName: "atlas",
  daemonNoiseKey: "ab".repeat(32),
  addresses: ["100.101.5.4:47800", "relay:https://connect.example"],
  online: true,
  relay: true,
};

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

beforeEach(() => {
  vi.stubGlobal("window", {
    localStorage: memoryStorage(),
    location: { search: "" },
  });
  writeActiveRemote(ACTIVE);
  host.restarts = 0;
});

afterEach(() => {
  setConnectClient(null);
  vi.unstubAllGlobals();
});

describe("signOut", () => {
  it("returns to local before a hung relay revocation can answer", async () => {
    let phaseWhenRevoked: string | null = null;
    // A relay request that never settles and ignores aborts: the worst case
    // for the old code, which awaited it before local cleanup.
    vi.stubGlobal("fetch", () => {
      phaseWhenRevoked = getRemoteState().phase;
      return new Promise<Response>(() => {});
    });
    setConnectClient({
      revokeSession: () => fetch("https://connect.example/v1/auth/logout"),
    } as never);

    await signOut();

    expect(phaseWhenRevoked).toBe("signed_out");
    expect(readActiveRemote()).toBeNull();
    expect(getRemoteState().active).toBeNull();
    expect(getRemoteState().phase).toBe("signed_out");
    expect(host.restarts).toBe(1);
    expect(readBootConfig().remote).toBe(false);
  });
});
