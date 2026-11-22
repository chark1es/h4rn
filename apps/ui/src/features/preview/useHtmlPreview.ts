import { useCallback, useEffect, useRef, useState } from "react";

import type { FsFileContent, ThreadSummary } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";
import {
  buildPreviewDocument,
  isHtmlPath,
  PREVIEW_MAX_TOTAL_BYTES,
  type PreviewAssetMap,
  type PreviewDocument,
  previewAssetFits,
  previewMimeType,
  previewToken,
  scanCssReferences,
  scanPreviewReferences,
} from "./htmlPreview";

export type PreviewFailure = { title: string; detail: string };

export type HtmlPreview = {
  /** The prepared document, once the first load succeeded. */
  document: PreviewDocument | null;
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

function tooLarge(size: number): PreviewFailure {
  const mb = (size / 1_048_576).toFixed(1);
  return {
    title: "This file is too large to preview.",
    detail: `${mb} MB is over the ${PREVIEW_MAX_TOTAL_BYTES / 1_048_576} MB the preview loads from the workspace.`,
  };
}

async function readFile(
  host: string,
  workspace: string,
  path: string,
  bytes: boolean,
): Promise<FsFileContent> {
  const response = await getClient().request({
    type: "fs_read",
    host,
    workspace,
    path,
    bytes_base64: bytes,
  });
  if (response.type !== "fs_file") {
    throw new Error("The daemon returned no file.");
  }
  return response.file;
}

/**
 * Reads one HTML file and everything it needs from the workspace, then
 * prepares the sandboxed `srcdoc` the Preview tab renders.
 *
 * The file list is watched through `fs_watch`, so a write by the agent
 * refreshes the preview without a manual reload.
 */
export function useHtmlPreview(thread: ThreadSummary | null, path: string | null): HtmlPreview {
  const host = thread?.host ?? null;
  const workspace = thread?.workspace_path ?? null;
  const [document, setDocument] = useState<PreviewDocument | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<PreviewFailure | null>(null);
  const [nonce, setNonce] = useState(0);
  const watchedRef = useRef<Set<string>>(new Set());
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
    const paths = new Set<string>([path]);
    watchedRef.current = paths;

    void (async () => {
      setLoading(true);
      setFailure(null);
      try {
        const file = await readFile(host, workspace, path, false);
        if (cancelled) return;
        if (file.truncated || file.size_bytes > PREVIEW_MAX_TOTAL_BYTES) {
          setDocument(null);
          setFailure(tooLarge(file.size_bytes));
          return;
        }
        if (file.content === null) {
          setDocument(null);
          setFailure({
            title: "This file isn’t UTF-8 text.",
            detail: file.binary
              ? "Only text HTML files can be previewed."
              : (file.hint ?? "The daemon could not decode it as text."),
          });
          return;
        }

        const html = file.content;
        const htmlBytes = new TextEncoder().encode(html).length;
        if (htmlBytes > PREVIEW_MAX_TOTAL_BYTES) {
          setDocument(null);
          setFailure(tooLarge(htmlBytes));
          return;
        }

        let total = htmlBytes;
        const assets: PreviewAssetMap = {};
        const scanned = scanPreviewReferences(html, path);
        const queue = [...scanned.refs];
        for (const ref of scanned.refs) paths.add(ref.path);
        const visited = new Set<string>();

        while (queue.length > 0) {
          const ref = queue.shift();
          if (!ref || visited.has(ref.path)) continue;
          visited.add(ref.path);
          if (total >= PREVIEW_MAX_TOTAL_BYTES) continue;
          const asset = await readFile(host, workspace, ref.path, ref.kind === "bytes").catch(
            () => null,
          );
          if (cancelled) return;
          if (!asset) continue;
          if (!previewAssetFits(total, asset.size_bytes)) continue;
          total += asset.size_bytes;
          if (ref.kind === "text") {
            if (asset.content === null) continue;
            assets[ref.path] = { kind: "text", text: asset.content };
            if (/\.css$/i.test(ref.path)) {
              const css = scanCssReferences(asset.content, ref.path);
              for (const nested of css.refs) {
                paths.add(nested.path);
                queue.push(nested);
              }
            }
          } else {
            if (!asset.bytes_base64) continue;
            assets[ref.path] = {
              kind: "bytes",
              base64: asset.bytes_base64,
              mime: previewMimeType(ref.path),
            };
          }
        }

        if (cancelled) return;
        const built = buildPreviewDocument({
          html,
          docPath: path,
          assets,
          token: previewToken(),
        });
        setDocument(built);
        setLoading(false);
      } catch (cause) {
        if (cancelled) return;
        const error = toProtoError(cause);
        const missing = error.code === "not_found";
        setDocument(null);
        setFailure({
          title: missing ? "This file is no longer in the workspace." : "The preview didn’t load.",
          detail: error.message,
        });
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [enabled, host, workspace, path, nonce]);

  // The daemon keeps one watch per workspace per connection, and the IDE uses
  // the same channel. Asking for the whole workspace is idempotent; leaving the
  // watch in place when the tab closes keeps the IDE's watch alive, and it ends
  // with the connection.
  useEffect(() => {
    if (!host || !workspace || !path) return;
    void getClient()
      .request({ type: "fs_watch", host, workspace, paths: [] })
      .catch(() => undefined);
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
