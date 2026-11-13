import { CheckCircle2Icon, ChevronDownIcon, ClockIcon, TriangleAlertIcon } from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessSeed, machineSeed } from "@/lib/entities";

const DAYS = [
  "Oct 30",
  "Oct 31",
  "Nov 1",
  "Nov 2",
  "Nov 3",
  "Nov 4",
  "Nov 5",
  "Nov 6",
  "Nov 7",
  "Nov 8",
  "Nov 9",
  "Nov 10",
  "Nov 11",
  "Nov 12",
];

const SERIES = [
  {
    key: "Claude Code",
    color: "var(--s1)",
    values: [62, 71, 18, 12, 84, 92, 77, 96, 88, 21, 15, 94, 102, 58],
  },
  {
    key: "Codex",
    color: "var(--s2)",
    values: [28, 33, 9, 4, 41, 38, 46, 52, 44, 11, 6, 49, 55, 31],
  },
  {
    key: "OpenCode",
    color: "var(--s3)",
    values: [12, 9, 0, 0, 14, 18, 11, 16, 13, 0, 2, 15, 19, 8],
  },
  { key: "Antigravity", color: "var(--s4)", values: [4, 6, 2, 0, 8, 5, 9, 7, 10, 3, 0, 9, 12, 6] },
];

const TOTALS = SERIES.map((series) => series.values.reduce((sum, value) => sum + value, 0));

const MODELS = [
  {
    host: "atlas",
    harness: "claude_code",
    model: "claude-sonnet-5-5",
    tokens: "682k",
    cost: "$10.24",
    tps: "58",
    ttft: "0.7 s",
    runs: "118",
    active: "21h 40m",
  },
  {
    host: "atlas",
    harness: "claude_code",
    model: "claude-opus-5-5",
    tokens: "198k",
    cost: "$4.95",
    tps: "41",
    ttft: "1.1 s",
    runs: "24",
    active: "6h 02m",
  },
  {
    host: "orion",
    harness: "codex",
    model: "gpt-5.1-codex",
    tokens: "247k",
    cost: "$2.31",
    tps: "61",
    ttft: "0.9 s",
    runs: "61",
    active: "9h 15m",
  },
  {
    host: "atlas-wsl",
    harness: "opencode",
    model: "claude-haiku-5-5",
    tokens: "57k",
    cost: "$0.18",
    tps: "88",
    ttft: "0.5 s",
    runs: "17",
    active: "1h 31m",
  },
  {
    host: "orion",
    harness: "antigravity",
    model: "gemini-3-pro",
    tokens: "51k",
    cost: "—",
    tps: "47",
    ttft: "1.4 s",
    runs: "9",
    active: "2h 44m",
  },
];

function Spark({ values }: { values: number[] }) {
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
  unit?: string;
  detail: string;
  spark?: number[];
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
        <span>{detail}</span>
        {spark ? <Spark values={spark} /> : null}
      </div>
    </div>
  );
}

function PhoneDayChart() {
  const max = 200;
  return (
    <div className="q-ph-group" style={{ padding: "14px 16px 12px" }}>
      <div className="q-seg" style={{ marginBottom: 14 }} role="tablist" aria-label="Usage metric">
        <button type="button" role="tab" aria-selected>
          Tokens
        </button>
        <button type="button" role="tab" aria-selected={false}>
          Cost
        </button>
        <button type="button" role="tab" aria-selected={false}>
          Speed
        </button>
      </div>
      <div
        role="img"
        aria-label="Tokens per day, last 14 days"
        style={{
          display: "flex",
          alignItems: "flex-end",
          gap: 4,
          height: 120,
          borderBottom: "1px solid var(--line-2)",
          paddingBottom: 3,
        }}
      >
        {DAYS.map((day, index) => (
          <div
            key={day}
            style={{
              flex: 1,
              display: "flex",
              flexDirection: "column",
              alignItems: "stretch",
              gap: 0,
            }}
            title={day}
          >
            {[3, 2, 1, 0].map((seriesIndex) => {
              const series = SERIES[seriesIndex];
              const value = series?.values[index] ?? 0;
              if (!series || value === 0) return null;
              return (
                <div
                  key={series.key}
                  style={{
                    height: Math.max(0, (value / max) * 104),
                    background: series.color,
                    borderRadius: seriesIndex === 3 ? "2px 2px 0 0" : 0,
                  }}
                />
              );
            })}
          </div>
        ))}
      </div>
      <div
        className="num"
        style={{
          display: "flex",
          justifyContent: "space-between",
          fontSize: 11,
          color: "var(--fg-3)",
          marginTop: 5,
        }}
      >
        <span>Oct 30</span>
        <span>Today</span>
      </div>
      <div
        className="q-legend"
        style={{
          marginTop: 10,
          display: "grid",
          gridTemplateColumns: "1fr 1fr",
          columnGap: 12,
          rowGap: 6,
          fontSize: 12,
        }}
      >
        {SERIES.map((series) => (
          <span key={series.key} style={{ minWidth: 0 }}>
            <i style={{ background: series.color }} />
            {series.key}
          </span>
        ))}
      </div>
    </div>
  );
}

function DayChart() {
  const max = 200;
  return (
    <div className="q-chart-wrap" style={{ height: 220 }}>
      <div
        style={{
          display: "flex",
          alignItems: "flex-end",
          gap: 0,
          height: "100%",
          padding: "6px 0 24px 36px",
          position: "relative",
        }}
      >
        {[0, 50, 100, 150, 200].map((tick) => (
          <span
            key={tick}
            className="num"
            style={{
              position: "absolute",
              left: 0,
              bottom: `calc(24px + ${(tick / max) * 190}px)`,
              width: 30,
              textAlign: "right",
              fontSize: 11,
              color: "var(--fg-3)",
              transform: "translateY(50%)",
            }}
          >
            {tick ? `${tick}k` : "0"}
          </span>
        ))}
        {DAYS.map((day, index) => (
          <div
            key={day}
            style={{
              flex: 1,
              display: "flex",
              flexDirection: "column",
              alignItems: "center",
              justifyContent: "flex-end",
              height: "100%",
              gap: 2,
              position: "relative",
            }}
            title={`${day} · ${SERIES.reduce((sum, series) => sum + (series.values[index] ?? 0), 0)}k`}
          >
            <div
              style={{
                display: "flex",
                flexDirection: "column",
                alignItems: "stretch",
                width: 20,
                gap: 0,
              }}
            >
              {[3, 2, 1, 0].map((seriesIndex) => {
                const series = SERIES[seriesIndex];
                const value = series?.values[index] ?? 0;
                if (!series || value === 0) return null;
                return (
                  <div
                    key={series.key}
                    style={{
                      height: Math.max(0, (value / max) * 186),
                      background: series.color,
                      borderRadius: seriesIndex === 3 ? "3px 3px 0 0" : 0,
                      marginTop: 0,
                    }}
                  />
                );
              })}
            </div>
            {index % 2 === 0 || index === DAYS.length - 1 ? (
              <span
                className="num"
                style={{ fontSize: 11, color: "var(--fg-3)", position: "absolute", bottom: -18 }}
              >
                {day.replace("Nov ", "")}
              </span>
            ) : null}
          </div>
        ))}
      </div>
    </div>
  );
}

/** Static Phase 4 surface: whitespace/typography port of the approved Usage page. */
export function UsagePage() {
  const isPhone = useIsPhone();
  const [tableView, setTableView] = useState(false);

  if (isPhone) {
    return (
      <main className="q-main" aria-label="Usage">
        <div className="q-ph-nav">
          <span style={{ width: 44 }} />
          <span className="ttl" />
          <button type="button" className="navbtn" aria-label="Range">
            14 days <ChevronDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
        <h1 className="q-ph-lt">Usage</h1>
        <div className="q-ph-body">
          <div className="q-ph-group" style={{ padding: "14px 16px" }}>
            <div style={{ display: "flex", gap: 10, alignItems: "flex-start" }}>
              <span className="q-warn" style={{ paddingTop: 2 }}>
                <ClockIcon className="q-icon s18" strokeWidth={1.5} aria-hidden />
              </span>
              <div style={{ flex: 1 }}>
                <div style={{ fontWeight: 600, fontSize: 16 }}>Migrate session store is paused</div>
                <div className="q-muted" style={{ fontSize: 14, lineHeight: "19px", marginTop: 2 }}>
                  Codex on orion hit its 5-hour limit.
                </div>
              </div>
            </div>
            <div
              className="num"
              style={{
                fontSize: 30,
                lineHeight: "36px",
                fontWeight: 600,
                letterSpacing: "-0.02em",
                marginTop: 12,
              }}
            >
              2h 10m 00s
            </div>
            <div className="q-faint" style={{ fontSize: 14, marginBottom: 14 }}>
              Auto-resumes at 14:32
            </div>
            <div style={{ display: "flex", gap: 10 }}>
              <button type="button" className="q-btn secondary sm" style={{ flex: 1 }}>
                Cancel
              </button>
              <button type="button" className="q-btn primary sm" style={{ flex: 1 }}>
                Resume now
              </button>
            </div>
          </div>
          <div className="q-ph-group" style={{ display: "grid", gridTemplateColumns: "1fr 1fr" }}>
            {[
              ["Tokens", "1.24M", "+12% vs prior"],
              ["Cost", "$18.42", "estimate"],
              ["Throughput", "54 tok/s", "p50"],
              ["First token", "0.8 s", "p50"],
            ].map(([label, value, detail], index) => (
              <div
                key={label}
                style={{
                  padding: "12px 16px",
                  borderLeft: index % 2 === 1 ? "1px solid var(--line)" : undefined,
                  borderTop: index > 1 ? "1px solid var(--line)" : undefined,
                }}
              >
                <div className="q-muted" style={{ fontSize: 13 }}>
                  {label}
                </div>
                <div
                  className="num"
                  style={{ fontSize: 22, fontWeight: 600, letterSpacing: "-0.015em", marginTop: 6 }}
                >
                  {value}
                </div>
                <div className="q-faint" style={{ fontSize: 13, marginTop: 4 }}>
                  {detail}
                </div>
              </div>
            ))}
          </div>
          <PhoneDayChart />
          <div className="q-ph-gh">Limits</div>
          <div className="q-ph-group">
            <div className="q-ph-li" style={{ display: "block" }}>
              <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 8 }}>
                <span>Codex · orion</span>
                <span className="q-danger" style={{ fontSize: 14, fontWeight: 500 }}>
                  Limited
                </span>
              </div>
              <div className="q-meter full">
                <i style={{ width: "100%" }} />
              </div>
              <div className="q-faint" style={{ fontSize: 13, marginTop: 6 }}>
                5-hour · resets 14:32
              </div>
            </div>
            <div className="q-ph-li" style={{ display: "block" }}>
              <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 8 }}>
                <span>Claude Code · atlas</span>
                <span className="q-warn num" style={{ fontSize: 14, fontWeight: 500 }}>
                  78%
                </span>
              </div>
              <div className="q-meter warn">
                <i style={{ width: "78%" }} />
              </div>
              <div className="q-faint" style={{ fontSize: 13, marginTop: 6 }}>
                Weekly · resets Mon 09:00
              </div>
            </div>
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
        <div className="q-seg inline" role="tablist">
          <button type="button" role="tab" aria-selected={false}>
            Today
          </button>
          <button type="button" role="tab" aria-selected>
            14 days
          </button>
          <button type="button" role="tab" aria-selected={false}>
            30 days
          </button>
        </div>
        <button type="button" className="q-btn secondary">
          All hosts{" "}
          <svg viewBox="0 0 24 24" className="q-icon s12" aria-hidden>
            <path d="m6 9 6 6 6-6" />
          </svg>
        </button>
        <button type="button" className="q-btn secondary">
          All harnesses{" "}
          <svg viewBox="0 0 24 24" className="q-icon s12" aria-hidden>
            <path d="m6 9 6 6 6-6" />
          </svg>
        </button>
      </header>

      <div className="q-page">
        <div className="q-wrap">
          <div className="q-resume" style={{ marginBottom: 28 }}>
            <span className="q-warn">
              <ClockIcon className="q-icon s20" strokeWidth={1.5} aria-hidden />
            </span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontWeight: 600 }}>Migrate session store to Redis is paused</div>
              <div className="q-muted" style={{ marginTop: 2 }}>
                Codex on orion hit its 5-hour limit at 09:32. h4rn resumes the turn at 14:32 with
                its saved context.
              </div>
            </div>
            <div style={{ textAlign: "right" }}>
              <div className="cd num">2h 10m 00s</div>
              <div className="q-faint" style={{ fontSize: 12 }}>
                until auto-resume
              </div>
            </div>
            <div style={{ display: "flex", gap: 6 }}>
              <button type="button" className="q-btn ghost">
                Fail over…
              </button>
              <button type="button" className="q-btn secondary">
                Cancel auto-resume
              </button>
              <button type="button" className="q-btn primary">
                Resume now
              </button>
            </div>
          </div>

          <div className="q-stats" style={{ marginBottom: 32 }}>
            <Stat
              label="Tokens"
              value="1.24"
              unit="M"
              detail="+12% vs prior"
              spark={[40, 52, 18, 14, 62, 70, 64, 72, 66, 20, 14, 70, 78, 46]}
            />
            <Stat
              label="Cost"
              value="$18.42"
              detail="estimate"
              spark={[9, 11, 4, 3, 14, 16, 14, 17, 15, 5, 3, 16, 18, 10]}
            />
            <Stat
              label="Throughput p50"
              value="54"
              unit="tok/s"
              detail="p95 71"
              spark={[51, 55, 49, 50, 56, 58, 52, 54, 57, 50, 49, 55, 56, 54]}
            />
            <Stat
              label="First token p50"
              value="0.8"
              unit="s"
              detail="p95 2.1 s"
              spark={[0.9, 0.8, 1.1, 1, 0.8, 0.7, 0.9, 0.8, 0.8, 1, 1.2, 0.7, 0.8, 0.8]}
            />
            <Stat label="Active" value="41h" unit="12m" detail="idle 19h 48m" />
            <Stat
              label="Runs"
              value="229"
              detail="3 failed"
              spark={[14, 18, 4, 2, 22, 25, 19, 24, 21, 6, 3, 23, 26, 12]}
            />
          </div>

          <div
            style={{ display: "grid", gridTemplateColumns: "1fr 300px", gap: 40, marginBottom: 36 }}
          >
            <div className="q-sec" style={{ margin: 0 }}>
              <div className="q-sec-h">
                <h2>Tokens per day by harness</h2>
                <button
                  type="button"
                  className="q-btn ghost"
                  style={{ height: 24 }}
                  onClick={() => setTableView((value) => !value)}
                >
                  {tableView ? "View as chart" : "View as table"}
                </button>
              </div>
              <div className="q-legend">
                {SERIES.map((series, index) => (
                  <span key={series.key}>
                    <i style={{ background: series.color }} />
                    {series.key} <b className="num">{TOTALS[index]}k</b>
                  </span>
                ))}
              </div>
              {tableView ? (
                <table className="q-tbl">
                  <thead>
                    <tr>
                      <th>Day</th>
                      {SERIES.map((series) => (
                        <th key={series.key} className="r">
                          {series.key}
                        </th>
                      ))}
                      <th className="r">Total</th>
                    </tr>
                  </thead>
                  <tbody>
                    {DAYS.map((day, index) => (
                      <tr key={day}>
                        <td>{day}</td>
                        {SERIES.map((series) => (
                          <td key={series.key} className="r num">
                            {series.values[index] ?? 0}k
                          </td>
                        ))}
                        <td className="r num">
                          {SERIES.reduce((sum, series) => sum + (series.values[index] ?? 0), 0)}k
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ) : (
                <DayChart />
              )}
            </div>

            <div className="q-sec" style={{ margin: 0 }}>
              <div className="q-sec-h">
                <h2>Active vs idle</h2>
              </div>
              <div className="q-split" style={{ margin: "6px 0 10px" }}>
                <i style={{ flex: 41.2, background: "var(--fg-2)" }} />
                <i style={{ flex: 19.8, background: "var(--fill-2)" }} />
              </div>
              <div
                style={{ display: "flex", justifyContent: "space-between", fontSize: 12 }}
                className="num"
              >
                <span>
                  <b style={{ fontWeight: 500 }}>41h 12m</b>{" "}
                  <span className="q-muted">active · 68%</span>
                </span>
                <span className="q-muted">19h 48m idle</span>
              </div>
              <p
                className="q-faint"
                style={{ fontSize: 12, margin: "10px 0 24px", lineHeight: "17px" }}
              >
                Idle counts time a turn waited on you: approvals, questions, and limits.
              </p>
              <div className="q-sec-h" style={{ marginBottom: 6 }}>
                <h2>Limits</h2>
                <a className="q-faint" style={{ fontSize: 12 }} href="#limits">
                  Auto-resume settings
                </a>
              </div>
              <div className="q-list">
                {[
                  {
                    host: "orion",
                    harness: "codex",
                    window: "5-hour",
                    tone: "danger",
                    label: "Limited",
                    width: 100,
                    note: "Resets 14:32 · in 2h 10m",
                    avatar: "machine" as const,
                    size: 12,
                  },
                  {
                    host: "atlas",
                    harness: "claude_code",
                    window: "weekly",
                    tone: "warn",
                    label: "78%",
                    width: 78,
                    note: "Resets Mon 09:00 · in 3d 20h",
                    avatar: "machine" as const,
                    size: 12,
                  },
                  {
                    host: "orion",
                    harness: "antigravity",
                    window: "daily",
                    tone: "",
                    label: "23%",
                    width: 23,
                    note: "Resets 00:00 · in 11h 38m",
                    avatar: "machine" as const,
                    size: 12,
                  },
                ].map((limit) => (
                  <div
                    key={`${limit.host}-${limit.harness}`}
                    className="q-li"
                    style={{ display: "block", padding: "10px 0" }}
                  >
                    <div
                      style={{ display: "flex", justifyContent: "space-between", marginBottom: 6 }}
                    >
                      <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
                        <EntityAvatar
                          seed={harnessSeed(limit.host, limit.harness as never)}
                          kind="harness"
                          label={limit.harness}
                          size={limit.size}
                        />
                        {limit.harness === "claude_code"
                          ? "Claude Code"
                          : limit.harness === "codex"
                            ? "Codex"
                            : "Antigravity"}{" "}
                        · {limit.host} <span className="q-faint">{limit.window}</span>
                      </span>
                      <span
                        className={`num ${limit.tone ? `q-${limit.tone}` : ""}`}
                        style={{ fontSize: 12, fontWeight: 500 }}
                      >
                        {limit.label}
                      </span>
                    </div>
                    <div className={`q-meter ${limit.tone}`}>
                      <i style={{ width: `${limit.width}%` }} />
                    </div>
                    <div className="q-faint num" style={{ fontSize: 12, marginTop: 5 }}>
                      {limit.note}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>

          <div className="q-sec">
            <div className="q-sec-h">
              <h2>By model</h2>
              <span className="aside">Cost uses your price table · estimates, not billing</span>
            </div>
            <table className="q-tbl">
              <thead>
                <tr>
                  <th>Host</th>
                  <th>Harness</th>
                  <th>Model</th>
                  <th className="r">Tokens</th>
                  <th className="r">Cost</th>
                  <th className="r">tok/s p50</th>
                  <th className="r">TTFT p50</th>
                  <th className="r">Runs</th>
                  <th className="r">Active</th>
                </tr>
              </thead>
              <tbody>
                {MODELS.map((row) => (
                  <tr key={`${row.host}-${row.model}`} className="hov">
                    <td>
                      <span className="q-cell-id">
                        <EntityAvatar
                          seed={machineSeed(row.host)}
                          kind="machine"
                          label={row.host}
                          size={14}
                        />
                        {row.host === "atlas-wsl" ? "Ubuntu-24.04" : row.host}
                      </span>
                    </td>
                    <td>
                      <span className="q-cell-id">
                        <EntityAvatar
                          seed={harnessSeed(row.host, row.harness as never)}
                          kind="harness"
                          label={row.harness}
                          size={14}
                        />
                        {row.harness === "claude_code"
                          ? "Claude Code"
                          : row.harness === "codex"
                            ? "Codex"
                            : row.harness === "opencode"
                              ? "OpenCode"
                              : "Antigravity"}
                      </span>
                    </td>
                    <td className="mono">{row.model}</td>
                    <td className="r num">{row.tokens}</td>
                    <td className={`r num ${row.cost === "—" ? "q-faint" : ""}`}>{row.cost}</td>
                    <td className="r num">{row.tps}</td>
                    <td className="r num">{row.ttft}</td>
                    <td className="r num">{row.runs}</td>
                    <td className="r num">{row.active}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <p className="q-faint" style={{ fontSize: 12, marginTop: 16 }}>
            <CheckCircle2Icon className="q-icon s12" aria-hidden /> Usage is counted by h4rn itself;{" "}
            <TriangleAlertIcon className="q-icon s12" aria-hidden /> a missing price shows “—”
            rather than $0.00.
          </p>
        </div>
      </div>
    </main>
  );
}
