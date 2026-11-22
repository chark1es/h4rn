import {
  ArrowLeftIcon,
  EyeIcon,
  Maximize2Icon,
  Minimize2Icon,
  RefreshCwIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import type { ThreadSummary } from "@/gen";
import { openExternalUrl } from "@/lib/platform/openExternal";
import { closePreview, collapsePreview, expandPreview, setPanelTab } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { PREVIEW_SANDBOX, previewOpenRequest } from "./htmlPreview";
import { type PreviewCapabilities, previewCapabilities } from "./previewFrame";
import { useHtmlPreview } from "./useHtmlPreview";

function fileName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

/**
 * How long the panel waits for the bridge's ready message after the frame
 * loads.
 *
 * `load` already means parsing finished, so this is only a delivery grace.
 * It grows with the document because a large prepared page — a report with
 * inlined images can reach 10 MB — takes longer to parse and hand its
 * postMessage to the parent; a fixed 800 ms turned that into a false
 * "scripts blocked" verdict.
 */
export function previewReadyTimeoutMs(documentBytes: number): number {
  return Math.min(20_000, 1_000 + Math.ceil(Math.max(0, documentBytes) / 2_048) * 10);
}

/**
 * The sandboxed page and the app's half of the link bridge.
 *
 * The page can forge anything inside the frame, including its token, so a
 * message never opens a link by itself: it only raises the inline bar, and the
 * user's click is what hands the URL to the system browser.
 */
export function PreviewFrame({
  srcdoc,
  url,
  token,
  title,
  capabilities = null,
  onReload,
}: {
  srcdoc: string;
  url: string | null;
  token: string;
  title: string;
  capabilities?: PreviewCapabilities | null;
  onReload?: () => void;
}) {
  const frame = useRef<HTMLIFrameElement | null>(null);
  const [pending, setPending] = useState<{ url: string; host: string } | null>(null);
  const [scriptsBlocked, setScriptsBlocked] = useState(false);
  const ready = useRef(false);
  const readyTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const frozen = useRef(false);
  const documentBytes = useMemo(() => new TextEncoder().encode(srcdoc).length, [srcdoc]);
  // The shell knows whether this WebView can run the shell's scripts at
  // document start. Without that feature Wry injects them into the HTML with
  // CSP hashes, so the page's bridge can never run; that is a fact about the
  // device, not something to infer from a missing message on every platform.
  const cannotRunScripts =
    capabilities?.platform === "android" && capabilities.documentStartScripts === false;

  // biome-ignore lint/correctness/useExhaustiveDependencies: a new document must dismiss an old confirmation
  useEffect(() => {
    frozen.current = false;
    ready.current = false;
    setScriptsBlocked(false);
    setPending(null);
  }, [srcdoc, url]);

  useEffect(
    () => () => {
      if (readyTimer.current) clearTimeout(readyTimer.current);
    },
    [],
  );

  // The bridge's ready message is the proof page scripts actually run. If it
  // never arrives the page may have an unclosed comment that swallowed the
  // bridge, or a policy that blocks inline scripts; the panel shows a plain
  // note with Reload instead of a page whose links silently do nothing.
  const onFrameLoad = () => {
    if (readyTimer.current) clearTimeout(readyTimer.current);
    readyTimer.current = setTimeout(() => {
      if (!ready.current) setScriptsBlocked(true);
    }, previewReadyTimeoutMs(documentBytes));
  };

  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow) return;
      const data = event.data as { type?: unknown; token?: unknown; href?: unknown } | null;
      if (data?.token !== token) return;
      if (data.type === "h4rn:preview-ready") {
        ready.current = true;
        if (readyTimer.current) clearTimeout(readyTimer.current);
        return;
      }
      if (data.type !== "h4rn:preview-open") return;
      // Once the bar is up its URL is frozen: another message from the page —
      // which controls the whole frame — cannot retarget the confirmation.
      if (frozen.current) return;
      const request = previewOpenRequest(data.href);
      if (request) {
        frozen.current = true;
        setPending(request);
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [token]);

  const dismiss = () => {
    frozen.current = false;
    setPending(null);
  };

  const open = () => {
    if (!pending) return;
    void openExternalUrl(pending.url);
    dismiss();
  };

  return (
    <>
      {pending ? (
        <div className="q-pv-link" aria-live="polite">
          <span className="q-pv-link-t">
            Open <span className="q-mono">{pending.host}</span> in your browser?
          </span>
          <span className="grow" />
          <button type="button" className="q-btn secondary" onClick={dismiss}>
            Cancel
          </button>
          <button type="button" className="q-btn primary" onClick={open}>
            Open
          </button>
        </div>
      ) : null}
      {cannotRunScripts ? (
        <div className="q-sub-empty q-pv-state" role="alert">
          <p className="q-pv-state-t">This device can’t run scripts in a preview.</p>
          <p>
            Its WebView blocks a sandboxed page’s own scripts, so the preview was turned off instead
            of showing the page without them. Update Android System WebView, then reload.
          </p>
        </div>
      ) : scriptsBlocked ? (
        <div className="q-sub-empty q-pv-state" role="alert">
          <p className="q-pv-state-t">Preview scripts didn’t start.</p>
          <p>
            The page loaded, but the script that keeps its links safe never ran. Reload the preview;
            a comment that is never closed, or a policy that blocks scripts, can cause this.
          </p>
          {onReload ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={onReload}
            >
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Reload
            </button>
          ) : null}
        </div>
      ) : (
        <iframe
          ref={frame}
          className="q-pv-frame"
          title={title}
          sandbox={PREVIEW_SANDBOX}
          {...(url ? { src: url } : { srcDoc: srcdoc })}
          referrerPolicy="no-referrer"
          onLoad={onFrameLoad}
        />
      )}
    </>
  );
}

export function PreviewFailure({
  title,
  detail,
  onReload,
}: {
  title: string;
  detail: string;
  onReload: () => void;
}) {
  return (
    <div className="q-sub-empty q-pv-state" role="alert">
      <p className="q-pv-state-t">{title}</p>
      <p>{detail}</p>
      <button type="button" className="q-btn secondary" style={{ height: 26 }} onClick={onReload}>
        <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        Reload
      </button>
    </div>
  );
}

function ReloadButton({
  disabled,
  loading,
  onClick,
}: {
  disabled: boolean;
  loading: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className="q-ibtn"
      aria-label="Reload preview"
      title="Reload preview"
      disabled={disabled}
      onClick={onClick}
    >
      <RefreshCwIcon className={cn("q-icon", loading && "q-spin")} strokeWidth={1.5} aria-hidden />
    </button>
  );
}

/**
 * Asks the shell once per open preview what its WebView can do.
 *
 * A shell answers before the document is framed; the browser build answers
 * `null`, which leaves the ready-message check as the only signal there.
 */
function usePreviewCapabilities(enabled: boolean): PreviewCapabilities | null {
  const [capabilities, setCapabilities] = useState<PreviewCapabilities | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    void previewCapabilities().then((reported) => {
      if (alive) setCapabilities(reported);
    });
    return () => {
      alive = false;
    };
  }, [enabled]);
  return capabilities;
}

/**
 * The HTML preview, shared by the right panel, the phone sheet and the
 * full-width main view.
 */
export function PreviewPanel({
  thread,
  path,
  variant = "panel",
}: {
  thread: ThreadSummary;
  path: string | null;
  variant?: "panel" | "full" | "phone";
}) {
  const preview = useHtmlPreview(thread, path);
  // Ask while the file is being read, so an Android WebView without
  // document-start scripts is known before the frame is created.
  const capabilities = usePreviewCapabilities(path !== null);
  const name = path ? fileName(path) : null;
  const missing = preview.document?.missing.length ?? 0;

  return (
    <div className={cn("q-pv", `q-pv-${variant}`)}>
      {variant === "panel" ? (
        <div className="q-rp-bar q-pv-bar">
          <EyeIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? "Preview"}
          </span>
          <span className="grow" />
          <ReloadButton
            disabled={!path || preview.loading}
            loading={preview.loading}
            onClick={preview.reload}
          />
          <button
            type="button"
            className="q-ibtn"
            aria-label="Open preview full width"
            title="Open full width"
            disabled={!path}
            onClick={expandPreview}
          >
            <Maximize2Icon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
          <button
            type="button"
            className="q-ibtn"
            aria-label="Close preview"
            title="Close preview"
            disabled={!path}
            onClick={closePreview}
          >
            <XIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
      ) : null}

      {variant === "phone" ? (
        <div className="q-pv-bar q-pv-phone-bar">
          <EyeIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? "Preview"}
          </span>
          <span className="grow" />
          <ReloadButton
            disabled={!path || preview.loading}
            loading={preview.loading}
            onClick={preview.reload}
          />
        </div>
      ) : null}

      {variant === "full" ? (
        <div className="q-pv-full-bar">
          <button type="button" className="q-btn secondary" onClick={collapsePreview}>
            <ArrowLeftIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Back to thread
          </button>
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? "Preview"}
          </span>
          <span className="grow" />
          <ReloadButton
            disabled={!path || preview.loading}
            loading={preview.loading}
            onClick={preview.reload}
          />
          <button
            type="button"
            className="q-ibtn"
            aria-label="Return the preview to the panel"
            title="Return to panel"
            onClick={collapsePreview}
          >
            <Minimize2Icon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
      ) : null}

      <div className="q-pv-body">
        {!path ? (
          <div className="q-sub-empty q-pv-state">
            <p className="q-pv-state-t">No preview open</p>
            <p>
              Choose an HTML file in Files and open its preview. The page runs sandboxed and can’t
              touch h4rn, its cookies or its storage.
            </p>
            {variant === "panel" ? (
              <button
                type="button"
                className="q-btn secondary"
                style={{ height: 26 }}
                onClick={() => setPanelTab("files")}
              >
                Show files
              </button>
            ) : null}
          </div>
        ) : preview.failure ? (
          <PreviewFailure
            title={preview.failure.title}
            detail={preview.failure.detail}
            onReload={preview.reload}
          />
        ) : !preview.document ? (
          <div className="q-pv-state" role="status" aria-busy="true" aria-label="Loading preview">
            <div className="q-skel" style={{ height: 10, width: "44%" }} />
            <div className="q-skel" style={{ height: 10, width: "66%", marginTop: 8 }} />
            <div className="q-skel" style={{ height: 180, marginTop: 16, borderRadius: 8 }} />
          </div>
        ) : (
          <>
            {missing > 0 ? (
              <p className="q-pv-note">
                {missing === 1
                  ? "1 file wasn’t loaded: it is missing, outside the workspace, or over the size limit."
                  : `${missing} files weren’t loaded: they are missing, outside the workspace, or over the size limit.`}
              </p>
            ) : null}
            <PreviewFrame
              srcdoc={preview.document.srcdoc}
              url={preview.document.frameUrl}
              token={preview.document.token}
              title={`Preview of ${name ?? "HTML file"}`}
              capabilities={capabilities}
              onReload={preview.reload}
            />
          </>
        )}
      </div>
    </div>
  );
}
