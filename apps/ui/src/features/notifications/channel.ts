/**
 * The OS notification channel under the view.
 *
 * Three shells, three honest answers:
 *
 * - desktop: `tauri-plugin-notification`, whose `notify` command shows the
 *   notification and whose Rust `on_action` handler forwards a click back as
 *   `h4rn://notification-click`;
 * - browser: the Web Notification API, with `onclick` focusing the tab and
 *   opening the thread;
 * - mobile: not wired yet, so there is no channel and Settings does not offer
 *   switches that would do nothing.
 */

import type { ThreadId } from "@/gen";
import { secretShell } from "@/lib/platform/secrets";
import { listenOnShell, tauriInternals } from "@/lib/platform/tauri";

export type NotificationPermissionState = "granted" | "denied" | "default" | "unsupported";

export type NotificationMessage = {
  /** A stable positive id for the desktop plugin. */
  id: number;
  /** Coalescing key; also the browser tag. */
  key: string;
  title: string;
  body: string;
  threadId: ThreadId | null;
};

export type NotificationChannel = {
  readonly kind: "browser" | "desktop";
  permission(): Promise<NotificationPermissionState>;
  requestPermission(): Promise<NotificationPermissionState>;
  show(message: NotificationMessage): void;
  onClick(listener: (threadId: ThreadId | null) => void): () => void;
};

/** A deterministic 31-bit id, so a repeat for the same key replaces the old one. */
export function notificationId(key: string): number {
  let hash = 2166136261;
  for (let index = 0; index < key.length; index += 1) {
    hash ^= key.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 1) & 0x7fffffff;
}

function normalizePermission(value: unknown): NotificationPermissionState {
  if (value === "granted") return "granted";
  if (value === "denied") return "denied";
  if (
    value === "default" ||
    value === "prompt" ||
    value === "prompt-with-rationale" ||
    value === "promptWithRationale"
  ) {
    return "default";
  }
  return value === null ? "default" : "denied";
}

function browserChannel(): NotificationChannel | null {
  if (typeof window === "undefined" || !("Notification" in window)) return null;
  const listeners = new Set<(threadId: ThreadId | null) => void>();
  const shown = new Map<string, Notification>();

  const emit = (threadId: ThreadId | null) => {
    for (const listener of listeners) listener(threadId);
  };

  return {
    kind: "browser",
    permission: async () => normalizePermission(Notification.permission),
    requestPermission: async () => normalizePermission(await Notification.requestPermission()),
    show: (message) => {
      try {
        shown.get(message.key)?.close();
        const notification = new Notification(message.title, {
          body: message.body,
          tag: message.key,
        });
        shown.set(message.key, notification);
        notification.onclick = () => {
          notification.close();
          shown.delete(message.key);
          window.focus();
          emit(message.threadId);
        };
        notification.onclose = () => {
          if (shown.get(message.key) === notification) shown.delete(message.key);
        };
      } catch {
        // Some browsers throw when permission was revoked between the check
        // and the constructor; the next flush asks again.
      }
    },
    onClick: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

type ShellAction = {
  actionId?: string;
  notification?: { extra?: Record<string, unknown> | null } | null;
};

function desktopChannel(): NotificationChannel | null {
  const shell = tauriInternals();
  if (!shell) return null;
  const listeners = new Set<(threadId: ThreadId | null) => void>();
  let listening: Promise<() => void> | null = null;

  const ensureListener = (): void => {
    listening ??= listenOnShell<ShellAction>("h4rn://notification-click", (payload) => {
      // A click is `tap`; a dismiss is not a request to open anything.
      if (payload.actionId && payload.actionId !== "tap") return;
      const extra = payload.notification?.extra ?? null;
      const threadId =
        typeof extra?.threadId === "string" && extra.threadId.length > 0 ? extra.threadId : null;
      for (const listener of listeners) listener(threadId);
    }).catch(() => () => undefined);
  };

  return {
    kind: "desktop",
    permission: async () => {
      const granted = await shell.invoke<boolean | null>(
        "plugin:notification|is_permission_granted",
      );
      return granted === null ? "default" : granted ? "granted" : "denied";
    },
    requestPermission: async () => {
      const state = await shell.invoke<unknown>("plugin:notification|request_permission");
      return normalizePermission(state);
    },
    show: (message) => {
      ensureListener();
      void shell
        .invoke("plugin:notification|notify", {
          options: {
            id: message.id,
            title: message.title,
            body: message.body,
            extra: { threadId: message.threadId },
          },
        })
        .catch(() => undefined);
    },
    onClick: (listener) => {
      ensureListener();
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
        if (listeners.size === 0) {
          void listening?.then((unlisten) => {
            unlisten();
            listening = null;
          });
        }
      };
    },
  };
}

/** The channel for this shell, or `null` when notifications are unavailable. */
export function notificationChannel(): NotificationChannel | null {
  if (typeof window === "undefined") return null;
  const shell = secretShell();
  if (shell === "desktop") return desktopChannel();
  if (shell === "mobile") return null;
  return browserChannel();
}
