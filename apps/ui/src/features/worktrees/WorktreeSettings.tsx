import { RefreshCwIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import type { TrackedWorktree, WorktreeOrigin } from "@/gen";
import { tildePath } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import {
  clearableWorktrees,
  clearWorktrees,
  dismissWorktreeOutcome,
  inUseWorktrees,
  loadWorktrees,
  settledWorktrees,
  useWorktreeList,
  useWorktrees,
} from "./worktreeStore";

const ORIGIN_TEXT: Record<WorktreeOrigin, string> = {
  subagent: "Subagent worktree",
  thread: "Thread workspace",
  harness: "Created by an agent",
};

/** The branch, or the folder name when the checkout is detached. */
function worktreeTitle(row: TrackedWorktree): string {
  if (row.branch) return row.branch;
  const parts = row.path.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) ?? row.path;
}

/** Where it came from, who used it, and anything worth knowing before removal. */
function subline(row: TrackedWorktree): string {
  const users = [...new Set(row.accessed_by.map((access) => access.title).filter(Boolean))];
  const parts = [ORIGIN_TEXT[row.origin]];
  if (users.length > 0) parts.push(users.join(", "));
  if (row.status === "orphaned") parts.push("Left behind after a restart");
  else if (!row.exists) parts.push("Directory removed, branch kept");
  if (row.dirty) parts.push("Uncommitted changes");
  return parts.join(" · ");
}

/** A path that keeps its last two folders when the row is narrow. */
function MiddlePath({ path }: { path: string }) {
  const shown = tildePath(path);
  const cut = shown.split("/").length > 2 ? shown.lastIndexOf("/", shown.lastIndexOf("/") - 1) : -1;
  const head = cut > 0 ? shown.slice(0, cut) : shown;
  const tail = cut > 0 ? shown.slice(cut) : "";
  return (
    <span
      className="q-mono q-muted"
      title={path}
      style={{ display: "flex", minWidth: 0, flex: "0 1 280px", fontSize: 12 }}
    >
      <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
        {head}
      </span>
      <span style={{ flex: "none", whiteSpace: "nowrap" }}>{tail}</span>
    </span>
  );
}

function WorktreeRow({
  row,
  action,
}: {
  row: TrackedWorktree;
  action?: { disabled: boolean; onClick: () => void };
}) {
  const title = worktreeTitle(row);
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t q-mono" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
          {title}
        </div>
        <div className="s">{subline(row)}</div>
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <MiddlePath path={row.path} />
        {action ? (
          <button
            type="button"
            className="q-btn secondary"
            disabled={action.disabled}
            aria-label={`Remove ${title}`}
            onClick={action.onClick}
          >
            Remove
          </button>
        ) : null}
      </div>
    </div>
  );
}

/** Settings → Worktrees: what h4rn tracks, what is in use, and a way to clear the rest. */
export function WorktreeSettings() {
  const online = useApp((state) => state.connection.status) === "online";
  useWorktreeList(online);
  useEffect(() => dismissWorktreeOutcome, []);

  const rows = useWorktrees((state) => state.rows);
  const loaded = useWorktrees((state) => state.loaded);
  const loading = useWorktrees((state) => state.loading);
  const clearing = useWorktrees((state) => state.clearing);
  const error = useWorktrees((state) => state.error);
  const outcome = useWorktrees((state) => state.outcome);
  const clearError = useWorktrees((state) => state.clearError);

  const [confirmAll, setConfirmAll] = useState(false);
  const [removing, setRemoving] = useState<TrackedWorktree | null>(null);
  const [forcing, setForcing] = useState(false);

  const inUse = inUseWorktrees(rows);
  const settled = settledWorktrees(rows);
  const clearable = clearableWorktrees(rows);
  const keptThreads = settled.length - clearable.length;
  const busy = clearing || !online;
  const skipped = outcome?.skipped ?? [];
  // Force only helps with dirty or unmerged work; an in-use or untracked
  // worktree is skipped either way, so it is not offered again.
  const forceable = skipped.filter((item) => settled.some((row) => row.path === item.path));

  return (
    <div className="q-form">
      <h1>Worktrees</h1>
      <p className="lead">
        Git worktrees h4rn created for subagents, and the ones your threads and their subagents
        worked in. Clearing removes the checkout from disk.
      </p>

      {!loaded ? (
        <div className="q-skel" style={{ height: 120 }} />
      ) : error ? (
        <div className="q-group">
          <p className="q-muted">{error.message}</p>
        </div>
      ) : rows.length === 0 && !outcome ? (
        <p className="q-muted">
          No worktrees yet. Subagents that run with worktree isolation, and threads working in a
          worktree, show up here.
        </p>
      ) : (
        <>
          {inUse.length > 0 ? (
            <div className="q-group">
              <div className="q-group-head">
                <h2>In use</h2>
              </div>
              {inUse.map((row) => (
                <WorktreeRow key={row.path} row={row} />
              ))}
            </div>
          ) : null}

          <div className="q-group">
            <div className="q-group-head">
              <h2>Settled</h2>
              <div style={{ display: "flex", gap: 8 }}>
                <button
                  type="button"
                  className="q-btn ghost"
                  disabled={loading || !online}
                  onClick={() => void loadWorktrees()}
                >
                  <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  Refresh
                </button>
                <button
                  type="button"
                  className="q-btn secondary"
                  disabled={busy || clearable.length === 0}
                  onClick={() => setConfirmAll(true)}
                >
                  Clear all
                </button>
              </div>
            </div>

            {outcome || clearError ? (
              <div className="q-harness-out" role="status" aria-live="polite">
                {outcome ? <p className="res">Removed {outcome.removed}.</p> : null}
                {clearError ? <p className="res bad">{clearError}</p> : null}
                {skipped.map((item) => (
                  <p className="res" key={item.path}>
                    <span className="q-mono" title={item.path}>
                      {tildePath(item.path)}
                    </span>{" "}
                    · {item.reason}
                  </p>
                ))}
                {forceable.length > 0 ? (
                  <button
                    type="button"
                    className="q-btn secondary"
                    style={{ marginTop: 8 }}
                    disabled={busy}
                    onClick={() => setForcing(true)}
                  >
                    Remove anyway
                  </button>
                ) : null}
              </div>
            ) : null}

            {settled.length === 0 ? (
              <p className="q-muted">Nothing has settled.</p>
            ) : (
              settled.map((row) => (
                <WorktreeRow
                  key={row.path}
                  row={row}
                  action={{ disabled: busy, onClick: () => setRemoving(row) }}
                />
              ))
            )}
          </div>
        </>
      )}

      <ConfirmDialog
        open={confirmAll}
        title={`Remove ${clearable.length} ${clearable.length === 1 ? "worktree" : "worktrees"}?`}
        description={`Checkouts are deleted from disk and h4rn/sa-* branches h4rn created are deleted. Worktrees with uncommitted changes or unmerged work are skipped.${
          keptThreads > 0
            ? ` ${keptThreads === 1 ? "One thread workspace is" : `${keptThreads} thread workspaces are`} kept; remove ${keptThreads === 1 ? "it" : "them"} one at a time.`
            : ""
        }`}
        confirmLabel="Clear all"
        onOpenChange={setConfirmAll}
        onConfirm={() => void clearWorktrees(null)}
      />

      <ConfirmDialog
        open={removing !== null}
        title="Remove this worktree?"
        description={
          removing
            ? `The checkout at ${removing.path} is deleted from disk.${
                removing.owned && removing.branch
                  ? ` The ${removing.branch} branch h4rn created is deleted too.`
                  : ""
              } Uncommitted changes or unmerged work are skipped.`
            : ""
        }
        confirmLabel="Remove"
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => {
          if (removing) void clearWorktrees([removing.path]);
          setRemoving(null);
        }}
      />

      <ConfirmDialog
        open={forcing}
        title={`Remove ${forceable.length} ${forceable.length === 1 ? "worktree" : "worktrees"} anyway?`}
        description="Uncommitted changes and unmerged commits in these worktrees are lost. This can't be undone."
        confirmLabel="Remove anyway"
        onOpenChange={setForcing}
        onConfirm={() =>
          void clearWorktrees(
            forceable.map((item) => item.path),
            true,
          )
        }
      />
    </div>
  );
}
