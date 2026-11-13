import { CircleAlertIcon, FileTextIcon, RotateCcwIcon, XIcon } from "lucide-react";

import { cn } from "@/lib/utils";
import { formatBytes } from "./upload";
import type { AttachmentDraft } from "./use-attachments";

function progressPercent(task: AttachmentDraft): number {
  const { snapshot } = task;
  if (snapshot.phase === "done") return 100;
  if (snapshot.totalBytes <= 0) return 0;
  return Math.min(99, Math.round((snapshot.sentBytes / snapshot.totalBytes) * 100));
}

function statusLine(task: AttachmentDraft): string {
  const { snapshot } = task;
  if (task.invalid) return "Not attached";
  switch (snapshot.phase) {
    case "queued":
      return "Waiting";
    case "hashing":
      return "Reading";
    case "uploading":
      return `${progressPercent(task)}%`;
    case "committing":
      return "Finishing";
    case "done":
      return formatBytes(snapshot.totalBytes);
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
  }
}

function Chip({ task, onRemove }: { task: AttachmentDraft; onRemove: () => void }) {
  const failed = Boolean(task.failure) && (task.invalid || task.snapshot.phase === "failed");
  const percent = progressPercent(task);
  const busy = !task.invalid && task.snapshot.phase !== "done" && task.snapshot.phase !== "failed";

  return (
    <div
      className={cn(
        "relative flex h-11 min-w-0 items-center gap-1.5 overflow-hidden rounded-sm border bg-panel py-1 pr-0.5 pl-1.5 sm:h-8",
        failed ? "border-strike/45" : "border-line",
      )}
      title={`${task.file.name} · ${formatBytes(task.file.size)}`}
    >
      {task.previewUrl ? (
        <img
          src={task.previewUrl}
          alt=""
          className="size-8 shrink-0 rounded-[2px] border border-line/70 object-cover sm:size-6"
        />
      ) : (
        <FileTextIcon className="size-4 shrink-0 text-ink-faint" aria-hidden />
      )}

      <div className="min-w-0">
        <p className="max-w-[30ch] truncate text-micro leading-tight text-ink">{task.file.name}</p>
        <p
          className={cn(
            "font-mono text-micro leading-tight tabular",
            failed ? "text-strike-ink" : "text-ink-faint",
          )}
        >
          {statusLine(task)}
        </p>
      </div>

      <button
        type="button"
        onClick={onRemove}
        aria-label={`Remove ${task.file.name}`}
        className="ml-auto flex size-10 shrink-0 items-center justify-center rounded-sm text-ink-faint transition-colors duration-[var(--dur-instant)] hover:bg-paper hover:text-ink focus-visible:ring-2 focus-visible:ring-cue/40 focus-visible:outline-none sm:size-6"
      >
        <XIcon className="size-3.5" aria-hidden />
      </button>

      {busy ? (
        <span
          aria-hidden
          className="absolute bottom-0 left-0 h-0.5 bg-cue transition-[width] duration-[var(--dur-fast)]"
          style={{ width: `${percent}%` }}
        />
      ) : null}
      {failed ? (
        <span aria-hidden className="absolute bottom-0 left-0 h-0.5 w-full bg-strike/70" />
      ) : null}
    </div>
  );
}

/** One-line actionable upload failure, kept beside the chips it belongs to. */
export function AttachmentTray({
  tasks,
  onRemove,
  onRetry,
}: {
  tasks: AttachmentDraft[];
  onRemove: (key: string) => void;
  onRetry: (key: string) => void;
}) {
  if (tasks.length === 0) return null;
  const failures = tasks.filter(
    (task) => Boolean(task.failure) && (task.invalid || task.snapshot.phase === "failed"),
  );

  return (
    <fieldset
      className="m-0 min-w-0 border-x-0 border-t-0 border-b border-line/70 p-0 px-2 py-2"
      aria-label="Attachments"
    >
      <div className="flex flex-wrap gap-1.5">
        {tasks.map((task) => (
          <Chip key={task.key} task={task} onRemove={() => onRemove(task.key)} />
        ))}
      </div>

      {failures.slice(0, 2).map((task) => (
        <div
          key={`failure-${task.key}`}
          className="mt-1.5 flex items-start gap-2 rounded-sm border border-strike/40 bg-strike-tint/60 px-2 py-1.5"
        >
          <CircleAlertIcon className="mt-0.5 size-3.5 shrink-0 text-strike-ink" aria-hidden />
          <div className="min-w-0 flex-1">
            <p className="text-micro font-medium text-strike-ink">
              {task.file.name} was not attached.
            </p>
            <p className="text-micro leading-relaxed text-strike-ink/85">
              {task.failure?.message}
              {task.failure?.hint ? ` ${task.failure.hint}` : ""}
            </p>
          </div>
          {!task.invalid ? (
            <button
              type="button"
              onClick={() => onRetry(task.key)}
              className="flex h-8 shrink-0 items-center gap-1 rounded-sm border border-strike/40 px-1.5 text-micro font-medium text-strike-ink transition-colors duration-[var(--dur-instant)] hover:bg-strike-tint focus-visible:ring-2 focus-visible:ring-cue/40 focus-visible:outline-none sm:h-6"
            >
              <RotateCcwIcon className="size-3" aria-hidden />
              Retry
            </button>
          ) : null}
        </div>
      ))}
    </fieldset>
  );
}
