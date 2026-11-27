import { XIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Resizer } from "@/components/shell/Resizer";
import { Tip } from "@/components/ui/tooltip";
import { PreviewPanel } from "@/features/preview/PreviewPanel";
import { SubagentsPanel } from "@/features/subagents/SubagentsPanel";
import { useSubagentCount } from "@/features/subagents/subagentStore";
import type { ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { formatNumber } from "@/lib/i18n";
import { openPreview, setPanelTab, setPanelWidth, togglePanel, useApp } from "@/lib/store/appStore";
import { ChangesSection } from "./ChangesSection";
import { usePanelChanges } from "./usePanelChanges";
import { WorkspaceFiles } from "./WorkspaceFiles";

function workspaceName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

/** The one right panel: Changes, Files, Preview and Subagents for the active thread. */
export function RightPanel({ thread, ide = false }: { thread: ThreadSummary; ide?: boolean }) {
  const { t } = useTranslation("thread");
  const tab = useApp((state) => state.ui.layout.panelTab);
  const width = useApp((state) => state.ui.layout.panelWidth);
  const ideActivePath = useApp((state) => state.ui.ideActivePath);
  const changes = usePanelChanges(thread);
  const changeCount = changes.changes?.files.length ?? 0;
  const subagents = useSubagentCount(thread.thread_id);
  const subagentTotal = subagents.total;
  const preview = useApp((state) => state.ui.preview);
  const previewPath = preview?.threadId === thread.thread_id ? preview.path : null;

  return (
    <aside
      className="q-rp"
      aria-label={t("panel.label")}
      style={{ ["--rp-w" as string]: `${width}px` }}
    >
      <div className="q-rp-head" role="tablist" aria-label={t("panel.tabs")}>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "changes"}
          className="q-rp-tab"
          title={t("panel.changes")}
          onClick={() => setPanelTab("changes")}
        >
          <span className="q-rp-tab-name">{t("panel.changes")}</span>
          {changeCount > 0 ? <span className="c q-num">{formatNumber(changeCount)}</span> : null}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "files"}
          className="q-rp-tab"
          title={t("panel.files")}
          onClick={() => setPanelTab("files")}
        >
          <span className="q-rp-tab-name">{t("panel.files")}</span>
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "preview"}
          className="q-rp-tab"
          title={previewPath ?? t("panel.previewHint")}
          onClick={() => setPanelTab("preview")}
        >
          <span className="q-rp-tab-name">{t("panel.preview")}</span>
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "subagents"}
          className="q-rp-tab"
          title={t("panel.subagents")}
          onClick={() => setPanelTab("subagents")}
        >
          <span className="q-rp-tab-name">{t("panel.subagents")}</span>
          {subagentTotal > 0 ? (
            <span className="c q-num">{formatNumber(subagentTotal)}</span>
          ) : null}
          {subagents.waiting > 0 ? (
            <span className="q-pip" role="img" aria-label={t("panel.needsYou")} />
          ) : null}
        </button>
        <span className="grow" />
        <Tip label={t("panel.close")} command="panel.toggle">
          <button
            type="button"
            className="q-ibtn"
            aria-label={t("panel.close")}
            onClick={togglePanel}
          >
            <XIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </Tip>
      </div>

      {tab === "changes" ? (
        <ChangesSection thread={thread} changes={changes} ide={ide} />
      ) : tab === "files" ? (
        <WorkspaceFiles
          host={thread.host}
          workspace={thread.workspace_path}
          workspaceName={workspaceName(thread.workspace_path)}
          agentChanges={changes.changes}
          onOpenAgentChanges={() => setPanelTab("changes")}
          activePath={ide ? ideActivePath : null}
          agentChangesLabel={ide ? `${thread.title} · ${harnessLabel(thread.harness)}` : null}
          onOpenPreview={(path) => openPreview(path, thread.thread_id)}
        />
      ) : tab === "preview" ? (
        <PreviewPanel
          key={`${thread.thread_id}:${previewPath ?? "empty"}`}
          thread={thread}
          path={previewPath}
        />
      ) : (
        <SubagentsPanel thread={thread} />
      )}

      <Resizer
        side="panel"
        width={width}
        min={320}
        max={640}
        label={t("panel.resize")}
        onChange={setPanelWidth}
      />
    </aside>
  );
}
