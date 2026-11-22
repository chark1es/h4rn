import { getClient } from "@/lib/store/appStore";
import type { H4rnClient } from "./client";

/**
 * Reference-counted workspace watches.
 *
 * The daemon keeps one `fs_watch` per workspace per connection, so two views
 * cannot each own one: when the IDE unmounted it sent `fs_unwatch` and the
 * preview silently stopped reloading. Everyone now retains the watch, and the
 * last release tells the daemon to stop.
 *
 * A watch lives on the connection, so a reconnect drops it. The first retain
 * on a client subscribes to that client's ready notice and re-sends every
 * watch still held by views on the same client instance, which makes the
 * preview and the IDE come back without a manual reload.
 */

type WatchClient = Pick<H4rnClient, "request" | "onReady">;

type Watch = { host: string; workspace: string };

const counts = new Map<string, number>();
const owners = new Map<string, Watch & { client: WatchClient }>();
const listeners = new WeakMap<WatchClient, () => void>();

function keyOf(host: string, workspace: string): string {
  return `${host}\u0000${workspace}`;
}

function sendWatch(client: WatchClient, watch: Watch): void {
  void client
    .request({ type: "fs_watch", host: watch.host, workspace: watch.workspace, paths: [] })
    .catch(() => undefined);
}

/** Re-sends this client's held watches when it reconnects, never before. */
function listenForReconnect(client: WatchClient): void {
  if (listeners.has(client)) return;
  const off = client.onReady((info) => {
    if (!info.reconnected) return;
    for (const entry of owners.values()) {
      if (entry.client === client) sendWatch(client, entry);
    }
  });
  listeners.set(client, off);
}

/**
 * Keeps the workspace watched until the returned function runs.
 *
 * The first caller sends `fs_watch`; the last release sends `fs_unwatch`.
 * Releasing twice is safe. The watch ends with the connection either way, and
 * is restored by the reconnect listener.
 */
export function retainWorkspaceWatch(host: string, workspace: string): () => void {
  const key = keyOf(host, workspace);
  const client = getClient() as WatchClient;
  listenForReconnect(client);
  const count = counts.get(key) ?? 0;
  counts.set(key, count + 1);
  owners.set(key, { host, workspace, client });
  if (count === 0) sendWatch(client, { host, workspace });

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
    owners.delete(key);
    void client.request({ type: "fs_unwatch", workspace }).catch(() => undefined);
  };
}

/** The host/workspace pairs currently watched, for tests and diagnostics. */
export function watchedWorkspaces(): string[] {
  return [...counts.keys()];
}
