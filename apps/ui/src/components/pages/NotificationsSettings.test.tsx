// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  notificationChannel,
  resetBrowserNotificationsForTests,
} from "@/features/notifications/channel";
import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";

import { SettingsPage } from "./SettingsPage";

const ANDROID_USER_AGENT =
  "Mozilla/5.0 (Linux; Android 16; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Mobile Safari/537.36";

/** An Android Tauri shell with notifications granted and a cached FCM token. */
function stubAndroidShell(): ReturnType<typeof vi.fn> {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(ANDROID_USER_AGENT);
  const invoke = vi.fn(async (command: string) => {
    if (command === "push_status") {
      return {
        available: true,
        permission: "granted",
        token: "fcm-token",
        channel: true,
        appVersion: "1.2.3",
      };
    }
    return null;
  });
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
  return invoke;
}

/** Signed in to Connect, with this phone's Device notifications on. */
function signInWithDeviceAlerts(): void {
  window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
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
  remoteStore.setState({
    ...remoteStore.getState(),
    phase: "signed_in",
    user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
  });
}

function stubBrowserNotifications(): void {
  // The OS permission is granted; only the constructor fails, which is a
  // property of this page and not of the permission.
  const ThrowingNotification = vi.fn(() => {
    throw new Error("this context forbids notifications");
  });
  Object.assign(ThrowingNotification, {
    permission: "granted",
    requestPermission: vi.fn(async () => "granted"),
  });
  vi.stubGlobal("Notification", ThrowingNotification);
}

beforeEach(() => {
  window.localStorage.clear();
  stubBrowserNotifications();
  vi.spyOn(console, "warn").mockImplementation(() => undefined);
});

afterEach(() => {
  cleanup();
  setConnectClient(null);
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading", user: null });
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  resetBrowserNotificationsForTests();
});

describe("notification settings", () => {
  it("says delivery stopped on this page without blaming the OS permission", async () => {
    render(<SettingsPage section="notifications" />);

    // The OS still says granted, so no row reads "Blocked in system settings".
    expect(await screen.findAllByRole("switch")).not.toHaveLength(0);
    expect(screen.queryByText("Blocked in system settings")).toBeNull();
    expect(screen.queryByText(/Notifications stopped working/)).toBeNull();

    // The notifier's own constructor fails. Settings reads the same page
    // state, even though this component never showed a notification itself.
    act(() => {
      notificationChannel()?.show({
        id: 1,
        key: "thread:t1",
        title: "A thread",
        body: "Needs an approval.",
        threadId: "t1",
      });
    });

    expect(await screen.findByText(/Notifications stopped working on this page/)).toBeTruthy();
    // The OS permission is untouched: the page failure is its own sentence.
    expect(screen.queryByText("Blocked in system settings")).toBeNull();
  });

  it("offers phone alerts but hides the device row outside the Android app", async () => {
    render(<SettingsPage section="notifications" />);

    expect(await screen.findByText("Send agent alerts to my phone through Connect")).toBeTruthy();
    // jsdom is neither Android nor a Tauri shell, so there is no device toggle
    // to promise an alert this build cannot receive.
    expect(screen.queryByText("Device notifications")).toBeNull();
    // iOS has no Live Update surface here, so the switch is not offered either.
    expect(screen.queryByText("Show running agents as live notifications")).toBeNull();
  });

  it("keeps the Live Updates switch inert while Device notifications are off", async () => {
    const invoke = stubAndroidShell();
    render(<SettingsPage section="notifications" />);

    const toggle = await screen.findByRole("switch", {
      name: "Show running agents as live notifications",
    });
    // A Live Update is one of this phone's alerts; with them off it could
    // never show, so the switch must not look like it does something.
    expect(toggle.getAttribute("aria-disabled")).toBe("true");
    expect(
      screen.getByText("Turn on Device notifications to use live notifications."),
    ).toBeTruthy();
    fireEvent.click(toggle);
    expect(window.localStorage.getItem("h4rn.push.liveUpdates")).toBeNull();
    expect(invoke).not.toHaveBeenCalledWith("push_set_live_updates", expect.anything());
  });

  it("offers the Live Updates switch on Android and mirrors it to the shell", async () => {
    const invoke = stubAndroidShell();
    signInWithDeviceAlerts();
    render(<SettingsPage section="notifications" />);

    const toggle = await screen.findByRole("switch", {
      name: "Show running agents as live notifications",
    });
    // Off by default: running turns stay ordinary until the person opts in.
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    await vi.waitFor(() => expect(toggle.getAttribute("aria-disabled")).toBeNull());
    expect(
      screen.queryByText("Turn on Device notifications to use live notifications."),
    ).toBeNull();

    fireEvent.click(toggle);

    expect(toggle.getAttribute("aria-checked")).toBe("true");
    expect(window.localStorage.getItem("h4rn.push.liveUpdates")).toBe("1");
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("push_set_live_updates", { enabled: true }),
    );

    fireEvent.click(toggle);
    expect(window.localStorage.getItem("h4rn.push.liveUpdates")).toBeNull();
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("push_set_live_updates", { enabled: false }),
    );
  });
});
