import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { ThreadId } from "@/gen";
import { renameThread } from "@/lib/store/appStore";

/**
 * Renames one thread. A name typed here is final: title generation never
 * replaces it, even when a model answer arrives afterwards.
 */
export function RenameThreadDialog({
  threadId,
  initial,
  open,
  onOpenChange,
}: {
  threadId: ThreadId;
  initial: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation(["dialogs", "common"]);
  const [value, setValue] = useState(initial);
  const [saving, setSaving] = useState(false);
  // The name the dialog opened with, and the thread it was opened for.
  const seeded = useRef(initial);
  const openedFor = useRef<ThreadId | null>(null);
  const input = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (!open) {
      openedFor.current = null;
      return;
    }
    // Seed once per open, or when the dialog switches threads. A generated
    // title arriving while the dialog is open is not the person's draft and
    // must never replace what they are typing.
    if (openedFor.current === threadId) return;
    openedFor.current = threadId;
    seeded.current = initial;
    setValue(initial);
    setSaving(false);
  }, [open, threadId, initial]);

  const submit = async () => {
    const title = value.trim();
    if (!title) return;
    // An untouched draft still records a rename when the live title changed
    // underneath: the person is confirming the name the dialog opened with,
    // and a generated title must not quietly win.
    if (title === seeded.current && initial === seeded.current) {
      onOpenChange(false);
      return;
    }
    setSaving(true);
    const ok = await renameThread(threadId, title);
    setSaving(false);
    if (ok) onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent size="sm">
        <DialogHeader>
          <DialogTitle>{t("rename.title")}</DialogTitle>
          <DialogDescription>{t("rename.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <input
            ref={input}
            className="q-input"
            aria-label={t("rename.field")}
            value={value}
            maxLength={200}
            onChange={(event) => setValue(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void submit();
              }
            }}
          />
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            {t("common:cancel")}
          </Button>
          <Button disabled={saving || !value.trim()} onClick={() => void submit()}>
            {saving ? t("common:saving") : t("rename.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
