/**
 * The Tauri webview's IPC surface, as the view sees it.
 *
 * The view never imports `@tauri-apps/api`; the bundles are built from
 * `apps/ui` and served in a browser too. This module is the one place that
 * knows how to reach the shell: plain `invoke`, plus the event listener
 * handshake Tauri's core plugin expects.
 */

export type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  transformCallback<T>(callback: (payload: T) => void, once?: boolean): number;
};

export function tauriInternals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" &&
    typeof candidate.transformCallback === "function"
    ? candidate
    : null;
}

/**
 * Subscribes to a shell event; resolves to an unsubscribe function.
 *
 * Tauri delivers `{ event, payload }` to the callback, so listeners receive
 * the payload alone.
 */
export async function listenOnShell<T>(
  event: string,
  listener: (payload: T) => void,
): Promise<() => void> {
  const shell = tauriInternals();
  if (!shell) return () => undefined;
  const handler = shell.transformCallback<{ payload: T }>((message) => listener(message.payload));
  const eventId = await shell.invoke<number>("plugin:event|listen", {
    event,
    target: { kind: "Any" },
    handler,
  });
  return () => {
    void shell.invoke("plugin:event|unlisten", { event, eventId }).catch(() => undefined);
  };
}
