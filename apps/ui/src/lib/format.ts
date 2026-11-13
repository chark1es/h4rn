const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

export function relativeAge(iso: string, now = Date.now()): string {
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return "—";
  const delta = Math.max(0, now - then);
  if (delta < MINUTE) return "just now";
  if (delta < HOUR) return `${Math.floor(delta / MINUTE)}m`;
  if (delta < DAY) return `${Math.floor(delta / HOUR)}h`;
  return `${Math.floor(delta / DAY)}d`;
}

export function absoluteTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "—";
  return new Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

export function absoluteDateTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "—";
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

export function duration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "0s";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  if (minutes < 60) return `${minutes}m ${rest.toString().padStart(2, "0")}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${(minutes % 60).toString().padStart(2, "0")}m`;
}

export function countdown(iso: string, now = Date.now()): string {
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return "reset time unknown";
  const delta = then - now;
  if (delta <= 0) return "now";
  if (delta < MINUTE) return `${Math.ceil(delta / 1000)}s`;
  if (delta < HOUR) return `${Math.ceil(delta / MINUTE)}m`;
  const hours = Math.floor(delta / HOUR);
  const minutes = Math.ceil((delta % HOUR) / MINUTE);
  return `${hours}h ${minutes.toString().padStart(2, "0")}m`;
}

export function tokens(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(2)}M`;
  if (count >= 1_000) return `${(count / 1_000).toFixed(1)}k`;
  return `${count}`;
}

export function cost(micros: number): string {
  if (micros <= 0) return "—";
  return `$${(micros / 1_000_000).toFixed(2)}`;
}

export function tokensPerSecond(outputTokens: number, durationMs: number): string {
  if (durationMs <= 0) return "—";
  const tps = outputTokens / (durationMs / 1000);
  if (!Number.isFinite(tps) || tps <= 0) return "—";
  return `${Math.round(tps)}`;
}

export function latency(ms: number | null): string {
  return ms === null ? "—" : `${Math.round(ms)} ms`;
}

export function cueNumber(index: number): string {
  return (index + 1).toString().padStart(2, "0");
}

export function elapsedSince(iso: string, now = Date.now()): string {
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return "—";
  const delta = Math.max(0, now - then);
  const seconds = Math.floor(delta / 1000);
  const minutes = Math.floor(seconds / 60);
  return `${minutes.toString().padStart(2, "0")}:${(seconds % 60).toString().padStart(2, "0")}`;
}

export function titleFromPrompt(prompt: string): string {
  return prompt.trim().split("\n")[0]?.slice(0, 72) || "New thread";
}
