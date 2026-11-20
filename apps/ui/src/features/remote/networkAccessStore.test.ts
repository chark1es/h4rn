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
  resetNetworkAccessStoreForTests,
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
});
