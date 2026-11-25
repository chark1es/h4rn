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

/** Short type label from the extension ("JSON", "PNG"), or "File". */
export function fileKind(name: string): string {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "File";
  const ext = name.slice(dot + 1);
  return ext.length <= 5 ? ext.toUpperCase() : "File";
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
      return `Uploading ${progressPercent(task)}%`;
    case "committing":
      return "Finishing";
    case "done":
      return `${fileKind(task.file.name)} · ${formatBytes(snapshot.totalBytes)}`;
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
      className={cn("q-att", failed && "bad")}
      title={`${task.file.name} · ${formatBytes(task.restored?.size ?? task.file.size)}`}
    >
      <span className="q-att-ic" aria-hidden>
        {task.previewUrl ? (
          <img src={task.previewUrl} alt="" />
        ) : failed ? (
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} />
        ) : (
          <FileTextIcon className="q-icon s14" strokeWidth={1.5} />
        )}
      </span>
      <span className="q-att-tx">
        <span className="q-att-nm">{task.file.name}</span>
        <span className="q-att-meta q-num">{statusLine(task)}</span>
      </span>
      <button
        type="button"
        onClick={onRemove}
        aria-label={`Remove ${task.file.name}`}
        title="Remove"
        className="q-att-x"
      >
        <XIcon className="q-icon s12" strokeWidth={2} aria-hidden />
      </button>
      {busy ? <span aria-hidden className="q-att-bar" style={{ width: `${percent}%` }} /> : null}
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
    <fieldset className="q-att-tray" aria-label="Attachments">
      <div className="q-att-list">
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
