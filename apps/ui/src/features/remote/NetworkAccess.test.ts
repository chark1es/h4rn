import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { PairScope } from "@/gen";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));
const host = vi.hoisted(() => ({
  status: "online" as string,
  transport: "direct" as string,
  active: null as object | null,
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  useApp: (
    selector: (state: {
      connection: { status: string; transport: string };
      hosts: Record<string, { name: string }>;
      localHostId: string | null;
    }) => unknown,
  ) =>
    selector({
      connection: { status: host.status, transport: host.transport },
      hosts: {},
      localHostId: null,
    }),
}));

vi.mock("./remoteStore", () => ({
  shellKind: () => "desktop",
  useRemote: (selector: (state: { active: object | null }) => unknown) =>
    selector({ active: host.active }),
}));

import {
  DirectDevicesTable,
  DirectPairingBlock,
  directDeviceName,
  NetworkAccessSettings,
  networkAccessVisible,
} from "./NetworkAccess";
import {
  createDirectPairing,
  getNetworkAccessState,
  resetNetworkAccessStoreForTests,
  setNetworkAccess,
} from "./networkAccessStore";

const OFFER = {
  type: "direct_pairing_offer" as const,
  uri: "h4rn://pair?e=192.168.1.20%3A47810&d=ab&p=cd",
  expires_at: Math.floor((Date.now() + 10 * 60_000) / 1_000),
  addresses: ["192.168.1.20:47810"],
  scopes: ["view", "approve"] as PairScope[],
};

async function enableNetwork(): Promise<void> {
  request.mockResolvedValueOnce({
    type: "network_access",
    enabled: true,
    listening: true,
    port: 47810,
    addresses: ["192.168.1.20:47810", "100.101.7.42:47810"],
    error: null,
  });
  await setNetworkAccess(true);
}

beforeEach(() => {
  resetNetworkAccessStoreForTests();
  request.mockReset();
  host.status = "online";
  host.transport = "direct";
  host.active = null;
});

describe("network access settings", () => {
  it("renders the toggle and the dial addresses", async () => {
    await enableNetwork();
    const html = renderToStaticMarkup(createElement(NetworkAccessSettings));
    expect(html).toContain("This computer");
    expect(html).toContain("Allow devices on this network");
    expect(html).toContain("192.168.1.20:47810");
    expect(html).toContain("100.101.7.42:47810");
    expect(html).toContain("Pair on this network");
  });

  it("stays off the page for a remote machine and a phone", () => {
    expect(networkAccessVisible("direct", false, "desktop")).toBe(true);
    expect(networkAccessVisible("encrypted", false, "desktop")).toBe(true);
    expect(networkAccessVisible("relay", false, "desktop")).toBe(false);
    expect(networkAccessVisible("tailscale", false, "desktop")).toBe(false);
    expect(networkAccessVisible("direct", true, "desktop")).toBe(false);
    expect(networkAccessVisible("direct", false, "mobile")).toBe(false);
  });

  it("disappears when this app drives another machine", () => {
    host.active = {};
    expect(renderToStaticMarkup(createElement(NetworkAccessSettings))).toBe("");
    host.active = null;
    host.transport = "relay";
    expect(renderToStaticMarkup(createElement(NetworkAccessSettings))).toBe("");
  });

  it("renders the QR and the scopes from the daemon's offer", async () => {
    request.mockResolvedValueOnce(OFFER);
    const offer = await createDirectPairing(["view", "approve"]);
    expect(offer).not.toBeNull();
    if (!offer) return;

    const html = renderToStaticMarkup(
      createElement(DirectPairingBlock, {
        offer,
        scopes: ["view", "approve"],
        hostName: "atlas",
        now: Date.now(),
        onToggleScope: () => undefined,
        onNewCode: () => undefined,
        onCancel: () => undefined,
      }),
    );
    expect(html).toContain("Direct pairing QR code for atlas");
    expect(html).toContain("<svg");
    expect(html).toContain("View threads");
    expect(html).toContain("Approve and reply");
    expect(html).toContain("Copy link");
    expect(html).toContain("Cancel");
    expect(html).toContain("Expires in");
    expect(getNetworkAccessState().offer?.uri).toBe(OFFER.uri);
  });

  it("lists directly paired devices with a revoke action each", () => {
    const now = Date.parse("2026-11-20T12:00:00Z");
    const html = renderToStaticMarkup(
      createElement(DirectDevicesTable, {
        devices: [
          {
            key: "3f9a1c0e".padEnd(64, "0"),
            scopes: ["view", "approve"],
            paired_at: "2026-11-18T12:00:00Z",
            connected: true,
          },
          {
            key: "c0ffee00".padEnd(64, "1"),
            scopes: ["view"],
            paired_at: null,
            connected: false,
          },
        ],
        now,
        revoking: null,
        onRevoke: async () => true,
      }),
    );
    expect(html).toContain("Device 3F9A1C0E");
    expect(html).toContain("Connected");
    expect(html).toContain("Not connected");
    expect(html).toContain("View only");
    expect(html).toContain('aria-label="Revoke Device C0FFEE00"');
  });

  it("says so when nothing has paired on this network", () => {
    const html = renderToStaticMarkup(
      createElement(DirectDevicesTable, {
        devices: [],
        now: Date.now(),
        revoking: null,
        onRevoke: async () => true,
      }),
    );
    expect(html).toContain("No devices paired on this network yet.");
  });

  it("names a key-only device by its first eight hex digits", () => {
    expect(directDeviceName("abcdef0123".padEnd(64, "0"))).toBe("Device ABCDEF01");
  });
});
