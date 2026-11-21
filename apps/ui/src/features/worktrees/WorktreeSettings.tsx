import { RefreshCwIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import type { TrackedWorktree } from "@/gen";
import { tildePath } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import {
  type ClearConfirmation,
  captureClear,
  clearAllDescription,
  clearableWorktrees,
  clearWorktrees,
  dismissWorktreeOutcome,
  forceCanFix,
  inUseWorktrees,
  loadWorktrees,
  removeDescription,
  settledWorktrees,
  useWorktreeList,
  useWorktrees,
  worktreeClientCurrent,
  worktreeOutcomeForClient,
  worktreePage,
  worktreeSubline,
} from "./worktreeStore";

/** The branch, or the folder name when the checkout is detached. */
function worktreeTitle(row: TrackedWorktree): string {
  if (row.branch) return row.branch;
  const parts = row.path.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) ?? row.path;
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
        <div className="s">{worktreeSubline(row)}</div>
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
  // Intentionally discard the last clear's result when the page is left.
  useEffect(() => dismissWorktreeOutcome, []);

  const source = useWorktrees((state) => state.source);
  const current = worktreeClientCurrent(source);
  const storedRows = useWorktrees((state) => state.rows);
  const rows = current ? storedRows : [];
  const loaded = useWorktrees((state) => state.loaded) && current;
  const loading = useWorktrees((state) => state.loading);
  const clearing = useWorktrees((state) => state.clearing);
  const error = useWorktrees((state) => state.error);
  const outcome = useWorktrees(worktreeOutcomeForClient);
  const clearError = useWorktrees((state) => state.clearError);

  // Each confirmation keeps the machine and paths it was opened for, so a
  // refresh behind the dialog cannot change its targets.
  const [confirmAll, setConfirmAll] = useState<ClearConfirmation | null>(null);
  const [removing, setRemoving] = useState<{
    row: TrackedWorktree;
    confirmation: ClearConfirmation;
  } | null>(null);
  const [forcing, setForcing] = useState<ClearConfirmation | null>(null);

  // A rebuilt client means the open dialogs and their outcome describe
  // another machine: close them rather than let a confirm act there.
  useEffect(() => {
    if (current) return;
    setConfirmAll(null);
    setRemoving(null);
    setForcing(null);
    dismissWorktreeOutcome();
  }, [current]);

  const inUse = inUseWorktrees(rows);
  const settled = settledWorktrees(rows);
  const clearable = clearableWorktrees(rows);
  const page = worktreePage(loaded, rows, current ? error : null);
  const keptThreads = settled.length - clearable.length;
  const clearAllCount = confirmAll?.paths.length ?? 0;
  const forceCount = forcing?.paths.length ?? 0;
  const busy = clearing || !online || !current;
  const skipped = outcome?.skipped ?? [];
  // Only the skips a forced removal can actually fix are offered again: an
  // in-use, untracked or vanished entry is skipped either way.
  const forceable = skipped.filter(
    (item) => forceCanFix(item.reason) && settled.some((row) => row.path === item.path),
  );
  // A thread workspace belongs to whoever made it, so a forced removal says so.
  const forceableThreads = forceable.filter((item) =>
    settled.some((row) => row.path === item.path && row.origin === "thread"),
  );
  const threadNote =
    forceableThreads.length === 0
      ? ""
      : forceableThreads.length === 1
        ? " One of these is a thread workspace another tool or you created."
        : ` ${forceableThreads.length} of these are thread workspaces another tool or you created.`;

  return (
    <div className="q-form">
      <h1>Worktrees</h1>
      <p className="lead">
        Git worktrees h4rn created for subagents, and the ones your threads and their subagents
        worked in. Clearing removes the checkout from disk.
      </p>

      {page.kind === "groups" && inUse.length > 0 ? (
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
              disabled={loading || !online || !current}
              onClick={() => void loadWorktrees()}
            >
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Refresh
            </button>
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy || clearable.length === 0}
              onClick={() =>
                setConfirmAll(
                  captureClear(
                    clearable.map((row) => row.path),
                    true,
                  ),
                )
              }
            >
              Clear all
            </button>
          </div>
        </div>

        {page.kind === "loading" ? (
          online ? (
            <div className="q-skel" style={{ height: 120 }} />
          ) : (
            <p className="q-muted">Offline</p>
          )
        ) : (
          <>
            {page.notice ? <p className="q-muted">{page.notice}</p> : null}

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
                    onClick={() =>
                      setForcing(
                        captureClear(
                          forceable.map((item) => item.path),
                          outcome?.bulk ?? false,
                        ),
                      )
                    }
                  >
                    Remove anyway
                  </button>
                ) : null}
              </div>
            ) : null}

            {settled.length === 0 ? (
              page.empty ? (
                <p className="q-muted">
                  No worktrees yet. Subagents that run with worktree isolation, and threads working
                  in a worktree, show up here.
                </p>
              ) : rows.length > 0 ? (
                <p className="q-muted">Nothing has settled.</p>
              ) : null
            ) : (
              settled.map((row) => (
                <WorktreeRow
                  key={row.path}
                  row={row}
                  action={{
                    disabled: busy,
                    onClick: () => setRemoving({ row, confirmation: captureClear([row.path]) }),
                  }}
                />
              ))
            )}
          </>
        )}
      </div>

      <ConfirmDialog
        open={confirmAll !== null}
        title={`Remove ${clearAllCount} ${clearAllCount === 1 ? "worktree" : "worktrees"}?`}
        description={clearAllDescription(keptThreads)}
        confirmLabel="Clear all"
        onOpenChange={(open) => {
          if (!open) setConfirmAll(null);
        }}
        onConfirm={() => {
          if (confirmAll) {
            void clearWorktrees(confirmAll.paths, false, confirmAll.client, confirmAll.bulk);
          }
          setConfirmAll(null);
        }}
      />

      <ConfirmDialog
        open={removing !== null}
        title="Remove this worktree?"
        description={removing ? removeDescription(removing.row) : ""}
        confirmLabel="Remove"
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => {
          if (removing) {
            void clearWorktrees(
              removing.confirmation.paths,
              false,
              removing.confirmation.client,
              removing.confirmation.bulk,
            );
          }
          setRemoving(null);
        }}
      />

      <ConfirmDialog
        open={forcing !== null}
        title={`Remove ${forceCount} ${forceCount === 1 ? "worktree" : "worktrees"} anyway?`}
        description={`Uncommitted changes and unmerged commits in these worktrees are lost. Any checkout now at these paths is deleted. This can't be undone.${threadNote}`}
        confirmLabel="Remove anyway"
        onOpenChange={(open) => {
          if (!open) setForcing(null);
        }}
        onConfirm={() => {
          if (forcing) {
            void clearWorktrees(forcing.paths, true, forcing.client, forcing.bulk);
          }
          setForcing(null);
        }}
      />
    </div>
  );
}
