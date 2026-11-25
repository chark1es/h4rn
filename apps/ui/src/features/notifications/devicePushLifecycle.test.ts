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

describe("a pending removal is retried without a relaunch", () => {
  it("tries again when the app comes back to the foreground", async () => {
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
          appVersion: "1.0.0",
        };
      }),
    });
    window.localStorage.setItem("h4rn.push.deregisterPending", "1");

    const deregisterPushDevice = vi.fn(async (): Promise<void> => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: true })),
    } as unknown as ConnectClient);

    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    startDevicePush();

    // Launch already retried once and could not reach Connect.
    await vi.waitFor(() => expect(deregisterPushDevice).toHaveBeenCalledOnce());
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");

    // The relay is reachable again when the app comes back.
    deregisterPushDevice.mockImplementation(async () => undefined);
    document.dispatchEvent(new Event("visibilitychange"));

    await vi.waitFor(() =>
      expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBeNull(),
    );
    expect(deregisterPushDevice).toHaveBeenCalledTimes(2);
  });
});
