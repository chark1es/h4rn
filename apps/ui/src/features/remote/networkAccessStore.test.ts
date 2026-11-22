import { beforeEach, describe, expect, it, vi } from "vitest";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
}));

import type { PairScope } from "@/gen";

import {
  cancelDirectPairing,
  createDirectPairing,
  getNetworkAccessState,
  loadDirectDevices,
  resetNetworkAccessStoreForTests,
  revokeDirectDevice,
  setNetworkAccess,
} from "./networkAccessStore";

const OFFER = {
  type: "direct_pairing_offer" as const,
  uri: "h4rn://pair?e=192.168.1.20%3A47810&d=ab&p=cd",
  expires_at: 1_800_000_000,
  addresses: ["192.168.1.20:47810"],
  scopes: ["view", "approve"] as PairScope[],
};

beforeEach(() => {
  resetNetworkAccessStoreForTests();
  request.mockReset();
});

describe("network access store", () => {
  it("turns the listener on and keeps the addresses", async () => {
    request.mockResolvedValueOnce({
      type: "network_access",
      enabled: true,
      listening: true,
      port: 47810,
      addresses: ["192.168.1.20:47810", "100.101.7.42:47810"],
      error: null,
    });

    expect(await setNetworkAccess(true)).toBe(true);
    expect(request).toHaveBeenCalledWith({ type: "set_network_access", enabled: true });
    expect(getNetworkAccessState().access).toEqual({
      enabled: true,
      listening: true,
      port: 47810,
      addresses: ["192.168.1.20:47810", "100.101.7.42:47810"],
      error: null,
    });
  });

  it("shows a bind failure without flipping to listening", async () => {
    request.mockResolvedValueOnce({
      type: "network_access",
      enabled: true,
      listening: false,
      port: 47810,
      addresses: [],
      error: "address already in use",
    });

    await setNetworkAccess(true);
    expect(getNetworkAccessState().access?.enabled).toBe(true);
    expect(getNetworkAccessState().access?.listening).toBe(false);
    expect(getNetworkAccessState().access?.error).toBe("address already in use");
  });

  it("opens a direct pairing with the chosen scopes", async () => {
    request.mockResolvedValueOnce({ ...OFFER, scopes: ["view", "approve", "terminal"] });

    const offer = await createDirectPairing(["view", "approve", "terminal"]);
    expect(request).toHaveBeenCalledWith({
      type: "create_direct_pairing",
      scopes: ["view", "approve", "terminal"],
      label: null,
    });
    expect(offer?.expiresAt).toBe(1_800_000_000_000);
    expect(getNetworkAccessState().offer?.uri).toBe(OFFER.uri);
  });

  it("cancels the open pairing on the daemon", async () => {
    request.mockResolvedValueOnce(OFFER);
    await createDirectPairing(["view", "approve"]);
    expect(getNetworkAccessState().offer).not.toBeNull();

    request.mockResolvedValueOnce({ type: "ack" });
    await cancelDirectPairing();
    expect(request).toHaveBeenLastCalledWith({ type: "cancel_direct_pairing" });
    expect(getNetworkAccessState().offer).toBeNull();
  });

  it("reports a rejected request instead of throwing", async () => {
    request.mockRejectedValueOnce(new Error("daemon is offline"));
    expect(await setNetworkAccess(true)).toBe(false);
    expect(getNetworkAccessState().error).toBe("daemon is offline");
    expect(getNetworkAccessState().saving).toBe(false);
  });

  it("lists directly paired devices and revokes one", async () => {
    const phone = {
      key: "aa".repeat(32),
      scopes: ["view", "approve"] as PairScope[],
      paired_at: "2026-11-18T10:00:00Z",
      connected: true,
    };
    const laptop = { ...phone, key: "bb".repeat(32), connected: false };
    request.mockResolvedValueOnce({ type: "direct_devices", devices: [phone, laptop] });
    await loadDirectDevices();
    expect(request).toHaveBeenCalledWith({ type: "list_direct_devices" });
    expect(getNetworkAccessState().devices).toEqual([phone, laptop]);

    request.mockResolvedValueOnce({ type: "direct_devices", devices: [laptop] });
    expect(await revokeDirectDevice(phone.key)).toBe(true);
    expect(request).toHaveBeenLastCalledWith({ type: "revoke_direct_device", key: phone.key });
    expect(getNetworkAccessState().devices).toEqual([laptop]);
    expect(getNetworkAccessState().revoking).toBeNull();
  });

  it("keeps the device and shows why when a revoke fails", async () => {
    const phone = {
      key: "aa".repeat(32),
      scopes: ["view"] as PairScope[],
      paired_at: null,
      connected: false,
    };
    request.mockResolvedValueOnce({ type: "direct_devices", devices: [phone] });
    await loadDirectDevices();
    request.mockRejectedValueOnce(new Error("no device paired directly with that key"));
    expect(await revokeDirectDevice(phone.key)).toBe(false);
    expect(getNetworkAccessState().devices).toEqual([phone]);
    expect(getNetworkAccessState().devicesError).toContain("no device paired directly");
  });
});
