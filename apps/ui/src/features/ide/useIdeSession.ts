import { useCallback, useEffect, useRef, useState } from "react";

import type {
  DiffAgainst,
  FsBranchList,
  FsDiff,
  FsDiffCommitList,
  FsDiffFiles,
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
import { retainWorkspaceWatch } from "@/lib/client/fsWatch";
import { getClient } from "@/lib/store/appStore";
import { conflictFromWrite, keepMineBaseEtag } from "./conflict";
import { againstForScope, chooseCommit, type DiffSource, firstDiffPath } from "./sources";
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
  /**
   * The file a Branch or Commits list read was told to open.
   *
   * Every list read records it here: entering a scope, the tab follow, a base
   * or commit change, and an explicit Refresh. `diffPath` only moves when a
   * reply lands, so while a list is out it can still name the previous file or
   * nothing at all. A review resumed after a workspace round-trip must reopen
   * the tab the load was started for, so the load records its preferred file
   * here and a direct open clears it.
   */
  diffListPath: string | null;
  diffSource: DiffSource;
  /** The branch scope's base; null asks the daemon for the repository default. */
  diffBase: string | null;
  /** The commit scope's commit. */
  diffCommit: string | null;
  diffLoading: boolean;
  diffError: ProtoError | null;
  diffFiles: FsDiffFiles | null;
  filesLoading: boolean;
  filesError: ProtoError | null;
  diffCommits: FsDiffCommitList | null;
  commitsLoading: boolean;
  commitsError: ProtoError | null;
  diffBranches: FsBranchList | null;
  branchesLoading: boolean;
  branchesError: ProtoError | null;
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
    diffListPath: null,
    diffSource: "head",
    diffBase: null,
    diffCommit: null,
    diffLoading: false,
    diffError: null,
    diffFiles: null,
    filesLoading: false,
    filesError: null,
    diffCommits: null,
    commitsLoading: false,
    commitsError: null,
    diffBranches: null,
    branchesLoading: false,
    branchesError: null,
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
  /** The newest diff request; replies for an older counter are dropped. */
  const diffRequestRef = useRef(0);
  /**
   * The newest base-branch list request. The list does not depend on the open
   * file, so it has its own counter: a slower file list or diff reply must not
   * throw it away, and only a newer branch read (or another workspace) may.
   */
  const branchesRequestRef = useRef(0);
  /**
   * The file the pending Branch or Commits list load will open after a
   * workspace round-trip, and the request generation it belongs to.
   *
   * Restoring a workspace resumes the list load the person left, and that load
   * already opens the tab's file. The pane's tab-follow effect runs on the same
   * render and would ask for the same list again; this names the file the
   * resumed load has taken, so the follow leaves it alone. A newer request
   * moves the generation, which retires the note.
   */
  const resumedListRef = useRef<{ token: number; path: string | null; source: DiffSource } | null>(
    null,
  );
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
          bytes_base64: false,
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

  /**
   * Starts a diff request and returns its token.
   *
   * Every diff loader bumps the counter before it awaits anything. A reply
   * whose counter has moved on is dropped, so a slow Branch list or an older
   * `fs_diff` can never replace a source or file the person picked afterwards.
   */
  const beginDiffRequest = useCallback(() => {
    diffRequestRef.current += 1;
    // The dropped reply's spinner must not outlive it. Clearing `diffLoading`
    // matters as much as the lists: a Branch or Commits load supersedes the
    // file diff that was still out, and the pane must show that load's own
    // state instead of an abandoned "Loading diff…". Every current request
    // sets its own flag again in the same tick, before its first await.
    //
    // The branch list has its own generation: a file diff opening does not
    // supersede it, so its spinner stays until that read answers (or fails).
    setState((current) =>
      current.diffLoading || current.filesLoading || current.commitsLoading
        ? {
            ...current,
            // A body that was still on its way must not stand in for the newer
            // choice: the abandoned request will never answer, so its half-
            // open diff (or the previous file's) is cleared with its spinner.
            ...(current.diffLoading ? { diff: null } : {}),
            diffLoading: false,
            filesLoading: false,
            commitsLoading: false,
          }
        : current,
    );
    return diffRequestRef.current;
  }, [setState]);

  const diffRequestCurrent = useCallback((token: number) => diffRequestRef.current === token, []);

  const openDiff = useCallback(
    async (
      path: string,
      source: DiffSource = stateRef.current.diffSource,
      overrides: { base?: string | null; commit?: string | null } = {},
    ) => {
      const token = beginDiffRequest();
      const target = workspaceRef.current;
      if (!target) return;
      const current = stateRef.current;
      const base = overrides.base !== undefined ? overrides.base : current.diffBase;
      const commit = overrides.commit !== undefined ? overrides.commit : current.diffCommit;
      const against: DiffAgainst | null = againstForScope({
        source,
        threadId: target.threadId,
        turnId: target.turnId,
        base,
        commit,
      });
      if (!against) {
        const missingTurn = source === "agent";
        setState((state) => ({
          ...state,
          diffSource: source,
          diffPath: path,
          diffListPath: null,
          diff: null,
          diffLoading: false,
          diffError: {
            code: "invalid_request",
            message: missingTurn
              ? "This workspace has no agent turn to compare against yet."
              : "Choose a commit to compare against first.",
            hint: missingTurn
              ? "Run a turn first, then open Turn changes."
              : "Pick a commit from the source picker.",
          },
        }));
        return;
      }
      setState((state) => ({
        ...state,
        diffSource: source,
        diffPath: path,
        diffListPath: null,
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
        if (!diffRequestCurrent(token)) return;
        if (response.type !== "fs_diff") return;
        setState((state) => ({
          ...state,
          diff: response.diff,
          diffLoading: false,
          diffError: null,
        }));
      } catch (error) {
        if (!diffRequestCurrent(token)) return;
        setState((state) => ({
          ...state,
          diff: null,
          diffLoading: false,
          diffError: toProtoError(error),
        }));
      }
    },
    [beginDiffRequest, diffRequestCurrent, setState],
  );

  const fetchDiffFiles = useCallback(
    async (against: DiffAgainst): Promise<FsDiffFiles | null> => {
      const target = workspaceRef.current;
      if (!target) return null;
      // The caller bumped the counter; this reply is only applied while it is
      // still the newest request.
      const token = diffRequestRef.current;
      setState((current) => ({ ...current, filesLoading: true, filesError: null }));
      try {
        const response = await getClient().request({
          type: "fs_diff_files",
          host: target.host,
          workspace: target.path,
          against,
        });
        if (!diffRequestCurrent(token)) return null;
        if (response.type !== "fs_diff_files") return null;
        const files = response.files;
        setState((current) => ({
          ...current,
          diffFiles: files,
          filesLoading: false,
          filesError: null,
          // One comparison has one name. The daemon resolves the default base
          // (and the default base) to this branch; the pane remembers the
          // resolved name so the picker, the next requests and the comment key
          // all name the same comparison however the base was chosen.
          ...(against.kind === "base" && files.base ? { diffBase: files.base } : {}),
        }));
        return files;
      } catch (error) {
        if (!diffRequestCurrent(token)) return null;
        setState((current) => ({
          ...current,
          diffFiles: null,
          filesLoading: false,
          filesError: toProtoError(error),
        }));
        return null;
      }
    },
    [diffRequestCurrent, setState],
  );

  const fetchDiffCommits = useCallback(
    async (base: string | null): Promise<FsDiffCommitList | null> => {
      const target = workspaceRef.current;
      if (!target) return null;
      const token = diffRequestRef.current;
      setState((current) => ({ ...current, commitsLoading: true, commitsError: null }));
      try {
        const response = await getClient().request({
          type: "fs_diff_commits",
          host: target.host,
          workspace: target.path,
          base,
        });
        if (!diffRequestCurrent(token)) return null;
        if (response.type !== "fs_diff_commits") return null;
        const commits = response.commits;
        setState((current) => ({
          ...current,
          diffCommits: commits,
          commitsLoading: false,
          commitsError: null,
        }));
        return commits;
      } catch (error) {
        if (!diffRequestCurrent(token)) return null;
        setState((current) => ({
          ...current,
          diffCommits: null,
          commitsLoading: false,
          commitsError: toProtoError(error),
        }));
        return null;
      }
    },
    [diffRequestCurrent, setState],
  );

  /**
   * Loads the branches the base picker offers.
   *
   * This reply is not tied to the open file or diff: a slower branch list is
   * still the answer to the picker's question after the file list and the diff
   * have opened. Only a newer branch read, or a different workspace, may
   * replace it.
   */
  const fetchBranches = useCallback(async (): Promise<FsBranchList | null> => {
    const target = workspaceRef.current;
    if (!target) return null;
    branchesRequestRef.current += 1;
    const token = branchesRequestRef.current;
    setState((current) => ({ ...current, branchesLoading: true, branchesError: null }));
    try {
      const response = await getClient().request({
        type: "fs_branches",
        host: target.host,
        workspace: target.path,
      });
      if (branchesRequestRef.current !== token) {
        // A newer read (or another workspace) owns the picker now. This reply
        // must not paint over it, and must not clear the newer spinner either.
        return null;
      }
      if (response.type !== "fs_branches") {
        setState((current) => ({ ...current, branchesLoading: false, branchesError: null }));
        return null;
      }
      const branches = response.branches;
      setState((current) => ({
        ...current,
        diffBranches: branches,
        branchesLoading: false,
        branchesError: null,
      }));
      return branches;
    } catch (error) {
      if (branchesRequestRef.current !== token) {
        // The newer read owns the spinner and the error.
        return null;
      }
      setState((current) => ({
        ...current,
        diffBranches: null,
        branchesLoading: false,
        branchesError: toProtoError(error),
      }));
      return null;
    }
  }, [setState]);

  const showDiffForFiles = useCallback(
    async (
      files: FsDiffFiles | null,
      source: DiffSource,
      preferred: string | null,
      overrides: { base?: string | null; commit?: string | null } = {},
    ) => {
      const path = firstDiffPath(files, preferred);
      if (!path) {
        setState((current) => ({
          ...current,
          diffSource: source,
          diffPath: null,
          diffListPath: null,
          diff: null,
          diffLoading: false,
          diffError: null,
        }));
        return;
      }
      await openDiff(path, source, overrides);
    },
    [openDiff, setState],
  );

  /** Loads the branch review: the branch list for its picker, then the files. */
  const loadBranch = useCallback(
    async (
      preferred: string | null,
      overrides: { base?: string | null; branches?: boolean } = {},
    ) => {
      const token = beginDiffRequest();
      // The load will open this file once its list lands. Remember it now: a
      // workspace round-trip before the reply resumes exactly this file, while
      // `diffPath` may still name the previous one.
      setState((state) => ({ ...state, diffListPath: preferred }));
      // A caller that restores another workspace's state passes its base: the
      // restored state has not rendered yet, so `stateRef` still holds the
      // previous workspace's choice.
      const base = overrides.base !== undefined ? overrides.base : stateRef.current.diffBase;
      // Refetch on entry: the agent may have fetched, committed or renamed a
      // branch since this pane last read the list. A tab follow keeps the list
      // it already has (or is reading) and passes `branches: false`.
      if (overrides.branches !== false) void fetchBranches();
      const files = await fetchDiffFiles({ kind: "base", base });
      if (!diffRequestCurrent(token)) return;
      // The list resolves the base (the default included); ask for that name so
      // the diff, the picker and the comment key all describe one comparison.
      await showDiffForFiles(files, "branch", preferred, { base: files?.base ?? base });
    },
    [
      beginDiffRequest,
      diffRequestCurrent,
      fetchBranches,
      fetchDiffFiles,
      setState,
      showDiffForFiles,
    ],
  );

  /** Loads the commits review: a fresh commit list, then one commit's files. */
  const loadCommit = useCallback(
    async (
      preferred: string | null,
      overrides: { base?: string | null; commit?: string | null } = {},
    ) => {
      const token = beginDiffRequest();
      const current = stateRef.current;
      // The load will open this file once its list lands; see `loadBranch`.
      setState((state) => ({ ...state, diffListPath: preferred }));
      // See `loadBranch`: restored values win over a `stateRef` that has not
      // caught up with the restore yet.
      const base = overrides.base !== undefined ? overrides.base : current.diffBase;
      const commit = overrides.commit !== undefined ? overrides.commit : current.diffCommit;
      const list = await fetchDiffCommits(base);
      if (!diffRequestCurrent(token)) return;
      const sha = chooseCommit(list, commit);
      if (!sha) {
        setState((state) => ({
          ...state,
          diffSource: "commit",
          diffCommit: null,
          diffPath: null,
          diffListPath: null,
          diff: null,
          diffLoading: false,
          diffError: state.commitsError,
        }));
        return;
      }
      setState((state) => ({ ...state, diffSource: "commit", diffCommit: sha }));
      const files = await fetchDiffFiles({ kind: "commit", sha });
      if (!diffRequestCurrent(token)) return;
      await showDiffForFiles(files, "commit", preferred, { commit: sha });
    },
    [
      beginDiffRequest,
      diffRequestCurrent,
      fetchDiffCommits,
      fetchDiffFiles,
      showDiffForFiles,
      setState,
    ],
  );

  // The workspace effect re-runs an interrupted diff load through these
  // without taking them as dependencies (that would re-run the whole restore).
  const resumeDiffRef = useRef({ openDiff, loadBranch, loadCommit });
  resumeDiffRef.current = { openDiff, loadBranch, loadCommit };

  /**
   * Moves the open scope to another file without dropping the scope's list.
   *
   * A tab switch in Branch or Commits must keep the base or commit the person
   * is reviewing: opening the tab as a plain diff would drop the in-flight
   * list, and a commit scope with no sha would answer with an error instead of
   * the list's own empty state. When the scope's list is not on screen yet, ask
   * for it again with the tab as its preferred file.
   */
  const followDiffTab = useCallback(
    async (path: string) => {
      const current = stateRef.current;
      const listing = current.filesLoading || current.commitsLoading;
      // A review resumed after a workspace round-trip is already loading this
      // tab's file: asking for the list again would read it twice. Only the
      // resumed request itself is skipped, so a later follow to another tab
      // still takes over.
      const resumed = resumedListRef.current;
      if (
        listing &&
        resumed?.token === diffRequestRef.current &&
        resumed.path === path &&
        resumed.source === current.diffSource
      ) {
        return;
      }
      // With no list read out, `diffPath` is the newest request's file (a
      // direct open names it at once), so a tab that matches it is already
      // shown or loading. While a list is out, `diffPath` still names the file
      // from before that load, and the load will open its own preferred file:
      // a tab switch must take over, even back to the file still on screen.
      if (!listing && current.diffPath === path) return;
      // The list on screen answers for the scope only when it was read for the
      // current choice and nothing newer is still loading: `setDiffBase`,
      // `selectDiffSource` and a refresh keep the previous scope's list up
      // while the new one loads.
      const against = current.diffFiles?.against;
      // A follow keeps the base picker's list; it only re-reads one that has
      // never arrived and is not on its way.
      const branches = !current.diffBranches && !current.branchesLoading;
      if (current.diffSource === "branch") {
        // The person's choice wins; the list's resolved base fills in only
        // when no base has been named yet.
        const base = current.diffBase ?? current.diffFiles?.base ?? null;
        const listed =
          against?.kind === "base" &&
          (against.base === base || current.diffFiles?.base === base) &&
          !listing;
        if (listed) await openDiff(path, "branch", { base });
        else await loadBranch(path, { base, branches });
        return;
      }
      if (current.diffSource === "commit") {
        const base = current.diffBase;
        const commit = current.diffCommit;
        // Only `loadCommit` names a commit and then reads its files, so with
        // no list out, a commit and a commit-scope file list on screen belong
        // together; while the commit list is out, both are from before it.
        const listed = commit !== null && against?.kind === "commit" && !listing;
        if (listed) {
          await openDiff(path, "commit", { base, commit });
        } else {
          await loadCommit(path, { base, commit });
        }
        return;
      }
      await openDiff(path, current.diffSource);
    },
    [loadBranch, loadCommit, openDiff],
  );

  /** Switches the pane's scope and loads whatever that scope needs. */
  const selectDiffSource = useCallback(
    async (source: DiffSource) => {
      // The picker shows the choice the moment it is made; a list or diff that
      // arrives later without being the newest request is dropped.
      beginDiffRequest();
      setState((state) => ({
        ...state,
        diffSource: source,
        diff: null,
        diffError: null,
      }));
      const current = stateRef.current;
      // Entering the pane shows the file the person is on. The remembered
      // `diffPath` belongs to the Branch/Commits file list, and a stale one
      // must never answer for another tab.
      const preferred = activeTab(current.tabs)?.path ?? current.diffPath;
      if (source === "head" || source === "agent") {
        if (preferred) {
          await openDiff(preferred, source);
        } else {
          setState((state) => ({
            ...state,
            diffSource: source,
            diffPath: null,
            diffListPath: null,
            diff: null,
            diffLoading: false,
            diffError: null,
          }));
        }
        return;
      }
      if (source === "branch") {
        await loadBranch(preferred);
        return;
      }
      await loadCommit(preferred);
    },
    [beginDiffRequest, loadBranch, loadCommit, openDiff, setState],
  );

  /** Changes the branch the review compares against. */
  const setDiffBase = useCallback(
    async (base: string | null) => {
      // A list that is still out will open the tab it was started for, so that
      // tab is this read's file too; `diffPath` may still name the one before.
      const preferred = stateRef.current.diffListPath ?? stateRef.current.diffPath;
      const token = beginDiffRequest();
      setState((current) => ({
        ...current,
        diffBase: base,
        diffCommit: null,
        diffCommits: null,
        // Record the file this load will open, so a workspace round-trip
        // reopens it and the tab follow leaves the resumed list alone.
        diffListPath: preferred,
        // The body on screen was compared against the old base; keep it off
        // the screen while the new comparison's list loads.
        diff: null,
        diffError: null,
      }));
      const files = await fetchDiffFiles({ kind: "base", base });
      if (!diffRequestCurrent(token)) return;
      await showDiffForFiles(files, "branch", preferred, { base: files?.base ?? base });
    },
    [beginDiffRequest, diffRequestCurrent, fetchDiffFiles, showDiffForFiles, setState],
  );

  /** Opens one commit's files. */
  const setDiffCommit = useCallback(
    async (sha: string) => {
      // See `setDiffBase`: a pending list's tab is this read's file too.
      const preferred = stateRef.current.diffListPath ?? stateRef.current.diffPath;
      const token = beginDiffRequest();
      setState((current) => ({
        ...current,
        diffCommit: sha,
        diffListPath: preferred,
        diff: null,
        diffError: null,
      }));
      const files = await fetchDiffFiles({ kind: "commit", sha });
      if (!diffRequestCurrent(token)) return;
      await showDiffForFiles(files, "commit", preferred, { commit: sha });
    },
    [beginDiffRequest, diffRequestCurrent, fetchDiffFiles, showDiffForFiles, setState],
  );

  /** Re-reads the branch or commit lists on an explicit refresh. */
  const refreshDiffLists = useCallback(async () => {
    beginDiffRequest();
    const current = stateRef.current;
    // A list still out will open the tab it was started for, so Refresh reads
    // that tab again; `diffPath` may still name the file from before it.
    const preferred = current.diffListPath ?? current.diffPath;
    if (current.diffSource === "branch") {
      await loadBranch(preferred);
    } else if (current.diffSource === "commit") {
      await loadCommit(preferred);
    }
  }, [beginDiffRequest, loadBranch, loadCommit]);

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
    // A reply addressed to the previous workspace must never paint this one:
    // bump both generations so every request still in flight is dropped.
    diffRequestRef.current += 1;
    branchesRequestRef.current += 1;
    resumedListRef.current = null;
    // Nothing is in flight for the state being restored, so a cached spinner
    // is the leftover of a read that has already been dropped.
    const idle: Partial<IdeState> = {
      diffLoading: false,
      diffError: null,
      filesLoading: false,
      filesError: null,
      commitsLoading: false,
      commitsError: null,
      branchesLoading: false,
      branchesError: null,
      diffListPath: null,
    };
    const cached = IDE_CACHE.get(workspaceKey);
    if (cached) {
      // A diff, file list or commit list that was still loading when the
      // workspace changed was dropped above. What the cache holds for it
      // belongs to the previous request (`openDiff` keeps the last body until
      // its reply lands), so never show it under the newer choice: clear it and
      // ask again for exactly what was being loaded.
      const interrupted = cached.diffLoading || cached.filesLoading || cached.commitsLoading;
      const stale: Partial<IdeState> = interrupted
        ? {
            diff: null,
            ...(cached.filesLoading ? { diffFiles: null } : {}),
            ...(cached.commitsLoading ? { diffCommits: null } : {}),
          }
        : {};
      // A project keeps its tabs. Do not auto-open over them; just refresh the
      // root listing so new or deleted files still show up.
      applyState({
        ...cached,
        ...idle,
        ...stale,
        workspaceKey,
        restoredTabs: cached.tabs.tabs.length,
        treeLoading: cached.listings[""] === undefined,
        treeError: null,
      });
      if (cached.listings[""] === undefined) void loadListing("");
      if (interrupted) {
        const resume = resumeDiffRef.current;
        // A list that was still out resumes the file it was started for. Its
        // reply never landed, so `diffPath` may still name the file from
        // before that read (or nothing); the load's own preferred file is what
        // it was about to open.
        const listPath = cached.diffListPath ?? cached.diffPath;
        // The restore has not rendered yet, so hand the cached choices over
        // explicitly instead of letting the loaders read `stateRef`.
        if (cached.diffSource === "branch") {
          void resume.loadBranch(listPath, { base: cached.diffBase });
          // The resumed load opens this file; the pane's follow effect must not
          // read the same list again for the tab it is going to show.
          resumedListRef.current = {
            token: diffRequestRef.current,
            path: listPath,
            source: "branch",
          };
        } else if (cached.diffSource === "commit") {
          void resume.loadCommit(listPath, {
            base: cached.diffBase,
            commit: cached.diffCommit,
          });
          resumedListRef.current = {
            token: diffRequestRef.current,
            path: listPath,
            source: "commit",
          };
        } else if (cached.diffPath) {
          void resume.openDiff(cached.diffPath, cached.diffSource, {
            base: cached.diffBase,
            commit: cached.diffCommit,
          });
        }
      }
    } else {
      applyState({ ...initialState(), ...idle, workspaceKey });
      void loadListing("");
    }
    const release = retainWorkspaceWatch(workspaceHost, workspacePath);
    return release;
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
            bytes_base64: false,
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
    followDiffTab,
    selectDiffSource,
    setDiffBase,
    setDiffCommit,
    refreshDiffLists,
    fetchBranches,
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
