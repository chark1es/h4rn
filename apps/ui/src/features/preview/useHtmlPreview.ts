import { useCallback, useEffect, useRef, useState } from "react";

import type { FsFileContent, ThreadSummary } from "@/gen";
import { retainWorkspaceWatch } from "@/lib/client/fsWatch";
import { getClient } from "@/lib/store/appStore";
import { isHtmlPath, type PreviewDocument } from "./htmlPreview";
import { loadPreview, type PreviewFailure } from "./loadPreview";
import { preparePreviewFrame, releasePreviewFrame } from "./previewFrame";

export type { PreviewFailure } from "./loadPreview";

/** A prepared document plus the URL of the shell's preview origin, if any. */
export type FrameDocument = PreviewDocument & { frameUrl: string | null };

export type HtmlPreview = {
  /** The prepared document, once the first load succeeded. */
  document: FrameDocument | null;
  loading: boolean;
  failure: PreviewFailure | null;
  reload: () => void;
};

const EMPTY: HtmlPreview = {
  document: null,
  loading: false,
  failure: null,
  reload: () => undefined,
};

async function readFile(
  host: string,
  workspace: string,
  path: string,
  bytes: boolean,
  maxBytes?: number,
): Promise<FsFileContent> {
  const response = await getClient().request({
    type: "fs_read",
    host,
    workspace,
    path,
    bytes_base64: bytes,
    ...(maxBytes === undefined ? {} : { max_bytes: maxBytes }),
  });
  if (response.type !== "fs_file") {
    throw new Error("The daemon returned no file.");
  }
  return response.file;
}

/**
 * Reads one HTML file and everything it needs from the workspace, then
 * prepares the sandboxed page the Preview tab renders.
 *
 * The workspace is watched (shared with the IDE through a reference count), so
 * a write by the agent refreshes the preview without a manual reload. A
 * packaged shell gets a URL on its own preview origin; the browser gets
 * `srcdoc`.
 */
export function useHtmlPreview(thread: ThreadSummary | null, path: string | null): HtmlPreview {
  const host = thread?.host ?? null;
  const workspace = thread?.workspace_path ?? null;
  const [document, setDocument] = useState<FrameDocument | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<PreviewFailure | null>(null);
  const [nonce, setNonce] = useState(0);
  const watchedRef = useRef<Set<string>>(new Set());
  const frameUrlRef = useRef<string | null>(null);
  const enabled = thread !== null;

  const reload = useCallback(() => setNonce((value) => value + 1), []);

  // `nonce` is the explicit reload trigger; it is intentionally not read here.
  // biome-ignore lint/correctness/useExhaustiveDependencies: nonce is a reload trigger
  useEffect(() => {
    if (!enabled || !host || !workspace || !path || !isHtmlPath(path)) {
      setDocument(null);
      setLoading(false);
      setFailure(null);
      watchedRef.current = new Set();
      return;
    }
    let cancelled = false;
    const dropFrame = () => {
      const url = frameUrlRef.current;
      frameUrlRef.current = null;
      void releasePreviewFrame(url);
    };

    void (async () => {
      setLoading(true);
      setFailure(null);
      try {
        const loaded = await loadPreview({
          path,
          read: (target, bytes, maxBytes) => readFile(host, workspace, target, bytes, maxBytes),
        });
        if (cancelled) return;
        watchedRef.current = new Set(loaded.watched);
        if (!loaded.document) {
          dropFrame();
          setDocument(null);
          setFailure(loaded.failure);
          return;
        }
        const frame = await preparePreviewFrame(loaded.document.srcdoc);
        if (cancelled) {
          // The load was replaced or the panel closed while the shell was
          // storing the document: release the URL instead of leaking it.
          void releasePreviewFrame(frame?.url ?? null);
          return;
        }
        const released = frameUrlRef.current;
        frameUrlRef.current = frame?.url ?? null;
        if (released && released !== frameUrlRef.current) void releasePreviewFrame(released);
        setDocument({ ...loaded.document, frameUrl: frame?.url ?? null });
      } catch (cause) {
        if (cancelled) return;
        dropFrame();
        setDocument(null);
        setFailure({
          title: "The preview didn’t load.",
          detail: cause instanceof Error ? cause.message : String(cause),
        });
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [enabled, host, workspace, path, nonce]);

  // A shell keeps the document in memory until the preview closes or is
  // replaced; the hook is the one that knows when that happens.
  useEffect(
    () => () => {
      const url = frameUrlRef.current;
      frameUrlRef.current = null;
      void releasePreviewFrame(url);
    },
    [],
  );

  // The IDE and the preview share one watch per workspace; the refcount means
  // one view unmounting does not stop the other from seeing changes.
  useEffect(() => {
    if (!host || !workspace || !path) return;
    return retainWorkspaceWatch(host, workspace);
  }, [host, workspace, path]);

  useEffect(() => {
    if (!workspace || !path) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = getClient().onEvent((envelope) => {
      const event = envelope.event;
      if (event.type !== "fs_changed") return;
      if (event.workspace !== workspace) return;
      if (!watchedRef.current.has(event.path)) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => setNonce((value) => value + 1), 200);
    });
    return () => {
      off();
      if (timer) clearTimeout(timer);
    };
  }, [workspace, path]);

  if (!thread || !path) return EMPTY;
  return { document, loading, failure, reload };
}
