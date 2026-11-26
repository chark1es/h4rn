// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import type { LimitStatus, ResumeSchedule, UsageReport, UsageTotals } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";

const NOW = Date.parse("2026-03-10T12:00:00Z");

const mock = vi.hoisted(() => ({
  app: {
    connection: { status: "online" as string },
    ui: { selectedThreadId: null as string | null },
    threads: {} as Record<string, unknown>,
  },
  responses: {} as Record<string, unknown>,
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.app),
  getState: () => mock.app,
  getClient: () => ({
    request: async (body: { type: string }) => mock.responses[body.type],
  }),
  navigate: vi.fn(),
  applyResumeSchedule: vi.fn(),
  applyLimitObservation: vi.fn(),
}));

vi.mock("@/lib/client/frames", () => ({ onUsageFrame: () => () => undefined }));

const { UsagePage } = await import("./UsagePage");
const { UsageFooter } = await import("./UsageFooter");
const { resetUsageStoreForTests } = await import("./usageStore");

function totals(overrides: Partial<UsageTotals> = {}): UsageTotals {
  return {
    input_tokens: 9_000,
    output_tokens: 3_400,
    cached_input_tokens: 0,
    cached_creation_tokens: 0,
    cost_micros: 1_250_000,
    ttft_ms: 640,
    tps: 42,
    wall_ms: 200_000,
    generation_ms: 100_000,
    active_ms: 125_000,
    idle_ms: 125_000,
    runtime_ms: 250_000,
    turns: 3,
    ...overrides,
  };
}

const point = { start: "2026-03-09T00:00:00Z", end: "2026-03-10T00:00:00Z", totals: totals() };

const report: UsageReport = {
  scope: "global",
  scope_id: null,
  range: { since: null, until: null },
  group_by: "harness",
  bucket: "day",
  series_group_by: "harness",
  totals: totals(),
  buckets: [point],
  series: [{ key: "claude_code", points: [point] }],
  groups: [{ key: "claude_code", totals: totals() }],
} as UsageReport;

const limit: LimitStatus = {
  thread_id: null,
  harness: "claude_code",
  account: "work",
  provider: "anthropic",
  model: null,
  reset_at: "2026-03-10T14:05:00Z",
  message: "5-hour limit reached",
  consumed: 80,
  limit: 100,
  observed_at: "2026-03-10T11:00:00Z",
  source: "harness",
};

const schedule = {
  schedule_id: "sch_1",
  thread_id: "thread_1",
  state: "scheduled",
  scheduled_at: "2026-03-10T11:00:00Z",
  resume_at: "2026-03-10T12:02:05Z",
  last_error: null,
} as unknown as ResumeSchedule;

const thread = {
  thread_id: "thread_1",
  title: "Fix the parser",
  harness: "claude_code",
  host: "atlas",
  usage: {
    input_tokens: 9_000,
    output_tokens: 3_400,
    cached_input_tokens: 0,
    cost_micros: 1_250_000,
    duration_ms: 125_000,
    generation_ms: 0,
    tps: 42,
  },
  ttft_ms: 640,
  context_fill_percent: 40,
};

beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.scrollIntoView ??= () => undefined;
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
});

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(NOW);
  mock.app.connection.status = "online";
  mock.app.threads = { thread_1: thread };
  mock.responses = {
    get_usage: { type: "usage_report", report },
    get_limits: { type: "limits", report: { limits: [limit] } },
    list_resets: {
      type: "resets",
      resets: { schedules: [schedule], history: [{ ...limit, source: "proxy" }] },
    },
    list_failover_options: {
      type: "failover_options",
      options: {
        thread_id: "thread_1",
        current_model: "claude-sonnet",
        default: null,
        options: [],
        unavailable: null,
      },
    },
  };
  resetUsageStoreForTests();
});

afterEach(async () => {
  cleanup();
  vi.useRealTimers();
  await act(() => setLocalePreference("en"));
});

async function renderPage() {
  render(<UsagePage />);
  await screen.findByText("Fix the parser", { exact: false });
  await screen.findAllByText(/claude|Claude/);
}

describe("the usage page in another language", () => {
  it("keeps English as it was", async () => {
    await renderPage();
    expect(screen.getByRole("heading", { name: "Usage" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Tokens per day by harness" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "14 days", selected: true })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Refresh usage" })).toBeTruthy();
    expect(screen.getByText("3 turns")).toBeTruthy();
    expect(screen.getByText("50% active")).toBeTruthy();
    expect(screen.getAllByText("2m 05s").length).toBeGreaterThan(0);
    expect(screen.getByText("5-hour")).toBeTruthy();
    expect(screen.getByText("Resets", { exact: false }).textContent).toContain("in 2h 05m");
    expect(screen.getByText("Fix the parser is paused")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Resume now" })).toBeTruthy();
  });

  it("words the page, its figures and the auto-resume card in Japanese", async () => {
    await act(() => setLocalePreference("ja"));
    await renderPage();
    expect(screen.getByRole("heading", { name: "使用量" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "harness 別の 1 日あたりのトークン" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "14 日", selected: true })).toBeTruthy();
    expect(screen.getByRole("tablist", { name: "期間" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "使用量を更新" })).toBeTruthy();
    // The token stat sets the compact unit apart: 1.2 and 万, never "12.4k".
    expect(screen.getByText("万")).toBeTruthy();
    expect(screen.queryByText(/12\.4k/i)).toBeNull();
    expect(screen.getByText("3 ターン")).toBeTruthy();
    expect(screen.getByText("5 時間")).toBeTruthy();
    expect(screen.getByText("プロキシ")).toBeTruthy();
    // Japanese durations use its own units, not a Latin "2m05s".
    expect(screen.getAllByText("2 分05 秒").length).toBeGreaterThan(0);
    expect(screen.queryByText(/\d+m\d+s/)).toBeNull();
    expect(screen.getByText("Fix the parser は一時停止中です")).toBeTruthy();
    expect(screen.getByRole("button", { name: "今すぐ再開" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "自動再開をキャンセル" })).toBeTruthy();
  });

  it("words the chart, the breakdown and the offline note in Spanish", async () => {
    mock.app.connection.status = "offline";
    await act(() => setLocalePreference("es"));
    render(<UsagePage />);
    expect(screen.getByRole("heading", { name: "Uso" })).toBeTruthy();
    expect(screen.getByText("Mostrando el último informe")).toBeTruthy();
    expect(screen.getByText(/El daemon está desconectado/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reintentar" })).toBeTruthy();

    mock.app.connection.status = "online";
    cleanup();
    await renderPage();
    expect(screen.getByRole("heading", { name: "Tokens por día y harness" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Por harness" })).toBeTruthy();
    expect(screen.getByRole("columnheader", { name: "Coste" })).toBeTruthy();
    expect(screen.getByText("3 turnos")).toBeTruthy();
    expect(screen.getByText("de 5 horas")).toBeTruthy();
    expect(screen.getAllByText("1,25 US$").length).toBeGreaterThan(0);
  });
});

describe("the usage footer under the composer", () => {
  it("writes tokens the way the receipt does, in every language", async () => {
    render(<UsageFooter threadId="thread_1" />);
    expect(screen.getByText("12.4K tokens")).toBeTruthy();
    expect(screen.getByText("40% context")).toBeTruthy();
    cleanup();

    await act(() => setLocalePreference("zh-CN"));
    render(<UsageFooter threadId="thread_1" />);
    expect(screen.getByText("1.2万 token")).toBeTruthy();
    expect(screen.getByText("活跃 2分钟05秒")).toBeTruthy();
    expect(screen.getByText("上下文 40%")).toBeTruthy();
  });
});
