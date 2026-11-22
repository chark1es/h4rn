import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import { PreviewFailure, PreviewPanel } from "@/features/preview/PreviewPanel";
import type { FileEntry, ThreadSummary, Usage } from "@/gen";
import { closePreview, getState, openPreview } from "@/lib/store/appStore";
import { WorkspaceFileRow } from "./WorkspaceFiles";

const usage: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  cost_micros: 0,
  duration_ms: 0,
  generation_ms: 0,
  tps: 0,
};

const thread: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Write the report",
  workspace_path: "/work",
  model: "claude-sonnet",
  tab: "working",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "done",
  usage,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  needs_attention: false,
  unread: false,
  resume_at: null,
  limit: null,
};

function file(path: string, kind: FileEntry["kind"] = "file"): FileEntry {
  return {
    name: path.split("/").at(-1) ?? path,
    path,
    kind,
    size_bytes: 1200,
    modified_at: null,
    git_status: null,
  };
}

function renderRow(entry: FileEntry): string {
  return renderToStaticMarkup(
    <WorkspaceFileRow
      entry={entry}
      depth={1}
      open={false}
      active={false}
      onToggle={() => undefined}
      onOpenFile={() => undefined}
      onOpenPreview={() => undefined}
    />,
  );
}

afterEach(() => {
  closePreview();
});

describe("WorkspaceFileRow", () => {
  it("offers Open preview on HTML files", () => {
    const markup = renderRow(file("reports/report.html"));
    expect(markup).toContain("Open preview of report.html");
    // The action sits beside the treeitem in a presentational wrapper, so the
    // tree still owns the row.
    expect(markup).toContain('role="presentation"');
    expect(renderRow(file("reports/report.htm"))).toContain("Open preview of report.htm");
  });

  it("leaves other files and folders alone", () => {
    expect(renderRow(file("reports/chart.png"))).not.toContain("Open preview");
    expect(renderRow(file("reports", "directory"))).not.toContain("Open preview");
  });

  it("opens the Preview tab through the store action", () => {
    openPreview("reports/report.html", "thread_1");
    const state = getState();
    expect(state.ui.preview).toEqual({ threadId: "thread_1", path: "reports/report.html" });
    expect(state.ui.layout.panelTab).toBe("preview");
    expect(state.ui.layout.panelOpen).toBe(true);
  });
});

describe("PreviewPanel", () => {
  it("explains the empty state before a file is chosen", () => {
    const markup = renderToStaticMarkup(<PreviewPanel thread={thread} path={null} />);
    expect(markup).toContain("No preview open");
    expect(markup).toContain("sandboxed");
    // The panel header can close the preview as well as reload or expand it.
    expect(markup).toContain("Close preview");
  });

  it("renders a plain failure message without crashing", () => {
    const markup = renderToStaticMarkup(
      <PreviewFailure
        title="This file is missing."
        detail="The file was deleted."
        onReload={() => undefined}
      />,
    );
    expect(markup).toContain("This file is missing.");
    expect(markup).toContain("The file was deleted.");
    expect(markup).toContain("Reload");
  });
});
