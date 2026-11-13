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
        "relative flex h-11 min-w-0 items-center gap-1.5 overflow-hidden rounded-sm border bg-bg-side py-1 pr-0.5 pl-1.5 sm:h-8",
        failed ? "border-danger/45" : "border-line",
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
        <FileTextIcon className="size-4 shrink-0 text-fg-3" aria-hidden />
      )}

      <div className="min-w-0">
        <p className="max-w-[30ch] truncate text-small leading-tight text-fg">{task.file.name}</p>
        <p
          className={cn(
            "font-mono text-small leading-tight tabular",
            failed ? "text-danger" : "text-fg-3",
          )}
        >
          {statusLine(task)}
        </p>
      </div>

      <button
        type="button"
        onClick={onRemove}
        aria-label={`Remove ${task.file.name}`}
        className="ml-auto flex size-10 shrink-0 items-center justify-center rounded-sm text-fg-3 transition-colors duration-[var(--dur-instant)] hover:bg-bg hover:text-fg focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none sm:size-6"
      >
        <XIcon className="size-3.5" aria-hidden />
      </button>

      {busy ? (
        <span
          aria-hidden
          className="absolute bottom-0 left-0 h-0.5 bg-accent transition-[width] duration-[var(--dur-fast)]"
          style={{ width: `${percent}%` }}
        />
      ) : null}
      {failed ? (
        <span aria-hidden className="absolute bottom-0 left-0 h-0.5 w-full bg-danger/70" />
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
          className="mt-1.5 flex items-start gap-2 rounded-md border border-danger/40 bg-danger-soft/60 px-2 py-1.5"
        >
          <CircleAlertIcon className="mt-0.5 size-3.5 shrink-0 text-danger" aria-hidden />
          <div className="min-w-0 flex-1">
            <p className="text-small font-medium text-danger">{task.file.name} was not attached.</p>
            <p className="text-small leading-relaxed text-danger/85">
              {task.failure?.message}
              {task.failure?.hint ? ` ${task.failure.hint}` : ""}
            </p>
          </div>
          {!task.invalid ? (
            <button
              type="button"
              onClick={() => onRetry(task.key)}
              className="flex h-8 shrink-0 items-center gap-1 rounded-sm border border-danger/40 px-1.5 text-small font-medium text-danger transition-colors duration-[var(--dur-instant)] hover:bg-danger-soft focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none sm:h-6"
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
