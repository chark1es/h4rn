import { useSyncExternalStore } from "react";
import type {
  AutoResumePolicy,
  LimitStatus,
  LimitsReport,
  ProtoError,
  Resets,
  ThreadId,
  Usage,
  UsageBucket,
  UsageRange,
  UsageReport,
  UsageScope,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { onUsageFrame } from "@/lib/client/frames";
import {
  applyLimitObservation,
  applyResumeSchedule,
  getState as getAppState,
  getClient,
} from "@/lib/store/appStore";

export type RangeKey = "today" | "14d" | "30d" | "all";
export type GroupKey = "harness" | "model";
export type ChartMetric = "tokens" | "cost";

export type LiveUsage = {
  thread_id: ThreadId;
  run_id: string | null;
  usage: Usage;
  ttft_ms: number;
  tps: number | null;
  elapsed_ms: number;
  tool_ms: number;
  at: string;
};

export type UsageQuery = {
  scope: UsageScope;
  scopeId: string | null;
  range: RangeKey;
  groupBy: GroupKey;
  tableView: boolean;
  metric: ChartMetric;
};

export type UsageState = {
  query: UsageQuery;
  report: UsageReport | null;
  reportLoading: boolean;
  reportError: ProtoError | null;
  limits: LimitsReport | null;
  limitsLoading: boolean;
  limitsError: ProtoError | null;
  resets: Resets | null;
  resetsLoading: boolean;
  resetsError: ProtoError | null;
  live: Record<ThreadId, LiveUsage>;
  pending: Record<ThreadId, "cancel" | "resume" | undefined>;
  actionError: ProtoError | null;
  policySaving: boolean;
  policyError: ProtoError | null;
  lastLoadedAt: number | null;
};

const DAY = 24 * 60 * 60 * 1000;

/** The presentation filter is only a `since`; all accounting stays in Rust. */
export function rangeFor(key: RangeKey, now = Date.now()): UsageRange {
  if (key === "all") return { since: null, until: null };
  if (key === "today") {
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    return { since: start.toISOString(), until: null };
  }
  const days = key === "14d" ? 14 : 30;
  return { since: new Date(now - days * DAY).toISOString(), until: null };
}

export function bucketFor(key: RangeKey): UsageBucket {
  return key === "today" ? "hour" : "day";
}

function initialState(): UsageState {
  return {
    query: {
      scope: "global",
      scopeId: null,
      range: "14d",
      groupBy: "harness",
      tableView: false,
      metric: "tokens",
    },
    report: null,
    reportLoading: false,
    reportError: null,
    limits: null,
    limitsLoading: false,
    limitsError: null,
    resets: null,
    resetsLoading: false,
    resetsError: null,
    live: {},
    pending: {},
    actionError: null,
    policySaving: false,
    policyError: null,
    lastLoadedAt: null,
  };
}

type Listener = () => void;

let state = initialState();
const listeners = new Set<Listener>();
let reportRequest = 0;

function setState(next: UsageState): void {
  state = next;
  for (const listener of listeners) listener();
}

function patch(next: Partial<UsageState>): void {
  setState({ ...state, ...next });
}

export function getUsageState(): UsageState {
  return state;
}

export function useUsage<T>(selector: (value: UsageState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** Token totals plotted by the chart; cached input is already inside input. */
export function seriesTotals(report: UsageReport | null): Array<{ key: string; total: number }> {
  if (!report) return [];
  return report.series.map((series) => ({
    key: series.key,
    total: series.points.reduce(
      (sum, point) => sum + point.totals.input_tokens + point.totals.output_tokens,
      0,
    ),
  }));
}

export function totalsValue(
  totals: { input_tokens: number; output_tokens: number; cost_micros: number },
  metric: ChartMetric,
): number {
  if (metric === "cost") return totals.cost_micros / 1_000_000;
  return totals.input_tokens + totals.output_tokens;
}

export async function loadUsage(): Promise<void> {
  const request = ++reportRequest;
  patch({ reportLoading: true, reportError: null });
  const query = state.query;
  try {
    const response = await getClient().request({
      type: "get_usage",
      scope: query.scope,
      scope_id: query.scope === "global" ? null : query.scopeId,
      range: rangeFor(query.range),
      group_by: query.groupBy,
      bucket: bucketFor(query.range),
      series_group_by: query.groupBy,
      include_children: query.scope === "thread",
    });
    if (response.type !== "usage_report" || request !== reportRequest) return;
    patch({
      report: response.report,
      reportLoading: false,
      reportError: null,
      lastLoadedAt: Date.now(),
    });
  } catch (error) {
    if (request !== reportRequest) return;
    patch({ reportLoading: false, reportError: toProtoError(error) });
  }
}

export async function loadLimits(): Promise<void> {
  patch({ limitsLoading: true, limitsError: null });
  try {
    const response = await getClient().request({ type: "get_limits" });
    if (response.type !== "limits") return;
    patch({ limits: response.report, limitsLoading: false, limitsError: null });
  } catch (error) {
    patch({ limitsLoading: false, limitsError: toProtoError(error) });
  }
}

export async function loadResets(): Promise<void> {
  patch({ resetsLoading: true, resetsError: null });
  try {
    const response = await getClient().request({ type: "list_resets" });
    if (response.type !== "resets") return;
    patch({ resets: response.resets, resetsLoading: false, resetsError: null });
  } catch (error) {
    patch({ resetsLoading: false, resetsError: toProtoError(error) });
  }
}

export function refreshUsage(): void {
  void loadUsage();
  void loadLimits();
  void loadResets();
}

export function setScope(scope: UsageScope): void {
  const thread = selectedThread();
  const query: UsageQuery = {
    ...state.query,
    scope,
    scopeId: scope === "global" ? null : scopeIdFor(scope, thread),
  };
  patch({ query });
  void loadUsage();
}

export function setRange(range: RangeKey): void {
  patch({ query: { ...state.query, range } });
  void loadUsage();
}

export function setGroupBy(groupBy: GroupKey): void {
  patch({ query: { ...state.query, groupBy } });
  void loadUsage();
}

export function setTableView(tableView: boolean): void {
  patch({ query: { ...state.query, tableView } });
}

export function setMetric(metric: ChartMetric): void {
  patch({ query: { ...state.query, metric } });
}

export function selectedThread() {
  const app = getAppState();
  const threadId =
    app.ui.selectedThreadId ?? (app.ui.route.kind === "thread" ? app.ui.route.threadId : null);
  return threadId ? (app.threads[threadId] ?? null) : null;
}

function scopeIdFor(scope: UsageScope, thread: ReturnType<typeof selectedThread>): string | null {
  if (!thread) return null;
  if (scope === "thread") return thread.thread_id;
  if (scope === "workspace") return thread.workspace_path;
  if (scope === "machine") return thread.host;
  return null;
}

export async function cancelResume(threadId: ThreadId): Promise<void> {
  patch({ pending: { ...state.pending, [threadId]: "cancel" }, actionError: null });
  try {
    const response = await getClient().request({ type: "cancel_resume", thread_id: threadId });
    if (response.type === "resume_changed") applyResumeSchedule(response.schedule);
    await Promise.all([loadResets(), loadLimits()]);
  } catch (error) {
    patch({ actionError: toProtoError(error) });
  } finally {
    patch({ pending: { ...state.pending, [threadId]: undefined } });
  }
}

export async function resumeNow(threadId: ThreadId): Promise<void> {
  patch({ pending: { ...state.pending, [threadId]: "resume" }, actionError: null });
  try {
    const response = await getClient().request({ type: "resume_now", thread_id: threadId });
    if (response.type === "resume_changed") applyResumeSchedule(response.schedule);
    await Promise.all([loadResets(), loadLimits()]);
  } catch (error) {
    patch({ actionError: toProtoError(error) });
  } finally {
    patch({ pending: { ...state.pending, [threadId]: undefined } });
  }
}

export async function setAutoResumePolicy(policy: AutoResumePolicy): Promise<boolean> {
  patch({ policySaving: true, policyError: null });
  try {
    await getClient().request({ type: "set_auto_resume_policy", policy });
    await loadLimits();
    return true;
  } catch (error) {
    patch({ policyError: toProtoError(error) });
    return false;
  } finally {
    patch({ policySaving: false });
  }
}

let watching = false;

/** Subscribes once for the life of the app; ticks arrive outside the journal. */
export function startUsageWatch(): void {
  if (watching) return;
  watching = true;
  onUsageFrame((event) => {
    if (event.type === "usage_tick") {
      setState({
        ...state,
        live: {
          ...state.live,
          [event.thread_id]: {
            thread_id: event.thread_id,
            run_id: event.run_id,
            usage: event.usage,
            ttft_ms: event.ttft_ms,
            tps: event.tps,
            elapsed_ms: event.elapsed_ms,
            tool_ms: event.tool_ms,
            at: event.at,
          },
        },
      });
      return;
    }
    if (event.type === "limit_status") {
      applyLimitObservation(event.status);
      const report = state.limits;
      if (report) {
        const limits: LimitStatus[] = [
          event.status,
          ...report.limits.filter(
            (limit) =>
              limit.observed_at !== event.status.observed_at ||
              limit.thread_id !== event.status.thread_id,
          ),
        ].slice(0, 40);
        setState({ ...state, limits: { ...report, limits } });
      }
      void loadResets();
      return;
    }
    applyResumeSchedule(event.schedule);
    void loadResets();
  });
}

export function resetUsageStoreForTests(): void {
  state = initialState();
  reportRequest += 1;
  for (const listener of listeners) listener();
}
