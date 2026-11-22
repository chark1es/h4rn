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
});
