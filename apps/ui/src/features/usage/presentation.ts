import type { UsageBucketPoint } from "@/gen";

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

/** Seconds below a minute, human milliseconds above it. */
export function firstToken(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}

export function percent(part: number, whole: number): number {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((part / whole) * 100)));
}

export function bucketLabel(point: UsageBucketPoint, bucket: "hour" | "day" | null): string {
  const date = new Date(point.start);
  if (Number.isNaN(date.getTime())) return "—";
  if (bucket === "hour") {
    return new Intl.DateTimeFormat(undefined, {
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    }).format(date);
  }
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" }).format(date);
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
  if (!iso) return "unknown";
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return "unknown";
  const delta = then - now;
  if (delta <= 0) return "now";
  const seconds = Math.ceil(delta / 1000);
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  if (hours > 0) return `${hours}h ${minutes.toString().padStart(2, "0")}m`;
  if (minutes > 0) return `${minutes}m ${rest.toString().padStart(2, "0")}s`;
  return `${rest}s`;
}
