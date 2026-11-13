import {
  Code2Icon,
  FileCodeIcon,
  FileDiffIcon,
  GitBranchIcon,
  Loader2Icon,
  PanelLeftIcon,
  SaveIcon,
  SearchIcon,
  TriangleAlertIcon,
  WifiOffIcon,
  XIcon,
} from "lucide-react";
import type * as React from "react";
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
import { Skeleton } from "@/components/ui/skeleton";
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

const EDITOR_SKELETON_WIDTHS = [30, 43, 56, 69, 82, 45, 58, 71, 84, 37, 50, 63];

function PanelShell({
  title,
  onClose,
  children,
  className,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex min-h-0 flex-col border-l border-line bg-paper", className)}>
      <div className="flex h-8 shrink-0 items-center justify-between border-b border-line bg-panel px-2">
        <span className="text-[10px] font-semibold tracking-[0.08em] text-ink-muted uppercase">
          {title}
        </span>
        <button
          type="button"
          onClick={onClose}
          aria-label={`Close ${title.toLowerCase()}`}
          className="grid size-5 place-items-center rounded-sm text-ink-faint hover:bg-raised hover:text-ink"
        >
          <XIcon className="size-3" aria-hidden />
        </button>
      </div>
      {children}
    </div>
  );
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
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const workspace = useMemo(
    () => options.find((option) => option.path === selectedPath) ?? options[0] ?? null,
    [options, selectedPath],
  );

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

  const toggleDiff = useCallback(() => {
    if (panel === "diff") {
      setPanel(null);
      return;
    }
    const path = active?.path;
    if (!path) {
      pushToast({ title: "Open a file first.", variant: "info" });
      return;
    }
    openDiff(path, state.diffMode);
  }, [panel, active?.path, openDiff, state.diffMode]);

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
      <main className="flex min-h-0 flex-1 flex-col bg-paper" aria-label="IDE">
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
  const changedCount = state.agentChanges?.files.length ?? 0;
  const threadName = workspace.threadTitle ?? "No thread";
  const harness = workspace.harness ? harnessLabel(workspace.harness) : "the agent";
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
        <div className="space-y-2 p-4" role="status" aria-label="Loading file">
          {EDITOR_SKELETON_WIDTHS.map((width) => (
            <Skeleton key={width} className="h-3" style={{ width: `${width}%` }} />
          ))}
        </div>
      );
    }
    if (active.error) {
      return (
        <div className="m-4 rounded-sm border border-line bg-raised p-4 text-compact">
          <p className="flex items-center gap-1.5 font-medium text-ink">
            <TriangleAlertIcon className="size-3.5 text-strike-ink" aria-hidden />
            {active.name} did not open.
          </p>
          <p className="mt-1 text-ink-muted">{active.error.message}</p>
          {active.error.hint ? <p className="mt-1 text-ink-faint">{active.error.hint}</p> : null}
          <Button
            className="mt-3"
            size="sm"
            variant="secondary"
            onClick={() => void session.reload(active.path)}
          >
            Try again
          </Button>
        </div>
      );
    }
    if ((active.binary || active.truncated) && active.content.length === 0) {
      return (
        <div className="m-4 rounded-sm border border-line bg-raised p-4 text-compact">
          <p className="flex items-center gap-1.5 font-medium text-ink">
            <TriangleAlertIcon className="size-3.5 text-cue-ink" aria-hidden />
            {active.binary ? "This is a binary file." : "This file is too large for the editor."}
          </p>
          <p className="mt-1 text-ink-muted">
            {active.hint ?? "h4rn leaves this file to a terminal or your full IDE."}
          </p>
          <div className="mt-3 flex items-center gap-2">
            <Button size="sm" variant="secondary" onClick={() => openDiff(active.path, "head")}>
              <FileDiffIcon aria-hidden />
              View diff
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setTreeOpen(true)}>
              Pick another file
            </Button>
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

  const panelBody =
    panel === "search" ? (
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
    ) : panel === "diff" ? (
      <DiffView
        path={state.diffPath ?? active?.path ?? null}
        mode={state.diffMode}
        diff={state.diff}
        loading={state.diffLoading}
        error={state.diffError}
        changes={state.agentChanges}
        onMode={(mode) => {
          const path = state.diffPath ?? active?.path;
          if (path) void session.openDiff(path, mode);
        }}
        onSelectFile={(path) => void session.openDiff(path, state.diffMode)}
        onOpenFile={(path) => {
          session.openFile(path);
          setPanel(null);
        }}
      />
    ) : null;

  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-paper text-ink" aria-label="IDE">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-line px-2">
        {isPhone ? (
          <button
            type="button"
            aria-label="Open files"
            onClick={() => setTreeOpen(true)}
            className="grid size-9 place-items-center rounded-sm text-ink-muted hover:bg-panel hover:text-ink"
          >
            <PanelLeftIcon className="size-4" aria-hidden />
          </button>
        ) : null}
        <label className="flex min-w-0 items-center gap-1.5">
          <span className="sr-only">Workspace</span>
          <select
            value={workspace.path}
            onChange={(event) => setSelectedPath(event.target.value)}
            className="h-8 max-w-[220px] rounded-sm border border-transparent bg-transparent px-1 font-mono text-[11px] text-ink hover:border-line focus-visible:border-cue focus-visible:outline-none"
          >
            {options.map((option) => (
              <option key={option.path} value={option.path}>
                {workspaceName(option.path)}
              </option>
            ))}
          </select>
        </label>
        <span className="hidden items-center gap-1 font-mono text-[10px] text-ink-faint sm:flex">
          <GitBranchIcon className="size-3.5" aria-hidden />
          main
        </span>
        {!isPhone ? (
          <span className="min-w-0 truncate font-mono text-[10px] text-ink-faint">
            {workspace.path}
          </span>
        ) : null}

        <div className="ml-auto flex items-center gap-1">
          {dirty ? (
            <span className="hidden font-mono text-[10px] text-cue-ink sm:inline">Unsaved</span>
          ) : null}
          <button
            type="button"
            onClick={() => void save()}
            disabled={!dirty || readOnly}
            aria-label="Save file"
            className="grid size-8 place-items-center rounded-sm text-ink-muted hover:bg-panel hover:text-ink disabled:pointer-events-none disabled:opacity-40"
          >
            <SaveIcon className="size-4" aria-hidden />
          </button>
          <button
            type="button"
            aria-label="Search workspace"
            aria-pressed={panel === "search"}
            onClick={() => setPanel((current) => (current === "search" ? null : "search"))}
            className={cn(
              "grid size-8 place-items-center rounded-sm",
              panel === "search"
                ? "bg-cue-tint text-cue-ink"
                : "text-ink-muted hover:bg-panel hover:text-ink",
            )}
          >
            <SearchIcon className="size-4" aria-hidden />
          </button>
          <button
            type="button"
            aria-label="Toggle diff"
            aria-pressed={panel === "diff"}
            onClick={toggleDiff}
            className={cn(
              "flex h-8 items-center gap-1 rounded-sm px-2 text-micro",
              panel === "diff"
                ? "bg-cue-tint text-cue-ink"
                : "text-ink-muted hover:bg-panel hover:text-ink",
            )}
          >
            <FileDiffIcon className="size-4" aria-hidden />
            <span className="hidden sm:inline">Diff</span>
          </button>
        </div>
      </header>

      <div className="flex min-h-0 flex-1">
        {!isPhone ? (
          <FileTree
            root={state.listings[""]}
            listings={state.listings}
            expanded={state.expanded}
            activePath={active?.path ?? null}
            agentChanges={state.agentChanges}
            agentLoading={state.agentLoading}
            loading={state.treeLoading}
            error={state.treeError}
            onToggle={session.toggleDirectory}
            onOpen={session.openFile}
            onReviewAll={() => {
              const path = state.agentChanges?.files[0]?.path ?? active?.path;
              if (path) openDiff(path, "agent");
            }}
            onOpenAgent={(path) => openDiff(path, "agent")}
            onRetry={() => void session.loadListing("")}
          />
        ) : null}

        <div className="flex min-w-0 flex-1 flex-col">
          <TabStrip
            tabs={state.tabs.tabs}
            activePath={state.tabs.activePath}
            onActivate={session.activateTab}
            onClose={close}
            onReorder={session.moveTab}
          />

          {external && active ? (
            <div className="flex shrink-0 items-center gap-2 border-b border-line bg-cue-tint px-3 py-1.5 text-micro text-cue-ink">
              <TriangleAlertIcon className="size-3.5 shrink-0" aria-hidden />
              <span className="min-w-0 flex-1">
                {external === "deleted"
                  ? "This file was deleted on disk."
                  : "This file changed on disk while you were editing."}
              </span>
              <button
                type="button"
                onClick={() => void session.reload(active.path)}
                className="shrink-0 underline underline-offset-2 hover:no-underline"
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
                className="shrink-0 underline underline-offset-2 hover:no-underline"
              >
                Keep mine
              </button>
            </div>
          ) : null}

          <div className="relative flex min-h-0 flex-1">
            <div className="flex min-h-0 flex-1 flex-col">
              {active && !active.loading && !active.error ? (
                <div className="flex h-7 shrink-0 items-center gap-2 border-b border-line bg-panel px-3 font-mono text-[10px] text-ink-faint">
                  <span className="min-w-0 flex-1 truncate text-ink-muted">{active.path}</span>
                  <span>{active.encoding}</span>
                  <span className="text-ink-faint">{languageLabel(active.path)}</span>
                </div>
              ) : null}
              <div className="flex min-h-0 flex-1 flex-col">{editor}</div>
            </div>

            {panelBody ? (
              <PanelShell
                title={panel === "search" ? "Search" : "Diff"}
                onClose={() => setPanel(null)}
                className={isPhone ? "absolute inset-0 z-20 border-l-0" : "w-[360px] shrink-0"}
              >
                {panelBody}
              </PanelShell>
            ) : null}
          </div>
        </div>
      </div>

      <footer className="flex h-7 shrink-0 items-center gap-3 border-t border-line bg-panel px-3 font-mono text-[10px] text-ink-faint">
        <span className="flex min-w-0 items-center gap-1">
          <GitBranchIcon className="size-3 shrink-0" aria-hidden />
          <span className="truncate text-ink-muted">main</span>
        </span>
        <span className="hidden min-w-0 items-center gap-1 sm:flex">
          <span className="truncate">
            {changedCount} {changedCount === 1 ? "file" : "files"} changed by {harness}
          </span>
          {state.agentChanges ? (
            <span className="shrink-0">
              <span className="text-standby-ink">
                +{state.agentChanges.files.reduce((total, file) => total + file.added, 0)}
              </span>{" "}
              <span className="text-strike-ink">
                −{state.agentChanges.files.reduce((total, file) => total + file.removed, 0)}
              </span>
            </span>
          ) : null}
        </span>
        <span className="ml-auto flex items-center gap-3">
          <span className="hidden sm:inline">{languageLabel(active?.path)}</span>
          <span>
            Ln {cursor.line}, Col {cursor.column}
          </span>
          <span className={cn(dirty ? "text-cue-ink" : "text-ink-faint")}>
            {state.saveState === "saving" ? "Saving…" : dirty ? "Unsaved" : "Up to date"}
          </span>
          {offline ? (
            <span className="flex items-center gap-1 text-strike-ink" title="Daemon offline">
              <WifiOffIcon className="size-3" aria-hidden />
              <span className="hidden sm:inline">Offline</span>
            </span>
          ) : null}
          <span className="hidden lg:inline">{threadName}</span>
        </span>
      </footer>

      {isPhone ? (
        <Sheet open={treeOpen} onOpenChange={setTreeOpen}>
          <SheetContent side="left" className="w-[86vw] max-w-sm p-0">
            <SheetTitle className="sr-only">Workspace files</SheetTitle>
            <div className="flex h-full min-h-0 flex-col [&>aside]:w-full [&>aside]:border-r-0">
              <FileTree
                root={state.listings[""]}
                listings={state.listings}
                expanded={state.expanded}
                activePath={active?.path ?? null}
                agentChanges={state.agentChanges}
                agentLoading={state.agentLoading}
                loading={state.treeLoading}
                error={state.treeError}
                onToggle={session.toggleDirectory}
                onOpen={(path) => {
                  session.openFile(path);
                  setTreeOpen(false);
                }}
                onReviewAll={() => {
                  const path = state.agentChanges?.files[0]?.path ?? active?.path;
                  if (path) {
                    openDiff(path, "agent");
                    setTreeOpen(false);
                  }
                }}
                onOpenAgent={(path) => {
                  openDiff(path, "agent");
                  setTreeOpen(false);
                }}
                onRetry={() => void session.loadListing("")}
              />
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
    </main>
  );
}
