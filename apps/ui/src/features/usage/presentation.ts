import type { UsageBucketPoint } from "@/gen";
import { activeLocale, formatDateTime, formatList, formatNumber, i18n } from "@/lib/i18n";

const SERIES_COLORS = ["var(--s1)", "var(--s2)", "var(--s3)", "var(--s4)"];

/** Stable colour per key so a harness keeps its hue across scopes. */
export function seriesColor(key: string, index: number): string {
  const harness = key.toLowerCase();
  if (harness.includes("claude")) return "var(--s1)";
  if (harness.includes("codex") || harness.includes("gpt")) return "var(--s2)";
  if (harness.includes("opencode")) return "var(--s3)";
  if (harness.includes("antigravity") || harness.includes("gemini")) return "var(--s4)";
  return SERIES_COLORS[index % SERIES_COLORS.length] ?? "var(--fg-2)";
}

type TimeUnit = "millisecond" | "second" | "minute" | "hour";

/**
 * How wide a unit reads in the usage figures. Each catalogue picks it: "2h 5m"
 * in English, but Japanese takes the short form ("2 時間5 分"), never a Latin
 * "2h5m".
 */
function unitDisplay(): "narrow" | "short" {
  return i18n.t("usage:unitDisplay") === "short" ? "short" : "narrow";
}

function unit(value: number, name: TimeUnit, options: Intl.NumberFormatOptions = {}): string {
  return formatNumber(value, { style: "unit", unit: name, unitDisplay: unitDisplay(), ...options });
}

/** Two units, the second padded so a ticking figure keeps its width: "2m 05s". */
function clock(seconds: number): string {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const pair = (major: string, minor: string) => formatList([major, minor], "unit", "narrow");
  if (hours > 0) {
    return pair(unit(hours, "hour"), unit(minutes, "minute", { minimumIntegerDigits: 2 }));
  }
  if (minutes > 0) {
    return pair(unit(minutes, "minute"), unit(rest, "second", { minimumIntegerDigits: 2 }));
  }
  return unit(rest, "second");
}

/** A length of time for a stat or a table cell: "9s", "2m 05s", "1h 30m". */
export function formatSpan(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return unit(0, "second");
  return clock(Math.round(ms / 1000));
}

/** Milliseconds below a second, seconds with one decimal above it. */
export function firstToken(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  if (ms < 1000) return unit(Math.round(ms), "millisecond");
  return unit(ms / 1000, "second", { minimumFractionDigits: 1, maximumFractionDigits: 1 });
}

/** A share already rounded to a whole percent (0–100), written the language's way. */
export function formatPercent(value: number): string {
  return formatNumber(value / 100, { style: "percent" });
}

/** A rate in tokens per second, or "—" when none was measured. */
export function formatRate(tps: number | null | undefined): string {
  if (tps === null || tps === undefined || !Number.isFinite(tps) || tps <= 0) return "—";
  return formatNumber(Math.round(tps));
}

/**
 * A compact token count split into its number and its unit ("12.4" and "K",
 * "1.2" and "万"), so a stat can set the unit smaller.
 */
export function tokenParts(count: number): { value: string; unit: string | undefined } {
  const parts = new Intl.NumberFormat(activeLocale(), {
    notation: "compact",
    maximumFractionDigits: 1,
  }).formatToParts(count);
  const unitText = parts
    .filter((part) => part.type === "compact")
    .map((part) => part.value)
    .join("");
  const value = parts
    .filter((part) => part.type !== "compact")
    .map((part) => part.value)
    .join("")
    .trim();
  return { value, unit: unitText || undefined };
}

/** A chart axis amount in dollars, narrow enough for the 42 px axis column. */
export function axisMoney(dollars: number): string {
  return formatNumber(dollars, {
    style: "currency",
    currency: "USD",
    currencyDisplay: "narrowSymbol",
    maximumFractionDigits: dollars > 0 && dollars < 10 ? 2 : 0,
  });
}

export function percent(part: number, whole: number): number {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((part / whole) * 100)));
}

export function bucketLabel(point: UsageBucketPoint, bucket: "hour" | "day" | null): string {
  const date = new Date(point.start);
  if (Number.isNaN(date.getTime())) return "—";
  if (bucket === "hour") {
    return formatDateTime(date, { hour: "2-digit", minute: "2-digit", hour12: false });
  }
  return formatDateTime(date, { month: "short", day: "numeric" });
}

/** One stacked bar's total, used only for the chart's scale. */
export function bucketTotal(point: UsageBucketPoint, metric: "tokens" | "cost"): number {
  return metric === "cost"
    ? point.totals.cost_micros / 1_000_000
    : point.totals.input_tokens + point.totals.output_tokens;
}

/**
 * A resume countdown is the one place seconds matter: the user is waiting for
 * it. The number is rendered from the daemon's `resume_at`, never scheduled in
 * the UI.
 */
export function countdownPrecise(iso: string | null, now = Date.now()): string {
  if (!iso) return i18n.t("usage:time.unknown");
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return i18n.t("usage:time.unknown");
  const delta = then - now;
  if (delta <= 0) return i18n.t("usage:time.now");
  return clock(Math.ceil(delta / 1000));
}
