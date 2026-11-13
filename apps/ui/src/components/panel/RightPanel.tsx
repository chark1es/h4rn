import { XIcon } from "lucide-react";

import { MOD, SHIFT } from "@/components/common/Kbd";
import { Resizer } from "@/components/shell/Resizer";
import { SubagentsPanel, subagentCount } from "@/features/subagents/SubagentsPanel";
import type { ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { setPanelTab, setPanelWidth, togglePanel, useApp } from "@/lib/store/appStore";
import { ChangesSection } from "./ChangesSection";
import { usePanelChanges } from "./usePanelChanges";
import { WorkspaceFiles } from "./WorkspaceFiles";

function workspaceName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

/** The one right panel: Changes, Files and Subagents for the active thread. */
export function RightPanel({ thread, ide = false }: { thread: ThreadSummary; ide?: boolean }) {
  const tab = useApp((state) => state.ui.layout.panelTab);
  const width = useApp((state) => state.ui.layout.panelWidth);
  const ideActivePath = useApp((state) => state.ui.ideActivePath);
  const changes = usePanelChanges(thread);
  const changeCount = changes.changes?.files.length ?? 0;
  const subagents = subagentCount(thread.thread_id);

  return (
    <aside
      className="q-rp"
      aria-label="Thread panel"
      style={{ ["--rp-w" as string]: `${width}px` }}
    >
      <div className="q-rp-head" role="tablist" aria-label="Panel">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "changes"}
          className="q-rp-tab"
          onClick={() => setPanelTab("changes")}
        >
          Changes {changeCount > 0 ? <span className="c q-num">{changeCount}</span> : null}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "files"}
          className="q-rp-tab"
          onClick={() => setPanelTab("files")}
        >
          Files
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "subagents"}
          className="q-rp-tab"
          onClick={() => setPanelTab("subagents")}
        >
          Subagents {subagents > 0 ? <span className="c q-num">{subagents}</span> : null}
          {subagents > 0 && thread.needs_attention ? (
            <span className="q-pip" role="img" aria-label="needs you" />
          ) : null}
        </button>
        <span className="grow" />
        <button
          type="button"
          className="q-ibtn"
          aria-label="Close panel"
          title={`Close panel  ${MOD}${SHIFT}⌥B`}
          onClick={togglePanel}
        >
          <XIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
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
        />
      ) : (
        <SubagentsPanel threadId={thread.thread_id} />
      )}

      <Resizer
        side="panel"
        width={width}
        min={320}
        max={640}
        label="Resize panel"
        onChange={setPanelWidth}
      />
    </aside>
  );
}
