import type { FsFileContent } from "@/gen";
import {
  buildPreviewDocument,
  isHtmlPath,
  PREVIEW_MAX_ASSET_BYTES,
  PREVIEW_MAX_DOCUMENT_BYTES,
  PREVIEW_MAX_TEXT_BYTES,
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
export type PreviewReader = (
  path: string,
  bytes: boolean,
  maxBytes?: number,
) => Promise<FsFileContent>;

export type PreviewLoad = {
  document: PreviewDocument | null;
  failure: PreviewFailure | null;
  /** Files that trigger a reload when they change on disk. */
  watched: string[];
};

function megabytes(bytes: number): string {
  return (bytes / 1_048_576).toFixed(1);
}

function kilobytes(bytes: number): string {
  return `${Math.ceil(bytes / 1024)} KiB`;
}

function documentTooLarge(size: number): PreviewFailure {
  return {
    title: "This file is too large to preview.",
    detail: `The preview reads at most ${kilobytes(
      PREVIEW_MAX_DOCUMENT_BYTES,
    )} of text so one response fits a transport frame. This file is ${kilobytes(
      size,
    )}, or needs more than that once its text is escaped for the wire. Open it in a terminal or split the report.`,
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
 * Only references that resolve inside the workspace are fetched, and text is
 * read with a frame-sized bound so one response always fits the secure
 * transport. That bound is the raw text, not a promise that every file under
 * it opens: JSON escaping of quotes, newlines and control characters can push
 * the serialized response past the frame, and the daemon then answers with
 * `content_omitted: too_large`. The 10 MB budget stops the fetching once it is
 * spent, and the document builder reserves the trusted prefix and bridge and
 * then charges every copy as it is inserted (including a file reached through
 * an imported stylesheet and the markup each insertion generates), leaving out
 * an insertion that would not fit rather than building an oversized page. One
 * asset over the per-response cap, missing, non-UTF-8 or outside the workspace
 * is skipped; the page still renders and reports the skipped count.
 */
export async function loadPreview(options: {
  path: string;
  read: PreviewReader;
  /** Total embedded-size bound; the production value is the exported cap. */
  maxTotalBytes?: number;
}): Promise<PreviewLoad> {
  const { path, read } = options;
  const cap = options.maxTotalBytes ?? PREVIEW_MAX_TOTAL_BYTES;
  const none: PreviewLoad = { document: null, failure: null, watched: [] };
  if (!isHtmlPath(path)) return none;

  let file: FsFileContent;
  try {
    // The selected file stays watched on every outcome, so recreating it (or
    // fixing an encoding) reloads the preview without closing the tab.
    file = await read(path, false, PREVIEW_MAX_TEXT_BYTES);
  } catch (cause) {
    return {
      document: null,
      failure: {
        title: "This file is no longer in the workspace.",
        detail: cause instanceof Error ? cause.message : String(cause),
      },
      watched: [path],
    };
  }

  if (
    file.truncated ||
    file.content_omitted === "too_large" ||
    file.size_bytes > PREVIEW_MAX_DOCUMENT_BYTES
  ) {
    return { document: null, failure: documentTooLarge(file.size_bytes), watched: [path] };
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
      watched: [path],
    };
  }

  const html = file.content;
  const htmlBytes = new TextEncoder().encode(html).length;
  if (htmlBytes > PREVIEW_MAX_DOCUMENT_BYTES) {
    return { document: null, failure: documentTooLarge(htmlBytes), watched: [path] };
  }

  const scanned = scanPreviewReferences(html, path);
  const watched = new Set<string>([path, ...scanned.refs.map((ref) => ref.path)]);

  // Breadth-first through the document and its stylesheets. Each asset is
  // charged to the budget as it arrives, so the budget stops the network of
  // reads instead of being checked after everything has already been fetched.
  type Queued = { path: string; kind: "text" | "bytes"; count: number };
  const queue: Queued[] = scanned.refs.map((ref) => ({
    path: ref.path,
    kind: ref.kind,
    count: ref.count,
  }));
  const copies = new Map<string, number>();
  const order: { path: string; asset: PreviewAsset }[] = [];
  const fetched = new Set<string>();
  let total = htmlBytes;
  let exhausted = total >= cap;
  const loadedByKey = new Map<string, PreviewAsset>();

  while (queue.length > 0) {
    const item = queue.shift();
    if (!item) break;
    copies.set(item.path, (copies.get(item.path) ?? 0) + item.count);
    const key = `${item.kind}:${item.path}`;
    if (fetched.has(key)) {
      // The asset is already in hand, but a later reference (for example from
      // an imported stylesheet) inserts another copy, so it is charged too.
      const known = loadedByKey.get(key);
      if (known) {
        const cost = previewEmbeddedBytes(known) * item.count;
        if (previewAssetFits(total, cost, cap)) total += cost;
        else exhausted = true;
      }
      continue;
    }
    fetched.add(key);

    // Once nothing more fits, the rest of the queue is reported as skipped
    // without another read: the budget stops the network of requests.
    if (exhausted) continue;

    const asset = await read(
      item.path,
      item.kind === "bytes",
      item.kind === "bytes" ? undefined : PREVIEW_MAX_TEXT_BYTES,
    ).catch(() => null);
    if (!asset) continue;
    watched.add(item.path);
    if (asset.bytes_omitted || asset.content_omitted) continue;

    let loaded: PreviewAsset | null = null;
    if (item.kind === "bytes") {
      if (!asset.bytes_base64 || asset.size_bytes > PREVIEW_MAX_ASSET_BYTES) continue;
      loaded = {
        kind: "bytes",
        base64: asset.bytes_base64,
        mime: previewMimeType(item.path),
      };
    } else if (asset.content !== null) {
      loaded = { kind: "text", text: asset.content };
    }
    if (!loaded) continue;

    loadedByKey.set(key, loaded);
    const count = copies.get(item.path) ?? 1;
    const cost = previewEmbeddedBytes(loaded) * count;
    if (!previewAssetFits(total, cost, cap)) {
      exhausted = true;
      continue;
    }
    total += cost;
    order.push({ path: item.path, asset: loaded });
    if (total >= cap) exhausted = true;

    if (loaded.kind === "text" && /\.css$/i.test(item.path)) {
      for (const nested of scanCssReferences(loaded.text, item.path).refs) {
        watched.add(nested.path);
        queue.push({ path: nested.path, kind: nested.kind, count: nested.count * item.count });
      }
    }
  }

  const assets: PreviewAssetMap = {};
  for (const entry of order) assets[entry.path] = entry.asset;

  const built = buildPreviewDocument({
    html,
    docPath: path,
    assets,
    token: previewToken(),
    maxBytes: cap,
  });
  const size = new TextEncoder().encode(built.srcdoc).length;
  if (size > cap) {
    return { document: null, failure: pageTooLarge(size), watched: [...watched] };
  }
  return { document: built, failure: null, watched: [...watched] };
}
