import type { HarnessKind, HostId, ThreadSummary, ThreadTab } from "@/gen";
import { harnessLabel, harnessSlug } from "@/lib/entities";
import type { AppState, ThreadFilterState } from "@/lib/store/appStore";

export const TAB_ORDER: ThreadTab[] = ["working", "done", "settled"];

export const TAB_LABELS: Record<ThreadTab, string> = {
  working: "Working",
  done: "Done",
  settled: "Settled",
};

export function isSnoozed(thread: ThreadSummary, now: number): boolean {
  if (!thread.snoozed_until) return false;
  const until = Date.parse(thread.snoozed_until);
  return Number.isFinite(until) ? until > now : false;
}

export function snoozedUntil(thread: ThreadSummary, now: number): number | null {
  if (!isSnoozed(thread, now) || !thread.snoozed_until) return null;
  return Date.parse(thread.snoozed_until);
}

export function needsAttention(thread: ThreadSummary, now: number): boolean {
  if (isSnoozed(thread, now)) return false;
  return (
    thread.needs_attention ||
    thread.agent_state === "awaiting_approval" ||
    thread.agent_state === "error" ||
    (thread.agent_state === "rate_limited" && !thread.pinned)
  );
}

export function compareThreads(a: ThreadSummary, b: ThreadSummary, now: number): number {
  const attention = Number(needsAttention(b, now)) - Number(needsAttention(a, now));
  if (attention !== 0) return attention;
  const pinned = Number(b.pinned) - Number(a.pinned);
  if (pinned !== 0) return pinned;
  return Date.parse(b.last_activity) - Date.parse(a.last_activity);
}

export function matchesQuery(thread: ThreadSummary, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  const haystack = [
    thread.title,
    thread.host,
    thread.workspace_path,
    thread.model,
    harnessLabel(thread.harness),
    harnessSlug(thread.harness),
  ]
    .join(" ")
    .toLowerCase();
  return haystack.includes(needle);
}

export function matchesFilter(
  thread: ThreadSummary,
  filter: ThreadFilterState,
  now: number,
  hostName?: string,
): boolean {
  if (filter.host && thread.host !== filter.host) return false;
  if (filter.harness && thread.harness !== filter.harness) return false;
  if (filter.needsAttention && !needsAttention(thread, now)) return false;
  if (filter.unread && !thread.unread) return false;
  if (filter.host && !hostName) return false;
  return true;
}

export function threadsForTab(state: AppState, tab: ThreadTab, now: number): ThreadSummary[] {
  return Object.values(state.threads)
    .filter(
      (thread) =>
        thread.tab === tab &&
        !isSnoozed(thread, now) &&
        matchesQuery(thread, state.ui.query) &&
        matchesFilter(thread, state.ui.filter, now, state.hosts[thread.host]?.name),
    )
    .sort((a, b) => compareThreads(a, b, now));
}

export function snoozedThreads(state: AppState, now: number): ThreadSummary[] {
  return Object.values(state.threads)
    .filter((thread) => isSnoozed(thread, now) && matchesQuery(thread, state.ui.query))
    .sort((a, b) => (snoozedUntil(a, now) ?? 0) - (snoozedUntil(b, now) ?? 0));
}

export function tabCounts(state: AppState, now: number): Record<ThreadTab, number> {
  const counts: Record<ThreadTab, number> = { working: 0, done: 0, settled: 0 };
  for (const thread of Object.values(state.threads)) {
    if (isSnoozed(thread, now)) continue;
    counts[thread.tab] += 1;
  }
  return counts;
}

export function pathForTab(tab: ThreadTab): ThreadTab {
  return tab;
}

export function harnessOptions(state: AppState): Array<{ harness: HarnessKind; host: HostId }> {
  const seen = new Set<string>();
  const options: Array<{ harness: HarnessKind; host: HostId }> = [];
  for (const host of Object.values(state.hosts)) {
    for (const harness of host.harnesses) {
      const key = `${host.host_id}:${harness}`;
      if (seen.has(key)) continue;
      seen.add(key);
      options.push({ harness, host: host.host_id });
    }
  }
  return options;
}
