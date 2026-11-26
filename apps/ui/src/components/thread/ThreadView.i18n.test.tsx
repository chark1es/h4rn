// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeAll, describe, expect, it } from "vitest";

import type { ThreadSummary, ToolCall } from "@/gen";
import { glyphForThread, glyphForToolStatus } from "@/lib/agent";
import { formatCost, formatTokens, toolStatsLabel } from "@/lib/format";
import { setLocalePreference } from "@/lib/i18n";
import { getState, rewindRefusal, store } from "@/lib/store/appStore";
import { createThreadLog, type StreamTurn } from "@/lib/store/projection";
import { stoppedNotice } from "@/lib/turn";
import { ThreadHeader } from "./ThreadHeader";
import { ThreadReceipt } from "./ThreadStream";
import { ToolCard } from "./ToolCard";
import { TurnBlock } from "./TurnBlock";

const summary: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Fix the parser",
  workspace_path: "/work",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "working",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "thinking",
  usage: {
    input_tokens: 9_000,
    output_tokens: 3_400,
    cached_input_tokens: 0,
    cost_micros: 1_250_000,
    duration_ms: 125_000,
    generation_ms: 0,
    tps: 0,
  },
  needs_attention: false,
  pending_guest_requests: 0,
  unread: false,
  queued_count: 0,
  queue_revision: 0,
  steer: false,
  resume_at: null,
  limit: null,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  rewind: "handoff",
  file_rewind: false,
};

function turn(overrides: Partial<StreamTurn> = {}): StreamTurn {
  return {
    id: "turn_1",
    startedAt: "2026-01-01T00:00:00Z",
    status: "completed",
    items: [
      {
        kind: "message",
        itemId: "item_1",
        role: "assistant",
        text: "Fixed the parser.",
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
    ...overrides,
  };
}

function markup(block: StreamTurn): string {
  return renderToStaticMarkup(
    <TurnBlock
      turn={block}
      turns={[block]}
      threadId="thread_1"
      summary={{ ...summary, agent_state: "done" }}
      onReply={() => {}}
    />,
  );
}

beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
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

afterEach(async () => {
  cleanup();
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

describe("the thread view in another language", () => {
  it("names the agent's state in the active language, for every surface that shows the orb", async () => {
    expect(glyphForThread({ ...summary, agent_state: "done", unread: true }, true).label).toBe(
      "Done · unread",
    );

    await setLocalePreference("zh-CN");
    expect(glyphForThread({ ...summary, agent_state: "done", unread: true }, true).label).toBe(
      "已完成 · 未读",
    );
    expect(glyphForThread(summary, false).label).toBe("主机无法连接");
    expect(glyphForToolStatus("denied").label).toBe("已拒绝");

    await setLocalePreference("ja");
    expect(glyphForThread({ ...summary, agent_state: "running_tool" }, true, "Bash").label).toBe(
      "Bash を実行中",
    );
  });

  it("translates the header's actions and its menu", async () => {
    await act(() => setLocalePreference("es"));
    store.setState({
      ...getState(),
      threads: { thread_1: summary },
      logs: { thread_1: { ...createThreadLog(), loaded: true, turns: [turn()] } },
    });
    render(
      <ThreadHeader
        summary={summary}
        host={{ ...hostFixture, online: true }}
        onBack={() => {}}
        onToggleTerminal={() => {}}
      />,
    );

    expect(screen.getByRole("button", { name: "Detener" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Hilo" })).toBeTruthy();
    expect(screen.getByText("Pensando")).toBeTruthy();
    const trigger = screen.getByRole("button", { name: "Acciones del hilo" });
    fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
    fireEvent.click(trigger);
    expect(
      await screen.findByRole("menuitem", { name: "Bifurcar desde el último turno" }),
    ).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: "Mantener en «En curso»" })).toBeTruthy();
  });

  it("words turn outcomes, usage limits and plans in the active language", async () => {
    await setLocalePreference("ja");
    expect(markup(turn({ status: "failed" }))).toContain(
      "ターンが失敗しました。これ以降は実行されていません。",
    );
    expect(stoppedNotice("token_limit")).toBe(
      "停止しました：エージェントがトークン上限に達しました。",
    );

    const limited = markup(
      turn({
        items: [
          { kind: "limit", itemId: "limit_1", resetAt: null, autoResume: false },
          {
            kind: "plan",
            itemId: "plan_1",
            entries: [{ content: "Write the test", status: "completed", priority: "medium" }],
          },
        ] as StreamTurn["items"],
      }),
    );
    expect(limited).toContain("使用量の上限に達しました。");
    expect(limited).toContain("自動再開はオフです。手動で再開してください。");
    expect(limited).toContain("（完了）");
    expect(limited).toContain("host_1 上");
  });

  it("pluralises tool results and formats their numbers for the language", async () => {
    await setLocalePreference("es");
    expect(toolStatsLabel({ kind: "matches", count: 1, unit: "matches" })).toEqual({
      kind: "text",
      text: "1 coincidencia",
    });
    expect(toolStatsLabel({ kind: "matches", count: 1200, unit: "files" })).toEqual({
      kind: "text",
      text: "1200 archivos",
    });
    expect(toolStatsLabel({ kind: "tests", passed: 3, failed: 1 })).toEqual({
      kind: "text",
      text: "3 superadas · 1 fallidas",
    });

    const call: ToolCall = {
      tool_call_id: "call_1",
      name: "Bash",
      summary: "ls",
      status: "failed",
      input: null,
      output: null,
      started_at: "2026-01-01T00:00:00Z",
      completed_at: null,
    };
    expect(renderToStaticMarkup(<ToolCard toolCall={call} />)).toContain("fallido");
  });

  it("writes the receipt's tokens and cost the language's way", async () => {
    expect(formatTokens(12_400)).toBe("12.4K");
    expect(formatCost(1_250_000)).toBe("$1.25");

    await setLocalePreference("zh-CN");
    expect(formatTokens(12_400)).toBe("1.2万");
    const receipt = renderToStaticMarkup(
      <ThreadReceipt summary={{ ...summary, tab: "done", agent_state: "done" }} turns={[turn()]} />,
    );
    expect(receipt).toContain("完成于");
    expect(receipt).toContain("2分钟5秒");
    expect(receipt).toContain("1.2万 token");
  });

  it("says why rewinding is unavailable in the active language", async () => {
    await setLocalePreference("es");
    expect(rewindRefusal(undefined, undefined)).toBe("El hilo aún no se ha cargado.");
    expect(rewindRefusal(summary, undefined)).toBe(
      "Espera a que termine el turno actual o detenlo primero.",
    );
  });
});

const hostFixture = {
  host_id: "host_1",
  name: "atlas",
  online: true,
  last_seen: "2026-01-01T00:00:00Z",
} as unknown as Parameters<typeof ThreadHeader>[0]["host"] & object;
