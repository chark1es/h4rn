import {
  FileDiffIcon,
  FileTextIcon,
  Loader2Icon,
  MessageSquareIcon,
  RefreshCwIcon,
  TriangleAlertIcon,
} from "lucide-react";
import { Fragment, type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Tip } from "@/components/ui/tooltip";
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
import { formatNumber, i18n } from "@/lib/i18n";
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
import { type DiffSource, describeSource, diffScopeKey, resolvedAgainst } from "./sources";

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
  /**
   * The daemon's label for the diff on screen ("vs main", "Commit 1a2b3c4"),
   * stored with a new comment so it can name its diff later.
   */
  scopeLabel: string | null;
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

const STATUS: Record<GitFileStatus, { letter: string; tone: string }> = {
  added: { letter: "A", tone: "q-add" },
  modified: { letter: "M", tone: "q-warn" },
  deleted: { letter: "D", tone: "q-del" },
  renamed: { letter: "R", tone: "q-warn" },
  untracked: { letter: "U", tone: "q-faint" },
  conflicted: { letter: "!", tone: "q-del" },
  type_changed: { letter: "T", tone: "q-warn" },
};

/** `+18 −4`, with each count written the active language's way. */
function counts(added: number, removed: number): string {
  return `+${formatNumber(added)} −${formatNumber(removed)}`;
}

/** A line number in prose ("Comment on line 1204"), without grouping. */
function lineNumber(value: number): string {
  return formatNumber(value, { useGrouping: false });
}

function splitPath(path: string): { dir: string; name: string } {
  const slash = path.lastIndexOf("/");
  return slash < 0
    ? { dir: "", name: path }
    : { dir: path.slice(0, slash + 1), name: path.slice(slash + 1) };
}

function fileTitle(file: FsDiffFile): string {
  const known = file.status in STATUS;
  const word = known ? i18n.t(`ide:files.status.${file.status}`) : file.status;
  const status = file.old_path
    ? i18n.t("ide:files.statusWas", { status: word, path: file.old_path })
    : word;
  const note = file.binary
    ? i18n.t("ide:files.binary")
    : file.counts_omitted
      ? i18n.t("ide:files.noCountsLong")
      : null;
  return [file.path, status, note].filter(Boolean).join(" · ");
}

type LineSelection = { anchor: number; focus: number };

/** The new comment's heading: "New lines 3–5 · branch vs main". */
function draftLabel(
  range: { start: number; end: number; side: string } | null,
  source: string,
): string {
  if (!range) return source;
  const lines =
    range.end === range.start
      ? lineNumber(range.start)
      : `${lineNumber(range.start)}–${lineNumber(range.end)}`;
  return range.side === "new"
    ? i18n.t("ide:comments.newLines", { lines, source })
    : i18n.t("ide:comments.oldLines", { lines, source });
}

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
  const { t } = useTranslation(["ide", "common"]);
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
      aria-label={t("comments.newOn", { label })}
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
        aria-label={t("comments.field")}
        placeholder={t("comments.placeholder")}
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
          {t("common:cancel")}
        </button>
        <button
          type="button"
          className="q-btn secondary"
          disabled={text.trim().length === 0}
          onClick={submit}
        >
          {t("comments.add")}
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
  const { t } = useTranslation(["ide", "common"]);
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(comment.text);
  const range =
    comment.startLine === comment.endLine
      ? t("comments.line", { line: lineNumber(comment.startLine) })
      : t("comments.range", {
          start: lineNumber(comment.startLine),
          end: lineNumber(comment.endLine),
        });
  return (
    <div className="q-review-note" role="note" aria-label={t("comments.on", { range })}>
      <div className="q-review-meta">
        <MessageSquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        {range} · {describeSource(comment.source, comment.sourceLabel)}
      </div>
      {editing ? (
        <>
          <textarea
            value={text}
            rows={2}
            aria-label={t("comments.editField")}
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
              {t("common:cancel")}
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
              {t("common:save")}
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="q-review-text">{comment.text}</p>
          <div className="q-review-actions">
            <button type="button" className="q-btn ghost" onClick={onDelete}>
              {t("common:delete")}
            </button>
            <button type="button" className="q-btn ghost" onClick={() => setEditing(true)}>
              {t("common:edit")}
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
  scopeLabel,
  changeTitle,
  changeAt = null,
  onSelectFile,
  onOpenFile,
  onRevert,
  onSendComments,
  onCompareHead,
  onRefresh,
}: DiffViewProps) {
  const { t } = useTranslation(["ide", "common"]);
  const isPhone = useIsPhone();
  const context = source === "agent" ? 10 : 3;
  const sourceText = describeSource(source, scopeLabel);
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
  // The identity of the diff on screen. The label above it is text a person
  // reads, so comments are matched on the request instead. A branch request
  // for the default base is keyed by the branch that default resolved to, so
  // picking that branch by name is the same comparison.
  const scopeKey = diffScopeKey(resolvedAgainst(diff?.against ?? null, files?.base ?? null));
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
      // A comment belongs to the diff it was written on. Showing it under a
      // line of another scope would move the note to a place it never meant,
      // even when the scope's display label happens to read the same.
      if (comment.scope !== scopeKey) {
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
  }, [commentsForPath, scopeKey, visible]);

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
      scope: scopeKey,
      source,
      sourceLabel: scopeLabel,
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
      <div
        className="q-center-empty"
        role="status"
        aria-label={t("diff.loading")}
        style={{ flex: 1 }}
      >
        <div className="q-empty" style={{ padding: 0 }}>
          <p style={{ margin: 0, display: "flex", alignItems: "center", gap: 8 }}>
            <Loader2Icon className="q-icon s14 animate-spin" aria-hidden />
            {t("diff.loadingText")}
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
            {t("diff.failed")}
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
              {t("diff.compareHead")}
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
          aria-label={t("diff.listing")}
          style={{ flex: 1 }}
        >
          <div className="q-empty" style={{ padding: 0 }}>
            <p style={{ margin: 0, display: "flex", alignItems: "center", gap: 8 }}>
              <Loader2Icon className="q-icon s14 animate-spin" aria-hidden />
              {t("diff.listingText")}
            </p>
          </div>
        </div>
      );
    }
    if (filesError) {
      return (
        <div className="q-scroll" style={{ flex: 1, padding: 16, fontSize: 12 }}>
          <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>{t("diff.listFailed")}</p>
          <p style={{ margin: "4px 0 0", color: "var(--fg-2)" }}>{filesError.message}</p>
          {source === "branch" ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ marginTop: 12, height: 26 }}
              onClick={onCompareHead}
            >
              {t("diff.compareHead")}
            </button>
          ) : null}
        </div>
      );
    }
    return (
      <div className="q-center-empty" style={{ flex: 1 }}>
        <div className="q-empty" style={{ padding: 0 }}>
          <FileTextIcon className="q-icon s18 q-faint" aria-hidden />
          <h3 style={{ fontSize: 14 }}>
            {emptyBranch
              ? files?.base
                ? t("diff.noBranchChanges", { base: files.base })
                : t("diff.noBranchChangesDefault")
              : emptyCommit
                ? t("diff.emptyCommit")
                : emptyRange
                  ? commits?.base
                    ? t("diff.noCommits", { base: commits.base })
                    : t("diff.noCommitsDefault")
                  : t("diff.openFile")}
          </h3>
        </div>
      </div>
    );
  }
  // A binary or oversized file says so in the diff body. The file list, the
  // source bar and the comment bar stay: another file or source is one click
  // away, and comments already written for this file remain reachable.
  const blocked = diff.too_large ? t("diff.tooLarge") : diff.binary ? t("diff.binary") : null;

  const range = rangeLabel(visible, selection);
  const draftRange = rangeLabel(visible, draft);
  const showFileList = source === "branch" || source === "commit";

  const fileList = (
    <aside className="q-diff-files" aria-label={t("files.label")}>
      <div className="q-diff-files-head">
        <span>{t("files.count", { count: scopeRows.length })}</span>
        {files?.truncated ? (
          <span className="q-faint">{t("files.first", { count: scopeRows.length })}</span>
        ) : null}
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
                <span className="q-faint">{t("files.binary")}</span>
              ) : file.counts_omitted ? (
                <span className="q-faint">{t("files.noCounts")}</span>
              ) : (
                <>
                  <span className="q-add">+{formatNumber(file.added)}</span>{" "}
                  <span className="q-del">−{formatNumber(file.removed)}</span>
                </>
              )}
            </span>
          </button>
        );
      })}
      {files?.truncated ? (
        <p className="q-diff-files-note">{t("files.truncated", { count: scopeRows.length })}</p>
      ) : null}
    </aside>
  );

  return (
    <section aria-label={t("diff.label")} className="q-main" style={{ flex: 1, minHeight: 0 }}>
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
            label={t("diff.file")}
            block
            className="q-mono"
            value={path ?? ""}
            options={scopeRows.map((file) => ({
              value: file.path,
              label: file.path,
              hint: file.binary
                ? t("files.binary")
                : file.counts_omitted
                  ? t("files.noCounts")
                  : counts(file.added, file.removed),
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
            label={t("diff.file")}
            block
            className="q-mono"
            value={path ?? ""}
            options={(changes?.files ?? []).map((file) => ({
              value: file.path,
              label: file.path,
              hint: counts(file.added, file.removed),
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
            <Trans
              t={t}
              i18nKey={changeAt ? "diff.bannerSince" : "diff.banner"}
              values={{
                harness: harnessLabel(harness),
                title: changeTitle ?? t("diff.thisTurn"),
                ...(changeAt ? { time: absoluteTime(changeAt) } : {}),
              }}
              components={{ b: <b style={{ fontWeight: 500, color: "var(--fg)" }} /> }}
            />
          </span>
          <span className="grow" />
          <span className="q-num">
            <span className="q-add">+{formatNumber(added)}</span>{" "}
            <span className="q-del">−{formatNumber(removed)}</span>
          </span>
          {diff.before !== null && path ? (
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              onClick={() => onRevert(path, diff.before ?? "")}
            >
              {t("diff.revertFile")}
            </button>
          ) : null}
          {path ? (
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 24 }}
              onClick={() => onOpenFile(path)}
            >
              {t("diff.accept")}
            </button>
          ) : null}
        </div>
      ) : null}
      <div className="q-diff-bar">
        <span className="q-diff-scope">
          {diff.label}
          {activeFile?.old_path ? ` · ${t("diff.was", { path: activeFile.old_path })}` : ""}
        </span>
        {source === "commit" && commits?.base ? (
          // Commit mode keeps the base picked for Branch (or the repository
          // default): the commit list is everything since that base. Say which
          // one, so the range is never implicit.
          <span className="q-faint" title={t("diff.sinceHint", { base: commits.base })}>
            {t("diff.since", { base: commits.base })}
          </span>
        ) : null}
        <span className="q-num">
          <span className="q-add">+{formatNumber(added)}</span>{" "}
          <span className="q-del">−{formatNumber(removed)}</span>
        </span>
        <span className="grow" />
        {source === "branch" || source === "commit" ? (
          <Tip label={t("diff.refreshHint")}>
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              aria-label={t("diff.refreshLabel")}
              onClick={onRefresh}
            >
              <RefreshCwIcon className="q-icon s14" aria-hidden />
              {isPhone ? null : t("common:refresh")}
            </button>
          </Tip>
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
            {range.end === range.start
              ? t("comments.commentOn", { line: lineNumber(range.start) })
              : t("comments.commentOnRange", {
                  start: lineNumber(range.start),
                  end: lineNumber(range.end),
                })}
          </button>
        ) : null}
        <span className="q-diff-comments">{t("comments.count", { count: comments.length })}</span>
        <button
          type="button"
          className="q-btn secondary"
          style={{ height: 24 }}
          disabled={comments.length === 0}
          title={t("comments.sendHint")}
          onClick={() => onSendComments(comments)}
        >
          {t("comments.send")}
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
            <section className="q-diff-lines" aria-label={t("diff.lines")}>
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
                      aria-label={
                        row.text
                          ? t("diff.line", { number, text: row.text })
                          : t("diff.blankLine", { number })
                      }
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
                          label={draftLabel(draftRange, sourceText)}
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
                  {t("comments.elsewhere", { count: lineComments.elsewhere.length })}
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
                  {t("comments.hidden", { count: lineComments.hidden.length })}
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
