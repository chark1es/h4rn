import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDownIcon,
  ChevronRightIcon,
  FileCodeIcon,
  FileDiffIcon,
  FileIcon,
  FileJsonIcon,
  FileTextIcon,
  FolderIcon,
  FolderOpenIcon,
  ImageIcon,
  RefreshCwIcon,
} from "lucide-react";
import type * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Skeleton } from "@/components/ui/skeleton";
import type { FileEntry, FsListing, FsTurnChanges, ProtoError } from "@/gen";
import { cn } from "@/lib/utils";
import { gitBadge } from "./useIdeSession";

type Row = { entry: FileEntry; depth: number };

const SKELETON_WIDTHS = [52, 63, 74, 85, 96, 107, 118, 129, 140];

type FileTreeProps = {
  root: FsListing | undefined;
  listings: Record<string, FsListing>;
  expanded: Record<string, true>;
  activePath: string | null;
  agentChanges: FsTurnChanges | null;
  agentLoading: boolean;
  loading: boolean;
  error: ProtoError | null;
  onToggle: (path: string) => void;
  onOpen: (path: string) => void;
  onReviewAll: () => void;
  onOpenAgent: (path: string) => void;
  onRetry: () => void;
};

function flatten(listings: Record<string, FsListing>, expanded: Record<string, true>): Row[] {
  const rows: Row[] = [];
  const visit = (directory: string, depth: number) => {
    const listing = listings[directory];
    if (!listing) return;
    for (const entry of listing.entries) {
      rows.push({ entry, depth });
      if (entry.kind === "directory" && expanded[entry.path]) visit(entry.path, depth + 1);
    }
  };
  visit("", 0);
  return rows;
}

function entryIcon(entry: FileEntry, open = false) {
  if (entry.kind === "directory") {
    return open ? <FolderOpenIcon className="size-3.5" /> : <FolderIcon className="size-3.5" />;
  }
  const extension = entry.name.split(".").at(-1)?.toLowerCase();
  if (extension === "json") return <FileJsonIcon className="size-3.5" />;
  if (extension === "png" || extension === "jpg" || extension === "svg") {
    return <ImageIcon className="size-3.5" />;
  }
  if (extension === "md" || extension === "txt") return <FileTextIcon className="size-3.5" />;
  if (
    extension &&
    ["ts", "tsx", "js", "jsx", "rs", "py", "go", "css", "html"].includes(extension)
  ) {
    return <FileCodeIcon className="size-3.5" />;
  }
  return <FileIcon className="size-3.5" />;
}

export function FileTree({
  root,
  listings,
  expanded,
  activePath,
  agentChanges,
  agentLoading,
  loading,
  error,
  onToggle,
  onOpen,
  onReviewAll,
  onOpenAgent,
  onRetry,
}: FileTreeProps) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const rows = useMemo(() => flatten(listings, expanded), [listings, expanded]);
  const [active, setActive] = useState(0);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 26,
    overscan: 12,
  });

  useEffect(() => {
    if (active >= rows.length) setActive(Math.max(0, rows.length - 1));
  }, [active, rows.length]);

  useEffect(() => {
    if (activePath) {
      const index = rows.findIndex((row) => row.entry.path === activePath);
      if (index >= 0) setActive(index);
    }
  }, [activePath, rows]);

  const activate = (row: Row) => {
    if (row.entry.kind === "directory") onToggle(row.entry.path);
    else onOpen(row.entry.path);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const row = rows[active];
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((index) => Math.min(index + 1, rows.length - 1));
      virtualizer.scrollToIndex(Math.min(active + 1, rows.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((index) => Math.max(index - 1, 0));
      virtualizer.scrollToIndex(Math.max(active - 1, 0));
    } else if (event.key === "ArrowRight" && row?.entry.kind === "directory") {
      event.preventDefault();
      if (!expanded[row.entry.path]) onToggle(row.entry.path);
    } else if (event.key === "ArrowLeft" && row?.entry.kind === "directory") {
      event.preventDefault();
      if (expanded[row.entry.path]) onToggle(row.entry.path);
    } else if (event.key === "Enter" && row) {
      event.preventDefault();
      activate(row);
    }
  };

  const changed = agentChanges?.files ?? [];
  const added = changed.reduce((total, file) => total + file.added, 0);
  const removed = changed.reduce((total, file) => total + file.removed, 0);

  return (
    <aside aria-label="Files" className="flex w-60 shrink-0 flex-col border-r border-line bg-panel">
      <div className="flex h-9 shrink-0 items-center justify-between border-b border-line px-3">
        <span className="text-micro font-semibold tracking-[0.08em] text-ink-muted uppercase">
          Files
        </span>
        <span className="font-mono text-[10px] text-ink-faint">{root?.entries.length ?? 0}</span>
      </div>

      {error ? (
        <div className="m-3 rounded-sm border border-line bg-raised p-3 text-compact">
          <p className="font-medium text-ink">The tree did not load.</p>
          <p className="mt-1 text-ink-muted">{error.message}</p>
          <button
            type="button"
            onClick={onRetry}
            className="mt-2 inline-flex items-center gap-1.5 text-prompt underline-offset-2 hover:underline"
          >
            <RefreshCwIcon className="size-3.5" aria-hidden />
            Retry
          </button>
        </div>
      ) : null}

      {loading && !root ? (
        <div className="space-y-2 p-3">
          {SKELETON_WIDTHS.map((width) => (
            <Skeleton key={width} className="h-3" style={{ width: `${width}%` }} />
          ))}
        </div>
      ) : null}

      <div
        ref={scrollRef}
        role="tree"
        aria-label="Workspace files"
        tabIndex={0}
        onKeyDown={onKeyDown}
        className="min-h-0 flex-1 overflow-auto py-1 outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
      >
        <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
          {virtualizer.getVirtualItems().map((virtual) => {
            const row = rows[virtual.index];
            if (!row) return null;
            const { entry, depth } = row;
            const open = expanded[entry.path] === true;
            const isActive = entry.path === activePath;
            const badge = gitBadge(entry.git_status);
            return (
              <button
                key={entry.path}
                type="button"
                role="treeitem"
                aria-expanded={entry.kind === "directory" ? open : undefined}
                aria-selected={isActive}
                onClick={() => {
                  setActive(virtual.index);
                  activate(row);
                }}
                onFocus={() => setActive(virtual.index)}
                className={cn(
                  "absolute left-0 flex h-[26px] w-full items-center gap-1.5 pr-2 text-left text-compact",
                  "border-l-2 border-transparent text-ink-muted transition-colors duration-[var(--dur-instant)]",
                  "hover:bg-raised hover:text-ink focus-visible:outline-none",
                  isActive && "border-cue bg-raised text-ink",
                  entry.kind === "directory" && "font-medium",
                )}
                style={{
                  top: virtual.start,
                  paddingLeft: 8 + depth * 12,
                }}
                title={entry.path}
              >
                {entry.kind === "directory" ? (
                  open ? (
                    <ChevronDownIcon className="size-3 shrink-0 text-ink-faint" aria-hidden />
                  ) : (
                    <ChevronRightIcon className="size-3 shrink-0 text-ink-faint" aria-hidden />
                  )
                ) : (
                  <span className="w-3 shrink-0" aria-hidden />
                )}
                <span className="shrink-0 text-ink-faint">{entryIcon(entry, open)}</span>
                <span className="min-w-0 flex-1 truncate">{entry.name}</span>
                {badge ? (
                  <span
                    role="img"
                    className={cn("shrink-0 font-mono text-[10px]", badge.tone)}
                    title={entry.git_status ?? undefined}
                    aria-label={entry.git_status ?? undefined}
                  >
                    {badge.letter}
                  </span>
                ) : null}
              </button>
            );
          })}
        </div>
      </div>

      <section className="shrink-0 border-t border-line bg-panel" aria-label="Agent changes">
        <div className="flex items-center justify-between px-3 pt-2.5">
          <span className="text-[10px] font-semibold tracking-[0.08em] text-ink-muted uppercase">
            Agent changes
          </span>
          <span className="font-mono text-[10px] text-ink-faint">
            {agentLoading ? "…" : `${changed.length} files`}
          </span>
        </div>
        <div className="flex items-baseline gap-2 px-3 pb-2">
          <span className="text-compact text-ink-muted">
            {changed.length === 0 ? "No changes in this turn" : `${changed.length} files`}
          </span>
          {changed.length > 0 ? (
            <span className="font-mono text-[10px]">
              <span className="text-standby-ink">+{added}</span>{" "}
              <span className="text-strike-ink">−{removed}</span>
            </span>
          ) : null}
        </div>
        {changed.length > 0 ? (
          <ul className="max-h-32 overflow-auto px-1 pb-1">
            {changed.map((file) => (
              <li key={file.path}>
                <button
                  type="button"
                  onClick={() => onOpenAgent(file.path)}
                  className="flex h-6 w-full items-center gap-1.5 rounded-sm px-2 text-left font-mono text-[10px] text-ink-muted hover:bg-raised hover:text-ink"
                  title={`Open ${file.path} diff vs agent changes`}
                >
                  <span className="min-w-0 flex-1 truncate">{file.path}</span>
                  <span className="text-standby-ink">+{file.added}</span>
                  <span className="text-strike-ink">−{file.removed}</span>
                </button>
              </li>
            ))}
          </ul>
        ) : null}
        <div className="px-3 pb-2.5">
          <button
            type="button"
            onClick={onReviewAll}
            disabled={changed.length === 0}
            className="inline-flex h-7 items-center gap-1.5 rounded-sm border border-control-border bg-raised px-2 text-micro text-ink transition-colors hover:bg-paper disabled:pointer-events-none disabled:opacity-45"
          >
            <FileDiffIcon className="size-3.5" aria-hidden />
            Review diff
          </button>
        </div>
      </section>
    </aside>
  );
}
