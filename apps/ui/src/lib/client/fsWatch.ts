import { getClient, onClientChange } from "@/lib/store/appStore";
import type { H4rnClient } from "./client";

/**
 * Reference-counted workspace watches.
 *
 * The daemon keeps one `fs_watch` per workspace per connection, so two views
 * cannot each own one: when the IDE unmounted it sent `fs_unwatch` and the
 * preview silently stopped reloading. Everyone now retains the watch, and the
 * last release tells the daemon to stop.
 *
 * A watch lives on the connection, and signing in, pairing, switching hosts or
 * recovering from offline rebuilds the client. The counts and owners are
 * therefore kept here, independent of any client instance, and every held
 * watch is re-sent on the connection that replaces the old one. Live reload
 * keeps working without a manual reload.
 */

type WatchClient = Pick<H4rnClient, "request" | "onReady">;

type Watch = { host: string; workspace: string };

const counts = new Map<string, number>();
const owners = new Map<string, Watch>();

/** The client that currently carries the watches. */
let bound: WatchClient | null = null;
let unbind: (() => void) | null = null;
/**
 * Workspaces already sent to the bound client.
 *
 * A boolean is not enough: retaining a second workspace before a replacement
 * client's first ready notice would mark that client as "sent" and leave the
 * watch retained earlier unsent forever. Each workspace is tracked on its
 * own, so the ready notice sends exactly the ones still outstanding.
 */
let sentOnThisClient = new Set<string>();
let listeningForReplacements = false;

function keyOf(host: string, workspace: string): string {
  return `${host}\u0000${workspace}`;
}

function sendWatch(client: WatchClient, watch: Watch): void {
  sentOnThisClient.add(keyOf(watch.host, watch.workspace));
  void client
    .request({ type: "fs_watch", host: watch.host, workspace: watch.workspace, paths: [] })
    .catch(() => undefined);
}

function resendHeldWatches(client: WatchClient): void {
  for (const [key, watch] of owners) {
    if (sentOnThisClient.has(key)) continue;
    sendWatch(client, watch);
  }
}

/**
 * Points the module at the client that must carry the watches now.
 *
 * A replacement client has never seen the held watches, so its first ready
 * notice re-sends every one of them even though the transport did not report
 * a reconnect. On the same client only a reconnect drops the daemon-side
 * watch, which clears what that client has been sent.
 */
function bindClient(client: WatchClient): void {
  if (bound === client) return;
  unbind?.();
  bound = client;
  sentOnThisClient = new Set();
  unbind = client.onReady((info) => {
    if (client !== bound) return;
    if (info.reconnected) sentOnThisClient.clear();
    resendHeldWatches(client);
  });
}

/** Re-binds to a replacement client as soon as the store creates one. */
function listenForReplacement(): void {
  if (listeningForReplacements) return;
  listeningForReplacements = true;
  onClientChange((client) => bindClient(client as WatchClient));
}

/**
 * Keeps the workspace watched until the returned function runs.
 *
 * The first caller sends `fs_watch`; the last release sends `fs_unwatch`.
 * Releasing twice is safe. A reconnect re-sends the watch on the same client,
 * and a replacement client re-sends every held watch on its first ready.
 */
export function retainWorkspaceWatch(host: string, workspace: string): () => void {
  const key = keyOf(host, workspace);
  const client = getClient() as WatchClient;
  listenForReplacement();
  bindClient(client);
  const count = counts.get(key) ?? 0;
  counts.set(key, count + 1);
  owners.set(key, { host, workspace });
  if (count === 0) {
    sendWatch(client, { host, workspace });
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
    owners.delete(key);
    sentOnThisClient.delete(key);
    const carrier = bound ?? client;
    void carrier.request({ type: "fs_unwatch", workspace }).catch(() => undefined);
  };
}

/** The host/workspace pairs currently watched, for tests and diagnostics. */
export function watchedWorkspaces(): string[] {
  return [...counts.keys()];
}
