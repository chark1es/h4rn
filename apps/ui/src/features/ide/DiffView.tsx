import {
  FileDiffIcon,
  FileTextIcon,
  Loader2Icon,
  MessageSquareIcon,
  RefreshCwIcon,
  TriangleAlertIcon,
} from "lucide-react";
import { Fragment, type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import type {
  FsDiff,
  FsDiffCommitList,
  FsDiffFile,
  FsDiffFiles,
  FsTurnChanges,
  GitFileStatus,
  HarnessKind,
  HostId,
  ProtoError,
  ThreadId,
} from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { collapseDiff, computeDiff, type DiffLine, diffStats } from "./diff";
import {
  addReviewComment,
  commentRange,
  deleteReviewComment,
  type ReviewComment,
  updateReviewComment,
  useReviewComments,
} from "./reviewComments";
import type { DiffSource } from "./sources";

type DiffViewProps = {
  path: string | null;
  source: DiffSource;
  diff: FsDiff | null;
  loading: boolean;
  error: ProtoError | null;
  changes: FsTurnChanges | null;
  /** The branch or commit range's file list; null for HEAD and turn scopes. */
  files: FsDiffFiles | null;
  filesLoading: boolean;
  filesError: ProtoError | null;
  commits: FsDiffCommitList | null;
  host: HostId | null;
  harness: HarnessKind | null;
  threadId: ThreadId | null;
  /** "branch vs main", "commit 1a2b3c4", "uncommitted (HEAD)". */
  sourceLabel: string;
  changeTitle: string | null;
  changeAt?: string | null;
  onSelectFile: (path: string) => void;
  onOpenFile: (path: string) => void;
  onRevert: (path: string, content: string) => void;
  onSendComments: (comments: ReviewComment[]) => void;
  /** A branch review has no base; the fallback is the uncommitted source. */
  onCompareHead: () => void;
  /** Re-read the branch or commit lists. */
  onRefresh: () => void;
};

const KEYWORDS = new Set([
  "as",
  "async",
  "await",
  "break",
  "case",
  "catch",
  "class",
  "const",
  "continue",
  "default",
  "delete",
  "do",
  "else",
  "export",
  "extends",
  "finally",
  "for",
  "from",
  "function",
  "if",
  "implements",
  "import",
  "in",
  "instanceof",
  "interface",
  "let",
  "new",
  "null",
  "of",
  "return",
  "static",
  "throw",
  "true",
  "false",
  "try",
  "type",
  "typeof",
  "undefined",
  "var",
  "void",
  "while",
]);

const TOKEN =
  /(\/\/[^\n]*)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)|\b(\d[\d_]*(?:\.\d+)?)\b|\b([A-Za-z_$][\w$]*)\b(?=\s*\()|\b([A-Za-z_$][\w$]*)\b/g;

/** A small TypeScript tokenizer; the prototype's code pane is not plain text. */
export function highlightCode(text: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  let last = 0;
  let key = 0;
  for (const match of text.matchAll(TOKEN)) {
    const index = match.index ?? 0;
    if (index > last) nodes.push(text.slice(last, index));
    const [token, comment, string, , call, identifier] = match;
    let className: string | undefined;
    if (comment) className = "q-sc";
    else if (string) className = "q-ss";
    else if (call) className = KEYWORDS.has(call) ? "q-sk" : "q-sf";
    else if (identifier) className = KEYWORDS.has(identifier) ? "q-sk" : undefined;
    nodes.push(
      className ? (
        <span key={`t${key++}`} className={className}>
          {token}
        </span>
      ) : (
        token
      ),
    );
    last = index + token.length;
  }
  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
}

const STATUS: Record<GitFileStatus, { letter: string; tone: string; label: string }> = {
  added: { letter: "A", tone: "q-add", label: "added" },
  modified: { letter: "M", tone: "q-warn", label: "modified" },
  deleted: { letter: "D", tone: "q-del", label: "deleted" },
  renamed: { letter: "R", tone: "q-warn", label: "renamed" },
  untracked: { letter: "U", tone: "q-faint", label: "untracked" },
  conflicted: { letter: "!", tone: "q-del", label: "conflicted" },
  type_changed: { letter: "T", tone: "q-warn", label: "type changed" },
};

function splitPath(path: string): { dir: string; name: string } {
  const slash = path.lastIndexOf("/");
  return slash < 0
    ? { dir: "", name: path }
    : { dir: path.slice(0, slash + 1), name: path.slice(slash + 1) };
}

function fileTitle(file: FsDiffFile): string {
  const status = STATUS[file.status]?.label ?? file.status;
  const rename = file.old_path ? ` (was ${file.old_path})` : "";
  const counts = file.binary ? " · binary" : file.counts_omitted ? " · line counts not read" : "";
  return `${file.path} · ${status}${rename}${counts}`;
}

type LineSelection = { anchor: number; focus: number };

function rangeLabel(
  visible: DiffLine[],
  selection: LineSelection | null,
): { start: number; end: number; side: string } | null {
  if (!selection) return null;
  const range = commentRange(visible, selection.anchor, selection.focus);
  if (!range) return null;
  return {
    start: range.startLine,
    end: range.endLine,
    side: range.side === "new" ? "new" : "old",
  };
}

function CommentEditor({
  label,
  onSubmit,
  onCancel,
}: {
  label: string;
  onSubmit: (text: string) => void;
  onCancel: () => void;
}) {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement | null>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  const submit = () => {
    if (text.trim().length === 0) return;
    onSubmit(text);
  };
  return (
    <form
      className="q-review-note editing"
      aria-label={`New review comment on ${label}`}
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <div className="q-review-meta">{label}</div>
      <textarea
        ref={ref}
        value={text}
        rows={2}
        aria-label="Review comment"
        placeholder="What should the agent change here?"
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            onCancel();
          }
          if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
            event.preventDefault();
            submit();
          }
        }}
      />
      <div className="q-review-actions">
        <button type="button" className="q-btn ghost" onClick={onCancel}>
          Cancel
        </button>
        <button
          type="button"
          className="q-btn secondary"
          disabled={text.trim().length === 0}
          onClick={submit}
        >
          Add comment
        </button>
      </div>
    </form>
  );
}

function CommentNote({
  comment,
  onUpdate,
  onDelete,
}: {
  comment: ReviewComment;
  onUpdate: (text: string) => void;
  onDelete: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(comment.text);
  const range =
    comment.startLine === comment.endLine
      ? `line ${comment.startLine}`
      : `lines ${comment.startLine}–${comment.endLine}`;
  return (
    <div className="q-review-note" role="note" aria-label={`Review comment on ${range}`}>
      <div className="q-review-meta">
        <MessageSquareIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        {range} · {comment.source}
      </div>
      {editing ? (
        <>
          <textarea
            value={text}
            rows={2}
            aria-label="Edit review comment"
            onChange={(event) => setText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.stopPropagation();
                setText(comment.text);
                setEditing(false);
              }
              if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                event.preventDefault();
                if (text.trim()) onUpdate(text);
                setEditing(false);
              }
            }}
          />
          <div className="q-review-actions">
            <button
              type="button"
              className="q-btn ghost"
              onClick={() => {
                setText(comment.text);
                setEditing(false);
              }}
            >
              Cancel
            </button>
            <button
              type="button"
              className="q-btn secondary"
              disabled={text.trim().length === 0}
              onClick={() => {
                onUpdate(text);
                setEditing(false);
              }}
            >
              Save
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="q-review-text">{comment.text}</p>
          <div className="q-review-actions">
            <button type="button" className="q-btn ghost" onClick={onDelete}>
              Delete
            </button>
            <button type="button" className="q-btn ghost" onClick={() => setEditing(true)}>
              Edit
            </button>
          </div>
        </>
      )}
    </div>
  );
}

export function DiffView({
  path,
  source,
  diff,
  loading,
  error,
  changes,
  files,
  filesLoading,
  filesError,
  commits,
  host,
  harness,
  threadId,
  sourceLabel,
  changeTitle,
  changeAt = null,
  onSelectFile,
  onOpenFile,
  onRevert,
  onSendComments,
  onCompareHead,
  onRefresh,
}: DiffViewProps) {
  const isPhone = useIsPhone();
  const context = source === "agent" ? 10 : 3;
  const flat = useMemo(() => computeDiff(diff?.before ?? "", diff?.after ?? ""), [diff]);
  const rows = useMemo(() => {
    let index = 0;
    return collapseDiff(flat, context).map((row, offset) => {
      const visibleIndex = row.kind === "hunk" ? null : index++;
      return { key: `${row.kind}-${offset}`, row, visibleIndex };
    });
  }, [flat, context]);
  const visible = useMemo(
    () => rows.flatMap(({ row }) => (row.kind === "hunk" ? [] : [row])),
    [rows],
  );
  const stats = useMemo(() => diffStats(flat), [flat]);
  const comments = useReviewComments(threadId);
  const commentsForPath = useMemo(
    () => comments.filter((comment) => comment.path === path),
    [comments, path],
  );
  const [selection, setSelection] = useState<LineSelection | null>(null);
  const [draft, setDraft] = useState<LineSelection | null>(null);
  const rowRefs = useRef(new Map<number, HTMLButtonElement>());
  const scopeRows = files?.files ?? [];
  const fileChange =
    source === "agent" ? (changes?.files ?? []).find((file) => file.path === path) : undefined;
  const activeFile = scopeRows.find((file) => file.path === path);
  const added = fileChange?.added ?? activeFile?.added ?? stats.added;
  const removed = fileChange?.removed ?? activeFile?.removed ?? stats.removed;

  // A new file or scope is a new set of line numbers; stale selections would
  // point at the wrong lines.
  const diffKey = `${path ?? ""}\u0000${source}\u0000${diff?.label ?? ""}\u0000${diff?.before?.length ?? 0}:${diff?.after?.length ?? 0}`;
  // biome-ignore lint/correctness/useExhaustiveDependencies: the key is the reset trigger.
  useEffect(() => {
    setSelection(null);
    setDraft(null);
  }, [diffKey]);

  const lineComments = useMemo(() => {
    const byLine = new Map<number, ReviewComment[]>();
    const hidden: ReviewComment[] = [];
    const elsewhere: ReviewComment[] = [];
    for (const comment of commentsForPath) {
      // A comment belongs to the source it was written in. Showing it under a
      // line of another source would move the note to a place it never meant.
      if (comment.source !== sourceLabel) {
        elsewhere.push(comment);
        continue;
      }
      let anchor = -1;
      for (let index = 0; index < visible.length; index += 1) {
        const line = visible[index];
        const number = comment.side === "new" ? line?.newLine : line?.oldLine;
        if (number === comment.endLine) {
          anchor = index;
          break;
        }
      }
      if (anchor < 0) {
        hidden.push(comment);
        continue;
      }
      byLine.set(anchor, [...(byLine.get(anchor) ?? []), comment]);
    }
    return { byLine, hidden, elsewhere };
  }, [commentsForPath, sourceLabel, visible]);

  const addComment = (range: LineSelection, text: string) => {
    if (!threadId || !path) return;
    const selected = commentRange(visible, range.anchor, range.focus);
    if (!selected) return;
    addReviewComment({
      threadId,
      path,
      side: selected.side,
      startLine: selected.startLine,
      endLine: selected.endLine,
      lines: selected.lines,
      text,
      source: sourceLabel,
    });
    setDraft(null);
  };

  const moveSelection = (from: number, delta: number, extend: boolean) => {
    const next = Math.max(0, Math.min(from + delta, visible.length - 1));
    if (next === from) return;
    if (extend && selection) setSelection({ anchor: selection.anchor, focus: next });
    else setSelection({ anchor: next, focus: next });
    requestAnimationFrame(() => rowRefs.current.get(next)?.focus());
  };

  const openDraft = (index: number) => {
    const range = selection ?? { anchor: index, focus: index };
    setSelection(range);
    setDraft(range);
  };

  if (loading) {
    return (
      <div className="q-center-empty" role="status" aria-label="Loading diff" style={{ flex: 1 }}>
        <div className="q-empty" style={{ padding: 0 }}>
          <p style={{ margin: 0, display: "flex", alignItems: "center", gap: 8 }}>
            <Loader2Icon className="q-icon s14 animate-spin" aria-hidden />
            Loading diff…
          </p>
        </div>
      </div>
    );
  }
  if (error) {
    return (
      <div className="q-scroll" style={{ flex: 1, padding: 16 }}>
        <div style={{ fontSize: 12, color: "var(--fg-2)" }}>
          <p
            style={{
              margin: 0,
              fontWeight: 500,
              color: "var(--fg)",
              display: "flex",
              alignItems: "center",
              gap: 6,
            }}
          >
            <TriangleAlertIcon
              className="q-icon s14"
              style={{ color: "var(--danger)" }}
              aria-hidden
            />
            The diff did not load.
          </p>
          <p style={{ margin: "4px 0 0" }}>{error.message}</p>
          {error.hint ? (
            <p style={{ margin: "4px 0 0", color: "var(--fg-3)" }}>{error.hint}</p>
          ) : null}
          {source === "branch" ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ marginTop: 12, height: 26 }}
              onClick={onCompareHead}
            >
              Compare with HEAD
            </button>
          ) : null}
        </div>
      </div>
    );
  }
  if (!diff) {
    const emptyBranch = source === "branch" && files && files.files.length === 0;
    const emptyCommit = source === "commit" && files && files.files.length === 0;
    const emptyRange = source === "commit" && commits && commits.commits.length === 0;
    if (filesLoading) {
      return (
        <div
          className="q-center-empty"
          role="status"
          aria-label="Loading changed files"
          style={{ flex: 1 }}
        >
          <div className="q-empty" style={{ padding: 0 }}>
            <p style={{ margin: 0, display: "flex", alignItems: "center", gap: 8 }}>
              <Loader2Icon className="q-icon s14 animate-spin" aria-hidden />
              Listing changed files…
            </p>
          </div>
        </div>
      );
    }
    if (filesError) {
      return (
        <div className="q-scroll" style={{ flex: 1, padding: 16, fontSize: 12 }}>
          <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>
            The file list did not load.
          </p>
          <p style={{ margin: "4px 0 0", color: "var(--fg-2)" }}>{filesError.message}</p>
          {source === "branch" ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ marginTop: 12, height: 26 }}
              onClick={onCompareHead}
            >
              Compare with HEAD
            </button>
          ) : null}
        </div>
      );
    }
    return (
      <div className="q-center-empty" style={{ flex: 1 }}>
        <div className="q-empty" style={{ padding: 0 }}>
          <FileTextIcon className="q-icon s18 q-faint" aria-hidden />
          <h3 style={{ fontSize: 13.5 }}>
            {emptyBranch
              ? `No changes against ${files?.base ?? "the base"}.`
              : emptyCommit
                ? "This commit changed no files."
                : emptyRange
                  ? `No commits beyond ${commits?.base ?? "the base"}.`
                  : "Open a file to compare it."}
          </h3>
        </div>
      </div>
    );
  }
  // A binary or oversized file says so in the diff body. The file list, the
  // source bar and the comment bar stay: another file or source is one click
  // away, and comments already written for this file remain reachable.
  const blocked = diff.too_large
    ? "This file is too large to diff. Open it in your editor or a terminal instead."
    : diff.binary
      ? "Both sides are binary, so there is no text diff to show."
      : null;

  const range = rangeLabel(visible, selection);
  const draftRange = rangeLabel(visible, draft);
  const showFileList = source === "branch" || source === "commit";

  const fileList = (
    <aside className="q-diff-files" aria-label="Changed files">
      <div className="q-diff-files-head">
        <span>{scopeRows.length} files</span>
        {files?.truncated ? <span className="q-faint">first {scopeRows.length}</span> : null}
      </div>
      {scopeRows.map((file) => {
        const status = STATUS[file.status] ?? STATUS.modified;
        const { dir, name } = splitPath(file.path);
        return (
          <button
            key={`${file.path}-${file.old_path ?? ""}`}
            type="button"
            className={cn("q-diff-file", file.path === path && "sel")}
            title={fileTitle(file)}
            aria-label={fileTitle(file)}
            aria-current={file.path === path ? "true" : undefined}
            onClick={() => onSelectFile(file.path)}
          >
            <span className={cn("badge", status.tone)} aria-hidden>
              {status.letter}
            </span>
            <span className="p">
              <span className="dir">{dir}</span>
              <span className="name">{name}</span>
              {file.old_path ? <span className="was"> ← {file.old_path}</span> : null}
            </span>
            <span className="counts">
              {file.binary ? (
                <span className="q-faint">binary</span>
              ) : file.counts_omitted ? (
                <span className="q-faint">no counts</span>
              ) : (
                <>
                  <span className="q-add">+{file.added}</span>{" "}
                  <span className="q-del">−{file.removed}</span>
                </>
              )}
            </span>
          </button>
        );
      })}
      {files?.truncated ? (
        <p className="q-diff-files-note">
          The list stops at {scopeRows.length} files. Review the rest from a terminal.
        </p>
      ) : null}
    </aside>
  );

  return (
    <section aria-label="Diff" className="q-main" style={{ flex: 1, minHeight: 0 }}>
      {isPhone && showFileList && scopeRows.length > 0 ? (
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            padding: "0 16px",
            height: 40,
            borderBottom: "1px solid var(--line)",
            fontSize: 12,
            color: "var(--fg-2)",
          }}
        >
          <FileDiffIcon className="q-icon s14 q-faint" aria-hidden />
          <ChipSelect
            label="File"
            block
            className="q-mono"
            value={path ?? ""}
            options={scopeRows.map((file) => ({
              value: file.path,
              label: file.path,
              hint: file.binary
                ? "binary"
                : file.counts_omitted
                  ? "no counts"
                  : `+${file.added} −${file.removed}`,
            }))}
            onChange={onSelectFile}
          />
        </div>
      ) : null}
      {isPhone && source === "agent" && (changes?.files.length ?? 0) > 0 ? (
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            padding: "0 16px",
            height: 40,
            borderBottom: "1px solid var(--line)",
            fontSize: 12,
            color: "var(--fg-2)",
          }}
        >
          <FileDiffIcon className="q-icon s14 q-faint" aria-hidden />
          <ChipSelect
            label="File"
            block
            className="q-mono"
            value={path ?? ""}
            options={(changes?.files ?? []).map((file) => ({
              value: file.path,
              label: file.path,
              hint: `+${file.added} −${file.removed}`,
            }))}
            onChange={onSelectFile}
          />
        </div>
      ) : null}
      {diff.note ? (
        <p
          style={{
            margin: 0,
            flex: "none",
            borderBottom: "1px solid var(--line)",
            background: "var(--accent-soft)",
            color: "var(--accent)",
            padding: "5px 16px",
            fontSize: 12,
          }}
        >
          {diff.note}
        </p>
      ) : null}
      {source === "agent" && harness && host ? (
        <div className="q-change-banner">
          <EntityAvatar
            seed={harnessSeed(host, harness)}
            kind="harness"
            label={harnessLabel(harness)}
            size={14}
          />
          <span>
            Changes by {harnessLabel(harness)} in{" "}
            <b style={{ fontWeight: 500, color: "var(--fg)" }}>{changeTitle ?? "this turn"}</b>
            {changeAt ? <> since {absoluteTime(changeAt)}</> : null}
          </span>
          <span className="grow" />
          <span className="q-num">
            <span className="q-add">+{added}</span> <span className="q-del">−{removed}</span>
          </span>
          {diff.before !== null && path ? (
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              onClick={() => onRevert(path, diff.before ?? "")}
            >
              Revert file
            </button>
          ) : null}
          {path ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 24 }}
              onClick={() => onOpenFile(path)}
            >
              Accept
            </button>
          ) : null}
        </div>
      ) : null}
      <div className="q-diff-bar">
        <span className="q-diff-scope">
          {diff.label}
          {activeFile?.old_path ? ` · was ${activeFile.old_path}` : ""}
        </span>
        <span className="q-num">
          <span className="q-add">+{added}</span> <span className="q-del">−{removed}</span>
        </span>
        <span className="grow" />
        {source === "branch" || source === "commit" ? (
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24 }}
            title="Read the branch and commit lists again"
            aria-label="Refresh the branch and commit lists"
            onClick={onRefresh}
          >
            <RefreshCwIcon className="q-icon s12" aria-hidden />
            {isPhone ? null : "Refresh"}
          </button>
        ) : null}
        {range ? (
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24 }}
            onClick={() =>
              setDraft({ anchor: selection?.anchor ?? 0, focus: selection?.focus ?? 0 })
            }
          >
            Comment on line {range.start}
            {range.end === range.start ? "" : `–${range.end}`}
          </button>
        ) : null}
        <span className="q-diff-comments">
          {comments.length} {comments.length === 1 ? "comment" : "comments"}
        </span>
        <button
          type="button"
          className="q-btn secondary"
          style={{ height: 24 }}
          disabled={comments.length === 0}
          title="Adds these comments to the thread's composer draft. Nothing sends until you press Send."
          onClick={() => onSendComments(comments)}
        >
          Send to agent
        </button>
      </div>
      <div className="q-diff-body">
        {showFileList && !isPhone ? fileList : null}
        <div className="q-hunk q-diff-scroll">
          {blocked ? (
            <div style={{ padding: 16 }}>
              <p className="q-faint" style={{ margin: 0, fontSize: 12 }}>
                {blocked}
              </p>
            </div>
          ) : (
            <section className="q-diff-lines" aria-label="Diff lines">
              {rows.map(({ key, row, visibleIndex }) => {
                if (row.kind === "hunk") {
                  return (
                    <div key={key} className="hh">
                      @@ -{row.oldStart},{row.oldCount} +{row.newStart},{row.newCount} @@
                      {row.context ? ` ${row.context}` : ""}
                    </div>
                  );
                }
                const lineIndex = visibleIndex ?? 0;
                const selected =
                  selection !== null &&
                  lineIndex >= Math.min(selection.anchor, selection.focus) &&
                  lineIndex <= Math.max(selection.anchor, selection.focus);
                const number = row.newLine ?? row.oldLine ?? "";
                return (
                  <Fragment key={key}>
                    <button
                      type="button"
                      ref={(node) => {
                        if (node) rowRefs.current.set(lineIndex, node);
                        else rowRefs.current.delete(lineIndex);
                      }}
                      aria-pressed={selected}
                      aria-label={`Line ${number}: ${row.text || "blank line"}`}
                      tabIndex={
                        selection
                          ? selection.focus === lineIndex
                            ? 0
                            : -1
                          : lineIndex === 0
                            ? 0
                            : -1
                      }
                      className={cn(
                        "q-ln",
                        row.kind === "added" && "a",
                        row.kind === "removed" && "d",
                        selected && "sel",
                      )}
                      onClick={(event) => {
                        if (event.shiftKey && selection) {
                          setSelection({ anchor: selection.anchor, focus: lineIndex });
                        } else {
                          setSelection({ anchor: lineIndex, focus: lineIndex });
                        }
                        rowRefs.current.get(lineIndex)?.focus();
                      }}
                      onFocus={() => {
                        if (!selection) setSelection({ anchor: lineIndex, focus: lineIndex });
                      }}
                      onKeyDown={(event) => {
                        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                          event.preventDefault();
                          moveSelection(
                            lineIndex,
                            event.key === "ArrowDown" ? 1 : -1,
                            event.shiftKey,
                          );
                          return;
                        }
                        if (event.key === "Enter" || event.key === "c") {
                          event.preventDefault();
                          openDraft(lineIndex);
                          return;
                        }
                        if (event.key === "Escape") {
                          setDraft(null);
                          setSelection(null);
                        }
                      }}
                    >
                      <span className="n">{row.newLine ?? row.oldLine ?? ""}</span>
                      <span className="sg" aria-hidden>
                        {row.kind === "added" ? "+" : row.kind === "removed" ? "−" : " "}
                      </span>
                      <span
                        className="q-cx"
                        style={
                          isPhone
                            ? { whiteSpace: "pre-wrap", wordBreak: "break-word", paddingRight: 16 }
                            : undefined
                        }
                      >
                        {row.text ? highlightCode(row.text) : " "}
                      </span>
                    </button>
                    {(lineComments.byLine.get(lineIndex) ?? []).map((comment) => (
                      <div key={comment.id} className="q-review-row">
                        <CommentNote
                          comment={comment}
                          onUpdate={(text) => updateReviewComment(comment.id, text)}
                          onDelete={() => deleteReviewComment(comment.id)}
                        />
                      </div>
                    ))}
                    {draft && draft.focus === lineIndex ? (
                      <div className="q-review-row">
                        <CommentEditor
                          label={
                            draftRange
                              ? `${draftRange.side === "new" ? "New" : "Old"} lines ${draftRange.start}${draftRange.end === draftRange.start ? "" : `–${draftRange.end}`} · ${sourceLabel}`
                              : sourceLabel
                          }
                          onSubmit={(text) => addComment(draft, text)}
                          onCancel={() => setDraft(null)}
                        />
                      </div>
                    ) : null}
                  </Fragment>
                );
              })}
            </section>
          )}
          {lineComments.hidden.length + lineComments.elsewhere.length > 0 ? (
            <div className="q-review-hidden">
              {lineComments.elsewhere.length > 0 ? (
                <p className="q-review-meta">
                  {lineComments.elsewhere.length}{" "}
                  {lineComments.elsewhere.length === 1 ? "comment belongs" : "comments belong"} to
                  another diff source, so{" "}
                  {lineComments.elsewhere.length === 1 ? "it is" : "they are"} listed here with the
                  source each was written in.
                </p>
              ) : null}
              {lineComments.elsewhere.map((comment) => (
                <div key={comment.id} className="q-review-row">
                  <CommentNote
                    comment={comment}
                    onUpdate={(text) => updateReviewComment(comment.id, text)}
                    onDelete={() => deleteReviewComment(comment.id)}
                  />
                </div>
              ))}
              {lineComments.hidden.length > 0 ? (
                <p className="q-review-meta">
                  {lineComments.hidden.length}{" "}
                  {lineComments.hidden.length === 1 ? "comment is" : "comments are"} on lines hidden
                  by collapsed context.
                </p>
              ) : null}
              {lineComments.hidden.map((comment) => (
                <div key={comment.id} className="q-review-row">
                  <CommentNote
                    comment={comment}
                    onUpdate={(text) => updateReviewComment(comment.id, text)}
                    onDelete={() => deleteReviewComment(comment.id)}
                  />
                </div>
              ))}
            </div>
          ) : null}
        </div>
      </div>
    </section>
  );
}
