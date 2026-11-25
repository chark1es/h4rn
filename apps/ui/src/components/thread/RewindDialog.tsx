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
import type { RewindPlan, ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import {
  filesRewindAvailable,
  planRewind,
  rewindRefusal,
  rewindThread,
} from "@/lib/store/appStore";
import type { ThreadLog } from "@/lib/store/projection";
import { cn } from "@/lib/utils";

type Choice = "keep" | "files";

/**
 * Edits one sent prompt: the message and everything after it leave the thread.
 *
 * The method the dialog names is the one the daemon would really use for the
 * selected prompt, and file restore is only shown when this machine's Labs
 * switch is on and the daemon would accept it. The warning says plainly that
 * the file choice also undoes the person's own later edits.
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
  const [plan, setPlan] = useState<RewindPlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const refusal = rewindRefusal(thread, log);
  const checkpointReady = filesRewindAvailable(log, itemId);
  const filesOffered = plan?.files_enabled === true;
  const canRestoreFiles = filesOffered && plan.files_available === true;

  useEffect(() => {
    if (!open) {
      setPlan(null);
      setPlanError(null);
      setChecking(false);
      return;
    }
    setChoice("keep");
    setSubmitting(false);
    if (!itemId) {
      setPlan(null);
      setPlanError(null);
      setChecking(false);
      return;
    }
    let live = true;
    setPlan(null);
    setPlanError(null);
    setChecking(true);
    void planRewind(thread.thread_id, itemId)
      .then((next) => {
        if (!live) return;
        setPlan(next);
        setChecking(false);
      })
      .catch((error: unknown) => {
        if (!live) return;
        setPlanError(error instanceof Error ? error.message : "The rewind plan could not be read.");
        setChecking(false);
      });
    return () => {
      live = false;
    };
  }, [open, itemId, thread.thread_id]);

  const blocked = Boolean(refusal) || Boolean(planError) || !plan;
  const submit = async () => {
    if (!itemId || blocked) return;
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

        {filesOffered ? (
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
                    Put this thread&apos;s worktree back to the checkpoint taken before this
                    message.
                  </span>
                </span>
              </label>
            ) : (
              <p className="q-rewind-note">
                {!thread.file_rewind
                  ? "This thread runs in the project folder, so only its conversation is rewound. Files are left alone."
                  : checkpointReady
                    ? "This worktree cannot be restored safely right now, so only its conversation is rewound. Files are left alone."
                    : "This message has no complete file checkpoint, so only its conversation is rewound. Files are left alone."}
              </p>
            )}
          </fieldset>
        ) : null}

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
          {plan?.method === "handoff"
            ? `${harnessLabel(thread.harness)} cannot drop turns from its own history. h4rn starts a new session with a handoff of the kept conversation.`
            : plan?.method === "native"
              ? `${harnessLabel(thread.harness)} drops this message and every later turn from its own history.`
              : plan?.method === "unsupported"
                ? `${harnessLabel(thread.harness)} cannot rewind this conversation.`
                : checking
                  ? "Checking what this rewind will do…"
                  : null}
        </p>

        {planError ? (
          <p className="q-rewind-note q-warn" role="status">
            {planError}
          </p>
        ) : null}

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
            disabled={submitting || blocked}
            onClick={() => void submit()}
          >
            {submitting ? "Rewinding…" : "Rewind thread"}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
