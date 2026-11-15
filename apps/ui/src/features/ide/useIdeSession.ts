import { useCallback, useEffect, useRef, useState } from "react";

import type {
  DiffAgainst,
  FsDiff,
  FsListing,
  FsSearchMatch,
  FsTurnChanges,
  FsWriteConflict,
  GitFileStatus,
  HarnessKind,
  HostId,
  ProtoError,
  ThreadId,
  TurnId,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";
import { conflictFromWrite, keepMineBaseEtag } from "./conflict";
import {
  activeTab,
  closeTab,
  type IdeTab,
  isDirty,
  openTab,
  reorderTabs,
  type TabState,
  updateTab,
} from "./tabs";

export type WorkspaceOption = {
  path: string;
  host: HostId;
  threadId: ThreadId | null;
  threadTitle: string | null;
  turnId: TurnId | null;
  harness: HarnessKind | null;
};

export type SaveState = "idle" | "saving" | "saved";

export type SearchState = {
  query: string;
  regex: boolean;
  caseSensitive: boolean;
  glob: string | null;
  running: boolean;
  results: FsSearchMatch[];
  truncated: boolean;
  error: ProtoError | null;
  searchId: string | null;
};

export type ConflictState = {
  path: string;
  mine: string;
  theirs: string;
  expectedEtag: string | null;
  /** Null when the file no longer exists on disk. */
  currentEtag: string | null;
  /** The file was deleted or moved while it was open. */
  deleted: boolean;
};

export type ExternalChange = "modified" | "deleted";

export type IdeState = {
  /** The host + workspace this state belongs to; guards cross-project hydration. */
  workspaceKey: string | null;
  /** Tabs brought back from a previous visit; the quiet "Restored N tabs" note. */
  restoredTabs: number;
  tabs: TabState;
  listings: Record<string, FsListing>;
  expanded: Record<string, true>;
  treeLoading: boolean;
  treeError: ProtoError | null;
  saveState: SaveState;
  savedAt: number | null;
  conflict: ConflictState | null;
  external: Record<string, ExternalChange>;
  search: SearchState;
  diff: FsDiff | null;
  diffPath: string | null;
  diffMode: "head" | "agent";
  diffLoading: boolean;
  diffError: ProtoError | null;
  agentChanges: FsTurnChanges | null;
  agentLoading: boolean;
  agentError: ProtoError | null;
};

const emptySearch: SearchState = {
  query: "",
  regex: false,
  caseSensitive: false,
  glob: null,
  running: false,
  results: [],
  truncated: false,
  error: null,
  searchId: null,
};

function initialState(): IdeState {
  return {
    workspaceKey: null,
    restoredTabs: 0,
    tabs: { tabs: [], activePath: null },
    listings: {},
    expanded: {},
    treeLoading: true,
    treeError: null,
    saveState: "idle",
    savedAt: null,
    conflict: null,
    external: {},
    search: emptySearch,
    diff: null,
    diffPath: null,
    diffMode: "head",
    diffLoading: false,
    diffError: null,
    agentChanges: null,
    agentLoading: false,
    agentError: null,
  };
}

function omit<T>(record: Record<string, T>, key: string): Record<string, T> {
  if (!(key in record)) return record;
  const next = { ...record };
  delete next[key];
  return next;
}

const CODE_EXTENSIONS = new Set([
  "ts",
  "tsx",
  "js",
  "jsx",
  "rs",
  "py",
  "go",
  "json",
  "md",
  "css",
  "html",
  "yaml",
  "yml",
  "toml",
]);

function isCodePath(path: string): boolean {
  const extension = path.split(".").at(-1)?.toLowerCase();
  return extension !== undefined && CODE_EXTENSIONS.has(extension);
}

/** The mock and screenshots expect this file; real repos usually have a src. */
const PREFERRED_FILES = [
  "src/auth/session.test.ts",
  "src/auth/session.ts",
  "src/app.ts",
  "README.md",
];

/** Open tabs belong to a project, not to the editor component's lifetime. */
const IDE_CACHE = new Map<string, IdeState>();
const projectKey = (host: string, path: string) => `${host}\u0000${path}`;

/**
 * Loads and mutates the IDE's daemon state.
 *
 * Every request goes through the shared protocol client, so mock mode and a
 * real daemon run the same code. The hook owns buffers, watchers, optimistic
 * saves and the two diff scopes; components only render.
 */
export function useIdeSession(workspace: WorkspaceOption | null) {
  const [state, applyState] = useState<IdeState>(initialState);
  const stateRef = useRef(state);
  stateRef.current = state;

  const searchIdRef = useRef<string | null>(null);
  const savingPathRef = useRef<string | null>(null);
  const autoOpenedRef = useRef<string | null>(null);
  const loadedKeyRef = useRef<string | null>(null);
  const workspaceRef = useRef(workspace);
  workspaceRef.current = workspace;

  /** Every accepted edit also refreshes the project's remembered tabs. */
  const setState = useCallback((next: IdeState | ((current: IdeState) => IdeState)) => {
    applyState((current) => {
      const value = typeof next === "function" ? next(current) : next;
      const key = loadedKeyRef.current;
      if (key) IDE_CACHE.set(key, value);
      return value;
    });
  }, []);

  const patch = useCallback(
    (next: Partial<IdeState>) => {
      setState((current) => ({ ...current, ...next }));
    },
    [setState],
  );

  const request = workspace ? { host: workspace.host, workspace: workspace.path } : null;

  // -------------------------------------------------------------------
  // Tree
  // -------------------------------------------------------------------

  const loadListing = useCallback(
    async (path: string) => {
      const target = workspaceRef.current;
      if (!target) return;
      try {
        const response = await getClient().request({
          type: "fs_list",
          host: target.host,
          workspace: target.path,
          path,
        });
        if (response.type !== "fs_listing") return;
        setState((current) => ({
          ...current,
          listings: { ...current.listings, [path]: response.listing },
          treeLoading: false,
          treeError: null,
        }));
      } catch (error) {
        setState((current) => ({
          ...current,
          treeLoading: false,
          treeError: toProtoError(error),
        }));
      }
    },
    [setState],
  );

  // -------------------------------------------------------------------
  // Files and tabs
  // -------------------------------------------------------------------

  const readInto = useCallback(
    async (path: string, line: number | null = null) => {
      const target = workspaceRef.current;
      if (!target) return;
      setState((current) => ({
        ...current,
        tabs: openTab(current.tabs, path),
      }));
      try {
        const response = await getClient().request({
          type: "fs_read",
          host: target.host,
          workspace: target.path,
          path,
        });
        if (response.type !== "fs_file") return;
        const file = response.file;
        setState((current) => ({
          ...current,
          external: omit(current.external, path),
          tabs: updateTab(current.tabs, path, {
            content: file.content ?? "",
            savedContent: file.content ?? "",
            etag: file.etag,
            encoding: file.encoding,
            loading: false,
            binary: file.binary,
            truncated: file.truncated,
            hint: file.hint,
            error: null,
            ...(line === null ? {} : { revealLine: line, revealNonce: Date.now() }),
          }),
        }));
      } catch (error) {
        setState((current) => ({
          ...current,
          tabs: updateTab(current.tabs, path, {
            loading: false,
            error: toProtoError(error),
          }),
        }));
      }
    },
    [setState],
  );

  const openFile = useCallback(
    (path: string, line: number | null = null) => {
      void readInto(path, line);
    },
    [readInto],
  );

  const activateTab = useCallback(
    (path: string) => {
      setState((current) => ({ ...current, tabs: { ...current.tabs, activePath: path } }));
    },
    [setState],
  );

  const setTabContent = useCallback(
    (path: string, content: string) => {
      setState((current) => ({
        ...current,
        saveState: "idle",
        tabs: updateTab(current.tabs, path, { content }),
      }));
    },
    [setState],
  );

  const saveTab = useCallback(
    async (path: string) => {
      const target = workspaceRef.current;
      const tab = stateRef.current.tabs.tabs.find((entry) => entry.path === path);
      if (!target || !tab || tab.loading || tab.binary || tab.truncated || tab.error) return;
      if (!isDirty(tab)) return;
      const content = tab.content;
      const previousSaved = tab.savedContent;
      const baseEtag = tab.etag;
      savingPathRef.current = path;
      setState((current) => ({
        ...current,
        saveState: "saving",
        tabs: updateTab(current.tabs, path, { savedContent: content }),
      }));
      try {
        const response = await getClient().request({
          type: "fs_write",
          host: target.host,
          workspace: target.path,
          path,
          content,
          base_etag: baseEtag,
        });
        if (response.type !== "fs_written") return;
        const result = response.result;
        if (result.conflict) {
          setState((current) => ({
            ...current,
            saveState: "idle",
            tabs: updateTab(current.tabs, path, { savedContent: previousSaved }),
            conflict: conflictFromWrite(path, content, result.conflict as FsWriteConflict),
          }));
          return;
        }
        setState((current) => ({
          ...current,
          saveState: "saved",
          savedAt: Date.now(),
          external: omit(current.external, path),
          tabs: updateTab(current.tabs, path, {
            savedContent: content,
            etag: result.etag,
          }),
        }));
      } catch (error) {
        setState((current) => ({
          ...current,
          saveState: "idle",
          tabs: updateTab(current.tabs, path, { savedContent: previousSaved }),
        }));
        throw error;
      } finally {
        savingPathRef.current = null;
      }
    },
    [setState],
  );

  /** Writes `content` over the file the diff showed as "before". */
  const revertTo = useCallback(
    async (path: string, content: string) => {
      const target = workspaceRef.current;
      const tab = stateRef.current.tabs.tabs.find((entry) => entry.path === path);
      if (!target || !tab) return;
      savingPathRef.current = path;
      try {
        const response = await getClient().request({
          type: "fs_write",
          host: target.host,
          workspace: target.path,
          path,
          content,
          base_etag: tab.etag,
        });
        if (response.type !== "fs_written" || response.result.conflict) return;
        await readInto(path);
      } finally {
        savingPathRef.current = null;
      }
    },
    [readInto],
  );

  const resolveConflictKeepMine = useCallback(async () => {
    const target = workspaceRef.current;
    const conflict = stateRef.current.conflict;
    if (!target || !conflict) return;
    savingPathRef.current = conflict.path;
    try {
      const response = await getClient().request({
        type: "fs_write",
        host: target.host,
        workspace: target.path,
        path: conflict.path,
        content: conflict.mine,
        base_etag: keepMineBaseEtag(conflict),
      });
      if (response.type !== "fs_written" || response.result.conflict) return;
      setState((current) => ({
        ...current,
        conflict: null,
        saveState: "saved",
        savedAt: Date.now(),
        tabs: updateTab(current.tabs, conflict.path, {
          savedContent: conflict.mine,
          etag: response.result.etag,
        }),
      }));
    } catch (error) {
      setState((current) => ({ ...current, conflict: null, saveState: "idle" }));
      throw error;
    } finally {
      savingPathRef.current = null;
    }
  }, [setState]);

  const resolveConflictTakeTheirs = useCallback(() => {
    const conflict = stateRef.current.conflict;
    if (!conflict) return;
    if (conflict.deleted) {
      // Nothing on disk to take: keep the tab open and flag it as deleted.
      setState((current) => ({
        ...current,
        conflict: null,
        saveState: "idle",
        external: { ...current.external, [conflict.path]: "deleted" },
      }));
      return;
    }
    setState((current) => ({
      ...current,
      conflict: null,
      saveState: "idle",
      external: omit(current.external, conflict.path),
      tabs: updateTab(current.tabs, conflict.path, {
        content: conflict.theirs,
        savedContent: conflict.theirs,
        etag: conflict.currentEtag ?? null,
      }),
    }));
  }, [setState]);

  const requestClose = useCallback(
    (path: string) => {
      const tab = stateRef.current.tabs.tabs.find((entry) => entry.path === path);
      if (!tab) return { dirty: false as const };
      if (!isDirty(tab)) {
        setState((current) => ({ ...current, tabs: closeTab(current.tabs, path) }));
        return { dirty: false as const };
      }
      return { dirty: true as const, tab };
    },
    [setState],
  );

  const confirmClose = useCallback(
    (path: string) => {
      setState((current) => ({ ...current, tabs: closeTab(current.tabs, path) }));
    },
    [setState],
  );

  const moveTab = useCallback(
    (path: string, index: number) => {
      setState((current) => ({ ...current, tabs: reorderTabs(current.tabs, path, index) }));
    },
    [setState],
  );

  const toggleDirectory = useCallback(
    (path: string) => {
      setState((current) => {
        const open = current.expanded[path] === true;
        const expanded = { ...current.expanded };
        if (open) delete expanded[path];
        else expanded[path] = true;
        return { ...current, expanded };
      });
      if (!stateRef.current.listings[path]) void loadListing(path);
    },
    [loadListing, setState],
  );

  // -------------------------------------------------------------------
  // Diff and agent changes
  // -------------------------------------------------------------------

  const openDiff = useCallback(
    async (path: string, mode: "head" | "agent") => {
      const target = workspaceRef.current;
      if (!target) return;
      let against: DiffAgainst;
      if (mode === "head") {
        against = { kind: "head" };
      } else if (target.threadId && target.turnId) {
        against = {
          kind: "agent_turn",
          thread_id: target.threadId,
          turn_id: target.turnId,
        };
      } else {
        setState((current) => ({
          ...current,
          diffMode: mode,
          diffPath: path,
          diff: null,
          diffLoading: false,
          diffError: {
            code: "invalid_request",
            message: "This workspace has no agent turn to compare against yet.",
            hint: "Run a turn first, then open Agent changes.",
          },
        }));
        return;
      }
      setState((current) => ({
        ...current,
        diffMode: mode,
        diffPath: path,
        diffLoading: true,
        diffError: null,
      }));
      try {
        const response = await getClient().request({
          type: "fs_diff",
          host: target.host,
          workspace: target.path,
          path,
          against,
        });
        if (response.type !== "fs_diff") return;
        setState((current) => ({
          ...current,
          diff: response.diff,
          diffLoading: false,
          diffError: null,
        }));
      } catch (error) {
        setState((current) => ({
          ...current,
          diff: null,
          diffLoading: false,
          diffError: toProtoError(error),
        }));
      }
    },
    [setState],
  );

  // -------------------------------------------------------------------
  // Search
  // -------------------------------------------------------------------

  const runSearch = useCallback(
    async (input: {
      query: string;
      regex: boolean;
      caseSensitive: boolean;
      glob: string | null;
    }) => {
      const target = workspaceRef.current;
      if (!target) return;
      if (input.query.trim().length === 0) {
        setState((current) => ({
          ...current,
          search: { ...current.search, ...input, results: [] },
        }));
        return;
      }
      searchIdRef.current = null;
      setState((current) => ({
        ...current,
        search: {
          ...current.search,
          ...input,
          running: true,
          results: [],
          truncated: false,
          error: null,
          searchId: null,
        },
      }));
      try {
        const response = await getClient().request({
          type: "fs_search",
          host: target.host,
          workspace: target.path,
          query: input.query,
          regex: input.regex,
          case_sensitive: input.caseSensitive,
          glob: input.glob,
          max_results: 500,
        });
        if (response.type !== "fs_search_started") return;
        searchIdRef.current = response.search_id;
        setState((current) => ({
          ...current,
          search: { ...current.search, searchId: response.search_id },
        }));
      } catch (error) {
        searchIdRef.current = null;
        setState((current) => ({
          ...current,
          search: { ...current.search, running: false, error: toProtoError(error) },
        }));
      }
    },
    [setState],
  );

  const setSearchQuery = useCallback(
    (query: string) => {
      setState((current) => ({ ...current, search: { ...current.search, query } }));
    },
    [setState],
  );

  const setSearchOption = useCallback(
    (patchValue: Partial<Pick<SearchState, "regex" | "caseSensitive" | "glob">>) => {
      setState((current) => ({
        ...current,
        search: { ...current.search, ...patchValue },
      }));
    },
    [setState],
  );

  // -------------------------------------------------------------------
  // Workspace lifecycle
  // -------------------------------------------------------------------

  const workspacePath = workspace?.path ?? null;
  const workspaceHost = workspace?.host ?? null;
  const workspaceKey =
    workspacePath && workspaceHost ? projectKey(workspaceHost, workspacePath) : null;
  useEffect(() => {
    if (!workspacePath || !workspaceHost || !workspaceKey) return;
    autoOpenedRef.current = null;
    loadedKeyRef.current = workspaceKey;
    const cached = IDE_CACHE.get(workspaceKey);
    if (cached) {
      // A project keeps its tabs. Do not auto-open over them; just refresh the
      // root listing so new or deleted files still show up.
      applyState({
        ...cached,
        workspaceKey,
        restoredTabs: cached.tabs.tabs.length,
        treeLoading: cached.listings[""] === undefined,
        treeError: null,
      });
      if (cached.listings[""] === undefined) void loadListing("");
    } else {
      applyState({ ...initialState(), workspaceKey });
      void loadListing("");
    }
    void getClient()
      .request({
        type: "fs_watch",
        host: workspaceHost,
        workspace: workspacePath,
        paths: [],
      })
      .catch(() => undefined);
    return () => {
      void getClient()
        .request({ type: "fs_unwatch", workspace: workspacePath })
        .catch(() => undefined);
    };
  }, [workspaceKey, workspacePath, workspaceHost, loadListing]);

  useEffect(() => {
    if (!workspace?.threadId || !workspace.turnId) {
      setState((current) => ({
        ...current,
        agentChanges: null,
        agentLoading: false,
        agentError: null,
      }));
      return;
    }
    let cancelled = false;
    setState((current) => ({ ...current, agentLoading: true, agentError: null }));
    getClient()
      .request({
        type: "fs_turn_changes",
        host: workspace.host,
        workspace: workspace.path,
        thread_id: workspace.threadId,
        turn_id: workspace.turnId,
      })
      .then((response) => {
        if (cancelled || response.type !== "fs_turn_changes") return;
        setState((current) => ({
          ...current,
          agentChanges: response.changes,
          agentLoading: false,
        }));
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setState((current) => ({
          ...current,
          agentChanges: null,
          agentLoading: false,
          agentError: toProtoError(error),
        }));
      });
    return () => {
      cancelled = true;
    };
  }, [workspace?.path, workspace?.host, workspace?.threadId, workspace?.turnId, setState]);

  // Auto-open the first plausible code file the first time a project is
  // opened. A project with remembered tabs is left exactly as it was.
  // Candidate reads are probes: a miss does not leave an error tab behind.
  useEffect(() => {
    if (!workspace || !workspaceKey) return;
    const root = state.listings[""];
    if (!root || state.workspaceKey !== workspaceKey) return;
    if (state.restoredTabs > 0) return;
    if (autoOpenedRef.current === workspaceKey) return;
    autoOpenedRef.current = workspaceKey;
    const topLevel = new Set(root.entries.map((entry) => entry.name));
    const candidates = [
      ...PREFERRED_FILES.filter((path) => topLevel.has(path.split("/")[0] ?? "")),
      ...root.entries
        .filter((entry) => entry.kind === "file" && isCodePath(entry.path))
        .map((entry) => entry.path),
      ...root.entries.filter((entry) => entry.kind === "file").map((entry) => entry.path),
    ];
    const target = workspace;
    void (async () => {
      for (const path of new Set(candidates)) {
        try {
          const response = await getClient().request({
            type: "fs_read",
            host: target.host,
            workspace: target.path,
            path,
          });
          if (response.type !== "fs_file") continue;
          if (response.file.content === null && !response.file.binary && !response.file.truncated) {
            continue;
          }
          setState((current) => ({
            ...current,
            tabs: updateTab(openTab(current.tabs, path), path, {
              content: response.file.content ?? "",
              savedContent: response.file.content ?? "",
              etag: response.file.etag,
              encoding: response.file.encoding,
              loading: false,
              binary: response.file.binary,
              truncated: response.file.truncated,
              hint: response.file.hint,
              error: null,
            }),
          }));
          return;
        } catch {
          // Try the next candidate.
        }
      }
    })();
  }, [workspace, workspaceKey, state.listings, state.workspaceKey, state.restoredTabs, setState]);

  // Events: streamed search batches and external file changes.
  const watchedPath = workspace?.path ?? null;
  useEffect(() => {
    if (!watchedPath) return;
    const off = getClient().onEvent((envelope) => {
      const event = envelope.event;
      if (event.type === "fs_search_batch") {
        if (!searchIdRef.current || event.search_id !== searchIdRef.current) return;
        setState((current) => ({
          ...current,
          search: {
            ...current.search,
            results: [...current.search.results, ...event.results],
            running: !event.done,
            truncated: current.search.truncated || event.truncated,
          },
        }));
        return;
      }
      if (event.type !== "fs_changed") return;
      if (event.workspace !== watchedPath) return;
      const tab = stateRef.current.tabs.tabs.find((entry) => entry.path === event.path);
      if (!tab) return;
      if (savingPathRef.current === event.path) return;
      if (isDirty(tab)) {
        setState((current) => ({
          ...current,
          external: {
            ...current.external,
            [event.path]: event.change === "deleted" ? "deleted" : "modified",
          },
        }));
        return;
      }
      void readInto(event.path);
    });
    return off;
  }, [watchedPath, readInto, setState]);

  return {
    state,
    request,
    active: activeTab(state.tabs),
    loadListing,
    openFile,
    activateTab,
    setTabContent,
    saveTab,
    revertTo,
    resolveConflictKeepMine,
    resolveConflictTakeTheirs,
    requestClose,
    confirmClose,
    moveTab,
    toggleDirectory,
    openDiff,
    runSearch,
    setSearchQuery,
    setSearchOption,
    patch,
    reload: readInto,
  };
}

export type IdeSession = ReturnType<typeof useIdeSession>;

export function languageLabel(path: string | null | undefined): string {
  if (!path) return "Plain text";
  const extension = path.split(".").at(-1)?.toLowerCase();
  const labels: Record<string, string> = {
    ts: "TypeScript",
    tsx: "TypeScript React",
    js: "JavaScript",
    jsx: "JavaScript React",
    rs: "Rust",
    py: "Python",
    go: "Go",
    json: "JSON",
    md: "Markdown",
    css: "CSS",
    html: "HTML",
    yaml: "YAML",
    yml: "YAML",
    toml: "TOML",
  };
  return (extension && labels[extension]) || "Plain text";
}

export function gitBadge(status: GitFileStatus | null): { letter: string; tone: string } | null {
  if (!status) return null;
  switch (status) {
    case "modified":
      return { letter: "M", tone: "text-warn" };
    case "added":
      return { letter: "A", tone: "text-ok" };
    case "untracked":
      return { letter: "U", tone: "text-fg-3" };
    case "deleted":
      return { letter: "D", tone: "text-danger" };
    case "renamed":
      return { letter: "R", tone: "text-warn" };
    case "conflicted":
      return { letter: "!", tone: "text-danger" };
    case "type_changed":
      return { letter: "T", tone: "text-warn" };
  }
}

export function isDirtyTab(tab: IdeTab | null): boolean {
  return tab ? isDirty(tab) : false;
}
