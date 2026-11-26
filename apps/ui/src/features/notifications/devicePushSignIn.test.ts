// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";

import { startDevicePush } from "./devicePush";

const IOS_USER_AGENT =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
  vi.unstubAllGlobals();
});

describe("an iPhone launch that signs in after the alerts start", () => {
  it("re-arms the flag, then registers again when the APNs token changes", async () => {
    vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(IOS_USER_AGENT);
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    let token = "apns-token-one";
    const invoke = vi.fn(async (command: string) => {
      if (command === "push_token") return token;
      if (command === "push_take_pending_link") return { url: null };
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "granted",
        token,
        channel: true,
        appVersion: "1.2.3",
      };
    });
    vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");

    // A real launch: the alerts start while the credentials are still loading.
    remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
    startDevicePush();
    // Nothing is decided while the stored session loads.
    expect(invoke).not.toHaveBeenCalledWith("push_set_enabled", expect.anything());

    const registerPushDevice = vi.fn(async (_input: Record<string, unknown>) => undefined);
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice,
      deregisterPushDevice: vi.fn(async () => undefined),
      pushStatus: vi.fn(async () => ({
        configured: true,
        platform_configured: true,
        devices: 1,
        registered: true,
      })),
    } as unknown as ConnectClient);
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });

    await vi.waitFor(() => expect(registerPushDevice).toHaveBeenCalledTimes(1));
    expect(registerPushDevice.mock.calls[0]?.[0]).toMatchObject({
      token: "apns-token-one",
      platform: "ios",
    });
    const flags = invoke.mock.calls.filter(([command]) => command === "push_set_enabled");
    expect(flags.at(-1)).toEqual(["push_set_enabled", { enabled: true }]);

    // APNs handed the phone a new token while it was in the background. Coming
    // back must register the new one, not leave the relay on the old row.
    token = "apns-token-two";
    document.dispatchEvent(new Event("visibilitychange"));

    await vi.waitFor(() => expect(registerPushDevice).toHaveBeenCalledTimes(2));
    expect(registerPushDevice.mock.calls[1]?.[0]).toMatchObject({
      token: "apns-token-two",
      platform: "ios",
    });
  });
});
