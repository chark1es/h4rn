// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  notificationChannel,
  resetBrowserNotificationsForTests,
} from "@/features/notifications/channel";

import { SettingsPage } from "./SettingsPage";

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
  });
});
