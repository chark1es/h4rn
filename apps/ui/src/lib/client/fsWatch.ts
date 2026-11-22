import { getClient } from "@/lib/store/appStore";

/**
 * Reference-counted workspace watches.
 *
 * The daemon keeps one `fs_watch` per workspace per connection, so two views
 * cannot each own one: when the IDE unmounted it sent `fs_unwatch` and the
 * preview silently stopped reloading. Everyone now retains the watch, and the
 * last release tells the daemon to stop.
 */

const counts = new Map<string, number>();

function keyOf(host: string, workspace: string): string {
  return `${host}\u0000${workspace}`;
}

/**
 * Keeps the workspace watched until the returned function runs.
 *
 * The first caller sends `fs_watch`; the last release sends `fs_unwatch`.
 * Releasing twice is safe. The watch ends with the connection either way.
 */
export function retainWorkspaceWatch(host: string, workspace: string): () => void {
  const key = keyOf(host, workspace);
  const count = counts.get(key) ?? 0;
  counts.set(key, count + 1);
  if (count === 0) {
    void getClient()
      .request({ type: "fs_watch", host, workspace, paths: [] })
      .catch(() => undefined);
  }

  let released = false;
  return () => {
    if (released) return;
    released = true;
    const next = (counts.get(key) ?? 1) - 1;
    if (next > 0) {
      counts.set(key, next);
      return;
    }
    counts.delete(key);
    void getClient()
      .request({ type: "fs_unwatch", workspace })
      .catch(() => undefined);
  };
}

/** The host/workspace pairs currently watched, for tests and diagnostics. */
export function watchedWorkspaces(): string[] {
  return [...counts.keys()];
}
