import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDownIcon,
  ChevronRightIcon,
  FileCodeIcon,
  FileIcon,
  FileJsonIcon,
  FileTextIcon,
  FolderIcon,
  FolderOpenIcon,
  ImageIcon,
  RefreshCwIcon,
  SearchIcon,
} from "lucide-react";
import type * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
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
  workspaceName?: string;
  host?: string;
  changeLabel?: string | undefined;
  className?: string | undefined;
  onToggle: (path: string) => void;
  onOpen: (path: string) => void;
  onOpenAgent: (path: string) => void;
  onRetry: () => void;
  onSearch?: () => void;
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

/** The change list shows source basenames but keeps test paths intact. */
function changeRowLabel(path: string): string {
  if (path.startsWith("src/")) return path.split("/").at(-1) ?? path;
  return path;
}

function entryIcon(entry: FileEntry, open = false) {
  if (entry.kind === "directory") {
    return open ? <FolderOpenIcon className="q-icon s12" /> : <FolderIcon className="q-icon s12" />;
  }
  const extension = entry.name.split(".").at(-1)?.toLowerCase();
  if (extension === "json") return <FileJsonIcon className="q-icon s14" />;
  if (extension === "png" || extension === "jpg" || extension === "svg") {
    return <ImageIcon className="q-icon s14" />;
  }
  if (extension === "md" || extension === "txt") return <FileTextIcon className="q-icon s14" />;
  if (
    extension &&
    ["ts", "tsx", "js", "jsx", "rs", "py", "go", "css", "html"].includes(extension)
  ) {
    return <FileCodeIcon className="q-icon s14" />;
  }
  return <FileIcon className="q-icon s14" />;
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
  workspaceName,
  host,
  changeLabel,
  className,
  onToggle,
  onOpen,
  onOpenAgent,
  onRetry,
  onSearch,
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
    <aside aria-label="Files" className={cn("q-ftree", className)}>
      <div className="q-pane-head">
        <h2>{workspaceName ?? "Files"}</h2>
        {host ? (
          <span className="q-faint" style={{ fontSize: 12 }}>
            {host}
          </span>
        ) : null}
        {onSearch ? (
          <button type="button" className="q-ibtn" aria-label="Search files" onClick={onSearch}>
            <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
        ) : null}
      </div>

      {error ? (
        <div style={{ margin: 12, fontSize: 12, color: "var(--fg-2)" }}>
          <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>The tree did not load.</p>
          <p style={{ margin: "4px 0 0" }}>{error.message}</p>
          <button type="button" onClick={onRetry} className="q-btn ghost" style={{ height: 24 }}>
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Retry
          </button>
        </div>
      ) : null}

      {loading && !root ? (
        <div style={{ display: "grid", gap: 8, padding: 12 }}>
          {SKELETON_WIDTHS.map((width) => (
            <div
              key={width}
              className="q-skel"
              style={{ height: 10, width: `${width}%` }}
              aria-hidden
            />
          ))}
        </div>
      ) : null}

      <div
        ref={scrollRef}
        role="tree"
        aria-label="Workspace files"
        tabIndex={0}
        onKeyDown={onKeyDown}
        className="q-scroll"
        style={{ paddingBottom: 8 }}
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
                className={cn("q-fnode", isActive && "sel")}
                style={
                  {
                    top: virtual.start,
                    position: "absolute",
                    left: 0,
                    "--d": depth,
                  } as React.CSSProperties
                }
                title={entry.path}
              >
                {entry.kind === "directory" ? (
                  open ? (
                    <ChevronDownIcon className="q-icon s12 q-faint" aria-hidden />
                  ) : (
                    <ChevronRightIcon className="q-icon s12 q-faint" aria-hidden />
                  )
                ) : (
                  <span style={{ width: 12, flex: "none" }} aria-hidden />
                )}
                <span className="dim">{entryIcon(entry, open)}</span>
                <span className="grow">{entry.name}</span>
                {badge ? (
                  <span
                    role="img"
                    className="gs t-warn"
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

      <section
        style={{
          borderTop: "1px solid var(--line)",
          padding: "12px 14px 14px",
          flex: "none",
        }}
        aria-label="Agent changes"
      >
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
          <span style={{ fontWeight: 600 }}>Agent changes</span>
          {changed.length > 0 ? (
            <span className="q-num" style={{ fontSize: 12 }}>
              <span className="add">+{added}</span> <span className="del">−{removed}</span>
            </span>
          ) : (
            <span className="q-num q-faint" style={{ fontSize: 12 }}>
              {agentLoading ? "…" : "0"}
            </span>
          )}
        </div>
        <div className="q-faint" style={{ fontSize: 12, margin: "2px 0 8px" }}>
          {changed.length === 0 ? "No changes in this turn" : (changeLabel ?? "Changes this turn")}
        </div>
        {changed.length > 0 ? (
          <div style={{ maxHeight: 128, overflow: "auto" }}>
            {changed.map((file) => (
              <button
                key={file.path}
                type="button"
                onClick={() => onOpenAgent(file.path)}
                className={cn("q-fnode", file.path === activePath && "sel")}
                style={{ margin: "0 -10px", width: "calc(100% + 20px)" }}
                title={`Open ${file.path} diff vs agent changes`}
              >
                <FileIcon className="q-icon s14 dim" aria-hidden />
                <span className="grow">{changeRowLabel(file.path)}</span>
                <span className="q-num" style={{ fontSize: 12 }}>
                  <span className="add">+{file.added}</span>
                  {file.removed > 0 ? <span className="del"> −{file.removed}</span> : null}
                </span>
              </button>
            ))}
          </div>
        ) : null}
      </section>
    </aside>
  );
}
