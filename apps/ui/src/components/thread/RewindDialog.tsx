import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useState } from "react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { filesRewindAvailable, rewindRefusal, rewindThread } from "@/lib/store/appStore";
import type { ThreadLog } from "@/lib/store/projection";
import { cn } from "@/lib/utils";

type Choice = "keep" | "files";

/**
 * Edits one sent prompt: the message and everything after it leave the thread.
 *
 * The file choice is only offered for a thread in its own worktree, and the
 * warning says plainly that it also undoes the person's own later edits.
 */
export function RewindDialog({
  thread,
  log,
  itemId,
  open,
  onOpenChange,
}: {
  thread: ThreadSummary;
  log: ThreadLog | undefined;
  itemId: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [choice, setChoice] = useState<Choice>("keep");
  const [submitting, setSubmitting] = useState(false);
  const refusal = rewindRefusal(thread, log);
  const canRestoreFiles = thread.file_rewind && filesRewindAvailable(log, itemId);

  useEffect(() => {
    if (!open) return;
    setChoice("keep");
    setSubmitting(false);
  }, [open]);

  const submit = async () => {
    if (!itemId || refusal) return;
    setSubmitting(true);
    const prompt = await rewindThread(thread.thread_id, itemId, choice === "files");
    setSubmitting(false);
    if (prompt) onOpenChange(false);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[460px]">
        <DialogHeader>
          <DialogTitle>Edit from here</DialogTitle>
          <DialogDescription>
            This message and everything after it leave {thread.title}. The prompt comes back to the
            composer so you can edit and resend it.
          </DialogDescription>
        </DialogHeader>

        <fieldset className="q-rewind-choices" aria-label="What a rewind does">
          <label className={cn("q-rewind-choice", choice === "keep" && "on")}>
            <input
              type="radio"
              name="rewind-choice"
              value="keep"
              checked={choice === "keep"}
              onChange={() => setChoice("keep")}
            />
            <span className="q-rewind-tx">
              <b>Revert and keep changes</b>
              <span>Workspace files stay exactly as they are now.</span>
            </span>
          </label>

          {canRestoreFiles ? (
            <label className={cn("q-rewind-choice", choice === "files" && "on")}>
              <input
                type="radio"
                name="rewind-choice"
                value="files"
                checked={choice === "files"}
                onChange={() => setChoice("files")}
              />
              <span className="q-rewind-tx">
                <b>Revert files too</b>
                <span>
                  Put this thread&apos;s worktree back to the checkpoint taken before this message.
                </span>
              </span>
            </label>
          ) : (
            <p className="q-rewind-note">
              {thread.file_rewind
                ? "This message has no complete file checkpoint, so only its conversation is rewound. Files are left alone."
                : "This thread runs in the project folder, so only its conversation is rewound. Files are left alone."}
            </p>
          )}
        </fieldset>

        {canRestoreFiles && choice === "files" ? (
          <div className="q-rewind-warning" role="note">
            <TriangleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            <span>
              This also undoes edits you made yourself after this message. Nothing outside the
              worktree is touched.
            </span>
          </div>
        ) : null}

        <p className="q-rewind-note">
          {thread.rewind === "handoff"
            ? `${harnessLabel(thread.harness)} cannot drop turns from its own history. h4rn starts a new session with a handoff of the kept conversation.`
            : thread.rewind === "native"
              ? `${harnessLabel(thread.harness)} drops this message and every later turn from its own history.`
              : `${harnessLabel(thread.harness)} cannot rewind this conversation.`}
        </p>

        {refusal ? (
          <p className="q-rewind-note q-warn" role="status">
            {refusal}
          </p>
        ) : null}

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={submitting || Boolean(refusal) || !itemId}
            onClick={() => void submit()}
          >
            {submitting ? "Rewinding…" : "Rewind thread"}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
