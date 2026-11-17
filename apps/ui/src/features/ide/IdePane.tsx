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
import { useCallback, useEffect, useMemo, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { EmptyState } from "@/components/common/EmptyState";
import { MOD, SHIFT } from "@/components/common/Kbd";
import { SidebarReveal } from "@/components/shell/Sidebar";
import { ViewCluster } from "@/components/thread/ThreadHeader";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import type { ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { glyphForAgentState, glyphForThread } from "@/lib/agent";
import { toProtoError } from "@/lib/client/errors";
import { machineSeed } from "@/lib/entities";
import { monogram } from "@/lib/grouping";
import { titleWithShortcut } from "@/lib/keybindings";
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
import { activeThread } from "@/lib/thread-selection";
import { cn } from "@/lib/utils";
import { CodeEditor, type CursorPosition } from "./CodeEditor";
import { DiffView } from "./DiffView";
import { computeDiff } from "./diff";
import { SearchPanel } from "./SearchPanel";
import { TabStrip } from "./TabStrip";
import type { IdeTab } from "./tabs";
import { languageLabel, useIdeSession, type WorkspaceOption } from "./useIdeSession";

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

export function IdePane() {
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
  const latestTurn = thread ? (logs[thread.thread_id]?.turns.at(-1)?.id ?? null) : null;
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
      title: "The save did not complete.",
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
    (path: string, mode: "head" | "agent") => {
      setPanel("diff");
      void session.openDiff(path, mode);
    },
    [session.openDiff],
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
      <main className="q-main" aria-label="IDE" style={{ flex: 1 }}>
        <EmptyState
          className="flex-1"
          icon={Code2Icon}
          title="No workspace to open."
          description="The IDE reads the working tree of a thread. Start or open a thread, then return here."
          action={<Button onClick={() => navigate({ kind: "inbox" })}>Go to inbox</Button>}
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
          title="No file is open."
          description="Choose a file in the tree, or search the workspace. Diffs work without opening the editor first."
          action={
            <>
              <Button variant="secondary" onClick={() => setPanelTab("files")}>
                Show files
              </Button>
              <Button variant="ghost" onClick={() => setPanel("search")}>
                <SearchIcon aria-hidden />
                Search
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
          aria-label="Loading file"
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
            {active.name} did not open.
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
            Try again
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
            {active.binary ? "This is a binary file." : "This file is too large for the editor."}
          </p>
          <p style={{ margin: "4px 0 0" }}>
            {active.hint ?? "h4rn leaves this file to a terminal or your full IDE."}
          </p>
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginTop: 12 }}>
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={() => openDiff(active.path, "head")}
            >
              View diff
            </button>
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 26 }}
              onClick={() => setPanelTab("files")}
            >
              Pick another file
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
      <main className="q-main" aria-label="IDE" style={{ flex: 1 }}>
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
              <span className="q-mono" style={{ fontSize: 11.5 }}>
                {workspace.path}
              </span>
            </div>
          </div>
          {thread ? (
            <button
              type="button"
              className="q-tchip"
              title={titleWithShortcut("Back to the thread", "view.toggle")}
              onClick={backToThread}
            >
              <AgentGlyph glyph={glyph} size={14} />
              <span className="t">{thread.title}</span>
              {thread.needs_attention ? <span className="s">Needs approval</span> : null}
            </button>
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
          note={
            state.restoredTabs > 0
              ? `Restored ${state.restoredTabs} ${state.restoredTabs === 1 ? "tab" : "tabs"}`
              : null
          }
          onActivate={session.activateTab}
          onClose={close}
          onReorder={session.moveTab}
        />

        <div className="q-ebar">
          <span
            className="q-mono"
            style={{
              fontSize: 12,
              color: "var(--fg)",
              minWidth: 0,
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
          >
            {active?.path ?? workspace.path}
          </span>
          <span className="grow" />
          <div className="q-seg inline" role="tablist" aria-label="Editor mode">
            <button
              type="button"
              role="tab"
              aria-selected={!showDiff}
              onClick={() => setPanel((current) => (current === "diff" ? null : current))}
            >
              Edit
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={showDiff && state.diffMode === "head"}
              disabled={!active}
              onClick={() => active && openDiff(active.path, "head")}
            >
              vs HEAD
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={showDiff && state.diffMode === "agent"}
              disabled={!active}
              onClick={() => active && openDiff(active.path, "agent")}
            >
              vs agent
            </button>
          </div>
          {active ? (
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              onClick={() => openNewThread()}
              title="Start a thread with this file in context"
            >
              <HandIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Hand to agent
            </button>
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
              {external === "deleted"
                ? "This file was deleted on disk."
                : "This file changed on disk while you were editing."}
            </span>
            <button
              type="button"
              onClick={() => void session.reload(active.path)}
              className="q-btn ghost"
              style={{ height: 22 }}
            >
              Reload
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
              Keep mine
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
                mode={state.diffMode}
                diff={state.diff}
                loading={state.diffLoading}
                error={state.diffError}
                changes={state.agentChanges}
                host={workspace.host}
                harness={workspace.harness}
                changeTitle={workspace.threadTitle}
                changeAt={
                  workspace.threadId ? (threads[workspace.threadId]?.last_activity ?? null) : null
                }
                onSelectFile={(path) => void session.openDiff(path, state.diffMode)}
                onOpenFile={(path) => {
                  session.openFile(path);
                  setPanel(null);
                }}
                onRevert={(path, content) => void session.revertTo(path, content)}
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
                <span style={{ flex: 1, fontSize: 12, fontWeight: 600 }}>Search</span>
                <button
                  type="button"
                  onClick={() => setPanel(null)}
                  aria-label="Close search"
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
          <span style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <GitBranchIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            main
          </span>
          <span>{workspace.host ?? "local"}</span>
          <span className="grow" />
          <span>
            Ln {cursor.line}, Col {cursor.column}
          </span>
          <span className={isPhone ? "hidden" : undefined}>Spaces: 2</span>
          <span>{active?.encoding === "utf-8" ? "UTF-8" : (active?.encoding ?? "UTF-8")}</span>
          <span>{languageLabel(active?.path)}</span>
          {offline ? (
            <span
              style={{ display: "flex", alignItems: "center", gap: 4, color: "var(--danger)" }}
              title="Daemon offline"
            >
              <WifiOffIcon className="q-icon s12" aria-hidden />
              <span className={isPhone ? "hidden" : undefined}>Offline</span>
            </span>
          ) : null}
        </div>
      </main>

      <Dialog open={pendingClose !== null} onOpenChange={(open) => !open && setPendingClose(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Save {pendingClose?.name} before closing?</DialogTitle>
            <DialogDescription>
              This tab has changes the daemon has not accepted yet. Closing it without saving
              discards them.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setPendingClose(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                if (pendingClose) session.confirmClose(pendingClose.path);
                setPendingClose(null);
              }}
            >
              Discard
            </Button>
            <Button onClick={() => void saveAndClose()} disabled={closing}>
              {closing ? <Loader2Icon className="animate-spin" aria-hidden /> : null}
              Save and close
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={state.conflict !== null}
        onOpenChange={(open) => !open && session.patch({ conflict: null })}
      >
        <DialogContent className="sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              {state.conflict?.path} {state.conflict?.deleted ? "was deleted" : "changed"} on disk.
            </DialogTitle>
            <DialogDescription>
              {state.conflict?.deleted
                ? "This file was removed after you opened it. Recreate it with your version, leave it deleted, or cancel and keep editing."
                : "Someone else saved this file after you opened it. Keep your version, take theirs, or cancel and keep editing."}
            </DialogDescription>
          </DialogHeader>
          {state.conflict ? (
            <div className="max-h-64 overflow-auto rounded-md border border-line bg-bg-side font-mono text-[10px] leading-4">
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
          <DialogFooter>
            <Button variant="ghost" onClick={() => session.patch({ conflict: null })}>
              Cancel
            </Button>
            <Button
              variant="secondary"
              onClick={() => {
                session.resolveConflictTakeTheirs();
              }}
            >
              {state.conflict?.deleted ? "Leave deleted" : "Take theirs"}
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
              {state.conflict?.deleted ? "Recreate with mine" : "Keep mine"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
