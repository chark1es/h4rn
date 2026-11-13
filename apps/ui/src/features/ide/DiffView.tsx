import { FileDiffIcon, FileTextIcon, Loader2Icon, TriangleAlertIcon } from "lucide-react";
import { type ReactNode, useMemo } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import type { FsDiff, FsTurnChanges, HarnessKind, HostId, ProtoError } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { collapseDiff, computeDiff, diffStats } from "./diff";

type DiffViewProps = {
  path: string | null;
  mode: "head" | "agent";
  diff: FsDiff | null;
  loading: boolean;
  error: ProtoError | null;
  changes: FsTurnChanges | null;
  host: HostId | null;
  harness: HarnessKind | null;
  changeTitle: string | null;
  changeAt?: string | null;
  onSelectFile: (path: string) => void;
  onOpenFile: (path: string) => void;
  onRevert: (path: string, content: string) => void;
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

export function DiffView({
  path,
  mode,
  diff,
  loading,
  error,
  changes,
  host,
  harness,
  changeTitle,
  changeAt = null,
  onSelectFile,
  onOpenFile,
  onRevert,
}: DiffViewProps) {
  const isPhone = useIsPhone();
  const rows = useMemo(() => {
    if (!diff) return [];
    const context = mode === "agent" ? 10 : 3;
    return collapseDiff(computeDiff(diff.before ?? "", diff.after ?? ""), context).map(
      (row, offset) => ({ key: `${row.kind}-${offset}`, row }),
    );
  }, [diff, mode]);
  const stats = useMemo(
    () => diffStats(computeDiff(diff?.before ?? "", diff?.after ?? "")),
    [diff],
  );
  const files = changes?.files ?? [];
  const fileChange = files.find((file) => file.path === path);
  const added = fileChange?.added ?? stats.added;
  const removed = fileChange?.removed ?? stats.removed;

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
        </div>
      </div>
    );
  }
  if (!diff) {
    return (
      <div className="q-center-empty" style={{ flex: 1 }}>
        <div className="q-empty" style={{ padding: 0 }}>
          <FileTextIcon className="q-icon s18 q-faint" aria-hidden />
          <h3 style={{ fontSize: 13.5 }}>Open a file to compare it.</h3>
        </div>
      </div>
    );
  }
  if (diff.binary) {
    return (
      <div className="q-scroll" style={{ flex: 1, padding: 16 }}>
        <p className="q-faint" style={{ margin: 0, fontSize: 12 }}>
          Both sides are binary, so there is no text diff to show.
        </p>
      </div>
    );
  }

  return (
    <section aria-label="Diff" className="q-main" style={{ flex: 1, minHeight: 0 }}>
      {isPhone && mode === "agent" && files.length > 0 ? (
        <label
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
          <select
            value={path ?? ""}
            onChange={(event) => onSelectFile(event.target.value)}
            className="q-mono"
            style={{
              flex: 1,
              minWidth: 0,
              height: 26,
              border: "1px solid var(--control-line)",
              borderRadius: 6,
              background: "var(--bg-raised)",
              color: "var(--fg)",
              fontSize: 11,
              padding: "0 6px",
            }}
          >
            {files.map((file) => (
              <option key={file.path} value={file.path}>
                {file.path} (+{file.added} −{file.removed})
              </option>
            ))}
          </select>
        </label>
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
      {mode === "agent" && harness && host ? (
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
      <div
        className="q-hunk"
        style={{
          flex: 1,
          minHeight: 0,
          borderTop: 0,
          font: "12.5px/20px var(--mono)",
          background: "var(--bg-sunk)",
          padding: "8px 0",
        }}
      >
        {rows.map(({ key, row }) =>
          row.kind === "hunk" ? null : (
            <div
              key={key}
              className={cn("q-ln", row.kind === "added" && "a", row.kind === "removed" && "d")}
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
            </div>
          ),
        )}
      </div>
    </section>
  );
}
