// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { formatDuration, setLocalePreference } from "@/lib/i18n";

const mcp = vi.hoisted(() => ({ removeMcpServer: vi.fn(async () => false) }));

vi.mock("@/lib/store/appStore", () => ({
  renameThread: vi.fn(async () => true),
  pushToast: vi.fn(),
  useApp: () => undefined,
}));

vi.mock("@/features/mcp/store", () => ({
  applyMcpImport: vi.fn(),
  loadMcpTools: vi.fn(),
  previewMcpImport: vi.fn(),
  removeMcpServer: mcp.removeMcpServer,
  resolveMcpConflict: vi.fn(),
  shareMcpServer: vi.fn(),
  testMcpTool: vi.fn(),
  upsertMcpServer: vi.fn(),
  useMcp: () => ({}),
}));

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { RenameThreadDialog } from "@/components/thread/RenameThreadDialog";
import { expiryText } from "@/features/guest/GuestShareDialog";
import { McpRemoveDialog } from "@/features/mcp/McpDialogs";
import { sourceLabel } from "@/features/skills/SkillDialogs";
import type { McpServerView } from "@/gen";

afterEach(async () => {
  cleanup();
  await act(() => setLocalePreference("en"));
});

function renderConfirm() {
  render(
    <ConfirmDialog
      open
      title="Remove the worktree?"
      description="Its folder is deleted."
      confirmLabel="Remove"
      onOpenChange={() => undefined}
      onConfirm={() => undefined}
    />,
  );
}

describe("dialogs in other languages", () => {
  it("keeps English as it was", () => {
    renderConfirm();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
    // The dialog's own close button, read by a screen reader.
    expect(screen.getByRole("button", { name: "Close" })).toBeTruthy();
  });

  it("translates the confirm's Cancel and close button, and keeps the caller's own text", async () => {
    await act(() => setLocalePreference("es"));
    renderConfirm();
    expect(screen.getByRole("button", { name: "Cancelar" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Cerrar" })).toBeTruthy();
    // An area not converted yet still passes its English title.
    expect(screen.getByText("Remove the worktree?")).toBeTruthy();
  });

  it("renames a thread in Japanese without translating its title", async () => {
    await act(() => setLocalePreference("ja"));
    render(
      <RenameThreadDialog
        threadId={"th_1" as never}
        initial="Fix the build"
        open
        onOpenChange={() => undefined}
      />,
    );
    expect(screen.getByText("スレッド名を変更")).toBeTruthy();
    const input = screen.getByLabelText("スレッドのタイトル") as HTMLInputElement;
    expect(input.value).toBe("Fix the build");
    expect(screen.getByRole("button", { name: "名前を保存" })).toBeTruthy();
  });

  it("names the MCP server as sent and translates the failure", async () => {
    await act(() => setLocalePreference("zh-CN"));
    const server = { name: "github" } as McpServerView;
    render(<McpRemoveDialog open server={server} onClose={() => undefined} onRemoved={() => {}} />);
    expect(screen.getByText("移除 github？")).toBeTruthy();
    await act(async () => {
      screen.getByRole("button", { name: "移除服务器" }).click();
    });
    expect(screen.getByText("守护进程未能移除这个服务器，它仍处于配置状态。")).toBeTruthy();
  });

  it("words a guest link's expiry in the active language", async () => {
    const now = Date.parse("2026-01-01T00:00:00Z");
    const in90 = new Date(now + 90 * 60_000).toISOString();
    expect(expiryText(in90, now)).toBe("Expires in 1 hr 30 min");
    expect(expiryText("2025-12-31T00:00:00Z", now)).toBe("Expired");

    await act(() => setLocalePreference("es"));
    expect(expiryText(in90, now)).toBe(`Caduca en ${formatDuration(90 * 60_000)}`);
    expect(expiryText("2025-12-31T00:00:00Z", now)).toBe("Caducado");
  });

  it("labels a skill's source in the active language and keeps URLs as written", async () => {
    await act(() => setLocalePreference("ja"));
    expect(
      sourceLabel({
        type: "git",
        url: "git@example.com:acme/skills.git",
        commit: "0123456789abcdef",
        subdir: null,
      }),
    ).toBe("Git · git@example.com:acme/skills.git @ 0123456");
    expect(sourceLabel({ type: "harness_dir", harness: "codex" } as never)).toBe(
      "Codex からインポート",
    );
  });
});
