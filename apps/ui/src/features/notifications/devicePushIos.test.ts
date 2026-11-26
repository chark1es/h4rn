// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";
import { deviceAlertsPlatform, deviceAlertsShell } from "@/lib/platform/push";

import {
  deregisterDeviceAlertsOnSignOut,
  deviceAvailabilityNote,
  deviceStatusLine,
  disableDeviceAlerts,
  enableDeviceAlerts,
  readDeviceAlerts,
} from "./devicePush";

const IOS_USER_AGENT =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";

/** An iOS Tauri shell with a granted permission and a cached APNs token. */
function stubIosShell(): ReturnType<typeof vi.fn> {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(IOS_USER_AGENT);
  const invoke = vi.fn(async (command: string) => {
    if (command === "push_token") return "apns-token";
    if (command !== "push_status") return null;
    return {
      available: true,
      permission: "granted",
      token: "apns-token",
      channel: true,
      appVersion: "1.2.3",
    };
  });
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
  return invoke;
}

function shellInvoke(): ReturnType<typeof vi.fn> {
  return (window as unknown as { __TAURI_INTERNALS__: { invoke: ReturnType<typeof vi.fn> } })
    .__TAURI_INTERNALS__.invoke;
}

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
  vi.unstubAllGlobals();
});

describe("the phone shell is recognised on iOS too", () => {
  it("detects an iPhone shell and its platform", () => {
    stubIosShell();
    expect(deviceAlertsShell()).toBe(true);
    expect(deviceAlertsPlatform()).toBe("ios");
  });

  it("does not treat a browser or desktop shell as a phone", () => {
    expect(deviceAlertsShell()).toBe(false);
    expect(deviceAlertsPlatform()).toBeNull();
    vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5)",
    );
    vi.stubGlobal("__TAURI_INTERNALS__", { invoke: vi.fn() });
    expect(deviceAlertsShell()).toBe(false);
    expect(deviceAlertsPlatform()).toBeNull();
  });
});

describe("turning iPhone alerts on", () => {
  it("asks for permission, then registers the APNs token as iOS", async () => {
    stubIosShell();
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    const registerPushDevice = vi.fn(async (_input: Record<string, unknown>) => undefined);
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice,
      pushStatus: vi.fn(async () => ({
        configured: true,
        platform_configured: true,
        devices: 1,
        registered: true,
      })),
    } as unknown as ConnectClient);

    const enabled = await enableDeviceAlerts();

    const invoke = shellInvoke();
    const asked = invoke.mock.calls.findIndex(([command]) => command === "push_request_permission");
    const readToken = invoke.mock.calls.findIndex(([command]) => command === "push_token");
    expect(asked).toBeGreaterThanOrEqual(0);
    expect(readToken).toBeGreaterThan(asked);
    expect(registerPushDevice).toHaveBeenCalledOnce();
    const registered = registerPushDevice.mock.calls[0]?.[0];
    expect(registered).toEqual({
      deviceId: "device-1",
      token: "apns-token",
      platform: "ios",
      appVersion: "1.2.3",
    });
    expect(registered && "packageName" in registered).toBe(false);
    expect(enabled.enabled).toBe(true);
    expect(deviceStatusLine(enabled, true)).toBe("Registered with this phone.");
  });

  it("does not claim Android's sender speaks for an iPhone", async () => {
    stubIosShell();
    setConnectClient({
      device: { id: "device-1" },
      pushStatus: vi.fn(async () => ({
        configured: true,
        platform_configured: false,
        devices: 1,
        registered: false,
      })),
    } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.configured).toBe(false);
    expect(deviceAvailabilityNote(device)).toContain("no push sender configured for this phone");
  });

  it("falls back to the account-wide flag on a relay that predates iOS", async () => {
    stubIosShell();
    setConnectClient({
      device: { id: "device-1" },
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: false })),
    } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.configured).toBe(true);
    expect(deviceAvailabilityNote(device)).toBeNull();
  });
});

describe("turning iPhone alerts off and signing out", () => {
  it("mirrors the switch to the native delegate and removes the row", async () => {
    stubIosShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    const deregisterPushDevice = vi.fn(async () => undefined);
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
      pushStatus: vi.fn(async () => ({
        configured: true,
        platform_configured: true,
        devices: 1,
        registered: true,
      })),
    } as unknown as ConnectClient);

    const off = await disableDeviceAlerts();

    expect(off.enabled).toBe(false);
    expect(shellInvoke()).toHaveBeenCalledWith("push_set_enabled", { enabled: false });
    expect(deregisterPushDevice).toHaveBeenCalledWith("device-1");
  });

  it("turns the local flag off at once on sign-out and keeps a failed removal", async () => {
    stubIosShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    const deregisterPushDevice = vi.fn(async (): Promise<void> => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
    } as unknown as ConnectClient);

    deregisterDeviceAlertsOnSignOut();

    expect(shellInvoke()).toHaveBeenCalledWith("push_set_enabled", { enabled: false });
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");
    await vi.waitFor(() => expect(deregisterPushDevice).toHaveBeenCalledOnce());
  });
});
