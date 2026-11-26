// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";
import { store } from "@/lib/store/appStore";

import { startDevicePush } from "./devicePush";

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
  vi.unstubAllGlobals();
});

describe("the iPhone foreground route", () => {
  it("names the exact thread on screen and clears it when the thread closes", async () => {
    vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(
      "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15",
    );
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
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice: vi.fn(async () => undefined),
      deregisterPushDevice: vi.fn(async () => undefined),
      pushStatus: vi.fn(async () => ({
        configured: true,
        platform_configured: true,
        devices: 1,
        registered: true,
      })),
    } as unknown as ConnectClient);

    const openThread = { kind: "thread", threadId: "th_1" } as const;
    store.setState({
      ...store.getState(),
      ui: { ...store.getState().ui, route: openThread },
    });
    startDevicePush();

    expect(invoke).toHaveBeenCalledWith("push_set_foreground_thread", { threadId: "th_1" });

    store.setState({
      ...store.getState(),
      ui: { ...store.getState().ui, route: { kind: "inbox" } },
    });

    expect(invoke).toHaveBeenLastCalledWith("push_set_foreground_thread", { threadId: null });
  });
});
