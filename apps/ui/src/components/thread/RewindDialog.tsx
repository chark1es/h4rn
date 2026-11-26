import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { QueuedMessage, RewindPlan, ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { formatList, i18n } from "@/lib/i18n";
import {
  filesRewindAvailable,
  planRewind,
  rewindRefusal,
  rewindThread,
} from "@/lib/store/appStore";
import type { ThreadLog } from "@/lib/store/projection";
import { cn } from "@/lib/utils";

type Choice = "keep" | "files";

/** The ignored paths a restore leaves alone: the first four, then a count. */
function leftAloneText(paths: string[] | undefined): string | null {
  if (!paths || paths.length === 0) return null;
  const shown = formatList(paths.slice(0, 4), "conjunction", "narrow");
  const rest = paths.length - 4;
  return rest > 0 ? i18n.t("dialogs:rewind.ignoredMore", { paths: shown, count: rest }) : shown;
}

/** A plan error with no message of its own; worded when the dialog renders. */
const PLAN_FAILED = "\u0000rewind.planError";

/**
 * Edits one sent prompt: the message and everything after it are removed from the thread.
 *
 * The method the dialog names is the one the daemon would really use for the
 * selected prompt, and file restore is only shown when this machine's Labs
 * switch is on and the daemon would accept it. The warning says plainly that
 * the file choice also undoes the person's own later edits.
 */
export function RewindDialog({
  thread,
  log,
  queued,
  itemId,
  open,
  onOpenChange,
}: {
  thread: ThreadSummary;
  log: ThreadLog | undefined;
  /** This thread's queue rows, so an uncertain delivery can refuse the edit. */
  queued?: QueuedMessage[] | undefined;
  itemId: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation(["dialogs", "common"]);
  const [choice, setChoice] = useState<Choice>("keep");
  const [submitting, setSubmitting] = useState(false);
  const [plan, setPlan] = useState<RewindPlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const refusal = rewindRefusal(thread, log, queued);
  const checkpointReady = filesRewindAvailable(log, itemId);
  const filesOffered = plan?.files_enabled === true;
  const canRestoreFiles = filesOffered && plan.files_available === true;
  const leftAlone = leftAloneText(plan?.left_alone);

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
        // Without a message of its own, the dialog words the error when it renders.
        setPlanError(error instanceof Error ? error.message : PLAN_FAILED);
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
          <DialogTitle>{t("rewind.title")}</DialogTitle>
          <DialogDescription>{t("rewind.description", { title: thread.title })}</DialogDescription>
        </DialogHeader>

        {filesOffered ? (
          <fieldset className="q-rewind-choices" aria-label={t("rewind.choices")}>
            <label className={cn("q-rewind-choice", choice === "keep" && "on")}>
              <input
                type="radio"
                name="rewind-choice"
                value="keep"
                checked={choice === "keep"}
                onChange={() => setChoice("keep")}
              />
              <span className="q-rewind-tx">
                <b>{t("rewind.keep.title")}</b>
                <span>{t("rewind.keep.body")}</span>
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
                  <b>{t("rewind.files.title")}</b>
                  <span>{t("rewind.files.body")}</span>
                </span>
              </label>
            ) : (
              <p className="q-rewind-note">
                {!thread.file_rewind
                  ? t("rewind.filesLeft.projectFolder")
                  : checkpointReady
                    ? t("rewind.filesLeft.unsafe")
                    : t("rewind.filesLeft.noCheckpoint")}
              </p>
            )}
          </fieldset>
        ) : null}

        {canRestoreFiles && choice === "files" ? (
          <div className="q-rewind-warning" role="note">
            <TriangleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            <span>{t("rewind.warning")}</span>
          </div>
        ) : null}

        {canRestoreFiles && choice === "files" && leftAlone ? (
          <p className="q-rewind-note" role="note">
            {t("rewind.ignored", { paths: leftAlone })}
          </p>
        ) : null}

        <p className="q-rewind-note">
          {plan?.method === "handoff"
            ? t("rewind.method.handoff")
            : plan?.method === "native"
              ? t("rewind.method.native", { harness: harnessLabel(thread.harness) })
              : plan?.method === "unsupported"
                ? t("rewind.method.unsupported", { harness: harnessLabel(thread.harness) })
                : checking
                  ? t("rewind.checking")
                  : null}
        </p>

        {planError ? (
          <p className="q-rewind-note q-warn" role="status">
            {planError === PLAN_FAILED ? t("rewind.planError") : planError}
          </p>
        ) : null}

        {refusal ? (
          <p className="q-rewind-note q-warn" role="status">
            {refusal}
          </p>
        ) : null}

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={() => onOpenChange(false)}>
            {t("common:cancel")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={submitting || blocked}
            onClick={() => void submit()}
          >
            {submitting ? t("rewind.submitting") : t("rewind.submit")}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
