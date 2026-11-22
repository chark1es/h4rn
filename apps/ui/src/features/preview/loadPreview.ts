import type { FsFileContent } from "@/gen";
import {
  buildPreviewDocument,
  isHtmlPath,
  PREVIEW_MAX_ASSET_BYTES,
  PREVIEW_MAX_DOCUMENT_BYTES,
  PREVIEW_MAX_TOTAL_BYTES,
  type PreviewAsset,
  type PreviewAssetMap,
  type PreviewDocument,
  previewAssetFits,
  previewEmbeddedBytes,
  previewMimeType,
  previewToken,
  scanCssReferences,
  scanPreviewReferences,
} from "./htmlPreview";

/** A plain, user-facing reason the preview could not be prepared. */
export type PreviewFailure = { title: string; detail: string };

/** One workspace read, injected so the loader is testable without a daemon. */
export type PreviewReader = (path: string, bytes: boolean) => Promise<FsFileContent>;

export type PreviewLoad = {
  document: PreviewDocument | null;
  failure: PreviewFailure | null;
  /** Files that trigger a reload when they change on disk. */
  watched: string[];
};

function megabytes(bytes: number): string {
  return (bytes / 1_048_576).toFixed(1);
}

function documentTooLarge(size: number): PreviewFailure {
  return {
    title: "This file is too large to preview.",
    detail: `${megabytes(size)} MiB is over the ${megabytes(
      PREVIEW_MAX_DOCUMENT_BYTES,
    )} MiB the daemon reads as text. Open it in a terminal or split the report.`,
  };
}

function pageTooLarge(size: number): PreviewFailure {
  return {
    title: "This page is too large to preview.",
    detail: `The prepared page is ${megabytes(size)} MiB; the preview embeds up to ${megabytes(
      PREVIEW_MAX_TOTAL_BYTES,
    )} MiB from the workspace.`,
  };
}

/**
 * Reads one HTML file and everything it needs from the workspace, then builds
 * the sandboxed document.
 *
 * Only references that resolve inside the workspace are fetched. One asset
 * over the per-response cap, missing, non-UTF-8 or outside the workspace is
 * skipped; the page still renders and reports the skipped count. The 10 MB
 * budget counts every embedded copy, so a page that repeats a large image
 * cannot slip past it.
 */
export async function loadPreview(options: {
  path: string;
  read: PreviewReader;
}): Promise<PreviewLoad> {
  const { path, read } = options;
  const none: PreviewLoad = { document: null, failure: null, watched: [] };
  if (!isHtmlPath(path)) return none;

  let file: FsFileContent;
  try {
    file = await read(path, false);
  } catch (cause) {
    return {
      document: null,
      failure: {
        title: "This file is no longer in the workspace.",
        detail: cause instanceof Error ? cause.message : String(cause),
      },
      watched: [],
    };
  }

  if (file.truncated || file.size_bytes > PREVIEW_MAX_DOCUMENT_BYTES) {
    return { document: null, failure: documentTooLarge(file.size_bytes), watched: [] };
  }
  if (file.content === null) {
    return {
      document: null,
      failure: {
        title: "This file isn’t UTF-8 text.",
        detail: file.binary
          ? "Only text HTML files can be previewed."
          : (file.hint ?? "The daemon could not decode it as text."),
      },
      watched: [],
    };
  }

  const html = file.content;
  const htmlBytes = new TextEncoder().encode(html).length;
  if (htmlBytes > PREVIEW_MAX_DOCUMENT_BYTES) {
    return { document: null, failure: documentTooLarge(htmlBytes), watched: [] };
  }

  const scanned = scanPreviewReferences(html, path);
  const watched = new Set<string>([path, ...scanned.refs.map((ref) => ref.path)]);

  // Breadth-first through the document and its stylesheets, fetching one copy
  // per unique path while counting how many times each path is embedded.
  type Queued = { path: string; kind: "text" | "bytes"; count: number };
  const queue: Queued[] = scanned.refs.map((ref) => ({
    path: ref.path,
    kind: ref.kind,
    count: ref.count,
  }));
  const copies = new Map<string, number>();
  const order: { path: string; asset: PreviewAsset }[] = [];
  const fetched = new Set<string>();

  while (queue.length > 0) {
    const item = queue.shift();
    if (!item) break;
    copies.set(item.path, (copies.get(item.path) ?? 0) + item.count);
    const key = `${item.kind}:${item.path}`;
    if (fetched.has(key)) continue;
    fetched.add(key);

    const asset = await read(item.path, item.kind === "bytes").catch(() => null);
    if (!asset) continue;
    watched.add(item.path);
    if (asset.bytes_omitted) continue;
    if (item.kind === "bytes") {
      if (!asset.bytes_base64 || asset.size_bytes > PREVIEW_MAX_ASSET_BYTES) continue;
      order.push({
        path: item.path,
        asset: { kind: "bytes", base64: asset.bytes_base64, mime: previewMimeType(item.path) },
      });
      continue;
    }
    if (asset.content === null) continue;
    order.push({ path: item.path, asset: { kind: "text", text: asset.content } });
    if (/\.css$/i.test(item.path)) {
      for (const nested of scanCssReferences(asset.content, item.path).refs) {
        watched.add(nested.path);
        queue.push({ path: nested.path, kind: nested.kind, count: nested.count * item.count });
      }
    }
  }

  // The budget counts what the document actually embeds: one copy per
  // reference occurrence, in the serialized form (base64 included).
  let total = htmlBytes;
  const assets: PreviewAssetMap = {};
  for (const entry of order) {
    const count = copies.get(entry.path) ?? 1;
    const cost = previewEmbeddedBytes(entry.asset) * count;
    if (!previewAssetFits(total, cost)) continue;
    total += cost;
    assets[entry.path] = entry.asset;
  }

  const built = buildPreviewDocument({
    html,
    docPath: path,
    assets,
    token: previewToken(),
  });
  const size = new TextEncoder().encode(built.srcdoc).length;
  if (size > PREVIEW_MAX_TOTAL_BYTES) {
    return { document: null, failure: pageTooLarge(size), watched: [...watched] };
  }
  return { document: built, failure: null, watched: [...watched] };
}
