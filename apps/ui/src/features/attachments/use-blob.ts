import { useEffect, useState } from "react";

import type { AttachmentId } from "@/gen";
import { i18n } from "@/lib/i18n";
import { getClient } from "@/lib/store/appStore";

import { blobObjectUrl } from "./download";

/** Resolves a content address to an object URL, revoked on unmount. */
export function useBlobUrl(
  id: AttachmentId,
  options: { thumbnail?: boolean } = {},
): {
  url: string | null;
  loading: boolean;
  error: string | null;
} {
  const thumbnail = options.thumbnail ?? false;
  const [state, setState] = useState<{
    url: string | null;
    loading: boolean;
    error: string | null;
  }>({ url: null, loading: true, error: null });

  useEffect(() => {
    let disposed = false;
    let url: string | null = null;
    setState({ url: null, loading: true, error: null });

    blobObjectUrl(getClient(), id, thumbnail)
      .then((next) => {
        if (disposed) {
          URL.revokeObjectURL(next);
          return;
        }
        url = next;
        setState({ url: next, loading: false, error: null });
      })
      .catch((error: unknown) => {
        if (disposed) return;
        setState({
          url: null,
          loading: false,
          error: error instanceof Error ? error.message : i18n.t("thread:attachments.loadFailed"),
        });
      });

    return () => {
      disposed = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [id, thumbnail]);

  return state;
}
