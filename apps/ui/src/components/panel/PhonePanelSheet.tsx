import { CheckIcon, ChevronDownIcon, CircleXIcon, FileIcon, PlusIcon } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { PreviewPanel } from "@/features/preview/PreviewPanel";
import { DelegateDialog } from "@/features/subagents/DelegateDialog";
import {
  loadSubagents,
  openDelegate,
  useSubagentCount,
  useSubagentViews,
} from "@/features/subagents/subagentStore";
import type { SubagentView, ThreadSummary } from "@/gen";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed, subagentSeed } from "@/lib/entities";
import { absoluteTime, tokens } from "@/lib/format";
import { openInIde, openThread, setPanelTab, useApp } from "@/lib/store/appStore";
import { FileDiff } from "./FileDiff";
import { usePanelChanges } from "./usePanelChanges";

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/** A flat subagent row: phones do not have room for the desktop tree. */
function SubagentRow({
  glyph,
  title,
  byline,
  status,
  tone,
  muted = false,
  child = false,
  selected = false,
  children,
}: {
  glyph: ReactNode;
  title: string;
  byline: ReactNode;
  status: ReactNode;
  tone?: "accent" | "danger" | undefined;
  muted?: boolean;
  child?: boolean;
  selected?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={`q-ph-li q-ph-node${child ? " kid" : ""}${selected ? " sel" : ""}`}>
      <span className="q-ph-node-gl">{glyph}</span>
      <div className="grow">
        <div className="q-ph-node-t" style={muted ? { color: "var(--fg-2)" } : undefined}>
          {title}
        </div>
        <div className="s" style={{ display: "flex", alignItems: "center", gap: 5 }}>
          {byline}
        </div>
        <div className="s" style={tone ? { color: `var(--${tone})`, fontWeight: 500 } : undefined}>
          {status}
        </div>
        {children}
      </div>
    </div>
  );
}

function phoneState(view: SubagentView): string {
  if (view.state === "awaiting_approval") {
    return `Needs approval${view.budget.max_turns ? ` · turn ${view.progress.turns_started + 1} of ${view.budget.max_turns}` : ""}`;
  }
  if (view.state === "failed" || view.state === "budget_exceeded") {
    return view.result?.error ?? (view.state === "failed" ? "Failed" : "Budget exceeded");
  }
  if (view.state === "completed") return `Done · ${tokens(view.progress.tokens)} tokens`;
  if (view.state === "cancelled") return "Cancelled";
  if (view.state === "starting") return "Starting";
  return `Running · ${tokens(view.progress.tokens)} tokens`;
}

function PhoneSubagent({
  view,
  parentThreadId,
  onReview,
}: {
  view: SubagentView;
  parentThreadId: string;
  onReview: () => void;
}) {
  const waiting = view.state === "awaiting_approval";
  const failed = view.state === "failed" || view.state === "budget_exceeded";
  return (
    <SubagentRow
      child
      selected={waiting}
      glyph={
        failed ? (
          <CircleXIcon className="q-icon s16 q-danger" strokeWidth={1.5} aria-hidden />
        ) : view.state === "completed" ? (
          <CheckIcon className="q-icon s16" strokeWidth={1.5} aria-hidden />
        ) : (
          <AgentGlyph glyph={glyphForAgentState(view.agent_state)} size={14} />
        )
      }
      title={view.task}
      byline={
        <>
          <EntityAvatar
            seed={subagentSeed(view.host, parentThreadId, view.subagent_id)}
            kind="subagent"
            label="subagent"
            size={14}
          />
          <EntityAvatar seed={machineSeed(view.host)} kind="machine" label={view.host} size={14} />
          {view.host} · {harnessLabel(view.harness)}
        </>
      }
      status={phoneState(view)}
      tone={waiting ? "accent" : failed ? "danger" : undefined}
    >
      {waiting ? (
        <div style={{ display: "flex", gap: 10, marginTop: 10 }}>
          <button
            type="button"
            className="q-btn secondary sm"
            style={{ flex: 1 }}
            disabled={!view.child_thread_id}
            onClick={() => {
              if (view.child_thread_id) openThread(view.child_thread_id);
            }}
          >
            Details
          </button>
          <button type="button" className="q-btn primary sm" style={{ flex: 1 }} onClick={onReview}>
            Review
          </button>
        </div>
      ) : null}
    </SubagentRow>
  );
}

/**
 * On a phone the right panel becomes one sheet: Changes and Subagents share a
 * segmented control. Files and the editor are desktop surfaces, and the sheet
 * says so rather than half-rendering them.
 */
export function PhonePanelSheet({
  thread,
  initialTab,
  onClose,
  onReview,
}: {
  thread: ThreadSummary;
  initialTab: "changes" | "subagents" | "preview";
  onClose: () => void;
  onReview?: () => void;
}) {
  const [tab, setTab] = useState(initialTab);
  const changes = usePanelChanges(thread);
  const files = changes.changes?.files ?? [];
  const subagents = useSubagentCount(thread.thread_id);
  const views = useSubagentViews(thread.thread_id);
  const preview = useApp((state) => state.ui.preview);
  const previewPath = preview?.threadId === thread.thread_id ? preview.path : null;
  const tabState = tab === "preview" && !previewPath ? "changes" : tab;

  useEffect(() => setTab(initialTab), [initialTab]);
  useEffect(() => {
    if (!previewPath && tab === "preview") setTab("changes");
  }, [previewPath, tab]);
  useEffect(() => {
    if (tab === "subagents") void loadSubagents(thread.thread_id);
  }, [tab, thread.thread_id]);

  const openFile = (path: string) => {
    // Editing is a desktop task, but a file can still be handed to the IDE for
    // the next time this session runs wide.
    openInIde(path);
    onClose();
  };

  return (
    <>
      <button
        type="button"
        className="q-ph-scrim"
        aria-label="Close changes and subagents"
        onClick={onClose}
      />
      <div className="q-ph-sheet large" role="dialog" aria-modal="true" aria-label="Thread panel">
        <div className="q-grab" />
        <div className="q-ph-sh-nav">
          <span style={{ width: 64 }} />
          <span className="ttl">{thread.title}</span>
          <button type="button" className="navbtn" onClick={onClose}>
            Done
          </button>
        </div>
        <div className="q-ph-seg" style={{ padding: "0 16px 10px" }}>
          <div className="q-seg" role="tablist" aria-label="Changes, preview and subagents">
            <button
              type="button"
              role="tab"
              aria-selected={tabState === "changes"}
              onClick={() => setTab("changes")}
            >
              Changes {files.length > 0 ? <span className="c q-num">{files.length}</span> : null}
            </button>
            {previewPath ? (
              <button
                type="button"
                role="tab"
                aria-selected={tabState === "preview"}
                onClick={() => {
                  setPanelTab("preview");
                  setTab("preview");
                }}
              >
                Preview
              </button>
            ) : null}
            <button
              type="button"
              role="tab"
              aria-selected={tabState === "subagents"}
              onClick={() => {
                setPanelTab("subagents");
                setTab("subagents");
              }}
            >
              Subagents{" "}
              {subagents.total > 0 ? <span className="c q-num">{subagents.total}</span> : null}
              {subagents.waiting > 0 ? (
                <span className="q-pip" role="img" aria-label="needs you" />
              ) : null}
            </button>
          </div>
        </div>

        {tabState === "preview" ? (
          <PreviewPanel
            key={`${thread.thread_id}:${previewPath ?? "empty"}`}
            thread={thread}
            path={previewPath}
            variant="phone"
          />
        ) : tabState === "subagents" ? (
          <div className="q-ph-body">
            <div className="q-ph-gh" style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span>
                {subagents.total === 0
                  ? "No subagents"
                  : `${subagents.total} subagent${subagents.total === 1 ? "" : "s"}${
                      subagents.waiting > 0 ? ` · ${subagents.waiting} waiting on you` : ""
                    }`}
              </span>
              <span className="grow" />
              <button type="button" className="q-btn ghost sm" onClick={openDelegate}>
                <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                Delegate
              </button>
            </div>
            {views.length === 0 ? (
              <div className="q-empty q-sub-empty">
                <h3>Nothing delegated</h3>
                <p>
                  When {harnessLabel(thread.harness)} hands work to a subagent, it shows here with
                  where it runs, its budget, and what it’s waiting on.
                </p>
                <button type="button" className="q-btn secondary" onClick={openDelegate}>
                  <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  Delegate
                </button>
              </div>
            ) : (
              <>
                <div className="q-ph-group">
                  <SubagentRow
                    glyph={<AgentGlyph glyph={glyphForAgentState(thread.agent_state)} size={14} />}
                    title={thread.title}
                    byline={
                      <>
                        <EntityAvatar
                          seed={machineSeed(thread.host)}
                          kind="machine"
                          label={thread.host}
                          size={14}
                        />
                        {thread.host} · {harnessLabel(thread.harness)}
                      </>
                    }
                    status={`Parent · ${thread.agent_state.replace(/_/g, " ")}`}
                  />
                  {views.map((view) => (
                    <PhoneSubagent
                      key={view.subagent_id}
                      view={view}
                      parentThreadId={thread.thread_id}
                      onReview={() => {
                        onClose();
                        onReview?.();
                      }}
                    />
                  ))}
                </div>
                <p className="q-ph-note" style={{ marginTop: -8 }}>
                  A subagent’s approval always comes to its parent thread, so it also shows above
                  the composer.
                </p>
              </>
            )}
          </div>
        ) : (
          <div className="q-ph-body">
            <div className="q-ph-sum">
              <span className="q-btn secondary sm">
                {changes.turns.length > 1 ? `All ${changes.turns.length} turns` : "1 turn"}{" "}
                <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              </span>
              <span className="grow" />
              <span className="q-num q-pm">
                <span className="q-add">+{changes.added}</span>{" "}
                <span className="q-del">−{changes.removed}</span>
              </span>
            </div>
            <div className="q-ph-gh" style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <EntityAvatar
                seed={harnessSeed(thread.host, thread.harness)}
                kind="harness"
                label={harnessLabel(thread.harness)}
                size={14}
              />
              {harnessLabel(thread.harness)} on {thread.host} · since{" "}
              {absoluteTime(thread.last_activity)}
            </div>
            {changes.error ? (
              <p className="q-ph-note">{changes.error}</p>
            ) : files.length === 0 ? (
              <p className="q-ph-note">
                {changes.loading ? "Loading changes…" : "No changes in this thread yet."}
              </p>
            ) : (
              <>
                <div className="q-ph-group">
                  {files.map((file) => {
                    const { directory, name } = splitPath(file.path);
                    return (
                      <button
                        key={file.path}
                        type="button"
                        className="q-ph-li"
                        onClick={() => openFile(file.path)}
                      >
                        <span className="q-faint">
                          <FileIcon className="q-icon s18" strokeWidth={1.5} aria-hidden />
                        </span>
                        <span
                          className="grow q-mono"
                          style={{
                            fontSize: 14,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          <span className="q-faint">{directory}</span>
                          {name}
                        </span>
                        <span className="q-num q-pm">
                          <span className="q-add">+{file.added}</span>{" "}
                          <span className="q-del">−{file.removed}</span>
                        </span>
                        <ChevronDownIcon
                          className="q-icon s16 q-faint"
                          strokeWidth={1.5}
                          style={{ transform: "rotate(-90deg)" }}
                          aria-hidden
                        />
                      </button>
                    );
                  })}
                </div>
                {files[0] ? (
                  <>
                    <div className="q-ph-gh q-mono" style={{ fontSize: 13 }}>
                      {files[0].path}
                    </div>
                    <div className="q-ph-diff">
                      <FileDiff
                        bare
                        compact
                        path={files[0].path}
                        diff={changes.diffs[files[0].path] ?? null}
                        added={files[0].added}
                        removed={files[0].removed}
                        loading={changes.diffsLoading}
                        onOpen={() => openFile(files[0]?.path ?? "")}
                      />
                    </div>
                  </>
                ) : null}
              </>
            )}
            <p className="q-ph-desktop-only">Files and the IDE are on desktop.</p>
          </div>
        )}
      </div>
      <DelegateDialog thread={thread} />
    </>
  );
}
