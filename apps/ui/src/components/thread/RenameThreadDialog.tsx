import { useEffect, useRef, useState } from "react";

import {
  Dialog,
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
  const [value, setValue] = useState(initial);
  const [saving, setSaving] = useState(false);
  const input = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (!open) return;
    setValue(initial);
    setSaving(false);
  }, [open, initial]);

  const submit = async () => {
    const title = value.trim();
    if (!title || title === initial) {
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
      <DialogContent className="sm:max-w-[420px]">
        <DialogHeader>
          <DialogTitle>Rename thread</DialogTitle>
          <DialogDescription>
            A model may still be writing a title. Your name wins and is kept.
          </DialogDescription>
        </DialogHeader>
        <input
          ref={input}
          className="q-input"
          aria-label="Thread title"
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
        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={saving || !value.trim()}
            onClick={() => void submit()}
          >
            {saving ? "Saving…" : "Save name"}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
