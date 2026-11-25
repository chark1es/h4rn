// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";

import { startDevicePush } from "./devicePush";

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
  vi.unstubAllGlobals();
});

describe("a launch while signed out", () => {
  it("does not re-arm the native flag when the switch was left on", async () => {
    vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(
      "Mozilla/5.0 (Linux; Android 14; Pixel 8)",
    );
    const invoke = vi.fn(async (command: string) => {
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "granted",
        token: "token",
        channel: true,
        appVersion: "1.0.0",
      };
    });
    vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");

    const registerPushDevice = vi.fn(async () => undefined);
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice,
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: true })),
    } as unknown as ConnectClient);

    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_out" });
    startDevicePush();

    expect(invoke).not.toHaveBeenCalledWith("push_set_enabled", { enabled: true });
    expect(invoke).toHaveBeenLastCalledWith("push_set_enabled", { enabled: false });
    expect(registerPushDevice).not.toHaveBeenCalled();
  });
});
