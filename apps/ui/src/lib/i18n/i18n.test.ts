// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";

import {
  activeLocale,
  formatAge,
  formatClock,
  formatClockUnit,
  formatDuration,
  formatNumber,
  formatRelativeTime,
  i18n,
  localePreference,
  matchLocale,
  prepareI18n,
  setLocalePreference,
  systemLocale,
} from "./index";

/** A key outside the typed catalogue, for the fallback probe. */
const untyped = i18n.t as unknown as (key: string, options?: Record<string, unknown>) => string;

function setBrowserLanguages(languages: string[]) {
  Object.defineProperty(window.navigator, "languages", { configurable: true, value: languages });
  Object.defineProperty(window.navigator, "language", {
    configurable: true,
    value: languages[0] ?? "",
  });
}

afterEach(async () => {
  await setLocalePreference("en");
  window.localStorage.clear();
});

describe("matching a language tag", () => {
  it("maps each supported language and its regional variants", () => {
    expect(matchLocale("en-GB")).toBe("en");
    expect(matchLocale("es-419")).toBe("es");
    expect(matchLocale("ES_mx")).toBe("es");
    expect(matchLocale("ja-JP")).toBe("ja");
    expect(matchLocale("zh")).toBe("zh-CN");
    expect(matchLocale("zh-CN")).toBe("zh-CN");
    expect(matchLocale("zh-SG")).toBe("zh-CN");
    expect(matchLocale("zh-Hans-HK")).toBe("zh-CN");
  });

  it("does not hand Simplified Chinese to a Traditional Chinese reader", () => {
    expect(matchLocale("zh-TW")).toBeNull();
    expect(matchLocale("zh-HK")).toBeNull();
    expect(matchLocale("zh-Hant")).toBeNull();
    expect(matchLocale("fr-FR")).toBeNull();
    expect(matchLocale("")).toBeNull();
  });

  it("takes the first supported system language, else English", () => {
    expect(systemLocale(["fr-FR", "ja-JP", "es"])).toBe("ja");
    expect(systemLocale(["zh-TW", "es-ES"])).toBe("es");
    expect(systemLocale(["fr-FR", "de"])).toBe("en");
    expect(systemLocale([])).toBe("en");
  });
});

describe("switching the language", () => {
  it("applies at once, sets <html lang> and remembers the choice", async () => {
    await setLocalePreference("ja");
    expect(activeLocale()).toBe("ja");
    expect(i18n.t("settings:page.label")).toBe("設定");
    expect(document.documentElement.lang).toBe("ja");
    expect(window.localStorage.getItem("h4rn.locale")).toBe("ja");

    await setLocalePreference("zh-CN");
    expect(i18n.t("settings:page.label")).toBe("设置");
    expect(document.documentElement.lang).toBe("zh-CN");
  });

  it("lets the last of two quick choices win", async () => {
    const first = setLocalePreference("es");
    const second = setLocalePreference("ja");
    await Promise.all([first, second]);
    expect(activeLocale()).toBe("ja");
    expect(localePreference()).toBe("ja");
  });

  it("follows the system language when the choice is System", async () => {
    setBrowserLanguages(["es-MX", "en-US"]);
    await setLocalePreference("system");
    expect(activeLocale()).toBe("es");
    expect(i18n.t("common:save")).toBe("Guardar");

    setBrowserLanguages(["fr-FR"]);
    await setLocalePreference("system");
    expect(activeLocale()).toBe("en");
  });

  it("restores the saved choice before the first render", async () => {
    window.localStorage.setItem("h4rn.locale", "es");
    await prepareI18n();
    expect(localePreference()).toBe("es");
    expect(activeLocale()).toBe("es");
    expect(document.documentElement.lang).toBe("es");
  });

  it("ignores a stored value that is not a known language", async () => {
    window.localStorage.setItem("h4rn.locale", "klingon");
    setBrowserLanguages(["en-US"]);
    await prepareI18n();
    expect(localePreference()).toBe("system");
    expect(activeLocale()).toBe("en");
  });
});

describe("missing translations", () => {
  it("fall back to English, never to the raw key", async () => {
    i18n.addResource("en", "settings", "fallbackProbe", "Only in English");
    await setLocalePreference("ja");
    expect(untyped("settings:fallbackProbe")).toBe("Only in English");
  });
});

describe("plurals and formatting", () => {
  it("uses each language's plural rules", () => {
    expect(i18n.t("settings:resume.observed", { count: 1 })).toBe("1 limit observed");
    expect(i18n.t("settings:resume.observed", { count: 3 })).toBe("3 limits observed");
    expect(i18n.t("settings:resume.observed", { count: 1, lng: "es" })).toBe("1 límite observado");
    expect(i18n.t("settings:resume.observed", { count: 3, lng: "es" })).toBe(
      "3 límites observados",
    );
    expect(i18n.t("settings:resume.observed", { count: 1, lng: "ja" })).toBe("上限を 1 件検出");
  });

  it("formats numbers, durations and relative times in the active language", async () => {
    const now = Date.parse("2026-11-25T12:00:00Z");
    expect(formatRelativeTime(now - 5 * 60_000, now)).toBe("5 minutes ago");
    expect(formatNumber(12345.5)).toBe("12,345.5");
    expect(formatDuration(2 * 3_600_000 + 30 * 60_000)).toBe("2 hr 30 min");

    await setLocalePreference("ja");
    expect(formatRelativeTime(now - 5 * 60_000, now)).toBe("5 分前");
    await setLocalePreference("es");
    expect(formatNumber(12345.5)).toBe("12.345,5");
  });

  it("formats a compact age in one unit, past or ahead", async () => {
    const now = Date.parse("2026-11-25T12:00:00Z");
    expect(formatAge(now - 20_000, now)).toBe("now");
    expect(formatAge(new Date(now - 5 * 60_000).toISOString(), now)).toBe("5m");
    expect(formatAge(now - 3 * 3_600_000, now)).toBe("3h");
    expect(formatAge(now - 2 * 86_400_000, now)).toBe("2d");
    // A snooze wakes in the future; the age is the distance either way.
    expect(formatAge(now + 2 * 3_600_000, now)).toBe("2h");
    expect(formatAge("not a date", now)).toBe("—");

    await setLocalePreference("zh-CN");
    expect(formatAge(now - 5 * 60_000, now)).toBe("5分钟");
    expect(formatAge(now, now)).toBe("现在");
    await setLocalePreference("es");
    expect(formatAge(now - 5 * 60_000, now)).toBe("5min");
    expect(formatAge(now, now)).toBe("ahora");
  });

  it("writes a clock figure with the language's own units, no space inside CJK", async () => {
    expect(formatClock(125)).toBe("2m 05s");
    expect(formatClock(5_400)).toBe("1h 30m");
    expect(formatClock(9)).toBe("9s");
    expect(formatClockUnit(450, "millisecond")).toBe("450ms");

    await setLocalePreference("es");
    expect(formatClock(125)).toBe("2min 05s");
    expect(formatClock(5_400)).toBe("1h 30min");

    await setLocalePreference("ja");
    expect(formatClock(125)).toBe("2分05秒");
    expect(formatClock(5_400)).toBe("1時間30分");
    expect(formatClock(9)).toBe("9秒");
    expect(formatClockUnit(450, "millisecond")).toBe("450ミリ秒");
    expect(formatClockUnit(1.7, "second", { minimumFractionDigits: 1 })).toBe("1.7秒");

    await setLocalePreference("zh-CN");
    expect(formatClock(125)).toBe("2分05秒");
    expect(formatClock(5_400)).toBe("1小时30分");
    expect(formatClockUnit(450, "millisecond")).toBe("450毫秒");
  });

  it("says how many threads need you in each language's plural", () => {
    expect(i18n.t("sidebar:showNeedsYou", { count: 1 })).toBe("Show sidebar. 1 thread needs you");
    expect(i18n.t("sidebar:showNeedsYou", { count: 3 })).toBe("Show sidebar. 3 threads need you");
    expect(i18n.t("sidebar:showNeedsYou", { count: 1, lng: "es" })).toBe(
      "Mostrar barra lateral. 1 hilo te necesita",
    );
  });
});
