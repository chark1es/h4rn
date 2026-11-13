import {
  Code2Icon,
  FileCodeIcon,
  GitBranchIcon,
  Loader2Icon,
  PanelLeftIcon,
  SaveIcon,
  SearchIcon,
  TriangleAlertIcon,
  WifiOffIcon,
  XIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import { EmptyState } from "@/components/common/EmptyState";
import { MOD, SHIFT } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import type { ThreadId, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { toProtoError } from "@/lib/client/errors";
import { harnessLabel } from "@/lib/entities";
import { ensureThread, navigate, pushToast, useApp } from "@/lib/store/appStore";
import type { ThreadLog } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { CodeEditor, type CursorPosition } from "./CodeEditor";
import { DiffView } from "./DiffView";
import { computeDiff } from "./diff";
import { FileTree } from "./FileTree";
import { SearchPanel } from "./SearchPanel";
import { TabStrip } from "./TabStrip";
import { type IdeTab, isDirty } from "./tabs";
import { languageLabel, useIdeSession, type WorkspaceOption } from "./useIdeSession";

function buildOptions(
  threads: Record<ThreadId, ThreadSummary>,
  logs: Record<ThreadId, ThreadLog>,
  selected: ThreadId | null,
): WorkspaceOption[] {
  const byPath = new Map<string, WorkspaceOption>();
  for (const thread of Object.values(threads)) {
    if (!thread.workspace_path) continue;
    const turn = logs[thread.thread_id]?.turns.at(-1)?.id ?? null;
    const option: WorkspaceOption = {
      path: thread.workspace_path,
      host: thread.host,
      threadId: thread.thread_id,
      threadTitle: thread.title,
      turnId: turn,
      harness: thread.harness,
    };
    const existing = byPath.get(thread.workspace_path);
    if (!existing || thread.thread_id === selected) byPath.set(thread.workspace_path, option);
  }
  return [...byPath.values()];
}

function workspaceName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

export function IdePane() {
  const isPhone = useIsPhone();
  const threads = useApp((state) => state.threads);
  const logs = useApp((state) => state.logs);
  const selectedThread = useApp((state) => state.ui.selectedThreadId);
  const connection = useApp((state) => state.connection.status);
  const offline = connection !== "online";

  const options = useMemo(
    () => buildOptions(threads, logs, selectedThread),
    [threads, logs, selectedThread],
  );
  const workspace = options[0] ?? null;

  const session = useIdeSession(workspace);
  const active = session.active;
  const { state } = session;

  const [panel, setPanel] = useState<"search" | "diff" | null>(null);
  const [treeOpen, setTreeOpen] = useState(false);
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
    if (workspace?.threadId) ensureThread(workspace.threadId);
  }, [workspace?.threadId]);

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
        if (treeOpen) setTreeOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [panel, save, treeOpen]);

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

  const dirty = active ? isDirty(active) : false;
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
              {isPhone ? (
                <Button variant="secondary" onClick={() => setTreeOpen(true)}>
                  <PanelLeftIcon aria-hidden />
                  Files
                </Button>
              ) : null}
              <Button variant={isPhone ? "ghost" : "secondary"} onClick={() => setPanel("search")}>
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
              onClick={() => setTreeOpen(true)}
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

  const tree = (
    <FileTree
      root={state.listings[""]}
      listings={state.listings}
      expanded={state.expanded}
      activePath={active?.path ?? null}
      agentChanges={state.agentChanges}
      agentLoading={state.agentLoading}
      loading={state.treeLoading}
      error={state.treeError}
      workspaceName={workspaceName(workspace.path)}
      host={workspace.host}
      changeLabel={
        workspace.threadTitle && workspace.harness
          ? `${workspace.threadTitle} · ${harnessLabel(workspace.harness)}`
          : undefined
      }
      className={isPhone ? "fill" : undefined}
      onToggle={session.toggleDirectory}
      onOpen={(path) => {
        session.openFile(path);
        if (isPhone) setTreeOpen(false);
      }}
      onOpenAgent={(path) => {
        openDiff(path, "agent");
        if (isPhone) setTreeOpen(false);
      }}
      onRetry={() => void session.loadListing("")}
      onSearch={() => setPanel((current) => (current === "search" ? null : "search"))}
    />
  );

  const showDiff = panel === "diff";

  return (
    <>
      {!isPhone ? tree : null}
      <main className="q-main" aria-label="IDE" style={{ flex: 1 }}>
        <TabStrip
          tabs={state.tabs.tabs}
          activePath={state.tabs.activePath}
          onActivate={session.activateTab}
          onClose={close}
          onReorder={session.moveTab}
        />

        <div className="q-ebar">
          {isPhone ? (
            <button
              type="button"
              aria-label="Open files"
              onClick={() => setTreeOpen(true)}
              className="q-ibtn"
              style={{ marginLeft: -6 }}
            >
              <PanelLeftIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          ) : null}
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
          {!isPhone ? (
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
          ) : null}
          {isPhone ? (
            <>
              <button
                type="button"
                onClick={() => void save()}
                disabled={!dirty || readOnly}
                aria-label="Save file"
                className="q-ibtn"
              >
                <SaveIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </button>
              <button
                type="button"
                aria-label="Search workspace"
                aria-pressed={panel === "search"}
                onClick={() => setPanel((current) => (current === "search" ? null : "search"))}
                className={cn("q-ibtn", panel === "search" && "on")}
              >
                <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </button>
            </>
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

      {isPhone ? (
        <Sheet open={treeOpen} onOpenChange={setTreeOpen}>
          <SheetContent side="left" className="w-[86vw] max-w-sm p-0">
            <SheetTitle className="sr-only">Workspace files</SheetTitle>
            <div style={{ display: "flex", height: "100%", minHeight: 0, flexDirection: "column" }}>
              {tree}
            </div>
          </SheetContent>
        </Sheet>
      ) : null}

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
            <DialogTitle>{state.conflict?.path} changed on disk.</DialogTitle>
            <DialogDescription>
              Someone else saved this file after you opened it. Keep your version, take theirs, or
              cancel and keep editing.
            </DialogDescription>
          </DialogHeader>
          {state.conflict ? (
            <div className="max-h-64 overflow-auto rounded-sm border border-line bg-panel font-mono text-[10px] leading-4">
              {computeDiff(state.conflict.theirs, state.conflict.mine)
                .slice(0, 80)
                .map((line) => (
                  <div
                    key={`${line.kind}-${line.oldLine}-${line.newLine}`}
                    className={cn(
                      "flex gap-2 px-2",
                      line.kind === "added" && "bg-standby-tint text-standby-ink",
                      line.kind === "removed" && "bg-strike-tint text-strike-ink",
                    )}
                  >
                    <span className="w-4 shrink-0 text-ink-faint">
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
              Take theirs
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
              Keep mine
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
