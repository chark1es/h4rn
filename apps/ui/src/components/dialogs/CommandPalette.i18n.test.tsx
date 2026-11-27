// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";
import { commandName } from "@/lib/keybindings";
import { getState, store } from "@/lib/store/appStore";
import { createThreadLog } from "@/lib/store/projection";
import { CommandPalette } from "./CommandPalette";

const summary: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Fix the parser",
  workspace_path: "/work/api",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "done",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "done",
  usage: {
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cost_micros: 0,
    duration_ms: 0,
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

function renderPalette() {
  store.setState({
    ...getState(),
    threads: { thread_1: summary },
    logs: { thread_1: { ...createThreadLog(), loaded: true, turns: [] } },
    ui: {
      ...getState().ui,
      route: { kind: "thread", threadId: "thread_1" },
      paletteOpen: true,
    },
  });
  render(<CommandPalette />);
}

function search(text: string) {
  const input = screen.getByRole("combobox");
  fireEvent.change(input, { target: { value: text } });
}

/** The visible rows' text, in order. */
function rows(): string[] {
  return [...document.querySelectorAll("[cmdk-item]")].map((row) => row.textContent ?? "");
}

beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
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
  class ResizeObserverStub {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  Object.defineProperty(globalThis, "ResizeObserver", {
    writable: true,
    value: ResizeObserverStub,
  });
});

afterEach(async () => {
  cleanup();
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

describe("command palette in another language", () => {
  it("shows the same command names as Settings → Shortcuts", async () => {
    await act(() => setLocalePreference("es"));
    renderPalette();

    expect(screen.getByRole("combobox").getAttribute("placeholder")).toBe(
      "Busca hilos, hosts, archivos o comandos",
    );
    expect(screen.getByText("Navegación")).toBeTruthy();
    // One name per command, shared with Settings → Shortcuts.
    expect(commandName("tab.working")).toBe("Mostrar «En curso»");
    expect(screen.getByText("Mostrar «En curso»")).toBeTruthy();
    expect(screen.getByText("Resolver el hilo")).toBeTruthy();
    expect(screen.getByText("Abrir api en el IDE")).toBeTruthy();
    expect(screen.queryByText("Show Working")).toBeNull();
    expect(screen.queryByText("This thread")).toBeNull();
  });

  it("finds a command by its translated name and by its English name", async () => {
    await act(() => setLocalePreference("es"));
    renderPalette();

    search("resolver");
    expect(rows()[0]).toContain("Resolver el hilo");

    // English muscle memory keeps working: the English name and its old
    // synonyms still match.
    search("settle thread");
    expect(rows()[0]).toContain("Resolver el hilo");
    search("archive");
    expect(rows().some((row) => row.includes("Resolver el hilo"))).toBe(true);
    search("Show Working");
    expect(rows()[0]).toContain("Mostrar «En curso»");
  });

  it("matches Spanish without accents and Chinese and Japanese by their words", async () => {
    await act(() => setLocalePreference("es"));
    renderPalette();
    search("ultimo turno");
    expect(rows().some((row) => row.includes("Bifurcar desde el último turno"))).toBe(true);
    cleanup();

    await act(() => setLocalePreference("zh-CN"));
    renderPalette();
    search("了结");
    expect(rows()[0]).toContain("了结对话");
    cleanup();

    await act(() => setLocalePreference("ja"));
    renderPalette();
    search("整理");
    expect(rows()[0]).toContain("スレッドを整理");
  });

  it("ends a row that opens a dialog in an ellipsis, not the shared command name", async () => {
    await act(() => setLocalePreference("ja"));
    renderPalette();

    expect(screen.getByText("新しいスレッド…")).toBeTruthy();
    expect(screen.getByText("ゲストと共有…")).toBeTruthy();
    // Settings → Shortcuts lists the same name without it.
    expect(commandName("thread.new")).toBe("新しいスレッド");

    search("shortcuts");
    expect(screen.getByText(`${commandName("shortcuts.open")}…`)).toBeTruthy();
    // A row that acts at once has none.
    search("settle");
    expect(screen.getByText("スレッドを整理")).toBeTruthy();
  });

  it("keeps keyboard hints the same in every language", async () => {
    await act(() => setLocalePreference("ja"));
    renderPalette();

    const row = screen.getByText("スレッドを整理").closest("[cmdk-item]");
    if (!(row instanceof HTMLElement)) throw new Error("the settle row is not a command item");
    const caps = [...row.querySelectorAll("[data-slot='kbd']")].map((cap) => cap.textContent);
    expect(caps).toEqual(["Ctrl", "Shift", "S"]);

    const footer = document.querySelector("[data-slot='command-footer']");
    if (!(footer instanceof HTMLElement)) throw new Error("no palette footer");
    expect(within(footer).getByText("Esc")).toBeTruthy();
    expect(footer.textContent).toContain("選択");
    expect(footer.textContent).toContain("検索対象：このマシン");
  });
});
