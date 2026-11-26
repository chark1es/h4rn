// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => ({
  state: {
    ui: { route: { kind: "inbox" } },
    threads: {} as Record<string, unknown>,
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  navigate: vi.fn(),
}));

import { setLocalePreference } from "@/lib/i18n";

import { PhoneNav } from "./PhoneNav";

afterEach(async () => {
  cleanup();
  mock.state.threads = {};
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

function tabNames(): string[] {
  return screen.getAllByRole("button").map((tab) => tab.textContent ?? "");
}

describe("Phone tab bar", () => {
  it("keeps its English names", () => {
    render(<PhoneNav />);
    expect(screen.getByRole("navigation", { name: "Main" })).toBeTruthy();
    expect(tabNames()).toEqual(["Inbox", "PRs", "Usage", "Hosts", "Settings"]);
  });

  it("uses the sidebar's names for each destination in every language", async () => {
    render(<PhoneNav />);
    for (const [locale, nav, names] of [
      ["zh-CN", "主导航", ["收件箱", "PR", "用量", "主机", "设置"]],
      ["es", "Principal", ["Bandeja", "PRs", "Uso", "Hosts", "Ajustes"]],
      ["ja", "メイン", ["受信トレイ", "PR", "使用量", "ホスト", "設定"]],
    ] as const) {
      await act(() => setLocalePreference(locale));
      expect(screen.getByRole("navigation", { name: nav })).toBeTruthy();
      expect(tabNames()).toEqual(names);
    }
  });

  it("counts threads that need you in the badge", () => {
    mock.state.threads = {
      a: { needs_attention: true, agent_state: "idle" },
      b: { needs_attention: false, agent_state: "error" },
      c: { needs_attention: false, agent_state: "idle" },
    };
    render(<PhoneNav />);
    expect(screen.getByRole("button", { current: "page" }).textContent).toBe("2Inbox");
  });
});
