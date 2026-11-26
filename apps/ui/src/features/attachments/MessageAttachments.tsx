import { DownloadIcon, FileTextIcon, ImageOffIcon } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { Attachment } from "@/gen";
import { getClient, pushToast } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { saveBlob } from "./download";
import { formatBytes } from "./upload";
import { useBlobUrl } from "./use-blob";

function isImage(mime: string): boolean {
  return mime.startsWith("image/");
}

function DownloadButton({ attachment, className }: { attachment: Attachment; className?: string }) {
  const { t } = useTranslation("thread");
  const [busy, setBusy] = useState(false);

  const download = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await saveBlob(getClient(), attachment.id, attachment.name);
    } catch (error) {
      pushToast({
        variant: "error",
        title: t("attachments.downloadFailed", { name: attachment.name }),
        description: t("attachments.downloadHint", {
          message: error instanceof Error ? error.message : t("attachments.notSent"),
        }),
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <button
      type="button"
      onClick={() => void download()}
      aria-label={t("attachments.download", { name: attachment.name })}
      title={t("attachments.download", { name: attachment.name })}
      className={cn(
        "flex size-10 shrink-0 items-center justify-center rounded-sm text-fg-3 transition-colors duration-[var(--dur-instant)] hover:bg-bg hover:text-fg focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none disabled:opacity-60 sm:size-7",
        className,
      )}
      disabled={busy}
    >
      <DownloadIcon className="size-3.5" aria-hidden />
    </button>
  );
}

function Thumbnail({ attachment, onOpen }: { attachment: Attachment; onOpen: () => void }) {
  const { t } = useTranslation("thread");
  const { url, loading, error } = useBlobUrl(attachment.id, { thumbnail: true });

  if (error) {
    return (
      <div className="flex min-h-24 flex-col items-start justify-between gap-1 rounded-md border border-line bg-bg-side p-2">
        <ImageOffIcon className="size-4 text-fg-3" aria-hidden />
        <div className="min-w-0">
          <p className="truncate text-small text-fg-2">{attachment.name}</p>
          <p className="text-small text-fg-3">{t("attachments.noPreview")}</p>
        </div>
        <DownloadButton attachment={attachment} className="self-end" />
      </div>
    );
  }

  return (
    <button
      type="button"
      onClick={onOpen}
      aria-label={t("attachments.open", { name: attachment.name })}
      title={`${attachment.name} · ${formatBytes(attachment.size)}`}
      className="group relative aspect-[4/3] min-h-24 w-full overflow-hidden rounded-md border border-line bg-bg-side focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none"
    >
      {loading || !url ? (
        <span className="absolute inset-0 animate-pulse bg-bg-side" aria-hidden />
      ) : (
        <img
          src={url}
          alt={attachment.name}
          loading="lazy"
          className="size-full object-cover transition-transform duration-[var(--dur-base)] ease-[var(--ease-out)] group-hover:scale-[1.015]"
        />
      )}
      <span className="absolute inset-x-0 bottom-0 truncate bg-[var(--scrim)] px-1.5 py-0.5 text-left text-small text-fg">
        {attachment.name}
      </span>
    </button>
  );
}

function Lightbox({ attachment, onClose }: { attachment: Attachment | null; onClose: () => void }) {
  if (!attachment) return null;
  return <LightboxBody attachment={attachment} onClose={onClose} />;
}

function LightboxBody({ attachment, onClose }: { attachment: Attachment; onClose: () => void }) {
  const { t } = useTranslation(["thread", "common"]);
  const { url, loading, error } = useBlobUrl(attachment.id);

  return (
    <Dialog open onOpenChange={(open) => (open ? undefined : onClose())}>
      <DialogContent className="max-h-[92dvh] gap-2 p-0 sm:max-w-3xl">
        <DialogHeader className="flex-row items-baseline justify-between gap-3 border-b border-line px-4 py-3">
          <div className="min-w-0">
            <DialogTitle className="truncate text-compact">{attachment.name}</DialogTitle>
            <DialogDescription className="text-small">
              {attachment.mime} · {formatBytes(attachment.size)}
            </DialogDescription>
          </div>
        </DialogHeader>

        <div className="flex min-h-40 items-center justify-center overflow-auto bg-bg-sunk px-2">
          {loading || !url ? (
            <p className="py-16 text-small text-fg-3">
              {t("attachments.loading", { name: attachment.name })}
            </p>
          ) : error ? (
            <p className="max-w-[42ch] py-16 text-center text-small text-danger">
              {t("attachments.previewFailed", { message: error })}
            </p>
          ) : (
            <img
              src={url}
              alt={attachment.name}
              className="max-h-[62dvh] max-w-full object-contain"
            />
          )}
        </div>

        <DialogFooter className="flex-row items-center justify-end gap-2 border-t border-line px-4 py-2">
          <DownloadButton attachment={attachment} />
          <button
            type="button"
            onClick={onClose}
            className="h-8 rounded-sm border border-line px-2.5 text-small font-medium text-fg transition-colors duration-[var(--dur-instant)] hover:bg-bg-side focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none"
          >
            {t("common:close")}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Images render as a thumbnail grid, other files as labelled rows. */
export function MessageAttachments({
  attachments,
  align = "start",
}: {
  attachments: Attachment[];
  align?: "start" | "end";
}) {
  const [open, setOpen] = useState<Attachment | null>(null);
  if (attachments.length === 0) return null;

  const images = attachments.filter((attachment) => isImage(attachment.mime));
  const files = attachments.filter((attachment) => !isImage(attachment.mime));

  return (
    <div className={cn("flex max-w-[68ch] flex-col gap-1.5", align === "end" && "items-end")}>
      {images.length > 0 ? (
        <div className="grid w-full max-w-[430px] grid-cols-2 gap-1.5 sm:grid-cols-3">
          {images.map((attachment) => (
            <Thumbnail
              key={attachment.id}
              attachment={attachment}
              onOpen={() => setOpen(attachment)}
            />
          ))}
        </div>
      ) : null}

      {files.length > 0 ? (
        <div className="flex w-full max-w-[430px] flex-col gap-1">
          {files.map((attachment) => (
            <div
              key={attachment.id}
              className="flex h-12 items-center gap-2 rounded-md border border-line bg-bg-side px-2 sm:h-9"
            >
              <FileTextIcon className="size-4 shrink-0 text-fg-3" aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="truncate text-small font-medium text-fg">{attachment.name}</p>
                <p className="text-small tabular text-fg-3">
                  {formatBytes(attachment.size)} · {attachment.mime}
                </p>
              </div>
              <DownloadButton attachment={attachment} />
            </div>
          ))}
        </div>
      ) : null}

      <Lightbox attachment={open} onClose={() => setOpen(null)} />
    </div>
  );
}
