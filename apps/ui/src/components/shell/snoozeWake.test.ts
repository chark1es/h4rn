import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { snoozeWakeLabel } from "./snoozeWake";

const now = Date.parse("2026-11-25T12:00:00Z");
const at = (ms: number) => new Date(now + ms).toISOString();

describe("snoozeWakeLabel", () => {
  it("counts down in one unit while a minute or more is left", () => {
    expect(snoozeWakeLabel(at(5 * 60_000), now)).toBe("wakes in 5m");
    expect(snoozeWakeLabel(at(60_000), now)).toBe("wakes in 1m");
    expect(snoozeWakeLabel(at(2 * 3_600_000), now)).toBe("wakes in 2h");
  });

  it("says soon under a minute, never “wakes in now”", () => {
    expect(snoozeWakeLabel(at(59_000), now)).toBe("wakes soon");
    expect(snoozeWakeLabel(at(0), now)).toBe("wakes soon");
    // Past due and not yet woken by the daemon.
    expect(snoozeWakeLabel(at(-30_000), now)).toBe("wakes soon");
    expect(snoozeWakeLabel(null, now)).toBe("wakes soon");
  });

  it("speaks the active language", async () => {
    await setLocalePreference("zh-CN");
    expect(snoozeWakeLabel(at(20_000), now)).toBe("即将提醒");
    expect(snoozeWakeLabel(at(5 * 60_000), now)).toBe("5分钟后提醒");
    await setLocalePreference("es");
    expect(snoozeWakeLabel(at(20_000), now)).toBe("vuelve pronto");
  });
});

afterEach(async () => {
  await setLocalePreference("en");
});
