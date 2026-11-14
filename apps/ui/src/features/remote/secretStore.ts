/**
 * Platform secret storage for h4rn-connect credentials.
 *
 * Session tokens must never sit in `localStorage`: an origin-wide script or a
 * stolen profile would hand over a refresh token. Desktop uses the OS keychain
 * through Tauri, mobile uses the Keystore/Keychain command, and the web build
 * keeps them in `sessionStorage` (cleared when the tab closes) with an
 * in-memory fallback for locked-down webviews.
 */

const SERVICE = "h4rn-connect";
const PREFIX = "h4rn.remote.";

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

type SecretResponse = { value: string | null } | null;

function internals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" ? candidate : null;
}

function isMobileShell(): boolean {
  if (typeof window === "undefined") return false;
  return internals() !== null && /Android|iPhone|iPad|iPod/i.test(window.navigator.userAgent);
}

function webStorage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.sessionStorage;
  } catch {
    return null;
  }
}

const memory = new Map<string, string>();

function readWeb(key: string): string | null {
  const shared = webStorage();
  if (shared) {
    try {
      return shared.getItem(PREFIX + key);
    } catch {
      // Fall through to memory.
    }
  }
  return memory.get(key) ?? null;
}

function writeWeb(key: string, value: string): void {
  memory.set(key, value);
  try {
    webStorage()?.setItem(PREFIX + key, value);
  } catch {
    // Private mode can reject writes; the in-memory copy still serves this tab.
  }
}

function deleteWeb(key: string): void {
  memory.delete(key);
  try {
    webStorage()?.removeItem(PREFIX + key);
  } catch {
    // Ignore.
  }
}

/** Reads one secret, or `null` when it was never stored. */
export async function getSecret(key: string): Promise<string | null> {
  const shell = internals();
  if (shell) {
    try {
      if (isMobileShell()) {
        const response = await shell.invoke<SecretResponse>("secret_get", {
          key: PREFIX + key,
        });
        return response?.value ?? null;
      }
      return await shell.invoke<string | null>("keychain_get", {
        service: SERVICE,
        account: key,
      });
    } catch {
      // A keychain that cannot answer is treated as empty; sign-in can retry.
      return null;
    }
  }
  return readWeb(key);
}

/** Stores one secret. */
export async function setSecret(key: string, value: string): Promise<void> {
  const shell = internals();
  if (shell) {
    try {
      if (isMobileShell()) {
        await shell.invoke("secret_set", { key: PREFIX + key, value });
      } else {
        await shell.invoke("keychain_set", { service: SERVICE, account: key, secret: value });
      }
      return;
    } catch (error) {
      // A failing keychain must not make sign-in look successful.
      throw new Error(
        error instanceof Error ? error.message : "The platform secret store rejected the value.",
      );
    }
  }
  writeWeb(key, value);
}

/** Removes one secret; a missing entry is not an error. */
export async function deleteSecret(key: string): Promise<void> {
  const shell = internals();
  if (shell) {
    try {
      if (isMobileShell()) {
        await shell.invoke("secret_delete", { key: PREFIX + key });
      } else {
        await shell.invoke("keychain_delete", { service: SERVICE, account: key });
      }
      return;
    } catch {
      return;
    }
  }
  deleteWeb(key);
}
