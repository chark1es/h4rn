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
import { useEffect, useState } from "react";
import type { FsListing, HostId } from "@/gen";
import { getClient } from "@/lib/store/appStore";

export type WorkspaceTarget = { host: HostId; workspace: string };

type Lister = (input: WorkspaceTarget & { path: string }) => Promise<FsListing>;

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

let testLister: Lister | null = null;

/** Test seam: answer `fs_list` without a socket. */
export function setWorkspaceFileListerForTests(next: Lister | null): void {
  testLister = next;
}

async function list(input: WorkspaceTarget & { path: string }): Promise<FsListing> {
  if (testLister) return testLister(input);
  const response = await getClient().request({
    type: "fs_list",
    host: input.host,
    workspace: input.workspace,
    path: input.path,
  });
  if (response.type !== "fs_listing") {
    throw new Error("The daemon answered a file listing with something else.");
  }
  return response.listing;
}

/** Walk one workspace, pruned and capped. A failed subdirectory leaves the index incomplete. */
export type WorkspaceIndex = {
  files: string[];
  /** True when a listing failed or a cap cut the walk short: the list is a prefix. */
  truncated: boolean;
};

export async function walkWorkspace(
  target: WorkspaceTarget,
  lister: Lister = list,
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

    let full = false;
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
            : new Error("The workspace could not be read.");
        }
        truncated = true;
        continue;
      }
      const depth = directory?.depth ?? 0;
      if (result.value.truncated) truncated = true;
      for (const entry of result.value.entries) {
        if (entry.kind === "directory") {
          if (depth + 1 > MAX_DEPTH) {
            truncated = true;
            continue;
          }
          if (IGNORED_DIRECTORIES.has(entry.name)) continue;
          queue.push({ path: entry.path, depth: depth + 1 });
          continue;
        }
        // Symlinks are left out: following them can leave the workspace or
        // loop, and the daemon reports them without resolving the target.
        if (entry.kind !== "file") continue;
        if (files.length >= MAX_INDEXED_FILES) {
          full = true;
          break;
        }
        files.push(entry.path);
      }
      // The rest of the batch answered, but its entries no longer fit: stop
      // adding to the index instead of letting each sibling add one more.
      if (files.length >= MAX_INDEXED_FILES) {
        full = true;
        break;
      }
    }
    if (full) {
      truncated = true;
      break;
    }
  }

  // Leftover directories mean a cap stopped the walk: the list is a prefix.
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
export async function loadWorkspaceFiles(target: WorkspaceTarget): Promise<WorkspaceIndex> {
  const key = cacheKey(target);
  const hit = cache.get(key);
  if (hit?.pending) return hit.pending;
  if (hit && Date.now() - hit.at < CACHE_TTL_MS) return hit.index;

  const pending = walkWorkspace(target);
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
  const [state, setState] = useState<WorkspaceFilesState>(EMPTY);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const cached = readWorkspaceFilesCache(target);
    setState({
      files: cached?.files ?? [],
      truncated: cached?.truncated ?? false,
      loading: true,
      error: null,
    });
    loadWorkspaceFiles(target)
      .then((index) => {
        if (cancelled) return;
        setState({
          files: index.files,
          truncated: index.truncated,
          loading: false,
          error: null,
        });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setState({
          files: [],
          truncated: false,
          loading: false,
          error: error instanceof Error ? error.message : "The workspace could not be read.",
        });
      });
    return () => {
      cancelled = true;
    };
  }, [target, enabled]);

  return state;
}
