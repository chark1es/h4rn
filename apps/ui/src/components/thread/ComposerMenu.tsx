import {
  Code2Icon,
  FileDiffIcon,
  FileIcon,
  PlusIcon,
  SquareIcon,
  TerminalIcon,
  WorkflowIcon,
} from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

import type { ComposerCommandIcon } from "@/lib/composerCommands";
import { type PopoverPlacement, placePopover, splitPath } from "@/lib/composerMenu";

export type ComposerMenuRow =
  | { kind: "file"; id: string; path: string }
  | {
      kind: "command";
      id: string;
      label: string;
      hint?: string;
      icon: ComposerCommandIcon;
    };

const COMMAND_ICONS = {
  changes: FileDiffIcon,
  ide: Code2Icon,
  "new-thread": PlusIcon,
  stop: SquareIcon,
  subagents: WorkflowIcon,
  terminal: TerminalIcon,
} as const;

/** A quiet count for the index footer: `3,000`, not `3000`. */
function countLabel(count: number): string {
  return new Intl.NumberFormat("en-US").format(count);
}

function cappedNote(indexed: number, truncated: boolean): string | null {
  if (!truncated) return null;
  return indexed > 0
    ? `Showing the first ${countLabel(indexed)} files.`
    : "Showing only the files the index could reach.";
}

function emptyText(
  kind: "file" | "command",
  query: string,
  loading: boolean,
  error: string | null,
  workspaceName: string,
): string {
  if (kind === "file" && error) return error;
  if (kind === "file" && loading) return "Searching this workspace…";
  if (query.trim().length > 0) {
    return kind === "file"
      ? `No files match “${query.trim()}”.`
      : `No commands match “${query.trim()}”.`;
  }
  return kind === "file"
    ? `No files in ${workspaceName}.`
    : "No commands are available for this thread.";
}

function MenuRow({
  row,
  id,
  active,
  onSelect,
  onHighlight,
}: {
  row: ComposerMenuRow;
  id: string;
  active: boolean;
  onSelect: () => void;
  onHighlight: () => void;
}) {
  if (row.kind === "file") {
    const { directory, name } = splitPath(row.path);
    return (
      <button
        type="button"
        role="option"
        id={id}
        aria-selected={active}
        className="q-comp-menu-item"
        onMouseEnter={onHighlight}
        onMouseDown={(event) => event.preventDefault()}
        onClick={onSelect}
      >
        <FileIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        <span className="q-mono truncate">
          <span className="q-faint">{directory}</span>
          {name}
        </span>
      </button>
    );
  }
  const Icon = COMMAND_ICONS[row.icon];
  return (
    <button
      type="button"
      role="option"
      id={id}
      aria-selected={active}
      className="q-comp-menu-item q-comp-menu-cmd"
      onMouseEnter={onHighlight}
      onMouseDown={(event) => event.preventDefault()}
      onClick={onSelect}
    >
      <Icon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
      <span className="truncate">{row.label}</span>
      {row.hint ? <span className="q-comp-menu-hint truncate">{row.hint}</span> : null}
    </button>
  );
}

export function ComposerMenu({
  kind,
  rows,
  activeIndex,
  anchor,
  loading,
  error,
  workspaceName,
  query,
  isPhone,
  indexedCount,
  truncated,
  listId,
  onSelect,
  onHighlight,
  onClose,
}: {
  kind: "file" | "command";
  rows: ComposerMenuRow[];
  activeIndex: number;
  anchor: { left: number; bottom: number };
  loading: boolean;
  error: string | null;
  workspaceName: string;
  query: string;
  isPhone: boolean;
  /** How many files the index holds, for the cap footer. */
  indexedCount: number;
  /** The index stopped at a cap, so a thin result may just be outside it. */
  truncated: boolean;
  listId: string;
  onSelect: (index: number) => void;
  onHighlight: (index: number) => void;
  onClose: () => void;
}) {
  const label = kind === "file" ? "Files" : "Commands";
  const empty = emptyText(kind, query, loading, error, workspaceName);
  const note = kind === "file" ? cappedNote(indexedCount, truncated) : null;
  const menuRef = useRef<HTMLDivElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const measured = useRef<PopoverPlacement | null>(null);
  const [placement, setPlacement] = useState<PopoverPlacement | null>(null);
  const rowsView = rows.map((row, index) => (
    <MenuRow
      key={row.id}
      id={`${listId}-option-${index}`}
      row={row}
      active={index === activeIndex}
      onSelect={() => onSelect(index)}
      onHighlight={() => onHighlight(index)}
    />
  ));

  // Follow the highlight with the keyboard: a long list must show the row it
  // is about to insert.
  useEffect(() => {
    const active = listRef.current?.querySelector<HTMLElement>(
      `[id="${listId}-option-${activeIndex}"]`,
    );
    active?.scrollIntoView?.({ block: "nearest" });
  }, [activeIndex, listId]);

  // Measure the popover before placing it: long paths make it wider than the
  // viewport guess, and a tall list near the top has to flip below the caret.
  // Runs after every render because the list (and so the height) follows the
  // query; the comparison keeps a re-measure from looping.
  useLayoutEffect(() => {
    if (isPhone) return;
    const element = menuRef.current;
    if (!element) return;
    const parent = element.offsetParent;
    if (!(parent instanceof HTMLElement)) return;
    const parentRect = parent.getBoundingClientRect();
    const next = placePopover({
      anchor,
      size: { width: element.offsetWidth, height: element.offsetHeight },
      parent: {
        left: parentRect.left,
        top: parentRect.top,
        width: parentRect.width,
        height: parentRect.height,
      },
      viewport: { width: window.innerWidth, height: window.innerHeight },
    });
    const previous = measured.current;
    if (
      previous &&
      previous.left === next.left &&
      previous.bottom === next.bottom &&
      previous.top === next.top
    ) {
      return;
    }
    measured.current = next;
    setPlacement(next);
  });

  if (isPhone) {
    return (
      <div
        className="q-ph-sheet q-comp-sheet"
        role="dialog"
        aria-label={label}
        data-placement="above"
      >
        <div className="q-grab" />
        <div className="q-comp-sheet-head">
          <span>{kind === "file" ? `Files in ${workspaceName}` : "Commands"}</span>
          <span className="grow" />
          <button type="button" className="q-comp-sheet-done" onClick={onClose}>
            Done
          </button>
        </div>
        <div
          className="q-comp-sheet-list"
          role="listbox"
          aria-label={label}
          id={listId}
          ref={listRef}
        >
          {rows.length > 0 ? rowsView : <p className="q-comp-menu-empty">{empty}</p>}
          {note ? <p className="q-comp-menu-note">{note}</p> : null}
        </div>
      </div>
    );
  }

  // Until the first measurement the anchor is the best guess; after it the
  // measured placement keeps the popover inside the viewport.
  const style = placement
    ? placement.top !== null
      ? { left: placement.left, top: placement.top }
      : { left: placement.left, bottom: placement.bottom ?? anchor.bottom }
    : { left: anchor.left, bottom: anchor.bottom };

  return (
    <div
      ref={menuRef}
      className="q-comp-menu"
      style={style}
      data-kind={kind}
      {...(placement?.top != null ? { "data-flipped": "below" } : {})}
    >
      <div className="q-comp-menu-list" role="listbox" aria-label={label} id={listId} ref={listRef}>
        {rows.length > 0 ? rowsView : <p className="q-comp-menu-empty">{empty}</p>}
        {note ? <p className="q-comp-menu-note">{note}</p> : null}
      </div>
      <div className="q-comp-menu-foot" aria-hidden>
        <span>↑↓ move</span>
        <span>↵ choose</span>
        <span>esc close</span>
      </div>
    </div>
  );
}
