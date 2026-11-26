import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";

vi.mock("@/lib/store/appStore", () => ({
  interruptThread: vi.fn(),
  openNewThread: vi.fn(),
  setPanelTab: vi.fn(),
  toggleTerminal: vi.fn(),
  toggleView: vi.fn(),
}));

import { composerCommands } from "./composerCommands";
import { rankCommands } from "./composerMenu";

const summary = {
  thread_id: "th_one",
  host: "atlas",
  tab: "working",
  workspace_path: "/home/me/code/h4rn",
} as ThreadSummary;

const options = { terminalOpen: false, view: "thread" as const };

describe("composerCommands", () => {
  afterEach(async () => {
    await setLocalePreference("en");
  });

  it("labels each command in English by default", () => {
    const labels = composerCommands(summary, options).map((command) => command.label);
    expect(labels).toEqual([
      "Stop the turn",
      "Open changes",
      "Open subagents",
      "Open the terminal",
      "Open the IDE",
      "New thread",
    ]);
    const newThread = composerCommands(summary, options).find((c) => c.id === "thread.new");
    expect(newThread?.hint).toBe("Start one in h4rn");
  });

  it("follows the active language, and an English word still finds a command", async () => {
    await setLocalePreference("zh-CN");
    const commands = composerCommands(summary, { ...options, terminalOpen: true });
    expect(commands.find((c) => c.id === "terminal.toggle")?.label).toBe("隐藏终端");
    expect(commands.find((c) => c.id === "thread.new")?.hint).toBe("在 h4rn 中开始新对话");
    expect(rankCommands(commands, "终端")[0]?.id).toBe("terminal.toggle");
    expect(rankCommands(commands, "interrupt")[0]?.id).toBe("thread.stop");
  });
});
