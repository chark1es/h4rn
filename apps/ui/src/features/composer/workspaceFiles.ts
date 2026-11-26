/**
 * The `@` picker's file index.
 *
 * The daemon has no filename-search request, so the index is a bounded,
 * breadth-first walk of `fs_list` from the workspace root. The daemon has
 * already applied `.gitignore` filtering to every listing; the walk additionally
 * skips dependency, build and metadata directories so a picker never turns into
 * a full repository scan. Results are cached per host and workspace for a
 * minute, and concurrent opens share one walk.
 */
import { createContext, useContext, useEffect, useState } from "react";
import type { FsListing, HostId } from "@/gen";
import { i18n } from "@/lib/i18n";
import { getClient } from "@/lib/store/appStore";

export type WorkspaceTarget = { host: HostId; workspace: string };

export type WorkspaceFileLister = (input: WorkspaceTarget & { path: string }) => Promise<FsListing>;

type Lister = WorkspaceFileLister;

/** Supply the listings the `@` picker reads; the app uses the daemon. */
export const WorkspaceFilesContext = createContext<WorkspaceFileLister | null>(null);

/** Directories a file picker should never descend into. */
const IGNORED_DIRECTORIES = new Set([
  ".git",
  ".hg",
  ".svn",
  ".next",
  ".nuxt",
  ".output",
  ".turbo",
  ".cache",
  ".venv",
  "venv",
  "__pycache__",
  ".idea",
  ".gradle",
  "build",
  "coverage",
  "dist",
  "node_modules",
  "target",
  "vendor",
]);

export const MAX_INDEXED_FILES = 3_000;
const MAX_DIRECTORIES = 250;
const MAX_DEPTH = 8;
const CONCURRENCY = 6;
const CACHE_TTL_MS = 60_000;

async function listFromDaemon(input: WorkspaceTarget & { path: string }): Promise<FsListing> {
  const response = await getClient().request({
    type: "fs_list",
    host: input.host,
    workspace: input.workspace,
    path: input.path,
  });
  if (response.type !== "fs_listing") {
    throw new Error(i18n.t("composer:menu.errors.unexpected"));
  }
  return response.listing;
}

/** Walk one workspace, pruned and capped. A failed subdirectory leaves the index incomplete. */
export type WorkspaceIndex = {
  files: string[];
  /** True when an entry or directory was actually skipped: the list is a prefix. */
  truncated: boolean;
};

export async function walkWorkspace(
  target: WorkspaceTarget,
  lister: Lister = listFromDaemon,
): Promise<WorkspaceIndex> {
  const files: string[] = [];
  const queue: Array<{ path: string; depth: number }> = [{ path: "", depth: 0 }];
  // Every listing the walk attempts spends the directory budget, whether it
  // answers or fails: a workspace full of unreadable folders must not turn
  // into an unbounded number of requests.
  let attempted = 0;
  let truncated = false;

  while (queue.length > 0 && attempted < MAX_DIRECTORIES && files.length < MAX_INDEXED_FILES) {
    // Never start more listings than the budget still allows.
    const remaining = MAX_DIRECTORIES - attempted;
    const batch = queue.splice(0, Math.min(CONCURRENCY, remaining));
    attempted += batch.length;
    const listings = await Promise.allSettled(
      batch.map((directory) => lister({ ...target, path: directory.path })),
    );

    for (let index = 0; index < listings.length; index += 1) {
      const result = listings[index];
      const directory = batch[index];
      if (result?.status !== "fulfilled") {
        // A directory that could not be read is not an empty directory: the
        // index is incomplete, and with no root there is no index at all, so
        // the picker can show its error instead of claiming the workspace is
        // empty.
        if (directory?.path === "") {
          throw result?.reason instanceof Error
            ? result.reason
            : new Error(i18n.t("composer:menu.errors.unreadable"));
        }
        truncated = true;
        continue;
      }
      const depth = directory?.depth ?? 0;
      if (result.value.truncated) truncated = true;
      for (const entry of result.value.entries) {
        if (entry.kind === "directory") {
          // Check the ignore list first: a `node_modules` that happens to sit
          // at the depth cap was never going to be read, so it is not an
          // omitted part of the index and must not mark the walk truncated.
          if (IGNORED_DIRECTORIES.has(entry.name)) continue;
          if (depth + 1 > MAX_DEPTH) {
            truncated = true;
            continue;
          }
          queue.push({ path: entry.path, depth: depth + 1 });
          continue;
        }
        // Symlinks are left out: following them can leave the workspace or
        // loop, and the daemon reports them without resolving the target.
        if (entry.kind !== "file") continue;
        if (files.length >= MAX_INDEXED_FILES) {
          // A file the index cannot hold is exactly what the note is for.
          // Finishing the batch without it (and without touching the rest)
          // keeps a workspace of exactly the cap complete.
          truncated = true;
          continue;
        }
        files.push(entry.path);
      }
    }
  }

  // Directories still queued were never read, so the index is a prefix. A walk
  // that ends with an empty queue and exactly the cap in hand dropped nothing.
  if (queue.length > 0) truncated = true;

  return { files, truncated };
}

type CacheEntry = {
  index: WorkspaceIndex;
  at: number;
  pending: Promise<WorkspaceIndex> | null;
};

const cache = new Map<string, CacheEntry>();

function cacheKey(target: WorkspaceTarget): string {
  return `${target.host}\u0000${target.workspace}`;
}

/** Files already indexed for this workspace, for a flash-free first render. */
export function readWorkspaceFilesCache(target: WorkspaceTarget): WorkspaceIndex | null {
  const hit = cache.get(cacheKey(target));
  if (!hit) return null;
  if (hit.pending) return hit.index.files.length ? hit.index : null;
  return hit.index;
}

/** Index one workspace, reusing a fresh cache or an in-flight walk. */
export async function loadWorkspaceFiles(
  target: WorkspaceTarget,
  lister: Lister = listFromDaemon,
): Promise<WorkspaceIndex> {
  const key = cacheKey(target);
  const hit = cache.get(key);
  if (hit?.pending) return hit.pending;
  if (hit && Date.now() - hit.at < CACHE_TTL_MS) return hit.index;

  const pending = walkWorkspace(target, lister);
  cache.set(key, {
    index: hit?.index ?? { files: [], truncated: false },
    at: hit?.at ?? 0,
    pending,
  });
  try {
    const index = await pending;
    cache.set(key, { index, at: Date.now(), pending: null });
    return index;
  } catch (error) {
    cache.delete(key);
    throw error;
  }
}

/** Clears every indexed workspace; tests use this to start clean. */
export function clearWorkspaceFilesCache(): void {
  cache.clear();
}

export type WorkspaceFilesState = {
  files: string[];
  /** The index hit a cap, so a sparse result may just be outside it. */
  truncated: boolean;
  loading: boolean;
  error: string | null;
};

const EMPTY: WorkspaceFilesState = { files: [], truncated: false, loading: false, error: null };

/**
 * The index for the open picker. It loads only while the menu asks for it and
 * keeps the previous files on screen while a refresh is in flight.
 */
export function useWorkspaceFiles(target: WorkspaceTarget, enabled: boolean): WorkspaceFilesState {
  const injected = useContext(WorkspaceFilesContext);
  const { host, workspace } = target;
  const targetKey = `${host}\u0000${workspace}`;
  // The state remembers which workspace produced it. A render for a different
  // target shows an empty list before the effect can run, instead of one frame
  // of another workspace's files.
  const [state, setState] = useState<{ key: string; value: WorkspaceFilesState }>({
    key: targetKey,
    value: EMPTY,
  });

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const active = { host, workspace };
    const cached = readWorkspaceFilesCache(active);
    setState({
      key: targetKey,
      value: {
        files: cached?.files ?? [],
        truncated: cached?.truncated ?? false,
        loading: true,
        error: null,
      },
    });
    loadWorkspaceFiles(active, injected ?? listFromDaemon)
      .then((index) => {
        if (cancelled) return;
        setState({
          key: targetKey,
          value: {
            files: index.files,
            truncated: index.truncated,
            loading: false,
            error: null,
          },
        });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setState({
          key: targetKey,
          value: {
            files: [],
            truncated: false,
            loading: false,
            error:
              error instanceof Error ? error.message : i18n.t("composer:menu.errors.unreadable"),
          },
        });
      });
    return () => {
      cancelled = true;
    };
  }, [host, workspace, targetKey, enabled, injected]);

  // A render for another target has no index yet. While the picker is open it
  // must read as loading, not as an empty workspace, until the effect's walk
  // lands: otherwise the first frame says "No files in …" for a folder that
  // simply has not been read.
  if (state.key !== targetKey) return { ...EMPTY, loading: enabled };
  return state.value;
}
