/**
 * Opens an absolute web link outside the app.
 *
 * The desktop and phone shells hand the URL to the OS browser through the
 * opener plugin; a plain browser opens a new tab with `noopener`. The preview
 * frame never navigates itself: it has an opaque origin and no popup grant.
 */
export async function openExternalUrl(url: string): Promise<void> {
  if (!/^https?:\/\//i.test(url)) return;
  if (typeof window === "undefined") return;

  const internals = (
    window as unknown as {
      __TAURI_INTERNALS__?: {
        invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
      };
    }
  ).__TAURI_INTERNALS__;

  if (internals) {
    try {
      await internals.invoke("plugin:opener|open_url", { url });
      return;
    } catch {
      // Fall through to a tab; the shell may not expose the plugin.
    }
  }
  window.open(url, "_blank", "noopener,noreferrer");
}
