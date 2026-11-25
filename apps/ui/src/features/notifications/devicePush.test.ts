// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { setConnectClient } from "@/features/remote/session";

import {
  type DeviceAlertsState,
  deviceAvailabilityNote,
  deviceStatusLine,
  readDeviceAlerts,
} from "./devicePush";

function state(patch: Partial<DeviceAlertsState> = {}): DeviceAlertsState {
  return {
    supported: true,
    available: true,
    permission: "granted",
    enabled: false,
    registered: false,
    configured: true,
    token: "token",
    error: null,
    ...patch,
  };
}

function stubAndroidShell(): void {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(
    "Mozilla/5.0 (Linux; Android 14; Pixel 8)",
  );
  vi.stubGlobal("__TAURI_INTERNALS__", {
    invoke: vi.fn(async (command: string) => {
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "granted",
        token: "token",
        channel: true,
        appVersion: "1.2.3",
      };
    }),
  });
}

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  vi.unstubAllGlobals();
});

describe("device alert settings states", () => {
  it("asks for a Connect sign-in before anything else", () => {
    expect(deviceStatusLine(state({ enabled: true }), false)).toBe("Sign in to Connect first.");
  });

  it("reports a build without Firebase rather than pretending", () => {
    expect(deviceStatusLine(state({ available: false }), true)).toBe("Unavailable in this build.");
  });

  it("says when Android blocked notifications", () => {
    expect(deviceStatusLine(state({ permission: "denied" }), true)).toBe(
      "Blocked in system settings.",
    );
  });

  it("distinguishes not asked yet from off", () => {
    expect(deviceStatusLine(state({ permission: "default" }), true)).toContain(
      "will ask for permission",
    );
    expect(deviceStatusLine(state(), true)).toBe("Off.");
  });

  it("says when registration is real", () => {
    expect(deviceStatusLine(state({ enabled: true, registered: true }), true)).toBe(
      "Registered with this phone.",
    );
    expect(deviceStatusLine(state({ enabled: true }), true)).toContain("has not confirmed");
  });

  it("does not blame the operator when a sender is configured", () => {
    expect(deviceAvailabilityNote(state({ configured: true }))).toBeNull();
  });

  it("names a missing sender when the relay says there is none", () => {
    expect(deviceAvailabilityNote(state({ configured: false }))).toContain(
      "no push sender configured",
    );
  });

  it("reports an unknown sender as unknown, not as missing", () => {
    const note = deviceAvailabilityNote(state({ configured: null, error: "offline" }));
    expect(note).toContain("Could not check");
    expect(note).not.toContain("no push sender configured");
  });
});

describe("reading this phone's real alert state", () => {
  it("uses the caller's own registration, not the account's device count", async () => {
    stubAndroidShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    const pushStatus = vi.fn(async () => ({
      configured: true,
      devices: 3,
      registered: false,
    }));
    setConnectClient({ pushStatus } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.registered).toBe(false);
    expect(device.enabled).toBe(true);
    expect(device.configured).toBe(true);
    expect(deviceStatusLine(device, true)).toContain("has not confirmed");
  });

  it("keeps a failed status call unknown instead of claiming no sender", async () => {
    stubAndroidShell();
    const pushStatus = vi.fn(async () => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({ pushStatus } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.configured).toBeNull();
    expect(device.error).toContain("unreachable");
    expect(deviceAvailabilityNote(device)).toContain("Could not check");
  });
});
