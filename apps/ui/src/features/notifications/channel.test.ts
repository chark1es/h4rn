import { afterEach, describe, expect, it, vi } from "vitest";

import { notificationChannel } from "./channel";

afterEach(() => {
  vi.unstubAllGlobals();
});

function desktopWindow() {
  const invoke = vi.fn(async (command: string, _args?: Record<string, unknown>) => {
    // The pinned plugin answers this unconditionally; if the view asked, it
    // would be repeating a claim it cannot verify.
    if (command === "plugin:notification|is_permission_granted") return true;
    return null;
  });
  return {
    navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    __TAURI_INTERNALS__: {
      invoke,
      transformCallback: vi.fn(() => 1),
    },
  };
}

describe("notification channel permission", () => {
  it("reports the desktop permission as unknown instead of a plugin guess", async () => {
    vi.stubGlobal("window", desktopWindow());
    const channel = notificationChannel();
    expect(channel?.kind).toBe("desktop");
    await expect(channel?.permission()).resolves.toBe("unknown");
    await expect(channel?.requestPermission()).resolves.toBe("unknown");
  });

  it("still reports the browser's real permission", async () => {
    const Notification = {
      permission: "denied",
      requestPermission: vi.fn(async () => "denied"),
    };
    vi.stubGlobal("Notification", Notification);
    vi.stubGlobal("window", {
      Notification,
      navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    });
    const channel = notificationChannel();
    expect(channel?.kind).toBe("browser");
    await expect(channel?.permission()).resolves.toBe("denied");
  });
});

describe("browser notification delivery", () => {
  it("logs once and stops retrying when the constructor throws", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const ThrowingNotification = vi.fn(() => {
      throw new Error("permission was revoked");
    });
    Object.assign(ThrowingNotification, {
      permission: "denied",
      requestPermission: vi.fn(async () => "denied"),
    });
    vi.stubGlobal("Notification", ThrowingNotification);
    vi.stubGlobal("window", {
      Notification: ThrowingNotification,
      navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    });
    const channel = notificationChannel();

    channel?.show({
      id: 1,
      key: "thread:t1",
      title: "A thread",
      body: "Needs an approval.",
      threadId: "t1",
    });
    channel?.show({
      id: 2,
      key: "thread:t2",
      title: "Another thread",
      body: "Finished.",
      threadId: "t2",
    });

    // The first throw is reported, and the dead channel never tries again.
    expect(ThrowingNotification).toHaveBeenCalledTimes(1);
    expect(warning).toHaveBeenCalledTimes(1);
    expect(warning.mock.calls[0]?.[0]).toContain("browser notification");
    await expect(channel?.permission()).resolves.toBe("denied");
  });
});

describe("desktop notification delivery", () => {
  it("shows through the plugin with the thread as click data", async () => {
    const win = desktopWindow();
    vi.stubGlobal("window", win);
    const channel = notificationChannel();

    channel?.show({
      id: 42,
      key: "thread:t1",
      title: "Fix the flaky auth test",
      body: "Wants to run: npm test",
      threadId: "t1",
    });
    await Promise.resolve();

    const call = win.__TAURI_INTERNALS__.invoke.mock.calls.find(
      ([command]) => command === "plugin:notification|notify",
    );
    expect(call?.[1]).toEqual({
      options: {
        id: 42,
        title: "Fix the flaky auth test",
        body: "Wants to run: npm test",
        extra: { threadId: "t1" },
      },
    });
  });

  it("logs a rejected notification instead of swallowing it", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const win = desktopWindow();
    win.__TAURI_INTERNALS__.invoke.mockImplementation(async (command: string) => {
      if (command === "plugin:notification|notify") throw new Error("no notification host");
      return null;
    });
    vi.stubGlobal("window", win);

    notificationChannel()?.show({
      id: 7,
      key: "thread:t1",
      title: "A thread",
      body: "Needs an approval.",
      threadId: "t1",
    });
    await Promise.resolve();
    await Promise.resolve();

    expect(warning).toHaveBeenCalledWith(
      expect.stringContaining("desktop notification"),
      expect.any(Error),
    );
  });
});
