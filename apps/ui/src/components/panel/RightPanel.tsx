import { XIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Resizer } from "@/components/shell/Resizer";
import { Tabs, TabsCount, TabsList, TabsPanel, TabsTab } from "@/components/ui/tabs";
import { Tip } from "@/components/ui/tooltip";
import { PreviewPanel } from "@/features/preview/PreviewPanel";
import { SubagentsPanel } from "@/features/subagents/SubagentsPanel";
import { useSubagentCount } from "@/features/subagents/subagentStore";
import type { ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { formatNumber } from "@/lib/i18n";
import {
  openPreview,
  type PanelTab,
  setPanelTab,
  setPanelWidth,
  togglePanel,
  useApp,
} from "@/lib/store/appStore";
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
      <Tabs
        className="min-h-0 flex-1"
        value={tab}
        onValueChange={(value) => {
          if (value) setPanelTab(value as PanelTab);
        }}
      >
        <div className="q-rp-head">
          <TabsList
            aria-label={t("panel.tabs")}
            className="-mb-px h-auto min-w-0 flex-1 gap-4.5 @max-[380px]/rp:gap-2.5 @max-[440px]/rp:[:lang(es)_&]:gap-2.5"
          >
            <TabsTab value="changes" className="shrink" title={t("panel.changes")}>
              <span className="min-w-0 truncate">{t("panel.changes")}</span>
              {changeCount > 0 ? <TabsCount>{formatNumber(changeCount)}</TabsCount> : null}
            </TabsTab>
            <TabsTab value="files" className="shrink" title={t("panel.files")}>
              <span className="min-w-0 truncate">{t("panel.files")}</span>
            </TabsTab>
            <TabsTab
              value="preview"
              className="shrink"
              title={previewPath ?? t("panel.previewHint")}
            >
              <span className="min-w-0 truncate">{t("panel.preview")}</span>
            </TabsTab>
            <TabsTab value="subagents" className="shrink" title={t("panel.subagents")}>
              <span className="min-w-0 truncate">{t("panel.subagents")}</span>
              {subagentTotal > 0 ? <TabsCount>{formatNumber(subagentTotal)}</TabsCount> : null}
              {subagents.waiting > 0 ? (
                <span className="q-pip" role="img" aria-label={t("panel.needsYou")} />
              ) : null}
            </TabsTab>
          </TabsList>
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

        <TabsPanel value="changes">
          <ChangesSection thread={thread} changes={changes} ide={ide} />
        </TabsPanel>
        <TabsPanel value="files">
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
        </TabsPanel>
        <TabsPanel value="preview">
          <PreviewPanel
            key={`${thread.thread_id}:${previewPath ?? "empty"}`}
            thread={thread}
            path={previewPath}
          />
        </TabsPanel>
        <TabsPanel value="subagents">
          <SubagentsPanel thread={thread} />
        </TabsPanel>
      </Tabs>

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
