import { Code2Icon, FileDigitIcon } from "lucide-react";
import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { highlightCode } from "@/features/ide/DiffView";
import { collapseDiff, computeDiff, type DiffRow } from "@/features/ide/diff";
import type { FsDiff } from "@/gen";
import { cn } from "@/lib/utils";

function firstHunk(rows: DiffRow[]): DiffRow[] {
  const second = rows.findIndex((row, index) => index > 0 && row.kind === "hunk");
  return second < 0 ? rows : rows.slice(0, second);
}

/** One file in the right panel's diff stream. */
export function FileDiff({
  path,
  diff,
  added,
  removed,
  loading,
  onOpen,
  compact = false,
  bare = false,
}: {
  path: string;
  diff: FsDiff | null;
  added: number;
  removed: number;
  loading: boolean;
  onOpen: () => void;
  /** The phone sheet shows the first hunk; the full stream stays on desktop. */
  compact?: boolean;
  /** The phone sheet labels the file itself, so skip the sticky header. */
  bare?: boolean;
}) {
  const { t } = useTranslation("thread");
  const rows = useMemo(() => {
    if (!diff) return [];
    const all = collapseDiff(computeDiff(diff.before ?? "", diff.after ?? ""), 4);
    // The sheet previews the first hunk, cut to a few lines so the sheet's
    // footer note stays above the fold.
    return (compact ? firstHunk(all).slice(0, 5) : all).map((row, index) => ({
      key: `${row.kind}-${index}`,
      row,
    }));
  }, [diff, compact]);
  const split = path.lastIndexOf("/");
  const directory = split >= 0 ? path.slice(0, split + 1) : "";
  const name = split >= 0 ? path.slice(split + 1) : path;

  return (
    <div className="q-dfile">
      {bare ? null : (
        <div className="q-dfile-h">
          <FileDigitIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <span className="p q-mono">
            <span className="q-faint">{directory}</span>
            {name}
          </span>
          <span className="grow" />
          <span className="q-num q-pm">
            <span className="q-add">+{added}</span> <span className="q-del">−{removed}</span>
          </span>
          <button
            type="button"
            className="q-ibtn"
            aria-label={t("changes.diff.open", { name })}
            title={t("changes.openInIde")}
            onClick={onOpen}
          >
            <Code2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
      )}
      {loading && !diff ? (
        <div className="q-hunk">
          <div className="hh">{t("changes.diff.loading")}</div>
        </div>
      ) : diff && !diff.binary && rows.length > 0 ? (
        <div
          className="q-hunk"
          style={{ font: "12px/18px var(--mono)", background: "var(--bg-sunk)" }}
        >
          {rows.map(({ key, row }) =>
            row.kind === "hunk" ? (
              <div key={key} className="hh">
                {`@@ -${row.oldStart},${row.oldCount} +${row.newStart},${row.newCount} @@`}
                {row.context ? ` ${row.context}` : ""}
              </div>
            ) : (
              <div
                key={key}
                className={cn("q-ln", row.kind === "added" && "a", row.kind === "removed" && "d")}
              >
                <span className="n">{row.newLine ?? row.oldLine ?? ""}</span>
                <span className="sg" aria-hidden>
                  {row.kind === "added" ? "+" : row.kind === "removed" ? "−" : " "}
                </span>
                <span className="q-cx">{row.text ? highlightCode(row.text) : " "}</span>
              </div>
            ),
          )}
        </div>
      ) : (
        <div className="q-hunk">
          <div className="hh">
            {diff?.note ?? t("changes.diff.collapsed", { count: added + removed })}
          </div>
        </div>
      )}
    </div>
  );
}
