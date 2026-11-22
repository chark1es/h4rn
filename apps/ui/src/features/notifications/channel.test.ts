import { afterEach, describe, expect, it, vi } from "vitest";

import { notificationChannel } from "./channel";

afterEach(() => {
  vi.unstubAllGlobals();
});

function desktopWindow() {
  const invoke = vi.fn(async (command: string) => {
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
