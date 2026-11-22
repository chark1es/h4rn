/**
 * Publishes a prepared preview document to the shell's own origin.
 *
 * A `srcdoc` frame inherits the shell document's CSP — in the desktop and
 * mobile shells that is `script-src 'self'`, so page scripts never run. The
 * packaged shells instead serve the prepared HTML from the `h4rn-preview`
 * scheme, which carries its own CSP header. The browser build has no such
 * origin and keeps `srcdoc`.
 *
 * These functions are the whole contract with the shells: `preview_prepare`
 * stores the document and answers with a one-use URL, `preview_release` drops
 * it when the preview closes or is replaced.
 */

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

/** A prepared document plus the token the frame's bridge reports with. */
export type PreparedFrame = { url: string; token: string };

function shellInternals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const internals = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return internals && typeof internals.invoke === "function" ? internals : null;
}

/** True when the app runs inside a Tauri shell. */
export function hasPreviewOrigin(): boolean {
  return shellInternals() !== null;
}

/**
 * Stores one document in the shell and returns the URL to frame it with.
 *
 * Returns `null` in the browser build (or when the shell refuses), which makes
 * the caller fall back to `srcdoc`.
 */
export async function preparePreviewFrame(html: string): Promise<PreparedFrame | null> {
  const internals = shellInternals();
  if (!internals) return null;
  try {
    const prepared = await internals.invoke<PreparedFrame>("preview_prepare", { html });
    if (!prepared || typeof prepared.url !== "string" || typeof prepared.token !== "string") {
      return null;
    }
    return prepared;
  } catch {
    return null;
  }
}

/** Forgets a stored document. Safe to call with `null` or twice. */
export async function releasePreviewFrame(url: string | null): Promise<void> {
  const internals = shellInternals();
  if (!internals || !url) return;
  try {
    await internals.invoke("preview_release", { url });
  } catch {
    // A closed window already dropped the document with its process.
  }
}
