import {
  Code2Icon,
  FileCodeIcon,
  GitBranchIcon,
  HandIcon,
  Loader2Icon,
  SearchIcon,
  TriangleAlertIcon,
  WifiOffIcon,
  XIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import { EmptyState } from "@/components/common/EmptyState";
import { SidebarReveal } from "@/components/shell/Sidebar";
import { ViewCluster } from "@/components/thread/ThreadHeader";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { MOD, SHIFT } from "@/components/ui/kbd";
import { Tabs, TabsList, TabsTab } from "@/components/ui/tabs";
import { Tip } from "@/components/ui/tooltip";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import type { ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { glyphForAgentState, glyphForThread } from "@/lib/agent";
import { toProtoError } from "@/lib/client/errors";
import { readThreadDraft, writeThreadDraft } from "@/lib/drafts";
import { machineSeed } from "@/lib/entities";
import { monogram } from "@/lib/grouping";
import { formatNumber, i18n } from "@/lib/i18n";
import {
  ensureThread,
  navigate,
  openNewThread,
  pushToast,
  setIdeActivePath,
  setPanelTab,
  setView,
  takePendingIdePath,
  toggleTerminal,
  useApp,
} from "@/lib/store/appStore";
import { latestRealTurn } from "@/lib/store/projection";
import { activeThread } from "@/lib/thread-selection";
import { cn } from "@/lib/utils";
import { CodeEditor, type CursorPosition } from "./CodeEditor";
import { DiffView } from "./DiffView";
import { computeDiff } from "./diff";
import { languageLabel } from "./language";
import { deleteReviewComment, type ReviewComment, sendCommentsToDraft } from "./reviewComments";
import { SearchPanel } from "./SearchPanel";
import {
  baseOptions,
  commitLabel,
  DIFF_SOURCES,
  type DiffSource,
  diffSourceHint,
  diffSourceName,
} from "./sources";
import { TabStrip } from "./TabStrip";
import type { IdeTab } from "./tabs";
import { useIdeSession, type WorkspaceOption } from "./useIdeSession";

function workspaceName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

function workspaceFor(thread: ThreadSummary | null, turnId: string | null): WorkspaceOption | null {
  if (!thread?.workspace_path) return null;
  return {
    path: thread.workspace_path,
    host: thread.host,
    threadId: thread.thread_id,
    threadTitle: thread.title,
    turnId,
    harness: thread.harness,
  };
}

/** A cursor position in the status bar, without grouping ("Ln 1204"). */
function position(value: number): string {
  return formatNumber(value, { useGrouping: false });
}

export function IdePane() {
  const { t } = useTranslation(["ide", "common"]);
  const isPhone = useIsPhone();
  const threads = useApp((state) => state.threads);
  const hosts = useApp((state) => state.hosts);
  const route = useApp((state) => state.ui.route);
  const logs = useApp((state) => state.logs);
  const selectedThread = useApp((state) => state.ui.selectedThreadId);
  const connection = useApp((state) => state.connection.status);
  const terminalOpen = useApp((state) => state.ui.layout.terminalOpen);
  const offline = connection !== "online";

  const thread = activeThread(threads, selectedThread);
  const latestTurn = thread ? (latestRealTurn(logs[thread.thread_id]?.turns)?.id ?? null) : null;
  const workspace = useMemo(() => workspaceFor(thread, latestTurn), [thread, latestTurn]);

  const session = useIdeSession(workspace);
  const active = session.active;
  const { state } = session;
  const pendingIdePath = useApp((store) => store.ui.pendingIdePath);
  const openFile = session.openFile;
  const ideActivePath = active?.path ?? null;

  // A click in the panel's Files tab asks the IDE to open that file; the IDE
  // consumes the request here so the panel never needs the editor's state.
  useEffect(() => {
    if (!pendingIdePath) return;
    const path = takePendingIdePath();
    if (path) openFile(path);
  }, [pendingIdePath, openFile]);

  // The panel's tree highlights the file the editor is showing.
  useEffect(() => {
    setIdeActivePath(ideActivePath);
  }, [ideActivePath]);

  useEffect(() => () => setIdeActivePath(null), []);

  const [panel, setPanel] = useState<"search" | "diff" | null>(null);
  const [terminalMounted, setTerminalMounted] = useState(terminalOpen);
  const [cursor, setCursor] = useState<CursorPosition>({ line: 1, column: 1 });
  const [pendingClose, setPendingClose] = useState<IdeTab | null>(null);
  const [closing, setClosing] = useState(false);
  const [resolving, setResolving] = useState(false);

  // The diff pane follows the editor tab in every mode. A file picked inside
  // the pane (the phone Turn picker, a Branch or Commits row) is the person's
  // choice and must stay until they pick another one or switch tabs; entries
  // in a wider review's file list remain one click away.
  const diffTabPath = active?.path ?? null;
  const followDiffTab = session.followDiffTab;
  const followedTabRef = useRef<string | null>(null);
  useEffect(() => {
    if (panel !== "diff" || !diffTabPath) {
      followedTabRef.current = null;
      return;
    }
    if (followedTabRef.current === diffTabPath) return;
    followedTabRef.current = diffTabPath;
    // Entering the pane (or switching the source) already asked for the tab's
    // file. `followDiffTab` itself skips a tab the pane is already showing,
    // and takes over a list load that would open another file.
    void followDiffTab(diffTabPath);
  }, [panel, diffTabPath, followDiffTab]);

  useEffect(() => {
    setCursor({ line: 1, column: 1 });
  }, []);

  // The IDE can be opened straight from a URL, before any thread screen has
  // hydrated the log that carries the latest turn id for agent diffs.
  useEffect(() => {
    if (thread?.thread_id) ensureThread(thread.thread_id);
  }, [thread?.thread_id]);

  const toastError = useCallback((error: unknown) => {
    const proto = toProtoError(error);
    pushToast({
      title: i18n.t("ide:save.failed"),
      description: proto.message,
      variant: "error",
    });
  }, []);

  const save = useCallback(() => {
    if (!active) return;
    void session.saveTab(active.path).catch(toastError);
  }, [active, session.saveTab, toastError]);

  const close = useCallback(
    (path: string) => {
      const result = session.requestClose(path);
      if (result.dirty) setPendingClose(result.tab);
    },
    [session.requestClose],
  );

  const saveAndClose = useCallback(async () => {
    const tab = pendingClose;
    if (!tab) return;
    setClosing(true);
    try {
      await session.saveTab(tab.path);
      session.confirmClose(tab.path);
      setPendingClose(null);
    } catch (error) {
      toastError(error);
    } finally {
      setClosing(false);
    }
  }, [pendingClose, session.saveTab, session.confirmClose, toastError]);

  const openDiff = useCallback(
    (path: string, source: DiffSource) => {
      // Asking for this file directly is the follow this tab already had; the
      // tab-follow effect must not ask a second time. The guard compares tab
      // paths, so remember the tab's file, not the row that was opened.
      followedTabRef.current = diffTabPath;
      setPanel("diff");
      void session.openDiff(path, source);
    },
    [diffTabPath, session.openDiff],
  );

  /** Adds every pending comment to the thread's draft; sending stays manual. */
  const sendReviewComments = useCallback(
    (comments: ReviewComment[]) => {
      if (!workspace?.threadId || comments.length === 0) return;
      sendCommentsToDraft({
        host: workspace.host,
        threadId: workspace.threadId,
        comments,
        read: readThreadDraft,
        write: writeThreadDraft,
        remove: deleteReviewComment,
      });
      pushToast({
        title: i18n.t("ide:comments.added", { count: comments.length }),
        description: i18n.t("ide:comments.addedBody"),
      });
    },
    [workspace],
  );

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const mod = event.metaKey || event.ctrlKey;
      if (mod && event.key.toLowerCase() === "s") {
        event.preventDefault();
        save();
        return;
      }
      if (mod && event.shiftKey && event.key.toLowerCase() === "f") {
        event.preventDefault();
        setPanel((current) => (current === "search" ? null : "search"));
        return;
      }
      if (event.key === "Escape") {
        if (panel) setPanel(null);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [panel, save]);

  if (!workspace) {
    return (
      <main className="q-main" aria-label={t("label")} style={{ flex: 1 }}>
        <EmptyState
          className="flex-1"
          icon={Code2Icon}
          title={t("noWorkspace.title")}
          description={t("noWorkspace.body")}
          action={
            <Button onClick={() => navigate({ kind: "inbox" })}>
              {t("noWorkspace.goToInbox")}
            </Button>
          }
        />
      </main>
    );
  }
  const external = active ? state.external[active.path] : undefined;
  const readOnly = offline || !active || active.binary || active.truncated;
  const editor = (() => {
    if (!active) {
      return (
        <EmptyState
          className="flex-1"
          icon={FileCodeIcon}
          title={t("noFile.title")}
          description={t("noFile.body")}
          action={
            <>
              <Button variant="secondary" onClick={() => setPanelTab("files")}>
                {t("noFile.showFiles")}
              </Button>
              <Button variant="ghost" onClick={() => setPanel("search")}>
                <SearchIcon aria-hidden />
                {t("search.run")}
              </Button>
            </>
          }
          shortcut={`${MOD} ${SHIFT} F`}
        />
      );
    }
    if (active.loading) {
      return (
        <div
          style={{ display: "grid", gap: 8, padding: 16 }}
          role="status"
          aria-label={t("file.loading")}
        >
          {[30, 43, 56, 69, 82, 45, 58, 71, 84, 37, 50, 63].map((width) => (
            <div key={width} className="q-skel" style={{ height: 11, width: `${width}%` }} />
          ))}
        </div>
      );
    }
    if (active.error) {
      return (
        <div style={{ margin: 16, fontSize: 12, color: "var(--fg-2)" }}>
          <p
            style={{
              margin: 0,
              fontWeight: 500,
              color: "var(--fg)",
              display: "flex",
              alignItems: "center",
              gap: 6,
            }}
          >
            <TriangleAlertIcon
              className="q-icon s14"
              style={{ color: "var(--danger)" }}
              aria-hidden
            />
            {t("file.didNotOpen", { name: active.name })}
          </p>
          <p style={{ margin: "4px 0 0" }}>{active.error.message}</p>
          {active.error.hint ? (
            <p style={{ margin: "4px 0 0", color: "var(--fg-3)" }}>{active.error.hint}</p>
          ) : null}
          <button
            type="button"
            className="q-btn secondary"
            style={{ marginTop: 12, height: 26 }}
            onClick={() => void session.reload(active.path)}
          >
            {t("common:tryAgain")}
          </button>
        </div>
      );
    }
    if ((active.binary || active.truncated) && active.content.length === 0) {
      return (
        <div style={{ margin: 16, fontSize: 12, color: "var(--fg-2)" }}>
          <p
            style={{
              margin: 0,
              fontWeight: 500,
              color: "var(--fg)",
              display: "flex",
              alignItems: "center",
              gap: 6,
            }}
          >
            <TriangleAlertIcon
              className="q-icon s14"
              style={{ color: "var(--warn)" }}
              aria-hidden
            />
            {active.binary ? t("file.binary") : t("file.tooLarge")}
          </p>
          <p style={{ margin: "4px 0 0" }}>{active.hint ?? t("file.leftToTerminal")}</p>
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginTop: 12 }}>
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={() => openDiff(active.path, "head")}
            >
              {t("file.viewDiff")}
            </button>
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 26 }}
              onClick={() => setPanelTab("files")}
            >
              {t("file.pickAnother")}
            </button>
          </div>
        </div>
      );
    }
    return (
      <CodeEditor
        path={active.path}
        content={active.content}
        readOnly={readOnly}
        revealLine={active.revealLine}
        revealNonce={active.revealNonce}
        onChange={(content) => session.setTabContent(active.path, content)}
        onSave={save}
        onCursor={setCursor}
      />
    );
  })();

  const showDiff = panel === "diff";
  const enterDiff = () => {
    // Entering the pane already asks for the tab's file; mark the tab as
    // handled so the follow effect does not ask twice.
    followedTabRef.current = active?.path ?? null;
    setPanel("diff");
    void session.selectDiffSource(state.diffSource);
  };
  const shownPath = showDiff
    ? (state.diffPath ?? active?.path ?? workspace.path)
    : (active?.path ?? workspace.path);
  const projectName = workspaceName(workspace.path);
  const glyph = thread
    ? glyphForThread(thread, hosts[thread.host]?.online ?? false)
    : glyphForAgentState("idle");

  const backToThread = () => {
    if (!thread) return;
    if (route.kind === "ide") navigate({ kind: "thread", threadId: thread.thread_id });
    else setView("thread");
  };

  return (
    <>
      <main className="q-main" aria-label={t("label")} style={{ flex: 1 }}>
        <header className="q-th-head bare">
          <SidebarReveal />
          <div className="q-th-titles">
            <h1 className="q-ws-title">
              <span className="q-pmono" aria-hidden="true">
                {monogram(projectName)}
              </span>
              {projectName}
            </h1>
            <div className="q-prov">
              <EntityAvatar
                seed={machineSeed(workspace.host)}
                kind="machine"
                label={workspace.host}
                size={14}
              />
              <span>{workspace.host}</span>
              <span className="q-sep">/</span>
              <span className="q-mono" style={{ fontSize: 12 }}>
                {workspace.path}
              </span>
            </div>
          </div>
          {thread ? (
            <Tip label={t("header.backToThread")} command="view.toggle">
              <button type="button" className="q-tchip" onClick={backToThread}>
                <AgentGlyph glyph={glyph} size={14} />
                <span className="t">{thread.title}</span>
                {thread.needs_attention ? (
                  <span className="s">{t("header.needsApproval")}</span>
                ) : null}
              </button>
            </Tip>
          ) : null}
          <div className="q-th-actions">
            <span className="q-vsep" />
            <ViewCluster
              terminalOpen={terminalOpen}
              onToggleTerminal={() => {
                setTerminalMounted(true);
                toggleTerminal();
              }}
            />
          </div>
        </header>

        <TabStrip
          tabs={state.tabs.tabs}
          activePath={state.tabs.activePath}
          note={state.restoredTabs > 0 ? t("tabs.restored", { count: state.restoredTabs }) : null}
          onActivate={session.activateTab}
          onClose={close}
          onReorder={session.moveTab}
        />

        <div className="q-ebar">
          <span
            className="q-mono"
            // The bar names what the main area shows: the diff's file while the
            // diff is open (it can outlive its tab), the editor's file otherwise.
            title={shownPath}
            style={{
              fontSize: 12,
              color: "var(--fg)",
              minWidth: 0,
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
          >
            {shownPath}
          </span>
          <span className="grow" />
          <Tabs
            className="flex-row"
            value={showDiff ? "diff" : "edit"}
            onValueChange={(value) => {
              if (value === "edit") {
                setPanel((current) => (current === "diff" ? null : current));
              } else if (value === "diff") {
                enterDiff();
              }
            }}
          >
            <TabsList variant="segmented" aria-label={t("bar.mode")}>
              <TabsTab value="edit">{t("common:edit")}</TabsTab>
              <TabsTab
                value="diff"
                disabled={!active && !state.diffPath}
                onClick={() => {
                  if (showDiff) enterDiff();
                }}
              >
                {t("bar.diff")}
              </TabsTab>
            </TabsList>
          </Tabs>
          <ChipSelect<DiffSource>
            label={t("bar.source")}
            value={state.diffSource}
            options={DIFF_SOURCES.map((source) => ({
              value: source,
              label: diffSourceName(source),
              hint: diffSourceHint(source),
            }))}
            onChange={(value) => {
              followedTabRef.current = active?.path ?? null;
              setPanel("diff");
              void session.selectDiffSource(value);
            }}
          />
          {showDiff && state.diffSource === "branch" ? (
            <ChipSelect
              label={t("bar.baseBranch")}
              value={state.diffBase ?? state.diffFiles?.base ?? ""}
              placeholder={
                state.branchesLoading
                  ? t("bar.loadingBranches")
                  : state.branchesError
                    ? t("bar.branchesUnavailable")
                    : state.diffFiles?.base
                      ? t("bar.vsBase", { base: state.diffFiles.base })
                      : t("bar.defaultBase")
              }
              options={baseOptions(
                state.diffBranches?.branches ?? [],
                state.diffBase ?? state.diffFiles?.base ?? null,
              ).map((option) => ({
                value: option.value,
                label: option.label,
                ...(option.current ? { hint: t("bar.currentBase") } : {}),
              }))}
              onChange={(value) => void session.setDiffBase(value)}
            />
          ) : null}
          {showDiff && state.diffSource === "commit" ? (
            <ChipSelect
              label={t("bar.commit")}
              value={state.diffCommit ?? ""}
              placeholder={
                state.commitsLoading
                  ? t("bar.loadingCommits")
                  : state.commitsError
                    ? t("bar.commitsUnavailable")
                    : t("bar.chooseCommit")
              }
              options={(state.diffCommits?.commits ?? []).map((commit) => ({
                value: commit.sha,
                label: commitLabel(commit),
                hint: `${commit.author} · ${commit.short_sha}`,
              }))}
              disabled={state.commitsLoading || !state.diffCommits}
              onChange={(value) => void session.setDiffCommit(value)}
            />
          ) : null}
          {active ? (
            <Tip label={t("bar.handToAgentHint")}>
              <button
                type="button"
                className="q-btn ghost q-inline-end"
                style={{ height: 24 }}
                onClick={() => openNewThread()}
              >
                <HandIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("bar.handToAgent")}
              </button>
            </Tip>
          ) : null}
        </div>

        {external && active ? (
          <div
            style={{
              display: "flex",
              flex: "none",
              alignItems: "center",
              gap: 8,
              padding: "5px 16px",
              borderBottom: "1px solid var(--line)",
              background: "var(--warn-soft)",
              color: "var(--warn)",
              fontSize: 12,
            }}
          >
            <TriangleAlertIcon className="q-icon s14" aria-hidden />
            <span style={{ minWidth: 0, flex: 1 }}>
              {external === "deleted" ? t("file.deletedOnDisk") : t("file.changedOnDisk")}
            </span>
            <button
              type="button"
              onClick={() => void session.reload(active.path)}
              className="q-btn ghost"
              style={{ height: 22 }}
            >
              {t("file.reload")}
            </button>
            <button
              type="button"
              onClick={() => {
                const next = { ...state.external };
                delete next[active.path];
                session.patch({ external: next });
              }}
              className="q-btn ghost"
              style={{ height: 22 }}
            >
              {t("file.keepMine")}
            </button>
          </div>
        ) : null}

        <div style={{ position: "relative", display: "flex", minHeight: 0, flex: 1 }}>
          <div
            style={{ display: "flex", minWidth: 0, minHeight: 0, flex: 1, flexDirection: "column" }}
          >
            {showDiff ? (
              <DiffView
                path={state.diffPath ?? active?.path ?? null}
                source={state.diffSource}
                diff={state.diff}
                loading={state.diffLoading}
                error={state.diffError}
                changes={state.agentChanges}
                files={state.diffFiles}
                filesLoading={state.filesLoading}
                filesError={state.filesError}
                commits={state.diffCommits}
                host={workspace.host}
                harness={workspace.harness}
                threadId={workspace.threadId}
                scopeLabel={state.diff?.label ?? state.diffFiles?.label ?? null}
                changeTitle={workspace.threadTitle}
                changeAt={
                  workspace.threadId ? (threads[workspace.threadId]?.last_activity ?? null) : null
                }
                onSelectFile={(path) => void session.openDiff(path, state.diffSource)}
                onOpenFile={(path) => {
                  session.openFile(path);
                  setPanel(null);
                }}
                onRevert={(path, content) => void session.revertTo(path, content)}
                onSendComments={sendReviewComments}
                onCompareHead={() => void session.selectDiffSource("head")}
                onRefresh={() => void session.refreshDiffLists()}
              />
            ) : (
              <div
                className="q-code"
                style={{ display: "flex", flexDirection: "column", padding: 0 }}
              >
                {editor}
              </div>
            )}
          </div>

          {panel === "search" ? (
            <div
              style={
                isPhone
                  ? {
                      position: "absolute",
                      inset: 0,
                      zIndex: 20,
                      display: "flex",
                      flexDirection: "column",
                      background: "var(--bg)",
                    }
                  : {
                      width: 300,
                      flex: "none",
                      display: "flex",
                      flexDirection: "column",
                      borderLeft: "1px solid var(--line)",
                      minHeight: 0,
                    }
              }
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  height: 40,
                  padding: "0 10px 0 14px",
                  borderBottom: "1px solid var(--line)",
                  flex: "none",
                }}
              >
                <span style={{ flex: 1, fontSize: 12, fontWeight: 600 }}>{t("search.title")}</span>
                <button
                  type="button"
                  onClick={() => setPanel(null)}
                  aria-label={t("search.close")}
                  className="q-ibtn"
                >
                  <XIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                </button>
              </div>
              <SearchPanel
                search={state.search}
                onQuery={session.setSearchQuery}
                onOption={session.setSearchOption}
                onRun={() =>
                  void session.runSearch({
                    query: state.search.query,
                    regex: state.search.regex,
                    caseSensitive: state.search.caseSensitive,
                    glob: state.search.glob,
                  })
                }
                onOpen={(path, line) => {
                  session.openFile(path, line);
                  setPanel(null);
                }}
              />
            </div>
          ) : null}
        </div>

        {terminalMounted && workspace.threadId ? (
          <TerminalPanel
            key={workspace.threadId}
            threadId={workspace.threadId}
            host={workspace.host}
            cwd={workspace.path}
            hidden={!terminalOpen}
            onClose={toggleTerminal}
          />
        ) : null}

        <div className="q-statusbar">
          {thread?.branch ? (
            <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <GitBranchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {thread.branch}
            </span>
          ) : null}
          <span>{workspace.host ?? t("status.local")}</span>
          <span className="grow" />
          <span>
            {t("status.position", {
              line: position(cursor.line),
              column: position(cursor.column),
            })}
          </span>
          <span className={isPhone ? "hidden" : undefined}>
            {t("status.spaces", { size: formatNumber(2) })}
          </span>
          <span>{active?.encoding === "utf-8" ? "UTF-8" : (active?.encoding ?? "UTF-8")}</span>
          <span>{languageLabel(active?.path) ?? t("status.plainText")}</span>
          {offline ? (
            <span
              style={{ display: "flex", alignItems: "center", gap: 4, color: "var(--danger)" }}
              title={t("status.daemonOffline")}
            >
              <WifiOffIcon className="q-icon s14" aria-hidden />
              <span className={isPhone ? "hidden" : undefined}>{t("status.offline")}</span>
            </span>
          ) : null}
        </div>
      </main>

      <Dialog open={pendingClose !== null} onOpenChange={(open) => !open && setPendingClose(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t("save.closeTitle", { name: pendingClose?.name ?? "" })}</DialogTitle>
            <DialogDescription>{t("save.closeBody")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setPendingClose(null)}>
              {t("common:cancel")}
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                if (pendingClose) session.confirmClose(pendingClose.path);
                setPendingClose(null);
              }}
            >
              {t("save.discard")}
            </Button>
            <Button onClick={() => void saveAndClose()} disabled={closing}>
              {closing ? <Loader2Icon className="animate-spin" aria-hidden /> : null}
              {t("save.saveAndClose")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={state.conflict !== null}
        onOpenChange={(open) => !open && session.patch({ conflict: null })}
      >
        <DialogContent size="lg">
          <DialogHeader>
            <DialogTitle>
              {state.conflict?.deleted
                ? t("conflict.deletedTitle", { path: state.conflict.path })
                : t("conflict.changedTitle", { path: state.conflict?.path ?? "" })}
            </DialogTitle>
            <DialogDescription>
              {state.conflict?.deleted ? t("conflict.deletedBody") : t("conflict.changedBody")}
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            {state.conflict ? (
              <div className="max-h-64 overflow-auto rounded-lg border border-line bg-bg-side font-mono text-xs leading-4">
                {computeDiff(state.conflict.theirs, state.conflict.mine)
                  .slice(0, 80)
                  .map((line) => (
                    <div
                      key={`${line.kind}-${line.oldLine}-${line.newLine}`}
                      className={cn(
                        "flex gap-2 px-2",
                        line.kind === "added" && "bg-ok-soft text-ok",
                        line.kind === "removed" && "bg-danger-soft text-danger",
                      )}
                    >
                      <span className="w-4 shrink-0 text-fg-3">
                        {line.kind === "added" ? "+" : line.kind === "removed" ? "−" : " "}
                      </span>
                      <span className="whitespace-pre-wrap">{line.text || " "}</span>
                    </div>
                  ))}
              </div>
            ) : null}
          </DialogBody>
          <DialogFooter>
            <Button variant="ghost" onClick={() => session.patch({ conflict: null })}>
              {t("common:cancel")}
            </Button>
            <Button
              variant="secondary"
              onClick={() => {
                session.resolveConflictTakeTheirs();
              }}
            >
              {state.conflict?.deleted ? t("conflict.leaveDeleted") : t("conflict.takeTheirs")}
            </Button>
            <Button
              disabled={resolving}
              onClick={() => {
                setResolving(true);
                void session
                  .resolveConflictKeepMine()
                  .catch(toastError)
                  .finally(() => setResolving(false));
              }}
            >
              {resolving ? <Loader2Icon className="animate-spin" aria-hidden /> : null}
              {state.conflict?.deleted ? t("conflict.recreate") : t("conflict.keepMine")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
