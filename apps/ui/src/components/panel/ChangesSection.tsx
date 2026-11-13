import { ChevronDownIcon, Code2Icon, FileIcon } from "lucide-react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MOD } from "@/components/common/Kbd";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { ThreadSummary } from "@/gen";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { openInIde, setView } from "@/lib/store/appStore";
import { FileDiff } from "./FileDiff";
import type { PanelChanges } from "./usePanelChanges";

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/** The Changes tab, shared by the right panel and the phone sheet's body. */
export function ChangesSection({
  thread,
  changes,
  ide,
  onFooterAction,
}: {
  thread: ThreadSummary;
  changes: PanelChanges;
  ide: boolean;
  onFooterAction?: () => void;
}) {
  const { mode, setMode, turns, turnId, changes: turnChanges, diffs, loading, error } = changes;
  const files = turnChanges?.files ?? [];
  const turnLabel = turns.length > 0 ? pickedLabel(turns, turnId) : "All turns";

  return (
    <>
      <div className="q-rp-bar">
        <div className="q-seg inline" role="tablist" aria-label="Compare">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "agent"}
            onClick={() => setMode("agent")}
          >
            vs agent
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "head"}
            onClick={() => setMode("head")}
          >
            vs HEAD
          </button>
        </div>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button type="button" className="q-btn ghost" style={{ height: 26 }}>
              {turnLabel} <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-44">
            <DropdownMenuLabel>Compare</DropdownMenuLabel>
            <DropdownMenuItem onSelect={() => changes.selectTurn(null)}>
              All {turns.length || 1} turns
            </DropdownMenuItem>
            {turns.map((turn) => (
              <DropdownMenuItem key={turn.id} onSelect={() => changes.selectTurn(turn.id)}>
                {turn.label}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
        <span className="grow" />
        <span className="q-num q-pm">
          <span className="q-add">+{changes.added}</span>{" "}
          <span className="q-del">−{changes.removed}</span>
        </span>
      </div>

      <div className="q-rp-body q-scroll">
        {error ? (
          <div className="q-sub-empty">
            <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>
              The change list did not load.
            </p>
            <p style={{ margin: "4px 0 10px" }}>{error}</p>
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={changes.reload}
            >
              Retry
            </button>
          </div>
        ) : loading && !turnChanges ? (
          <div className="q-sub-empty" role="status">
            Loading changes…
          </div>
        ) : files.length === 0 ? (
          <div className="q-empty q-rp-empty">
            <h3>No changes yet</h3>
            <p>
              Files {harnessLabel(thread.harness)} edits in this thread show up here, grouped by
              turn, with a diff against HEAD one click away.
            </p>
          </div>
        ) : (
          <>
            <div className="q-rp-by">
              <EntityAvatar
                seed={harnessSeed(thread.host, thread.harness)}
                kind="harness"
                label={harnessLabel(thread.harness)}
                size={14}
              />
              <span>
                {harnessLabel(thread.harness)} on {thread.host} · {files.length}{" "}
                {files.length === 1 ? "file" : "files"} since {absoluteTime(thread.last_activity)}
              </span>
            </div>
            <div className="q-flist">
              {files.map((file) => {
                const { directory, name } = splitPath(file.path);
                return (
                  <button
                    key={file.path}
                    type="button"
                    className="q-fr"
                    onClick={() => openInIde(file.path)}
                  >
                    <FileIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                    <span className="p q-mono">
                      <span>{directory}</span>
                      {name}
                    </span>
                    {file.before_etag === null ? (
                      <span className="q-badge ok" style={{ height: 18 }}>
                        New
                      </span>
                    ) : null}
                    <span className="q-num q-pm">
                      <span className="q-add">+{file.added}</span>
                      {file.removed > 0 ? <span className="q-del"> −{file.removed}</span> : null}
                    </span>
                  </button>
                );
              })}
            </div>
            {ide ? (
              <p className="q-rp-note">
                Files open in the editor, compared with the agent’s starting point. Pick a turn
                above to narrow it.
              </p>
            ) : (
              files.map((file) => (
                <FileDiff
                  key={file.path}
                  path={file.path}
                  diff={diffs[file.path] ?? null}
                  added={file.added}
                  removed={file.removed}
                  loading={changes.diffsLoading}
                  onOpen={() => openInIde(file.path)}
                />
              ))
            )}
          </>
        )}
      </div>

      <div className="q-rp-foot">
        <button
          type="button"
          className="q-btn ghost"
          disabled={files.length === 0}
          onClick={() => void changes.revertAll()}
        >
          Revert all…
        </button>
        <span className="grow" />
        {ide ? (
          <button type="button" className="q-btn secondary" onClick={() => setView("thread")}>
            Back to thread <span className="q-kbd">{MOD}E</span>
          </button>
        ) : (
          <button
            type="button"
            className="q-btn secondary"
            onClick={onFooterAction ?? (() => setView("ide"))}
          >
            <Code2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Open in IDE <span className="q-kbd">{MOD}E</span>
          </button>
        )}
      </div>
    </>
  );
}

function pickedLabel(turns: Array<{ id: string; label: string }>, turnId: string | null): string {
  if (turns.length <= 1) return "All turns";
  const picked = turns.find((turn) => turn.id === turnId);
  if (!picked || picked.id === turns.at(-1)?.id) return `All ${turns.length} turns`;
  return picked.label;
}
