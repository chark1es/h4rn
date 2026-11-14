import { ClockIcon, RefreshCwIcon, TriangleAlertIcon } from "lucide-react";
import { useEffect } from "react";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import type {
  UsageBucketPoint,
  UsageGroup,
  UsageReport,
  UsageScope,
  UsageSeries,
  UsageTotals,
} from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteDateTime, absoluteTime, cost, duration, tokens } from "@/lib/format";
import { navigate, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { AutoResumeCards } from "./AutoResumeCard";
import {
  bucketLabel,
  bucketTotal,
  countdownPrecise,
  firstToken,
  percent,
  seriesColor,
} from "./presentation";
import {
  type ChartMetric,
  type GroupKey,
  getUsageState,
  loadUsage,
  refreshUsage,
  selectedThread,
  setGroupBy,
  setMetric,
  setRange,
  setScope,
  setTableView,
  useUsage,
} from "./usageStore";

const RANGES = [
  { key: "today", label: "Today" },
  { key: "14d", label: "14 days" },
  { key: "30d", label: "30 days" },
  { key: "all", label: "All" },
] as const;

const SCOPES: Array<{ key: UsageScope; label: string }> = [
  { key: "thread", label: "Thread" },
  { key: "workspace", label: "Workspace" },
  { key: "machine", label: "Machine" },
  { key: "global", label: "Global" },
];

const SKELETON_STATS = ["tokens", "cost", "throughput", "first-token", "active", "runtime"];

function Spark({ values }: { values: number[] }) {
  if (values.length < 2) return null;
  const width = 56;
  const height = 18;
  const max = Math.max(...values);
  const min = Math.min(...values);
  const points = values
    .map((value, index) => {
      const x = (index / (values.length - 1)) * width;
      const y = height - 2 - ((value - min) / (max - min || 1)) * (height - 4);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg className="q-spark" viewBox={`0 0 ${width} ${height}`} aria-hidden>
      <polyline
        points={points}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.5}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}

function Stat({
  label,
  value,
  unit,
  detail,
  spark,
}: {
  label: string;
  value: string;
  unit?: string | undefined;
  detail: string;
  spark?: number[] | undefined;
}) {
  return (
    <div className="q-stat">
      <div className="l">
        <span>{label}</span>
      </div>
      <div className="v num">
        {value}
        {unit ? <small>{unit}</small> : null}
      </div>
      <div className="d num">
        <span className="cap" title={detail}>
          {detail}
        </span>
        {spark ? <Spark values={spark} /> : null}
      </div>
    </div>
  );
}

function ScopePicker({ compact = false }: { compact?: boolean }) {
  const scope = useUsage((state) => state.query.scope);
  const thread = useApp((state) =>
    state.ui.selectedThreadId ? state.threads[state.ui.selectedThreadId] : undefined,
  );
  // Scopes that need a thread are offered only when one is open.
  const options = SCOPES.filter((option) => option.key === "global" || thread).map((option) => ({
    value: option.key,
    label: option.label,
  }));
  return (
    <ChipSelect
      label="Scope"
      value={scope}
      options={options}
      onChange={setScope}
      {...(compact ? { block: true } : { width: 120 })}
    />
  );
}

function RangePicker({ compact = false }: { compact?: boolean }) {
  const range = useUsage((state) => state.query.range);
  return (
    <div className={cn("q-seg", compact ? undefined : "inline")} role="tablist" aria-label="Range">
      {RANGES.map((option) => (
        <button
          key={option.key}
          type="button"
          role="tab"
          aria-selected={range === option.key}
          onClick={() => setRange(option.key)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

function GroupPicker() {
  const groupBy = useUsage((state) => state.query.groupBy);
  return (
    <div className="q-seg inline" role="tablist" aria-label="Group by">
      {(
        [
          ["harness", "Harness"],
          ["model", "Model"],
        ] as Array<[GroupKey, string]>
      ).map(([key, label]) => (
        <button
          key={key}
          type="button"
          role="tab"
          aria-selected={groupBy === key}
          onClick={() => setGroupBy(key)}
        >
          {label}
        </button>
      ))}
    </div>
  );
}

function MetricPicker() {
  const metric = useUsage((state) => state.query.metric);
  return (
    <div className="q-seg inline" role="tablist" aria-label="Chart metric">
      {(
        [
          ["tokens", "Tokens"],
          ["cost", "Cost"],
        ] as Array<[ChartMetric, string]>
      ).map(([key, label]) => (
        <button
          key={key}
          type="button"
          role="tab"
          aria-selected={metric === key}
          onClick={() => setMetric(key)}
        >
          {label}
        </button>
      ))}
    </div>
  );
}

function seriesValue(point: UsageBucketPoint, metric: ChartMetric): number {
  return metric === "cost" ? point.totals.cost_micros / 1_000_000 : bucketTotal(point, metric);
}

/** Rounds up to 1, 2, 2.5 or 5 times a power of ten, so the axis reads 0 / 50k / 100k. */
function niceCeil(value: number): number {
  const power = 10 ** Math.floor(Math.log10(value));
  const step = [1, 2, 2.5, 5, 10].find((candidate) => candidate * power >= value) ?? 10;
  return step * power;
}

function UsageChart({ report, metric }: { report: UsageReport; metric: ChartMetric }) {
  const bucket = report.bucket;
  const labels = report.buckets.map((point) => bucketLabel(point, bucket));
  const totals = report.buckets.map((_, index) =>
    report.series.reduce((sum, series) => {
      const pointValue = series.points[index];
      return sum + (pointValue ? seriesValue(pointValue, metric) : 0);
    }, 0),
  );
  const max = niceCeil(Math.max(...totals, 1));
  const ticks = [max, max / 2, 0];

  if (report.buckets.length === 0) {
    return (
      <div className="q-empty" style={{ padding: "28px 0" }}>
        <p style={{ margin: 0 }}>No metered turns in this range yet.</p>
      </div>
    );
  }

  return (
    <div className="q-chart-wrap" style={{ height: 220 }}>
      <div
        style={{
          display: "flex",
          alignItems: "flex-end",
          gap: 0,
          height: "100%",
          padding: "6px 0 24px 48px",
          position: "relative",
        }}
      >
        {ticks.map((tick) => (
          <span
            key={tick}
            className="num"
            style={{
              position: "absolute",
              left: 0,
              bottom: `calc(24px + ${(tick / max) * 190}px)`,
              width: 42,
              textAlign: "right",
              fontSize: 11,
              color: "var(--fg-3)",
              transform: "translateY(50%)",
            }}
          >
            {metric === "cost" ? `$${tick.toFixed(2)}` : tokens(tick)}
          </span>
        ))}
        {report.buckets.map((point, index) => {
          const total = totals[index] ?? 0;
          return (
            <div
              key={point.start}
              style={{
                flex: 1,
                display: "flex",
                flexDirection: "column",
                alignItems: "center",
                justifyContent: "flex-end",
                height: "100%",
                position: "relative",
              }}
              title={`${labels[index] ?? ""} · ${metric === "cost" ? cost(point.totals.cost_micros) : tokens(total)}`}
            >
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "stretch",
                  width: report.buckets.length > 24 ? 10 : 20,
                  justifyContent: "flex-end",
                }}
              >
                {[...report.series].reverse().map((series, reverseIndex) => {
                  const seriesIndex = report.series.length - 1 - reverseIndex;
                  const seriesPoint = series.points[index];
                  const value = seriesPoint ? seriesValue(seriesPoint, metric) : 0;
                  if (value <= 0) return null;
                  return (
                    <div
                      key={series.key}
                      style={{
                        height: Math.max(1, (value / max) * 186),
                        background: seriesColor(series.key, seriesIndex),
                        borderRadius: reverseIndex === 0 ? "3px 3px 0 0" : 0,
                      }}
                    />
                  );
                })}
              </div>
              {index % 2 === 0 || index === report.buckets.length - 1 ? (
                <span
                  className="num"
                  style={{ fontSize: 11, color: "var(--fg-3)", position: "absolute", bottom: -18 }}
                >
                  {labels[index]}
                </span>
              ) : null}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function Legend({ report }: { report: UsageReport }) {
  const label = (key: string) =>
    report.series_group_by === "harness" ? harnessLabel(key as never) : key;
  return (
    <div className="q-legend">
      {report.series.map((series: UsageSeries, index) => {
        const total = series.points.reduce(
          (sum, point) => sum + point.totals.input_tokens + point.totals.output_tokens,
          0,
        );
        return (
          <span key={series.key}>
            <i style={{ background: seriesColor(series.key, index) }} />
            {label(series.key)} <b className="num">{tokens(total)}</b>
          </span>
        );
      })}
    </div>
  );
}

function SeriesTable({ report, metric }: { report: UsageReport; metric: ChartMetric }) {
  const cell = (point: UsageBucketPoint | undefined): string => {
    if (!point) return metric === "cost" ? "—" : "0";
    return metric === "cost"
      ? cost(point.totals.cost_micros)
      : tokens(point.totals.input_tokens + point.totals.output_tokens);
  };
  return (
    <table className="q-tbl">
      <thead>
        <tr>
          <th>{report.bucket === "hour" ? "Hour" : "Day"}</th>
          {report.series.map((series) => (
            <th key={series.key} className="r">
              {report.series_group_by === "harness"
                ? harnessLabel(series.key as never)
                : series.key}
            </th>
          ))}
          <th className="r">Total</th>
        </tr>
      </thead>
      <tbody>
        {report.buckets.map((point, index) => {
          const rowTotal = report.series.reduce(
            (sum, series) =>
              sum + (series.points[index] ? bucketTotal(series.points[index], metric) : 0),
            0,
          );
          return (
            <tr key={point.start} className="hov">
              <td>{bucketLabel(point, report.bucket)}</td>
              {report.series.map((series) => (
                <td key={series.key} className="r num">
                  {cell(series.points[index])}
                </td>
              ))}
              <td className="r num">
                {metric === "cost" ? `$${rowTotal.toFixed(2)}` : tokens(rowTotal)}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function Breakdown({ report }: { report: UsageReport }) {
  const groupBy = useUsage((state) => state.query.groupBy);
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>By {groupBy}</h2>
        <span className="aside">Cost uses your price table · estimates, not billing</span>
      </div>
      {report.groups.length === 0 ? (
        <p className="q-faint" style={{ margin: 0, fontSize: 13 }}>
          Nothing metered in this range.
        </p>
      ) : (
        <table className="q-tbl">
          <thead>
            <tr>
              <th>{groupBy === "harness" ? "Harness" : "Model"}</th>
              <th className="r">Tokens</th>
              <th className="r">Cost</th>
              <th className="r">tok/s</th>
              <th className="r">TTFT</th>
              <th className="r">Runs</th>
              <th className="r">Active</th>
            </tr>
          </thead>
          <tbody>
            {report.groups.map((group: UsageGroup) => (
              <tr key={group.key} className="hov">
                <td>
                  {groupBy === "harness" ? (
                    <span className="q-cell-id">
                      <EntityAvatar
                        seed={harnessSeed("atlas", group.key as never)}
                        kind="harness"
                        label={group.key}
                        size={14}
                      />
                      {harnessLabel(group.key as never)}
                    </span>
                  ) : (
                    <span className="q-mono">{group.key}</span>
                  )}
                </td>
                <td className="r num">
                  {tokens(group.totals.input_tokens + group.totals.output_tokens)}
                </td>
                <td className={cn("r num", group.totals.cost_micros <= 0 && "q-faint")}>
                  {cost(group.totals.cost_micros)}
                </td>
                <td className="r num">
                  {group.totals.tps > 0 ? Math.round(group.totals.tps) : "—"}
                </td>
                <td className="r num">{firstToken(group.totals.ttft_ms)}</td>
                <td className="r num">{group.totals.turns}</td>
                <td className="r num">{duration(group.totals.active_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

/** Names the window a limit covers when its message does, else who reported it. */
function limitWindow(limit: import("@/gen").LimitStatus): string {
  const hours = limit.message?.match(/(\d+)[- ]hour/i);
  if (hours) return `${hours[1]}-hour`;
  const named = limit.message?.match(/\b(weekly|daily|monthly)\b/i);
  if (named?.[1]) return named[1].toLowerCase();
  return limit.source === "harness"
    ? "reported by the harness"
    : limit.source === "proxy"
      ? "from the proxy"
      : "from the provider";
}

function LimitRow({ limit }: { limit: import("@/gen").LimitStatus }) {
  const now = useNow(1_000);
  const thread = useApp((state) => (limit.thread_id ? state.threads[limit.thread_id] : undefined));
  const consumed = limit.limit > 0 ? percent(limit.consumed, limit.limit) : 0;
  const label =
    limit.limit > 0 ? (consumed >= 100 ? `Limited · ${consumed}%` : `${consumed}%`) : "Limited";
  const tone = consumed >= 100 ? "danger" : consumed >= 75 ? "warn" : "";
  const harness = limit.harness ?? thread?.harness ?? "claude_code";
  const host = thread?.host ?? limit.account ?? "h4rn";
  return (
    <div className="q-li" style={{ display: "block", padding: "10px 0" }}>
      <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 6 }}>
        <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
          <EntityAvatar
            seed={harnessSeed(host, harness)}
            kind="harness"
            label={harness}
            size={12}
          />
          {harnessLabel(harness)} · {limit.account ?? limit.provider ?? "account"}
          <span className="q-faint">{limitWindow(limit)}</span>
        </span>
        <span
          className={cn("num", tone ? `q-${tone}` : "")}
          style={{ fontSize: 12, fontWeight: 500 }}
        >
          {label}
        </span>
      </div>
      <div className={cn("q-meter", tone)}>
        <i style={{ width: `${limit.limit > 0 ? Math.max(consumed, 2) : 100}%` }} />
      </div>
      <div className="q-faint num" style={{ fontSize: 12, marginTop: 5 }}>
        {limit.reset_at ? (
          <>
            Resets {absoluteTime(limit.reset_at)} · in {countdownPrecise(limit.reset_at, now)}
          </>
        ) : (
          (limit.message ?? "No reset time reported.")
        )}
      </div>
    </div>
  );
}

function LimitsPanel() {
  const limits = useUsage((state) => state.limits);
  const loading = useUsage((state) => state.limitsLoading);
  const error = useUsage((state) => state.limitsError);
  if (loading && !limits) {
    return (
      <div style={{ display: "grid", gap: 10 }}>
        <div className="q-skel" style={{ height: 10, width: "72%" }} />
        <div className="q-skel" style={{ height: 6 }} />
        <div className="q-skel" style={{ height: 10, width: "54%" }} />
      </div>
    );
  }
  if (error && !limits) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        Limits are unavailable right now. {error.hint ?? error.message}
      </p>
    );
  }
  if (!limits || limits.limits.length === 0) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        No limits observed. h4rn reads them from the harness or the proxy when they appear.
      </p>
    );
  }
  return (
    <div className="q-list">
      {limits.limits.slice(0, 6).map((limit) => (
        <LimitRow key={`${limit.thread_id ?? limit.account}:${limit.observed_at}`} limit={limit} />
      ))}
    </div>
  );
}

function ResetHistory() {
  const resets = useUsage((state) => state.resets);
  const loading = useUsage((state) => state.resetsLoading);
  if (loading && !resets) {
    return <div className="q-skel" style={{ height: 10, width: "60%" }} />;
  }
  if (!resets || resets.history.length === 0) return null;
  return (
    <div className="q-sec" style={{ marginTop: 24 }}>
      <div className="q-sec-h">
        <h2>Reset history</h2>
        <span className="aside">Newest first</span>
      </div>
      <table className="q-tbl">
        <thead>
          <tr>
            <th>Observed</th>
            <th>Harness</th>
            <th>Account</th>
            <th>Reset</th>
            <th>Source</th>
          </tr>
        </thead>
        <tbody>
          {resets.history.slice(0, 10).map((item) => (
            <tr key={`${item.observed_at}:${item.account ?? ""}`} className="hov">
              <td className="num">{absoluteDateTime(item.observed_at)}</td>
              <td>{item.harness ? harnessLabel(item.harness) : "—"}</td>
              <td>{item.account ?? item.provider ?? "—"}</td>
              <td className="num">{item.reset_at ? absoluteDateTime(item.reset_at) : "unknown"}</td>
              <td className="q-faint">{item.source}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Stats({ report, phone = false }: { report: UsageReport | null; phone?: boolean }) {
  const marginBottom = phone ? 0 : 32;
  if (!report) {
    return (
      <div className="q-stats" style={{ marginBottom }} aria-hidden>
        {(phone ? SKELETON_STATS.slice(0, 4) : SKELETON_STATS).map((key) => (
          <div className="q-stat" key={key}>
            <div className="q-skel" style={{ height: 9, width: 64 }} />
            <div className="q-skel" style={{ height: 18, width: 86, marginTop: 10 }} />
            <div className="q-skel" style={{ height: 8, width: 48, marginTop: 8 }} />
          </div>
        ))}
      </div>
    );
  }
  const totals = report.totals;
  const spark = (pick: (point: UsageTotals) => number) =>
    report.buckets.map((point) => pick(point.totals));
  const activeRatio = percent(totals.active_ms, totals.active_ms + totals.idle_ms);
  return (
    <div className="q-stats" style={{ marginBottom }}>
      <Stat
        label="Tokens"
        value={tokens(totals.input_tokens + totals.output_tokens).replace(/[a-zA-Z]+$/, "")}
        unit={tokens(totals.input_tokens + totals.output_tokens).match(/[a-zA-Z]+$/)?.[0]}
        detail={`${tokens(totals.input_tokens)} in`}
        spark={spark((point) => point.input_tokens + point.output_tokens)}
      />
      <Stat
        label="Cost"
        value={cost(totals.cost_micros)}
        detail="estimate"
        spark={spark((point) => point.cost_micros / 1_000_000)}
      />
      <Stat
        label="Throughput"
        value={totals.tps > 0 ? `${Math.round(totals.tps)}` : "—"}
        unit={totals.tps > 0 ? "tok/s" : undefined}
        detail={`${totals.turns} turns`}
        spark={spark((point) => point.tps)}
      />
      <Stat
        label="First token"
        value={firstToken(totals.ttft_ms)}
        detail="mean"
        spark={spark((point) => point.ttft_ms)}
      />
      {phone ? null : (
        <Stat label="Active" value={duration(totals.active_ms)} detail={`${activeRatio}% active`} />
      )}
      {phone ? null : (
        <Stat
          label="Runtime"
          value={duration(totals.runtime_ms)}
          detail="wall time"
          spark={spark((point) => point.wall_ms)}
        />
      )}
    </div>
  );
}

function ActiveIdle({ report }: { report: UsageReport }) {
  const active = report.totals.active_ms;
  const idle = report.totals.idle_ms;
  const activeWidth = percent(active, active + idle);
  return (
    <div className="q-sec" style={{ margin: 0 }}>
      <div className="q-sec-h">
        <h2>Active vs idle</h2>
      </div>
      <div className="q-split" style={{ margin: "6px 0 10px" }}>
        <i style={{ flex: Math.max(activeWidth, 1), background: "var(--fg-2)" }} />
        <i style={{ flex: Math.max(100 - activeWidth, 1), background: "var(--fill-2)" }} />
      </div>
      <div
        style={{ display: "flex", justifyContent: "space-between", fontSize: 12 }}
        className="num"
      >
        <span>
          <b style={{ fontWeight: 500 }}>{duration(active)}</b>{" "}
          <span className="q-muted">active · {activeWidth}%</span>
        </span>
        <span className="q-muted">{duration(idle)} idle</span>
      </div>
      <p className="q-faint" style={{ fontSize: 12, margin: "10px 0 24px", lineHeight: "17px" }}>
        Idle counts time a turn waited on you: approvals, questions, and limits.
      </p>
      <div className="q-sec-h" style={{ marginBottom: 6 }}>
        <h2>Limits</h2>
        <button
          type="button"
          className="q-link"
          style={{ fontSize: 12 }}
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/resume" })
          }
        >
          Auto-resume settings
        </button>
      </div>
      <LimitsPanel />
    </div>
  );
}

function OfflineNote() {
  const status = useApp((state) => state.connection.status);
  if (status === "online") return null;
  return (
    <div className="q-resume" style={{ marginBottom: 20 }}>
      <span className="q-warn">
        <TriangleAlertIcon className="q-icon s20" strokeWidth={1.5} aria-hidden />
      </span>
      <div style={{ flex: 1 }}>
        <div style={{ fontWeight: 600 }}>Showing the last report</div>
        <div className="q-muted" style={{ marginTop: 2, fontSize: 13 }}>
          The daemon is {status}. Numbers stay as last read; refresh when it reconnects.
        </div>
      </div>
      <button type="button" className="q-btn secondary" onClick={refreshUsage}>
        <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> Retry
      </button>
    </div>
  );
}

function EmptyOrError({ report }: { report: UsageReport | null }) {
  const loading = useUsage((state) => state.reportLoading);
  const error = useUsage((state) => state.reportError);
  if (error && !report) {
    return (
      <div className="q-empty" style={{ padding: "28px 0" }}>
        <p style={{ margin: 0 }}>{error.message}</p>
        <p className="q-faint" style={{ margin: "4px 0 12px" }}>
          {error.hint ?? "Try a wider range or refresh."}
        </p>
        <button type="button" className="q-btn secondary" onClick={() => void loadUsage()}>
          Try again
        </button>
      </div>
    );
  }
  if (!report && loading) {
    return (
      <div className="q-sec">
        <div className="q-skel" style={{ height: 220 }} />
      </div>
    );
  }
  return null;
}

export function UsagePage() {
  const isPhone = useIsPhone();
  const report = useUsage((state) => state.report);
  const tableView = useUsage((state) => state.query.tableView);
  const metric = useUsage((state) => state.query.metric);
  const groupBy = useUsage((state) => state.query.groupBy);
  const scope = useUsage((state) => state.query.scope);
  const scopeId = useUsage((state) => state.query.scopeId);
  const thread = useApp((state) =>
    state.ui.selectedThreadId ? state.threads[state.ui.selectedThreadId] : undefined,
  );
  const connection = useApp((state) => state.connection.status);

  // A direct link mounts the page before the handshake finishes; asking then
  // would surface a spurious "not connected" error, so wait for online.
  useEffect(() => {
    if (connection === "online") startPage();
  }, [connection]);

  useEffect(() => {
    if (scope === "global") return;
    const expected =
      scope === "thread"
        ? thread?.thread_id
        : scope === "workspace"
          ? thread?.workspace_path
          : thread?.host;
    if (!thread || expected !== scopeId) {
      if (thread) setScope(scope);
      else setScope("global");
    }
  }, [scope, scopeId, thread]);

  const context = thread
    ? scope === "workspace"
      ? thread.workspace_path
      : scope === "machine"
        ? thread.host
        : thread.title
    : "All threads on every machine";

  if (isPhone) {
    return (
      <main className="q-main" aria-label="Usage">
        <div className="q-ph-nav">
          <span style={{ width: 44 }} />
          <span className="ttl" />
          <ScopePicker compact />
        </div>
        <h1 className="q-ph-lt">Usage</h1>
        <div className="q-ph-body">
          <div style={{ padding: "0 16px 10px" }}>
            <RangePicker compact />
          </div>
          <OfflineNote />
          <AutoResumeCards compact />
          <div className="q-ph-group">
            <Stats report={report} phone />
          </div>
          <div className="q-ph-group" style={{ padding: "14px 16px" }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 8,
                marginBottom: 10,
              }}
            >
              <GroupPicker />
              <MetricPicker />
            </div>
            {report ? <UsageChart report={report} metric={metric} /> : null}
            {report ? <Legend report={report} /> : null}
            {!report ? <EmptyOrError report={report} /> : null}
          </div>
          <div className="q-ph-gh">Limits</div>
          <div className="q-ph-group" style={{ padding: "4px 16px 10px" }}>
            <LimitsPanel />
          </div>
          <div className="q-ph-gh">By {groupBy}</div>
          <div className="q-ph-group" style={{ padding: "4px 16px 12px" }}>
            <Breakdown report={report ?? emptyReport()} />
          </div>
        </div>
      </main>
    );
  }

  return (
    <main className="q-main" aria-label="Usage">
      <header className="q-page-head">
        <h1>Usage</h1>
        <span className="grow" />
        <ScopePicker />
        <RangePicker />
        <button
          type="button"
          className="q-btn secondary"
          title="Refresh usage"
          aria-label="Refresh usage"
          onClick={refreshUsage}
        >
          <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
      </header>

      <div className="q-page">
        <div className="q-wrap">
          <p className="q-faint" style={{ fontSize: 12, margin: "-8px 0 16px" }}>
            {context}
          </p>
          <OfflineNote />
          <AutoResumeCards />
          <Stats report={report} />

          <div
            style={{ display: "grid", gridTemplateColumns: "1fr 300px", gap: 40, marginBottom: 36 }}
          >
            <div className="q-sec" style={{ margin: 0 }}>
              <div className="q-sec-h">
                <h2>
                  {metric === "cost" ? "Cost" : "Tokens"} per{" "}
                  {report?.bucket === "hour" ? "hour" : "day"} by {groupBy}
                </h2>
                <button
                  type="button"
                  className="q-btn ghost"
                  style={{ height: 24 }}
                  onClick={() => setTableView(!tableView)}
                >
                  {tableView ? "View as chart" : "View as table"}
                </button>
              </div>
              <div
                style={{
                  display: "flex",
                  gap: 8,
                  alignItems: "center",
                  flexWrap: "wrap",
                  marginBottom: 10,
                }}
              >
                <GroupPicker />
                <MetricPicker />
              </div>
              {report ? (
                <>
                  <Legend report={report} />
                  {tableView ? (
                    <SeriesTable report={report} metric={metric} />
                  ) : (
                    <UsageChart report={report} metric={metric} />
                  )}
                </>
              ) : null}
              <EmptyOrError report={report} />
            </div>

            {report ? (
              <ActiveIdle report={report} />
            ) : (
              <div className="q-sec" style={{ margin: 0 }}>
                <div className="q-skel" style={{ height: 180 }} />
              </div>
            )}
          </div>

          {report ? <Breakdown report={report} /> : null}
          <ResetHistory />
          <p className="q-faint" style={{ fontSize: 12, marginTop: 16 }}>
            <ClockIcon className="q-icon s12" aria-hidden /> Usage is counted by h4rn itself; a
            missing price shows “—” rather than $0.00. Costs are estimates, not billing.
          </p>
        </div>
      </div>
    </main>
  );
}

function startPage(): void {
  if (getUsageState().query.scope !== "global" && !selectedThread()) setScope("global");
  refreshUsage();
}

function emptyReport(): UsageReport {
  return {
    scope: "global",
    scope_id: null,
    range: { since: null, until: null },
    group_by: "harness",
    bucket: null,
    series_group_by: null,
    totals: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cached_creation_tokens: 0,
      cost_micros: 0,
      ttft_ms: 0,
      tps: 0,
      wall_ms: 0,
      generation_ms: 0,
      active_ms: 0,
      idle_ms: 0,
      runtime_ms: 0,
      turns: 0,
    },
    buckets: [],
    series: [],
    groups: [],
  };
}
