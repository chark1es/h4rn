import { FileDiffIcon, FileTextIcon, Loader2Icon, TriangleAlertIcon } from "lucide-react";
import { useMemo } from "react";

import type { FsDiff, FsTurnChanges, ProtoError } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { cn } from "@/lib/utils";
import { collapseDiff, computeDiff, diffStats } from "./diff";
import { diffStatsLabel } from "./format";

type DiffViewProps = {
  path: string | null;
  mode: "head" | "agent";
  diff: FsDiff | null;
  loading: boolean;
  error: ProtoError | null;
  changes: FsTurnChanges | null;
  onMode: (mode: "head" | "agent") => void;
  onSelectFile: (path: string) => void;
  onOpenFile: (path: string) => void;
};

export function DiffView({
  path,
  mode,
  diff,
  loading,
  error,
  changes,
  onMode,
  onSelectFile,
  onOpenFile,
}: DiffViewProps) {
  const isPhone = useIsPhone();
  const rows = useMemo(() => {
    if (!diff) return [];
    let offset = 0;
    return collapseDiff(computeDiff(diff.before ?? "", diff.after ?? "")).map((row) => {
      const key = `${row.kind}-${offset}`;
      offset += row.kind === "skip" ? row.count : 1;
      return { key, row };
    });
  }, [diff]);
  const stats = useMemo(
    () => diffStats(computeDiff(diff?.before ?? "", diff?.after ?? "")),
    [diff],
  );
  const files = changes?.files ?? [];

  return (
    <section aria-label="Diff" className="flex min-h-0 flex-1 flex-col bg-paper">
      <header className="flex shrink-0 flex-col gap-2 border-b border-line px-3 py-2">
        <div className="flex items-center justify-between gap-2">
          <div
            className="flex items-center gap-0.5 rounded-sm border border-line p-0.5"
            role="tablist"
          >
            <button
              type="button"
              role="tab"
              aria-selected={mode === "head"}
              onClick={() => onMode("head")}
              className={cn(
                "h-6 rounded-[2px] px-2 font-mono text-[10px] transition-colors",
                mode === "head" ? "bg-cue-tint text-cue-ink" : "text-ink-muted hover:text-ink",
              )}
            >
              HEAD
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={mode === "agent"}
              onClick={() => onMode("agent")}
              className={cn(
                "h-6 rounded-[2px] px-2 font-mono text-[10px] transition-colors",
                mode === "agent" ? "bg-cue-tint text-cue-ink" : "text-ink-muted hover:text-ink",
              )}
            >
              Agent changes
            </button>
          </div>
          <span className="font-mono text-[10px] text-ink-faint">
            <span className="text-standby-ink">+{stats.added}</span>{" "}
            <span className="text-strike-ink">−{stats.removed}</span>
          </span>
        </div>

        {isPhone && mode === "agent" && files.length > 0 ? (
          <label className="flex items-center gap-2 text-micro text-ink-muted">
            File
            <select
              value={path ?? ""}
              onChange={(event) => onSelectFile(event.target.value)}
              className="h-8 min-w-0 flex-1 rounded-sm border border-control-border bg-raised px-2 font-mono text-[11px] text-ink"
            >
              {files.map((file) => (
                <option key={file.path} value={file.path}>
                  {file.path} (+{file.added} −{file.removed})
                </option>
              ))}
            </select>
          </label>
        ) : (
          <div className="flex items-center gap-2">
            <FileDiffIcon className="size-3.5 shrink-0 text-ink-faint" aria-hidden />
            <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-ink">
              {path ?? "Pick a file"}
            </span>
            {diff ? (
              <span className="shrink-0 font-mono text-[10px] text-ink-faint">{diff.label}</span>
            ) : null}
          </div>
        )}
      </header>

      {loading ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-compact text-ink-muted">
          <Loader2Icon className="size-4 animate-spin" aria-hidden />
          Loading diff…
        </div>
      ) : error ? (
        <div className="m-3 rounded-sm border border-line bg-raised p-3 text-compact">
          <p className="flex items-center gap-1.5 font-medium text-ink">
            <TriangleAlertIcon className="size-3.5 text-strike-ink" aria-hidden />
            The diff did not load.
          </p>
          <p className="mt-1 text-ink-muted">{error.message}</p>
          {error.hint ? <p className="mt-1 text-ink-faint">{error.hint}</p> : null}
        </div>
      ) : !diff ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center">
          <FileTextIcon className="size-7 text-ink-faint" aria-hidden />
          <p className="text-compact text-ink-muted">Open a file to compare it.</p>
        </div>
      ) : diff.binary ? (
        <div className="m-3 rounded-sm border border-line bg-raised p-3 text-compact text-ink-muted">
          Both sides are binary, so there is no text diff to show.
        </div>
      ) : (
        <>
          {diff.note ? (
            <p className="shrink-0 border-b border-line bg-cue-tint px-3 py-1.5 text-micro text-cue-ink">
              {diff.note}
            </p>
          ) : null}
          <div className="min-h-0 flex-1 overflow-auto font-mono text-[11px] leading-5">
            {rows.map(({ key, row }) =>
              row.kind === "skip" ? (
                <div
                  key={key}
                  className="flex h-5 items-center gap-2 border-y border-line bg-panel px-3 text-[10px] text-ink-faint"
                >
                  <span aria-hidden>⋯</span>
                  {row.count} unchanged {row.count === 1 ? "line" : "lines"}
                </div>
              ) : (
                <div
                  key={key}
                  className={cn(
                    "grid min-w-full grid-cols-[44px_44px_16px_1fr] items-start",
                    row.kind === "added" && "bg-standby-tint",
                    row.kind === "removed" && "bg-strike-tint",
                  )}
                >
                  <span className="select-none border-r border-line/60 px-1 text-right text-ink-faint">
                    {row.oldLine ?? ""}
                  </span>
                  <span className="select-none border-r border-line/60 px-1 text-right text-ink-faint">
                    {row.newLine ?? ""}
                  </span>
                  <span
                    className={cn(
                      "select-none text-center",
                      row.kind === "added"
                        ? "text-standby-ink"
                        : row.kind === "removed"
                          ? "text-strike-ink"
                          : "text-transparent",
                    )}
                    aria-hidden
                  >
                    {row.kind === "added" ? "+" : row.kind === "removed" ? "−" : " "}
                  </span>
                  <span
                    className={cn(
                      "pr-3",
                      isPhone ? "whitespace-pre-wrap break-words" : "whitespace-pre",
                      row.kind === "added" && "text-standby-ink",
                      row.kind === "removed" && "text-strike-ink",
                    )}
                  >
                    {row.text || " "}
                  </span>
                </div>
              ),
            )}
          </div>
          {mode === "agent" ? (
            <footer className="flex shrink-0 items-center justify-between gap-2 border-t border-line bg-panel px-3 py-1.5">
              <span className="font-mono text-[10px] text-ink-faint">
                {changes?.files.length ?? 0} files · {diffStatsLabel(changes)}
              </span>
              {path ? (
                <button
                  type="button"
                  onClick={() => onOpenFile(path)}
                  className="h-6 rounded-sm px-1.5 text-micro text-prompt hover:bg-raised"
                >
                  Open file
                </button>
              ) : null}
            </footer>
          ) : null}
        </>
      )}
    </section>
  );
}
