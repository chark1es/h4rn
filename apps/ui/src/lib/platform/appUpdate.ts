/**
 * The desktop shell's updater, as the view sees it.
 *
 * The update check, the six-hour timer, the download and the relaunch all live
 * in the Tauri shell (`apps/desktop/src-tauri/src/updates.rs`); this module is
 * only the typed doorway to it. Web and mobile have no shell updater, so
 * `appUpdateBridge()` returns `null` there and the notice never renders
 * (mobile updates arrive through the stores).
 *
 * A mock bridge exists for screenshots and tests: `?mock=1&appupdate=1`.
 */

import { MOCK_ALLOWED } from "@/lib/client/config";

import { secretShell } from "./secrets";

export type AvailableAppUpdate = { version: string; notes: string | null };

export type AppUpdateStatus = {
  current_version: string;
  available: AvailableAppUpdate | null;
  checked_at_ms: number | null;
  auto_check: boolean;
  error: string | null;
};

export type AppUpdateProgress =
  | { phase: "downloading"; downloaded: number; total: number | null }
  | { phase: "installing" }
  /**
   * The shell always sends its own sentence, shown as sent. `null` is the
   * client's own install failure, which the notice words in the language.
   */
  | { phase: "failed"; message: string | null };

export type AppUpdateBridge = {
  status(): Promise<AppUpdateStatus>;
  check(): Promise<AppUpdateStatus>;
  setAuto(enabled: boolean): Promise<AppUpdateStatus>;
  /** Resolves only when the install failed; success relaunches the app. */
  install(): Promise<void>;
  onChecked(listener: (status: AppUpdateStatus) => void): Promise<() => void>;
  onProgress(listener: (progress: AppUpdateProgress) => void): Promise<() => void>;
};

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  transformCallback<T>(callback: (payload: T) => void, once?: boolean): number;
};

const EVENT_CHECKED = "h4rn://update-checked";
const EVENT_PROGRESS = "h4rn://update-progress";

function internals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" &&
    typeof candidate.transformCallback === "function"
    ? candidate
    : null;
}

function desktopBridge(shell: TauriInternals): AppUpdateBridge {
  const listen = async <T>(event: string, listener: (payload: T) => void) => {
    const handler = shell.transformCallback<{ payload: T }>((message) => listener(message.payload));
    const eventId = await shell.invoke<number>("plugin:event|listen", {
      event,
      target: { kind: "Any" },
      handler,
    });
    return () => {
      void shell.invoke("plugin:event|unlisten", { event, eventId }).catch(() => undefined);
    };
  };
  return {
    status: () => shell.invoke<AppUpdateStatus>("app_update_status"),
    check: () => shell.invoke<AppUpdateStatus>("app_update_check"),
    setAuto: (enabled) => shell.invoke<AppUpdateStatus>("app_update_set_auto", { enabled }),
    install: () => shell.invoke<void>("app_update_install"),
    onChecked: (listener) => listen<AppUpdateStatus>(EVENT_CHECKED, listener),
    onProgress: (listener) => listen<AppUpdateProgress>(EVENT_PROGRESS, listener),
  };
}

/** A stand-in with one pending release, for screenshots and the mock daemon. */
function mockBridge(): AppUpdateBridge {
  let status: AppUpdateStatus = {
    current_version: "1.0.1",
    available: { version: "1.1.0", notes: null },
    checked_at_ms: Date.now() - 4 * 60_000,
    auto_check: true,
    error: null,
  };
  const checked = new Set<(value: AppUpdateStatus) => void>();
  const progress = new Set<(value: AppUpdateProgress) => void>();
  return {
    status: async () => status,
    check: async () => {
      status = { ...status, checked_at_ms: Date.now() };
      for (const listener of checked) listener(status);
      return status;
    },
    setAuto: async (enabled) => {
      status = { ...status, auto_check: enabled };
      return status;
    },
    install: async () => {
      const total = 48_000_000;
      for (const fraction of [0.15, 0.4, 0.62]) {
        for (const listener of progress)
          listener({ phase: "downloading", downloaded: Math.round(total * fraction), total });
        await new Promise((resolve) => setTimeout(resolve, 600));
      }
      for (const listener of progress) listener({ phase: "installing" });
    },
    onChecked: async (listener) => {
      checked.add(listener);
      return () => checked.delete(listener);
    },
    onProgress: async (listener) => {
      progress.add(listener);
      return () => progress.delete(listener);
    },
  };
}

/** The shell updater, or `null` on web and mobile. */
export function appUpdateBridge(): AppUpdateBridge | null {
  if (typeof window === "undefined") return null;
  if (
    MOCK_ALLOWED &&
    new URLSearchParams(window.location.search).get("mock") === "1" &&
    new URLSearchParams(window.location.search).get("appupdate") === "1"
  ) {
    return mockBridge();
  }
  if (secretShell() !== "desktop") return null;
  const shell = internals();
  return shell ? desktopBridge(shell) : null;
}

/** Which shell this page runs in, for the "how updates arrive" note. */
export function updateShell(): "desktop" | "mobile" | "web" {
  return secretShell();
}
