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
import { forgetMachine, getRemoteState, remoteStore } from "./remoteStore";

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
  remoteStore.setState({
    ...getRemoteState(),
    active: ACTIVE,
    activeMachineId: ACTIVE.machineId,
    machines: [],
  });
  host.restarts = 0;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("forgetMachine offline", () => {
  it("returns boot to the local transport even when no machines loaded", () => {
    // What an offline boot looks like: the descriptor is stored, the relay's
    // machine list never arrived, and every Disconnect affordance that read
    // `machines` is gone.
    expect(getRemoteState().machines).toEqual([]);
    expect(readBootConfig().remote).toBe(true);

    forgetMachine();

    expect(readActiveRemote()).toBeNull();
    expect(getRemoteState().active).toBeNull();
    expect(getRemoteState().activeMachineId).toBeNull();
    expect(host.restarts).toBe(1);
    const boot = readBootConfig();
    expect(boot.remote).toBe(false);
    // No stored secure pairing is left behind, so the rebuilt client dials the
    // plaintext loopback daemon instead of the remote Noise transport.
    expect(boot.secure).toBe(false);
  });
});
