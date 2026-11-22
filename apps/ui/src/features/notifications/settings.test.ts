import { beforeEach, describe, expect, it, vi } from "vitest";

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => {
      values.delete(key);
    },
    setItem: (key, value) => {
      values.set(key, value);
    },
  };
}

beforeEach(() => {
  vi.resetModules();
  vi.stubGlobal("window", { localStorage: memoryStorage() });
});

describe("notification settings", () => {
  it("starts with approvals and finished turns on, host offline off", async () => {
    const settings = await import("./settings");
    expect(settings.getNotificationSettings()).toEqual({
      approvals: true,
      finishedTurns: true,
      hostOffline: false,
    });
  });

  it("persists a choice on this device", async () => {
    const settings = await import("./settings");
    settings.setNotificationSetting("hostOffline", true);
    expect(JSON.parse(window.localStorage.getItem("h4rn.notifications") ?? "{}")).toEqual({
      approvals: true,
      finishedTurns: true,
      hostOffline: true,
    });

    vi.resetModules();
    const reloaded = await import("./settings");
    expect(reloaded.getNotificationSettings().hostOffline).toBe(true);
  });

  it("falls back to the defaults when storage is malformed", async () => {
    window.localStorage.setItem("h4rn.notifications", "{not json");
    const settings = await import("./settings");
    expect(settings.getNotificationSettings().approvals).toBe(true);

    window.localStorage.setItem("h4rn.notifications", JSON.stringify({ approvals: "yes" }));
    vi.resetModules();
    const coerced = await import("./settings");
    expect(coerced.getNotificationSettings()).toEqual({
      approvals: true,
      finishedTurns: true,
      hostOffline: false,
    });
  });

  it("shows a default permission as off without losing the preference", async () => {
    const settings = await import("./settings");
    expect(settings.displayedNotificationSetting("default", true)).toBe(false);
    expect(settings.getNotificationSettings().approvals).toBe(true);
    expect(settings.displayedNotificationSetting("granted", true)).toBe(true);
    expect(settings.displayedNotificationSetting("unknown", true)).toBe(true);
    expect(settings.displayedNotificationSetting("denied", true)).toBe(false);
  });
});

describe("host offline row visibility", () => {
  it("stays hidden until the first account answer", async () => {
    const settings = await import("./settings");
    // No flash of "no linked host" (or a row) before the relay answers.
    expect(
      settings.hostOfflineVisibility({
        phase: "loading",
        signedIn: true,
        linkedSupported: true,
        linkedMachineCount: 1,
      }),
    ).toEqual({ kind: "hidden" });
  });

  it("appears for a filtered relay with linked machines", async () => {
    const settings = await import("./settings");
    expect(
      settings.hostOfflineVisibility({
        phase: "signed_in",
        signedIn: true,
        linkedSupported: true,
        linkedMachineCount: 2,
      }),
    ).toEqual({ kind: "available" });
  });

  it("says there are no links after a filtered answer with none", async () => {
    const settings = await import("./settings");
    expect(
      settings.hostOfflineVisibility({
        phase: "signed_in",
        signedIn: true,
        linkedSupported: true,
        linkedMachineCount: 0,
      }),
    ).toEqual({ kind: "no-links" });
  });

  it("names an older relay instead of claiming there are no links", async () => {
    const settings = await import("./settings");
    expect(
      settings.hostOfflineVisibility({
        phase: "signed_in",
        signedIn: true,
        linkedSupported: false,
        linkedMachineCount: 0,
      }),
    ).toEqual({ kind: "relay-too-old" });
  });
});
