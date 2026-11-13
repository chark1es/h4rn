import type { AttachmentId } from "@/gen";
import type { H4rnClient } from "@/lib/client/client";

import { BinaryKind, binaryRouter } from "./frames";

export type DownloadedBlob = {
  bytes: Uint8Array;
  mime: string;
  name: string;
  thumbnail: boolean;
};

const DOWNLOAD_TIMEOUT_MS = 30_000;

/**
 * Streams one blob (or its thumbnail) from the daemon.
 *
 * `GetBlob` answers with JSON metadata and a binary stream id; the router
 * buffers chunks that overtake the response, so a small blob that arrives in
 * the same tick is still collected.
 */
export async function downloadBlob(
  client: H4rnClient,
  id: AttachmentId,
  options: { thumbnail?: boolean; signal?: AbortSignal } = {},
): Promise<DownloadedBlob> {
  binaryRouter.attach(client);
  const response = await client.request({
    type: "get_blob",
    id,
    range: null,
    thumbnail: options.thumbnail ?? false,
  });
  if (response.type !== "blob") {
    throw new Error("The daemon answered the blob request with an unexpected response.");
  }

  const download = response.download;
  const chunks: Uint8Array[] = [];
  let received = 0;

  return new Promise<DownloadedBlob>((resolve, reject) => {
    let settled = false;
    let off: () => void = () => undefined;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const abort = () => finish(new Error("The download was cancelled."));
    const finish = (error?: Error) => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      off();
      options.signal?.removeEventListener("abort", abort);
      if (error) {
        reject(error);
        return;
      }
      const bytes = new Uint8Array(received);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.length;
      }
      resolve({ bytes, mime: download.mime, name: download.name, thumbnail: download.thumbnail });
    };

    options.signal?.addEventListener("abort", abort, { once: true });
    if (options.signal?.aborted) {
      abort();
      return;
    }

    off = binaryRouter.listen(download.stream_id, (frame) => {
      if (frame.kind !== BinaryKind.BlobChunk) return;
      if (frame.payload.length > 0) {
        chunks.push(frame.payload.slice());
        received += frame.payload.length;
      }
      if (received >= download.size) finish();
    });
    if (settled) return;

    timer = setTimeout(
      () => finish(new Error("The daemon stopped sending the file.")),
      DOWNLOAD_TIMEOUT_MS,
    );

    // An empty blob has no chunk to wait for.
    if (download.size === 0) finish();
  });
}

/** Fetches a blob and wraps it in an object URL the UI can render. */
export async function blobObjectUrl(
  client: H4rnClient,
  id: AttachmentId,
  thumbnail = false,
): Promise<string> {
  const download = await downloadBlob(client, id, { thumbnail });
  return URL.createObjectURL(
    new Blob([new Uint8Array(download.bytes).buffer], { type: download.mime }),
  );
}

/** Downloads a blob to the user's machine through a temporary anchor. */
export async function saveBlob(client: H4rnClient, id: AttachmentId, name: string): Promise<void> {
  const download = await downloadBlob(client, id);
  const url = URL.createObjectURL(
    new Blob([new Uint8Array(download.bytes).buffer], { type: download.mime }),
  );
  try {
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = name || download.name || "attachment";
    anchor.rel = "noopener";
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
  } finally {
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  }
}
