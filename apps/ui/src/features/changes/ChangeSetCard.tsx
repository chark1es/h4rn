import { FileIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { FileDiff } from "@/components/panel/FileDiff";
import { againstFor, loadFileDiff, requestChangesTurn, totalLines } from "@/features/changes/store";
import type { FsDiff, ThreadSummary } from "@/gen";
import { getClient, openInIde } from "@/lib/store/appStore";
import type { StreamTurn } from "@/lib/store/projection";

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/**
 * The one bordered block in the stream.
 *
 * The daemon attaches the per-turn change set to `TurnCompleted`; this card
 * only fetches the first file's diff so the first hunk can sit inline without
 * loading every file's patch into the stream.
 */
export function ChangeSetCard({ turn, summary }: { turn: StreamTurn; summary: ThreadSummary }) {
  const changes = turn.changes;
  const first = changes?.files[0];
  const [diff, setDiff] = useState<FsDiff | null>(null);

  useEffect(() => {
    if (!first || !changes) return;
    let cancelled = false;
    const against = againstFor("agent", summary, changes.turn_id);
    loadFileDiff((request) => getClient().request(request), summary, first.path, against)
      .then((next) => {
        if (!cancelled) setDiff(next);
      })
      .catch(() => {
        if (!cancelled) setDiff(null);
      });
    return () => {
      cancelled = true;
    };
  }, [changes, first, summary]);

  if (!changes || changes.files.length === 0) return null;
  const totals = totalLines(changes.files);
  const openChanges = () => {
    window.dispatchEvent(
      new CustomEvent("h4rn:open-panel", { detail: { tab: "changes", turnId: changes.turn_id } }),
    );
    requestChangesTurn(changes.turn_id);
  };

  return (
    <div className="q-changes">
      <div className="q-ch-head">
        <span className="t">
          {changes.files.length} {changes.files.length === 1 ? "file" : "files"} changed
        </span>
        <span className="q-num q-pm">
          <span className="q-add">+{totals.added}</span>{" "}
          <span className="q-del">−{totals.removed}</span>
        </span>
        <button
          type="button"
          className="q-btn ghost"
          onClick={openChanges}
          title="Open in the Changes panel  D"
        >
          Review
        </button>
      </div>
      {changes.files.map((file) => {
        const { directory, name } = splitPath(file.path);
        return (
          <div className="q-ch-file" key={file.path}>
            <FileIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
            <span className="p q-mono">
              <span>{directory}</span>
              {name}
            </span>
            <span className="q-num q-pm">
              <span className="q-add">+{file.added}</span>
              {file.removed > 0 ? <span className="q-del"> −{file.removed}</span> : null}
            </span>
          </div>
        );
      })}
      {first ? (
        <FileDiff
          path={first.path}
          diff={diff}
          added={first.added}
          removed={first.removed}
          loading={diff === null}
          compact
          bare
          onOpen={() => openInIde(first.path)}
        />
      ) : null}
      {changes.note ? (
        <p className="q-rp-note" style={{ margin: "8px 14px 10px" }}>
          {changes.note}
        </p>
      ) : null}
    </div>
  );
}
