import {
  ArrowLeftIcon,
  EyeIcon,
  Maximize2Icon,
  Minimize2Icon,
  RefreshCwIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { Tip } from "@/components/ui/tooltip";
import type { ThreadSummary } from "@/gen";
import { formatNumber, i18n } from "@/lib/i18n";
import { openExternalUrl } from "@/lib/platform/openExternal";
import { closePreview, collapsePreview, expandPreview, setPanelTab } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { PREVIEW_SANDBOX, previewOpenRequest } from "./htmlPreview";
import type { PreviewFailure as PreviewFailureKind } from "./loadPreview";
import { type PreviewCapabilities, previewCapabilities } from "./previewFrame";
import { useHtmlPreview } from "./useHtmlPreview";

function fileName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? path;
}

/** "400 KiB": a document's size next to the text limit, in one unit. */
function kibibytes(bytes: number): string {
  return i18n.t("preview:failure.kib", { size: formatNumber(Math.ceil(bytes / 1024)) });
}

/** "10.0 MiB": a prepared page's size next to the embedding budget. */
function mebibytes(bytes: number): string {
  const size = formatNumber(bytes / 1_048_576, {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  });
  return i18n.t("preview:failure.mib", { size });
}

/**
 * The words for a failed preview, in the active language. The daemon's own
 * error text, when there is one, is shown as sent.
 */
export function describePreviewFailure(failure: PreviewFailureKind): {
  title: string;
  detail: string;
} {
  const noFile = i18n.t("preview:failure.noFile");
  switch (failure.kind) {
    case "gone":
      return { title: i18n.t("preview:failure.goneTitle"), detail: failure.detail ?? noFile };
    case "documentTooLarge":
      return {
        title: i18n.t("preview:failure.documentTooLargeTitle"),
        // A file under the limit got here because escaping its text for the
        // wire made it too large; naming the limit would read as a contradiction.
        detail:
          failure.size <= failure.limit
            ? i18n.t("preview:failure.documentTooLargeEscaped", { size: kibibytes(failure.size) })
            : i18n.t("preview:failure.documentTooLarge", {
                limit: kibibytes(failure.limit),
                size: kibibytes(failure.size),
              }),
      };
    case "pageTooLarge":
      return {
        title: i18n.t("preview:failure.pageTooLargeTitle"),
        detail: i18n.t("preview:failure.pageTooLarge", {
          size: mebibytes(failure.size),
          limit: mebibytes(failure.limit),
        }),
      };
    case "notText":
      return {
        title: i18n.t("preview:failure.notTextTitle"),
        detail: failure.binary
          ? i18n.t("preview:failure.binary")
          : (failure.hint ?? i18n.t("preview:failure.undecodable")),
      };
    case "loadFailed":
      return { title: i18n.t("preview:failure.loadFailedTitle"), detail: failure.detail ?? noFile };
  }
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
 * A Reload button inside a notice.
 *
 * A notice can be about the WebView itself, so its Reload asks the shell for
 * the capabilities again (the panel's reload nonce) as well as re-reading the
 * file; a WebView that was updated out from under the app clears the notice.
 */
function NoticeReload({ onReload }: { onReload: (() => void) | undefined }) {
  const { t } = useTranslation("preview");
  if (!onReload) return null;
  return (
    <button type="button" className="q-btn secondary" style={{ height: 26 }} onClick={onReload}>
      <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      {t("reload")}
    </button>
  );
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
  const { t } = useTranslation(["preview", "common"]);
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
  // A shell that could not tell (an error, or an unparseable WebView version)
  // reports unknown, which counts as unsupported here: the page is not framed
  // until a Reload gets a real answer.
  const cannotRunScripts =
    capabilities?.platform === "android" && capabilities.documentStartScripts === false;
  const capabilityUnknown = capabilities?.documentStartScripts === "unknown";

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
            <Trans
              t={t}
              i18nKey="link.ask"
              values={{ host: pending.host }}
              components={{ host: <span className="q-mono" /> }}
            />
          </span>
          <span className="grow" />
          <button type="button" className="q-btn secondary" onClick={dismiss}>
            {t("common:cancel")}
          </button>
          <button type="button" className="q-btn primary" onClick={open}>
            {t("link.open")}
          </button>
        </div>
      ) : null}
      {cannotRunScripts ? (
        <div className="q-sub-empty q-pv-state" role="alert">
          <p className="q-pv-state-t">{t("scripts.deviceTitle")}</p>
          <p>{t("scripts.deviceBody")}</p>
          <NoticeReload onReload={onReload} />
        </div>
      ) : capabilityUnknown || scriptsBlocked ? (
        <div className="q-sub-empty q-pv-state" role="alert">
          <p className="q-pv-state-t">{t("scripts.blockedTitle")}</p>
          <p>{capabilityUnknown ? t("scripts.unknownBody") : t("scripts.blockedBody")}</p>
          <NoticeReload onReload={onReload} />
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
  failure,
  onReload,
}: {
  failure: PreviewFailureKind;
  onReload: () => void;
}) {
  const { t } = useTranslation("preview");
  const { title, detail } = describePreviewFailure(failure);
  return (
    <div className="q-sub-empty q-pv-state" role="alert">
      <p className="q-pv-state-t">{title}</p>
      <p>{detail}</p>
      <button type="button" className="q-btn secondary" style={{ height: 26 }} onClick={onReload}>
        <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        {t("reload")}
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
  const { t } = useTranslation("preview");
  return (
    <Tip label={t("reloadPreview")}>
      <button
        type="button"
        className="q-ibtn"
        aria-label={t("reloadPreview")}
        disabled={disabled}
        onClick={onClick}
      >
        <RefreshCwIcon
          className={cn("q-icon", loading && "q-spin")}
          strokeWidth={1.5}
          aria-hidden
        />
      </button>
    </Tip>
  );
}

/**
 * Asks the shell what its WebView can do.
 *
 * A shell answers before the document is framed; the browser build answers
 * `null`, which leaves the ready-message check as the only signal there. The
 * `revision` is the preview's reload nonce: asking again on Reload lets an
 * updated WebView clear a stale notice instead of leaving it up until the tab
 * is closed.
 */
function usePreviewCapabilities(enabled: boolean, revision: number): PreviewCapabilities | null {
  const [capabilities, setCapabilities] = useState<PreviewCapabilities | null>(null);
  // `revision` is the reload trigger; it is intentionally not read here.
  // biome-ignore lint/correctness/useExhaustiveDependencies: revision refetches the report
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    void previewCapabilities().then((reported) => {
      if (alive) setCapabilities(reported);
    });
    return () => {
      alive = false;
    };
  }, [enabled, revision]);
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
  const { t } = useTranslation("preview");
  const preview = useHtmlPreview(thread, path);
  // Ask while the file is being read, so an Android WebView without
  // document-start scripts is known before the frame is created; a Reload
  // asks again, and the nonce keeps the two in step.
  const capabilities = usePreviewCapabilities(path !== null, preview.revision);
  const name = path ? fileName(path) : null;
  const missing = preview.document?.missing.length ?? 0;

  return (
    <div className={cn("q-pv", `q-pv-${variant}`)}>
      {variant === "panel" ? (
        <div className="q-rp-bar q-pv-bar">
          <EyeIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? t("preview")}
          </span>
          <span className="grow" />
          <ReloadButton
            disabled={!path || preview.loading}
            loading={preview.loading}
            onClick={preview.reload}
          />
          <Tip label={t("openFullWidth")}>
            <button
              type="button"
              className="q-ibtn"
              aria-label={t("openFullWidthLabel")}
              disabled={!path}
              onClick={expandPreview}
            >
              <Maximize2Icon className="q-icon" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
          <Tip label={t("close")}>
            <button
              type="button"
              className="q-ibtn"
              aria-label={t("close")}
              disabled={!path}
              onClick={closePreview}
            >
              <XIcon className="q-icon" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
        </div>
      ) : null}

      {variant === "phone" ? (
        <div className="q-pv-bar q-pv-phone-bar">
          <EyeIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? t("preview")}
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
            {t("backToThread")}
          </button>
          <span className="q-pv-name q-mono" title={path ?? undefined}>
            {name ?? t("preview")}
          </span>
          <span className="grow" />
          <ReloadButton
            disabled={!path || preview.loading}
            loading={preview.loading}
            onClick={preview.reload}
          />
          <Tip label={t("returnToPanel")}>
            <button
              type="button"
              className="q-ibtn"
              aria-label={t("returnToPanelLabel")}
              onClick={collapsePreview}
            >
              <Minimize2Icon className="q-icon" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
        </div>
      ) : null}

      <div className="q-pv-body">
        {!path ? (
          <div className="q-sub-empty q-pv-state">
            <p className="q-pv-state-t">{t("empty.title")}</p>
            <p>{t("empty.body")}</p>
            {variant === "panel" ? (
              <button
                type="button"
                className="q-btn secondary"
                style={{ height: 26 }}
                onClick={() => setPanelTab("files")}
              >
                {t("empty.showFiles")}
              </button>
            ) : null}
          </div>
        ) : preview.failure ? (
          <PreviewFailure failure={preview.failure} onReload={preview.reload} />
        ) : !preview.document ? (
          <div className="q-pv-state" role="status" aria-busy="true" aria-label={t("loading")}>
            <div className="q-skel" style={{ height: 10, width: "44%" }} />
            <div className="q-skel" style={{ height: 10, width: "66%", marginTop: 8 }} />
            <div className="q-skel" style={{ height: 180, marginTop: 16, borderRadius: 8 }} />
          </div>
        ) : (
          <>
            {missing > 0 ? <p className="q-pv-note">{t("missing", { count: missing })}</p> : null}
            <PreviewFrame
              srcdoc={preview.document.srcdoc}
              url={preview.document.frameUrl}
              token={preview.document.token}
              title={t("frameTitle", { name: name ?? path })}
              capabilities={capabilities}
              onReload={preview.reload}
            />
          </>
        )}
      </div>
    </div>
  );
}
