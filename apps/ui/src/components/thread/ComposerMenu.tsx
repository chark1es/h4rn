import {
  Code2Icon,
  FileDiffIcon,
  FileIcon,
  PlusIcon,
  SquareIcon,
  TerminalIcon,
  WorkflowIcon,
} from "lucide-react";

import type { ComposerCommandIcon } from "@/lib/composerCommands";
import { splitPath } from "@/lib/composerMenu";

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
  active,
  onSelect,
  onHighlight,
}: {
  row: ComposerMenuRow;
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
  listId: string;
  onSelect: (index: number) => void;
  onHighlight: (index: number) => void;
  onClose: () => void;
}) {
  const label = kind === "file" ? "Files" : "Commands";
  const empty = emptyText(kind, query, loading, error, workspaceName);
  const rowsView = rows.map((row, index) => (
    <MenuRow
      key={row.id}
      row={row}
      active={index === activeIndex}
      onSelect={() => onSelect(index)}
      onHighlight={() => onHighlight(index)}
    />
  ));

  if (isPhone) {
    return (
      <div className="q-ph-sheet q-comp-sheet" role="dialog" aria-label={label}>
        <div className="q-comp-sheet-head">
          <span>{kind === "file" ? `Files in ${workspaceName}` : "Commands"}</span>
          <span className="grow" />
          <button type="button" className="q-comp-sheet-done" onClick={onClose}>
            Done
          </button>
        </div>
        <div className="q-comp-sheet-list" role="listbox" aria-label={label} id={listId}>
          {rows.length > 0 ? rowsView : <p className="q-comp-menu-empty">{empty}</p>}
        </div>
      </div>
    );
  }

  return (
    <div
      className="q-comp-menu"
      style={{ left: anchor.left, bottom: anchor.bottom }}
      data-kind={kind}
    >
      <div className="q-comp-menu-list" role="listbox" aria-label={label} id={listId}>
        {rows.length > 0 ? rowsView : <p className="q-comp-menu-empty">{empty}</p>}
      </div>
      <div className="q-comp-menu-foot" aria-hidden>
        <span>↑↓ move</span>
        <span>↵ choose</span>
        <span>esc close</span>
      </div>
    </div>
  );
}
