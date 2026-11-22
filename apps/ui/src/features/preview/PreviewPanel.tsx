import {
  ArrowLeftIcon,
  EyeIcon,
  Maximize2Icon,
  Minimize2Icon,
  RefreshCwIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import type { ThreadSummary } from "@/gen";
import { openExternalUrl } from "@/lib/platform/openExternal";
import { closePreview, collapsePreview, expandPreview, setPanelTab } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { PREVIEW_SANDBOX, previewOpenRequest } from "./htmlPreview";
import { useHtmlPreview } from "./useHtmlPreview";

function fileName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
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
}: {
  srcdoc: string;
  url: string | null;
  token: string;
  title: string;
}) {
  const frame = useRef<HTMLIFrameElement | null>(null);
  const [pending, setPending] = useState<{ url: string; host: string } | null>(null);
  const frozen = useRef(false);

  // biome-ignore lint/correctness/useExhaustiveDependencies: a new document must dismiss an old confirmation
  useEffect(() => {
    frozen.current = false;
    setPending(null);
  }, [srcdoc, url]);

  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow) return;
      const data = event.data as { type?: unknown; token?: unknown; href?: unknown } | null;
      if (data?.type !== "h4rn:preview-open") return;
      if (data.token !== token) return;
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
      <iframe
        ref={frame}
        className="q-pv-frame"
        title={title}
        sandbox={PREVIEW_SANDBOX}
        {...(url ? { src: url } : { srcDoc: srcdoc })}
        referrerPolicy="no-referrer"
      />
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
            />
          </>
        )}
      </div>
    </div>
  );
}
