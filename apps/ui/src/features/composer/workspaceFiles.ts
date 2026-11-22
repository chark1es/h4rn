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

/** Walk one workspace, pruned and capped. A failed directory is skipped. */
export async function walkWorkspace(
  target: WorkspaceTarget,
  lister: Lister = list,
): Promise<string[]> {
  const files: string[] = [];
  const queue: Array<{ path: string; depth: number }> = [{ path: "", depth: 0 }];
  let directories = 0;

  while (queue.length > 0 && directories < MAX_DIRECTORIES && files.length < MAX_INDEXED_FILES) {
    const batch = queue.splice(0, CONCURRENCY);
    const listings = await Promise.allSettled(
      batch.map((directory) => lister({ ...target, path: directory.path })),
    );

    for (let index = 0; index < listings.length; index += 1) {
      const result = listings[index];
      if (result?.status !== "fulfilled") continue;
      directories += 1;
      const depth = batch[index]?.depth ?? 0;
      for (const entry of result.value.entries) {
        if (entry.kind === "directory") {
          if (depth + 1 > MAX_DEPTH) continue;
          if (IGNORED_DIRECTORIES.has(entry.name)) continue;
          queue.push({ path: entry.path, depth: depth + 1 });
          continue;
        }
        // Symlinks are left out: following them can leave the workspace or
        // loop, and the daemon reports them without resolving the target.
        if (entry.kind !== "file") continue;
        files.push(entry.path);
        if (files.length >= MAX_INDEXED_FILES) break;
      }
    }
  }

  return files;
}

type CacheEntry = {
  files: string[];
  at: number;
  pending: Promise<string[]> | null;
};

const cache = new Map<string, CacheEntry>();

function cacheKey(target: WorkspaceTarget): string {
  return `${target.host}\u0000${target.workspace}`;
}

/** Files already indexed for this workspace, for a flash-free first render. */
export function readWorkspaceFilesCache(target: WorkspaceTarget): string[] | null {
  const hit = cache.get(cacheKey(target));
  if (!hit || hit.pending) return hit?.files.length ? hit.files : null;
  return hit.files;
}

/** Index one workspace, reusing a fresh cache or an in-flight walk. */
export async function loadWorkspaceFiles(target: WorkspaceTarget): Promise<string[]> {
  const key = cacheKey(target);
  const hit = cache.get(key);
  if (hit?.pending) return hit.pending;
  if (hit && Date.now() - hit.at < CACHE_TTL_MS) return hit.files;

  const pending = walkWorkspace(target);
  cache.set(key, { files: hit?.files ?? [], at: hit?.at ?? 0, pending });
  try {
    const files = await pending;
    cache.set(key, { files, at: Date.now(), pending: null });
    return files;
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
  loading: boolean;
  error: string | null;
};

const EMPTY: WorkspaceFilesState = { files: [], loading: false, error: null };

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
    setState({ files: cached ?? [], loading: true, error: null });
    loadWorkspaceFiles(target)
      .then((files) => {
        if (cancelled) return;
        setState({ files, loading: false, error: null });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setState({
          files: [],
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
