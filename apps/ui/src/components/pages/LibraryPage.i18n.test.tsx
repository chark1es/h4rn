// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import type { McpServerView, SkillInventory } from "@/gen";

const mock = vi.hoisted(() => ({
  app: {
    connection: { status: "online" },
    localHostId: "atlas" as string | null,
    hosts: { atlas: { name: "atlas", online: true } } as Record<string, unknown>,
    ui: { layout: { librarySelected: "github" } },
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.app),
  getState: () => mock.app,
  getClient: () => ({ onEvent: () => () => undefined, onReady: () => () => undefined }),
  pushToast: vi.fn(),
  setLibrarySelected: vi.fn(),
}));

import { McpApprovalCard } from "@/features/mcp/McpApprovalCard";
import { McpPage } from "@/features/mcp/McpPage";
import {
  applyMcpEvent,
  loadMcpServers,
  resetMcpStore,
  setMcpRequesterForTests,
  useMcp,
} from "@/features/mcp/store";
import { SkillsPage } from "@/features/skills/SkillsPage";
import {
  loadSkills,
  resetSkillsStore,
  setSkillsRequesterForTests,
  useSkills,
} from "@/features/skills/store";
import { clientProto, H4rnRequestError, protoError } from "@/lib/client/errors";
import { setLocalePreference } from "@/lib/i18n";
import { LibraryErrorBanner } from "./LibraryErrorBanner";

const github: McpServerView = {
  id: "srv_1",
  name: "github",
  transport: {
    type: "stdio",
    command: "npx",
    args: ["-y", "@modelcontextprotocol/server-github"],
    cwd: null,
    env_keys: ["GITHUB_TOKEN"],
  },
  tags: [],
  enabled: { by_harness: [], by_machine: [] },
  status: "ready",
  tool_count: 12,
  last_error: null,
  runs_on: null,
  shared_to: [],
  imported_from: null,
  updated_at: "2026-11-13T10:00:00.000Z",
};

const inventory: SkillInventory = {
  skills: [
    {
      id: "skl_1",
      name: "review",
      description: "Review a diff before it lands.",
      version: "abc123",
      source: { type: "harness_dir", harness: "codex" } as never,
      total_bytes: 4096,
      updated_at: "2026-11-13T10:00:00.000Z",
      installs: [
        {
          harness: "codex",
          installed: true,
          managed: true,
          enabled: true,
          version: "abc123",
          path: "/home/atlas/.codex/skills/review",
        },
      ],
      conflicts: 2,
    },
  ],
  harnesses: [
    {
      harness: "codex",
      detected: true,
      user_root: "/home/atlas/.codex/skills",
      project_root: ".codex/skills",
    },
  ],
} as never;

beforeAll(() => {
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
  resetMcpStore();
  resetSkillsStore();
  mock.app.localHostId = "atlas";
});

afterEach(async () => {
  cleanup();
  setMcpRequesterForTests(null);
  setSkillsRequesterForTests(null);
  await act(() => setLocalePreference("en"));
});

function McpBanner() {
  const error = useMcp((state) => state.error);
  const action = useMcp((state) => state.errorAction);
  return <LibraryErrorBanner error={error} action={action} onRetry={() => undefined} />;
}

function SkillsBanner() {
  const error = useSkills((state) => state.error);
  const action = useSkills((state) => state.errorAction);
  return <LibraryErrorBanner error={error} action={action} onRetry={() => undefined} />;
}

describe("the library pages' error banner", () => {
  it("words the failed request where it is shown, and relabels on a language switch", async () => {
    setMcpRequesterForTests(() =>
      Promise.reject(new H4rnRequestError(clientProto("unsupported", "noHost"))),
    );
    await loadMcpServers();

    render(<McpBanner />);
    expect(
      screen.getByText("The daemon has not reported its host yet. (list servers)"),
    ).toBeTruthy();

    await act(() => setLocalePreference("ja"));
    expect(
      screen.getByText("デーモンはまだホストを報告していません。（サーバーの一覧取得）"),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "再試行" })).toBeTruthy();
  });

  it("shows a daemon's own text as sent, with the request in the reader's language", async () => {
    await act(() => setLocalePreference("es"));
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? Promise.reject(new H4rnRequestError(protoError("internal", "registry is locked", null)))
        : Promise.resolve({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();

    render(<SkillsBanner />);
    expect(screen.getByText("registry is locked (listar skills)")).toBeTruthy();
    expect(screen.getByText("El daemon no respondió.")).toBeTruthy();
  });
});

describe("the library pages in other languages", () => {
  it("renders the MCP page in Japanese, keeping names and commands as sent", async () => {
    setMcpRequesterForTests(() => Promise.resolve({ type: "mcp_servers", servers: [github] }));
    await loadMcpServers();
    await act(() => setLocalePreference("ja"));

    render(<McpPage />);
    const main = screen.getByRole("main", { name: "MCP サーバーの詳細" });
    const text = main.textContent ?? "";
    expect(text).toContain("github");
    expect(text).toContain("npx -y @modelcontextprotocol/server-github");
    expect(text).toContain("ツール 12 件");
    expect(text).toContain("環境変数 1 件 · 値は書き込み専用");
    expect(text).toContain("実行場所");
    expect(text).toContain("ホストの設定ファイル");
    expect(text).toContain("正常");
    expect(text).not.toMatch(/Where it runs|Credential|tools|Healthy|Host config file/);
    expect(
      screen.getByRole("switch", { name: "atlas の Claude Code で github をオフにする" }),
    ).toBeTruthy();
  });

  it("renders the Skills page in Japanese, including the skill's source in its row", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? Promise.resolve({ type: "skills", inventory })
        : Promise.resolve({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();
    await act(() => setLocalePreference("ja"));

    render(<SkillsPage />);
    const text = screen.getByRole("main", { name: "スキル" }).textContent ?? "";
    expect(text).toContain("インストール済みのスキル");
    expect(text).toContain("harness 1 件中 1 件 · Codex からインポート");
    expect(text).toContain("競合 2 件");
    expect(text).toContain("Review a diff before it lands.");
    expect(text).not.toMatch(/Installed skills|harnesses|Imported from|conflicts|Sync now/);
  });

  it("asks for a shared tool approval in Spanish", async () => {
    await act(() => setLocalePreference("es"));
    applyMcpEvent({
      type: "mcp_share_approval_requested",
      approval: {
        approval_id: "mcp-1",
        machine: "atlas",
        server: "github",
        tool: "write_file",
        arguments_preview: "",
        requested_at: "2026-11-14T10:00:00.000Z",
        expires_at: new Date(Date.now() + 5 * 60_000).toISOString(),
      },
    });

    render(<McpApprovalCard />);
    const card = screen.getByRole("region", { name: "Aprobación de herramienta compartida" });
    expect(card.textContent).toContain("atlas quiere ejecutar github__write_file");
    expect(card.textContent).toContain(
      "Servidor MCP compartido · la solicitud caduca dentro de 5 minutos",
    );
    expect(card.textContent).toContain("Sin argumentos");
    expect(screen.getByRole("button", { name: "Aprobar una vez" })).toBeTruthy();
  });
});
