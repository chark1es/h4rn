import { CheckIcon, ChevronDownIcon, CircleXIcon, FileIcon, PlusIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

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
import { absoluteTime, formatTokens } from "@/lib/format";
import { formatNumber, i18n } from "@/lib/i18n";
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
    return view.budget.max_turns
      ? i18n.t("thread:subagents.phone.needsApprovalTurn", {
          turn: view.progress.turns_started + 1,
          cap: view.budget.max_turns,
        })
      : i18n.t("thread:subagents.state.awaiting_approval");
  }
  if (view.state === "failed" || view.state === "budget_exceeded") {
    return view.result?.error ?? i18n.t(`thread:subagents.state.${view.state}`);
  }
  const tokens = formatTokens(view.progress.tokens);
  if (view.state === "completed") return i18n.t("thread:subagents.phone.doneTokens", { tokens });
  if (view.state === "cancelled") return i18n.t("thread:subagents.state.cancelled");
  if (view.state === "starting") return i18n.t("thread:subagents.state.starting");
  return i18n.t("thread:subagents.phone.runningTokens", { tokens });
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
  const { t } = useTranslation("thread");
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
            label={t("subagents.avatar")}
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
            {t("approval.phone.details")}
          </button>
          <button type="button" className="q-btn primary sm" style={{ flex: 1 }} onClick={onReview}>
            {t("approval.phone.review")}
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
  const { t } = useTranslation(["thread", "common"]);
  const [tab, setTab] = useState(initialTab);
  const sheetRef = useRef<HTMLDivElement | null>(null);
  const changes = usePanelChanges(thread);
  const files = changes.changes?.files ?? [];
  const subagents = useSubagentCount(thread.thread_id);
  const views = useSubagentViews(thread.thread_id);
  const preview = useApp((state) => state.ui.preview);
  const previewPath = preview?.threadId === thread.thread_id ? preview.path : null;
  const tabState = tab === "preview" && !previewPath ? "changes" : tab;

  useEffect(() => setTab(initialTab), [initialTab]);
  // The sheet is modal: take focus on open so a `/` command that opens it
  // leaves the composer, and typing no longer edits the draft behind it.
  // Focus the container rather than Done, so no action looks pre-chosen.
  useEffect(() => {
    sheetRef.current?.focus({ preventScroll: true });
  }, []);
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
        aria-label={t("changes.phone.close")}
        onClick={onClose}
      />
      <div
        ref={sheetRef}
        className="q-ph-sheet large"
        role="dialog"
        aria-modal="true"
        aria-label={t("panel.label")}
        tabIndex={-1}
      >
        <div className="q-grab" />
        <div className="q-ph-sh-nav">
          <span style={{ width: 64 }} />
          <span className="ttl">{thread.title}</span>
          <button type="button" className="navbtn" onClick={onClose}>
            {t("common:done")}
          </button>
        </div>
        <div className="q-ph-seg" style={{ padding: "0 16px 10px" }}>
          <div className="q-seg" role="tablist" aria-label={t("changes.phone.tabs")}>
            <button
              type="button"
              role="tab"
              aria-selected={tabState === "changes"}
              onClick={() => setTab("changes")}
            >
              {t("panel.changes")}{" "}
              {files.length > 0 ? (
                <span className="c q-num">{formatNumber(files.length)}</span>
              ) : null}
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
                {t("panel.preview")}
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
              {t("panel.subagents")}{" "}
              {subagents.total > 0 ? (
                <span className="c q-num">{formatNumber(subagents.total)}</span>
              ) : null}
              {subagents.waiting > 0 ? (
                <span className="q-pip" role="img" aria-label={t("panel.needsYou")} />
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
                  ? t("subagents.none")
                  : subagents.waiting > 0
                    ? `${t("subagents.count", { count: subagents.total })} · ${t("subagents.waiting", { count: subagents.waiting })}`
                    : t("subagents.count", { count: subagents.total })}
              </span>
              <span className="grow" />
              <button type="button" className="q-btn ghost sm" onClick={openDelegate}>
                <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("subagents.delegate")}
              </button>
            </div>
            {views.length === 0 ? (
              <div className="q-empty q-sub-empty">
                <h3>{t("subagents.emptyTitle")}</h3>
                <p>{t("subagents.emptyBody", { harness: harnessLabel(thread.harness) })}</p>
                <button type="button" className="q-btn secondary" onClick={openDelegate}>
                  <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  {t("subagents.delegate")}
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
                    status={t("subagents.phone.parent", {
                      state: glyphForAgentState(thread.agent_state).label,
                    })}
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
                  {t("subagents.phone.note")}
                </p>
              </>
            )}
          </div>
        ) : (
          <div className="q-ph-body">
            <div className="q-ph-sum">
              <span className="q-btn secondary sm">
                {t("changes.phone.allTurns", { count: Math.max(1, changes.turns.length) })}{" "}
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
              {t("changes.phone.since", {
                harness: harnessLabel(thread.harness),
                host: thread.host,
                time: absoluteTime(thread.last_activity),
              })}
            </div>
            {changes.error ? (
              <p className="q-ph-note">{changes.error}</p>
            ) : files.length === 0 ? (
              <p className="q-ph-note">
                {changes.loading ? t("changes.phone.loading") : t("changes.phone.empty")}
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
            <p className="q-ph-desktop-only">{t("changes.phone.desktopOnly")}</p>
          </div>
        )}
      </div>
      <DelegateDialog thread={thread} />
    </>
  );
}
