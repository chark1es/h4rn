import i18n, { type Resource, type ResourceLanguage } from "i18next";
import { useSyncExternalStore } from "react";
import { initReactI18next } from "react-i18next";

import enCommands from "./locales/en/commands.json";
import enCommon from "./locales/en/common.json";
import enComposer from "./locales/en/composer.json";
import enIssues from "./locales/en/issues.json";
import enPullRequests from "./locales/en/pullRequests.json";
import enSettings from "./locales/en/settings.json";
import enSidebar from "./locales/en/sidebar.json";
import enThread from "./locales/en/thread.json";

/**
 * UI translations.
 *
 * English is the source language and is bundled with the app, so a missing
 * key in any other language falls back to the English text, never to the raw
 * key. The other catalogues load on demand when the person (or their system)
 * picks that language, so switching applies at once without a reload.
 *
 * See docs/i18n.md for how to add a string, an area or a language.
 */

export const LOCALES = ["en", "zh-CN", "es", "ja"] as const;
export type Locale = (typeof LOCALES)[number];
export type LocalePreference = "system" | Locale;

/** Every namespace, one per UI area plus `common` for shared words. */
export const NAMESPACES = [
  "common",
  "commands",
  "composer",
  "issues",
  "pullRequests",
  "settings",
  "sidebar",
  "thread",
] as const;
export type Namespace = (typeof NAMESPACES)[number];

export const DEFAULT_LOCALE: Locale = "en";

/** Each language in its own name, for the Language picker. */
export const LOCALE_NAMES: Record<Locale, string> = {
  en: "English",
  "zh-CN": "简体中文",
  es: "Español",
  ja: "日本語",
};

export const englishResources = {
  common: enCommon,
  commands: enCommands,
  composer: enComposer,
  issues: enIssues,
  pullRequests: enPullRequests,
  settings: enSettings,
  sidebar: enSidebar,
  thread: enThread,
} as const;

const STORAGE_KEY = "h4rn.locale";

/**
 * Lazily loaded catalogues for every language but English. Vite turns each
 * JSON file into its own chunk, so a person who never leaves English never
 * downloads the others. English is imported above and left out here, so it is
 * not also offered as a dynamic chunk.
 */
const catalogueLoaders = import.meta.glob<{ default: ResourceLanguage[string] }>([
  "./locales/*/*.json",
  "!./locales/en/*.json",
]);

/**
 * The supported language for a BCP 47 tag, or null when h4rn has none.
 *
 * Chinese maps to Simplified Chinese only for Simplified tags (`zh`, `zh-CN`,
 * `zh-SG`, `zh-Hans…`); a Traditional reader (`zh-TW`, `zh-HK`, `zh-Hant…`)
 * gets the next language they prefer, or English, rather than a script they
 * did not ask for.
 */
export function matchLocale(tag: string): Locale | null {
  const lower = tag.trim().toLowerCase().replace(/_/g, "-");
  if (!lower) return null;
  const [language, ...rest] = lower.split("-");
  switch (language) {
    case "en":
      return "en";
    case "es":
      return "es";
    case "ja":
      return "ja";
    case "zh": {
      if (rest.includes("hant")) return null;
      if (rest.includes("hans")) return "zh-CN";
      const region = rest.find((part) => part.length === 2);
      if (region === "tw" || region === "hk" || region === "mo") return null;
      return "zh-CN";
    }
    default:
      return null;
  }
}

/** The first of the system's preferred languages h4rn supports, else English. */
export function systemLocale(
  languages: readonly string[] | undefined = typeof navigator === "undefined"
    ? undefined
    : navigator.languages?.length
      ? navigator.languages
      : [navigator.language],
): Locale {
  for (const tag of languages ?? []) {
    const match = typeof tag === "string" ? matchLocale(tag) : null;
    if (match) return match;
  }
  return DEFAULT_LOCALE;
}

function isPreference(value: unknown): value is LocalePreference {
  return value === "system" || (LOCALES as readonly unknown[]).includes(value);
}

function readPreference(): LocalePreference {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    if (isPreference(stored)) return stored;
  } catch {
    // localStorage may be unavailable in a locked-down webview.
  }
  return "system";
}

let preference: LocalePreference = "system";
const listeners = new Set<() => void>();

export function resolveLocale(pref: LocalePreference): Locale {
  return pref === "system" ? systemLocale() : pref;
}

const loaded = new Set<Locale>([DEFAULT_LOCALE]);

async function loadCatalogues(locale: Locale): Promise<void> {
  if (loaded.has(locale)) return;
  await Promise.all(
    NAMESPACES.map(async (namespace) => {
      const load = catalogueLoaders[`./locales/${locale}/${namespace}.json`];
      if (!load) return;
      const module = await load();
      i18n.addResourceBundle(locale, namespace, module.default, true, true);
    }),
  );
  loaded.add(locale);
}

function setDocumentLanguage(locale: string): void {
  if (typeof document === "undefined") return;
  document.documentElement.lang = locale;
}

let switchToken = 0;

/**
 * Load a language's catalogues and switch every mounted component to it.
 *
 * A failed load (an offline webview missing a chunk) keeps the current
 * language rather than showing keys; the next switch tries again.
 */
async function applyLocale(locale: Locale): Promise<void> {
  const token = ++switchToken;
  try {
    await loadCatalogues(locale);
  } catch {
    return;
  }
  // A later choice won the race; do not undo it.
  if (token !== switchToken) return;
  await i18n.changeLanguage(locale);
}

i18n.use(initReactI18next).init({
  lng: DEFAULT_LOCALE,
  fallbackLng: DEFAULT_LOCALE,
  supportedLngs: [...LOCALES],
  // Keep `zh-CN` as one code; i18next would otherwise also look for `zh`.
  load: "currentOnly",
  ns: [...NAMESPACES],
  defaultNS: "common",
  resources: { en: englishResources } as unknown as Resource,
  // English is bundled, so the first render never waits.
  initAsync: false,
  interpolation: {
    // React escapes text already.
    escapeValue: false,
  },
  returnNull: false,
  returnEmptyString: false,
});

i18n.on("languageChanged", (lng) => {
  setDocumentLanguage(lng);
  for (const listener of listeners) listener();
});

/**
 * Read the saved choice and switch to it before the first render, so a
 * non-English reader does not see an English flash. Never rejects.
 */
export async function prepareI18n(): Promise<void> {
  preference = readPreference();
  setDocumentLanguage(i18n.language);
  if (typeof window !== "undefined") {
    window.addEventListener("languagechange", () => {
      if (preference === "system") void applyLocale(systemLocale());
    });
  }
  await applyLocale(resolveLocale(preference));
}

export function localePreference(): LocalePreference {
  return preference;
}

/** Save the person's choice and apply it at once. */
export async function setLocalePreference(next: LocalePreference): Promise<void> {
  preference = next;
  try {
    window.localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // Non-fatal: the choice still applies for this session.
  }
  for (const listener of listeners) listener();
  await applyLocale(resolveLocale(next));
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useLocalePreference(): LocalePreference {
  return useSyncExternalStore(subscribe, localePreference, () => "system");
}

/** The language the UI shows now. */
export function activeLocale(): Locale {
  const current = i18n.resolvedLanguage ?? i18n.language;
  return (LOCALES as readonly string[]).includes(current) ? (current as Locale) : DEFAULT_LOCALE;
}

export function useActiveLocale(): Locale {
  return useSyncExternalStore(subscribe, activeLocale, () => DEFAULT_LOCALE);
}

// Formatting. Dates, times and numbers go through Intl with the active
// language, never through string building.

export function formatNumber(value: number, options?: Intl.NumberFormatOptions): string {
  return new Intl.NumberFormat(activeLocale(), options).format(value);
}

export function formatDateTime(
  value: Date | number | string,
  options: Intl.DateTimeFormatOptions = { dateStyle: "medium", timeStyle: "short" },
): string {
  const date = value instanceof Date ? value : new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return new Intl.DateTimeFormat(activeLocale(), options).format(date);
}

const RELATIVE_STEPS: Array<[Intl.RelativeTimeFormatUnit, number]> = [
  ["second", 60],
  ["minute", 60],
  ["hour", 24],
  ["day", 7],
  ["week", 4.34524],
  ["month", 12],
  ["year", Number.POSITIVE_INFINITY],
];

/** "3 minutes ago" / "in 2 hours", in the active language. */
export function formatRelativeTime(
  value: Date | number | string,
  now: number = Date.now(),
  options: Intl.RelativeTimeFormatOptions = { numeric: "auto" },
): string {
  const time = value instanceof Date ? value.getTime() : new Date(value).getTime();
  if (Number.isNaN(time)) return "";
  let delta = (time - now) / 1000;
  const format = new Intl.RelativeTimeFormat(activeLocale(), options);
  for (const [unit, size] of RELATIVE_STEPS) {
    if (Math.abs(delta) < size) return format.format(Math.round(delta), unit);
    delta /= size;
  }
  return format.format(Math.round(delta), "year");
}

const AGE_STEPS: Array<[number, "minute" | "hour" | "day"]> = [
  [86_400_000, "day"],
  [3_600_000, "hour"],
  [60_000, "minute"],
];

/**
 * How long ago (or how far ahead) a moment is, as one compact unit for a
 * narrow column: "now", "5m", "3h", "2d" in English, "5分钟" in Chinese.
 * The caller words the direction ("wakes in {{age}}"). An unknown time is "—".
 */
export function formatAge(value: Date | number | string, now: number = Date.now()): string {
  const time =
    value instanceof Date ? value.getTime() : typeof value === "number" ? value : Date.parse(value);
  if (!Number.isFinite(time)) return "—";
  const delta = Math.abs(now - time);
  for (const [size, unit] of AGE_STEPS) {
    if (delta >= size) {
      return formatNumber(Math.floor(delta / size), {
        style: "unit",
        unit,
        unitDisplay: "narrow",
      });
    }
  }
  return new Intl.RelativeTimeFormat(activeLocale(), { numeric: "auto" }).format(0, "second");
}

/**
 * A length of time as hours, minutes and seconds in the active language
 * ("2 hr 30 min" / "2小时30分钟"). The largest two non-zero units are shown.
 */
export function formatDuration(ms: number): string {
  const total = Number.isFinite(ms) && ms > 0 ? Math.round(ms / 1000) : 0;
  const parts: Array<[number, "hour" | "minute" | "second"]> = [
    [Math.floor(total / 3600), "hour"],
    [Math.floor((total % 3600) / 60), "minute"],
    [total % 60, "second"],
  ];
  const shown = parts.filter(([amount]) => amount > 0).slice(0, 2);
  const units = (shown.length ? shown : [[0, "second"] as const]).map(([amount, unit]) =>
    formatNumber(amount, { style: "unit", unit, unitDisplay: "short" }),
  );
  return new Intl.ListFormat(activeLocale(), { style: "narrow", type: "unit" }).format(units);
}

/**
 * A list joined the way the active language joins one ("a, b and c").
 * `narrow` suits a table cell of names: "a, b, c" in English, "a、b、c" in
 * Chinese and Japanese.
 */
export function formatList(
  items: readonly string[],
  type: Intl.ListFormatType = "conjunction",
  style: Intl.ListFormatStyle = "long",
) {
  return new Intl.ListFormat(activeLocale(), { style, type }).format(items);
}

export { i18n };
