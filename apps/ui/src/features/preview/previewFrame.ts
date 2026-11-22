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
 * it when the preview closes or is replaced, and `preview_capabilities`
 * reports what the WebView can do before the page is trusted with scripts.
 */

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

/** A prepared document's one-use URL. The bridge token lives in the document. */
export type PreparedFrame = { url: string };

/** Where the preview is running, as the shell reports it. */
export type PreviewPlatform = "android" | "ios" | "desktop" | "web" | "unknown";

/** What the shell's WebView can do with the prepared document. */
export type PreviewCapabilities = {
  platform: PreviewPlatform;
  /**
   * Whether the WebView runs the shell's initialization scripts at document
   * start. `"unknown"` means the shell could not tell — its capability command
   * failed, or the WebView version it reports could not be parsed. The panel
   * treats that as unsupported: it cannot confirm the page's scripts will run,
   * so it shows the generic note with Reload instead of framing the page.
   *
   * Android WebViews without that feature make Wry inject the scripts into the
   * HTML and add their hashes to `script-src`, which makes browsers ignore
   * `'unsafe-inline'`: the page's own scripts, and its link bridge, never run.
   */
  documentStartScripts: boolean | "unknown";
};

const UNKNOWN_CAPABILITIES: PreviewCapabilities = {
  platform: "unknown",
  documentStartScripts: "unknown",
};

/**
 * Reads a shell's capability answer, reporting anything unreadable as unknown.
 *
 * A shell that reports no field, an unexpected platform or a null version
 * (an error or an unparseable WebView version) must not default to "supported":
 * the panel would then frame a page whose scripts may never run.
 */
export function normalizePreviewCapabilities(reported: unknown): PreviewCapabilities {
  if (!reported || typeof reported !== "object") return UNKNOWN_CAPABILITIES;
  const record = reported as { platform?: unknown; documentStartScripts?: unknown };
  const platform: PreviewPlatform =
    record.platform === "android" ||
    record.platform === "ios" ||
    record.platform === "desktop" ||
    record.platform === "web"
      ? record.platform
      : "unknown";
  const documentStartScripts =
    typeof record.documentStartScripts === "boolean" ? record.documentStartScripts : "unknown";
  return { platform, documentStartScripts };
}

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
    if (!prepared || typeof prepared.url !== "string") return null;
    return { url: prepared.url };
  } catch {
    return null;
  }
}

/**
 * Asks the shell what the preview's WebView can do.
 *
 * The browser has no shell and therefore no capability report; the caller
 * treats `null` as "not a packaged shell" and falls back to the ready message
 * alone. A shell that is there but fails, or answers something unreadable, is
 * `"unknown"` — never "supported".
 */
export async function previewCapabilities(): Promise<PreviewCapabilities | null> {
  const internals = shellInternals();
  if (!internals) return null;
  try {
    const reported = await internals.invoke<unknown>("preview_capabilities");
    return normalizePreviewCapabilities(reported);
  } catch {
    return UNKNOWN_CAPABILITIES;
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
