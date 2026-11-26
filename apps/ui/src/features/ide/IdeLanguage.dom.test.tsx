// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { WorkspaceFileRow } from "@/components/panel/WorkspaceFiles";
import {
  describePreviewFailure,
  PreviewFailure,
  PreviewPanel,
} from "@/features/preview/PreviewPanel";
import type { FsDiff, FsDiffFiles, ThreadId, ThreadSummary } from "@/gen";
import { i18n, setLocalePreference } from "@/lib/i18n";

import { DiffView } from "./DiffView";
import { languageLabel } from "./language";
import {
  addReviewComment,
  listReviewComments,
  resetReviewComments,
  reviewCommentBlock,
} from "./reviewComments";
import { SearchPanel } from "./SearchPanel";
import { describeSource, diffSourceHint, diffSourceName } from "./sources";
import { TabStrip } from "./TabStrip";
import { createTab } from "./tabs";
import type { SearchState } from "./useIdeSession";

const thread = "th_1" as ThreadId;

const diff: FsDiff = {
  path: "a.txt",
  against: { kind: "base", base: "main" },
  label: "vs main",
  before: "a\nb\n",
  after: "a\nB\n",
  binary: false,
  too_large: false,
  note: null,
};

const files: FsDiffFiles = {
  against: { kind: "base", base: "main" },
  label: "vs main",
  base: "main",
  merge_base: "abc",
  truncated: false,
  note: null,
  files: [
    {
      path: "a.txt",
      old_path: null,
      status: "modified",
      added: 1,
      removed: 1,
      binary: false,
      counts_omitted: false,
    },
    {
      path: "b.txt",
      old_path: "old-b.txt",
      status: "renamed",
      added: 1204,
      removed: 0,
      binary: false,
      counts_omitted: false,
    },
  ],
};

function branchView() {
  return (
    <DiffView
      path="a.txt"
      source="branch"
      diff={diff}
      loading={false}
      error={null}
      changes={null}
      files={files}
      filesLoading={false}
      filesError={null}
      commits={null}
      host={null}
      harness={null}
      threadId={thread}
      scopeLabel="vs main"
      changeTitle={null}
      onSelectFile={() => undefined}
      onOpenFile={() => undefined}
      onRevert={() => undefined}
      onSendComments={() => undefined}
      onCompareHead={() => undefined}
      onRefresh={() => undefined}
    />
  );
}

function search(overrides: Partial<SearchState> = {}): SearchState {
  return {
    query: "clock",
    regex: false,
    caseSensitive: false,
    glob: null,
    running: false,
    results: [],
    truncated: false,
    error: null,
    searchId: "s1",
    ...overrides,
  };
}

beforeEach(() => {
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
  resetReviewComments();
  await setLocalePreference("en");
});

describe("the IDE in another language", () => {
  it("names the diff sources in the active language", async () => {
    expect(diffSourceName("agent")).toBe("Turn");
    await setLocalePreference("zh-CN");
    expect(diffSourceName("agent")).toBe("回合");
    await setLocalePreference("es");
    expect(diffSourceName("head")).toBe("Sin confirmar");
    expect(diffSourceHint("branch")).toBe("contra la base de fusión");
    // The daemon's label ("vs main") is shown as sent inside the phrase.
    expect(describeSource("branch", "vs main")).toBe("rama vs main");
  });

  it("quotes the translated Turn label in the no-turn hint", async () => {
    for (const locale of ["en", "zh-CN", "es", "ja"] as const) {
      await setLocalePreference(locale);
      expect(i18n.t("shell:errors.noAgentTurn.hint"), locale).toContain(diffSourceName("agent"));
    }
  });

  it("keeps the draft block for the agent in English", async () => {
    await setLocalePreference("ja");
    const comment = addReviewComment({
      threadId: thread,
      path: "a.txt",
      side: "new",
      startLine: 2,
      endLine: 2,
      lines: [{ number: 2, sign: "+", text: "B" }],
      text: "名前を戻してください。",
      scope: "base:main",
      source: "branch",
      sourceLabel: "vs main",
    });
    expect(reviewCommentBlock(comment)).toContain(
      "Review comment · a.txt · new side, line 2 · branch vs main",
    );
    expect(reviewCommentBlock(comment)).toContain("Comment: 名前を戻してください。");
  });

  it("words the diff, its file list and a comment note in Spanish", async () => {
    await setLocalePreference("es");
    addReviewComment({
      threadId: thread,
      path: "a.txt",
      side: "new",
      startLine: 2,
      endLine: 2,
      lines: [{ number: 2, sign: "+", text: "B" }],
      text: "Escrito en el diff de la rama.",
      scope: "base:main",
      source: "branch",
      sourceLabel: "vs main",
    });
    render(branchView());
    expect(screen.getByRole("complementary", { name: "Archivos cambiados" })).toBeTruthy();
    expect(screen.getByText("2 archivos")).toBeTruthy();
    expect(screen.getByText("1 comentario")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Enviar al agente" })).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "b.txt · renombrado (antes old-b.txt)" }),
    ).toBeTruthy();
    // Counts are written the language's way.
    expect(screen.getByText("+1204")).toBeTruthy();
    // A comment note names its source in Spanish around the daemon's label.
    expect(screen.getByText(/línea 2 · rama vs main/)).toBeTruthy();
    expect(listReviewComments(thread)[0]?.sourceLabel).toBe("vs main");
  });

  it("relabels the search panel and keeps line numbers ungrouped", async () => {
    await setLocalePreference("zh-CN");
    const results = [
      { path: "src/a.ts", line_number: 1204, column: 3, preview: "clock.now()" },
      { path: "src/a.ts", line_number: 1300, column: 1, preview: "clock" },
    ];
    render(
      <SearchPanel
        search={search({ results, truncated: true })}
        onQuery={() => undefined}
        onOption={() => undefined}
        onRun={() => undefined}
        onOpen={() => undefined}
      />,
    );
    expect(screen.getByRole("region", { name: "搜索文件" })).toBeTruthy();
    expect(screen.getByText("2 个匹配项")).toBeTruthy();
    expect(screen.getByText("已达到结果上限")).toBeTruthy();
    expect(screen.getByText("1204")).toBeTruthy();
  });

  it("names the tab strip's controls and the plain-text fallback in Japanese", async () => {
    await setLocalePreference("ja");
    const tab = { ...createTab("src/a.ts"), loading: false, content: "x", savedContent: "" };
    render(
      <TabStrip
        tabs={[tab]}
        activePath="src/a.ts"
        onActivate={() => undefined}
        onClose={() => undefined}
        onReorder={() => undefined}
      />,
    );
    expect(screen.getByRole("tablist", { name: "開いているファイル" })).toBeTruthy();
    expect(screen.getByRole("img", { name: "未保存の変更" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "a.ts を閉じる" })).toBeTruthy();
    // Language names read the same everywhere; plain text is worded by the bar.
    expect(languageLabel("src/a.ts")).toBe("TypeScript");
    expect(languageLabel("notes")).toBeNull();
  });

  it("words the Files tab's preview action in Spanish", async () => {
    await setLocalePreference("es");
    render(
      <WorkspaceFileRow
        entry={{
          name: "report.html",
          path: "out/report.html",
          kind: "file",
          size_bytes: 10,
          modified_at: null,
          git_status: null,
        }}
        depth={1}
        open={false}
        active={false}
        onToggle={() => undefined}
        onOpenFile={() => undefined}
        onOpenPreview={() => undefined}
      />,
    );
    expect(
      screen.getByRole("button", { name: "Abrir la vista previa de report.html" }),
    ).toBeTruthy();
  });
});

describe("the preview in another language", () => {
  it("words a failure from its kind, with sizes in the language's numbers", async () => {
    await setLocalePreference("es");
    const page = describePreviewFailure({
      kind: "pageTooLarge",
      size: 11 * 1_048_576,
      limit: 10 * 1_048_576,
    });
    expect(page.title).toBe("Esta página es demasiado grande para la vista previa.");
    expect(page.detail).toContain("11,0 MiB");
    expect(page.detail).toContain("10,0 MiB");

    await setLocalePreference("ja");
    expect(describePreviewFailure({ kind: "gone", detail: null }).detail).toBe(
      "デーモンからファイルが返されませんでした。",
    );
    // The daemon's own text is shown as sent.
    expect(describePreviewFailure({ kind: "loadFailed", detail: "EACCES" }).detail).toBe("EACCES");
  });

  it("relabels a failure on screen when the language changes", async () => {
    render(
      <PreviewFailure
        failure={{ kind: "notText", binary: true, hint: null }}
        onReload={() => undefined}
      />,
    );
    expect(screen.getByText("This file isn’t UTF-8 text.")).toBeTruthy();
    await setLocalePreference("zh-CN");
    expect(await screen.findByText("此文件不是 UTF-8 文本。")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重新加载" })).toBeTruthy();
  });

  it("words the empty panel in Japanese", async () => {
    await setLocalePreference("ja");
    const owner = { host: "host_1", workspace_path: "/work" } as ThreadSummary;
    render(<PreviewPanel thread={owner} path={null} />);
    expect(screen.getByText("開いているプレビューはありません")).toBeTruthy();
    expect(screen.getByRole("button", { name: "プレビューを閉じる" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "ファイルを表示" })).toBeTruthy();
  });
});
