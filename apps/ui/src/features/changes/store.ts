import type {
  ClientRequest,
  DiffAgainst,
  FsDiff,
  FsTurnChanges,
  FsTurnFile,
  ResponseBody,
  ThreadId,
  ThreadSummary,
  TurnId,
} from "@/gen";

/**
 * The Changes tab's data layer.
 *
 * Every call goes through the daemon: the UI never reads or writes the
 * workspace directly. Keeping the requests here, behind one injected request
 * function, lets the store be tested without a socket and keeps optimistic
 * revert behaviour in one place.
 */
export type ChangesRequest = (request: ClientRequest) => Promise<ResponseBody>;

/**
 * Window event used by the stream's change-set card to ask the right panel to
 * open one turn. A DOM event keeps the stream and panel independent.
 */
export const CHANGES_SELECT_EVENT = "h4rn:changes-turn";

export function requestChangesTurn(turnId: TurnId | null): void {
  window.dispatchEvent(new CustomEvent(CHANGES_SELECT_EVENT, { detail: { turnId } }));
}

export type RevertOutcome = "reverted" | "deleted" | "skipped" | "failed";

export type FileRevert = {
  path: string;
  outcome: RevertOutcome;
  message?: string;
};

export function totalLines(files: FsTurnFile[]): { added: number; removed: number } {
  return files.reduce(
    (totals, file) => ({
      added: totals.added + file.added,
      removed: totals.removed + file.removed,
    }),
    { added: 0, removed: 0 },
  );
}

/** The latest logged turn, which is what the panel shows by default. */
export function latestTurn(turns: Array<{ id: TurnId; label: string }>): TurnId | null {
  return turns.at(-1)?.id ?? null;
}

export function againstFor(
  mode: "agent" | "head",
  thread: ThreadSummary,
  turnId: TurnId | null,
): DiffAgainst {
  return mode === "head"
    ? { kind: "head" }
    : {
        kind: "agent_turn",
        thread_id: thread.thread_id as ThreadId,
        turn_id: (turnId ?? "") as TurnId,
      };
}

export async function loadTurnChanges(
  request: ChangesRequest,
  thread: ThreadSummary,
  turnId: TurnId,
): Promise<FsTurnChanges> {
  const response = await request({
    type: "fs_turn_changes",
    host: thread.host,
    workspace: thread.workspace_path,
    thread_id: thread.thread_id,
    turn_id: turnId,
  });
  if (response.type !== "fs_turn_changes") {
    throw new Error("The daemon did not return a change set.");
  }
  return response.changes;
}

export async function loadFileDiff(
  request: ChangesRequest,
  thread: ThreadSummary,
  path: string,
  against: DiffAgainst,
): Promise<FsDiff | null> {
  const response = await request({
    type: "fs_diff",
    host: thread.host,
    workspace: thread.workspace_path,
    path,
    against,
  });
  return response.type === "fs_diff" ? response.diff : null;
}

/**
 * Reverts one file to its pre-turn bytes.
 *
 * A file the turn created has no `before`; reverting it means deleting it.
 * A file with no diff yet is fetched first so the caller never has to load
 * diffs eagerly.
 */
export async function revertFile(
  request: ChangesRequest,
  thread: ThreadSummary,
  file: FsTurnFile,
  diff: FsDiff | null,
  against: DiffAgainst,
): Promise<FileRevert> {
  let resolved = diff;
  if (!resolved) {
    try {
      resolved = await loadFileDiff(request, thread, file.path, against);
    } catch (error) {
      return {
        path: file.path,
        outcome: "failed",
        message: error instanceof Error ? error.message : String(error),
      };
    }
  }
  if (!resolved) {
    return { path: file.path, outcome: "skipped", message: "No diff is available." };
  }
  try {
    if (resolved.before === null) {
      const response = await request({
        type: "fs_delete",
        host: thread.host,
        workspace: thread.workspace_path,
        path: file.path,
      });
      return response.type === "fs_deleted"
        ? { path: file.path, outcome: "deleted" }
        : { path: file.path, outcome: "failed", message: "The daemon did not delete the file." };
    }
    const response = await request({
      type: "fs_write",
      host: thread.host,
      workspace: thread.workspace_path,
      path: file.path,
      content: resolved.before,
      base_etag: file.after_etag,
    });
    if (response.type !== "fs_written") {
      return { path: file.path, outcome: "failed", message: "The daemon did not write the file." };
    }
    return response.result.conflict
      ? {
          path: file.path,
          outcome: "failed",
          message: "The file changed after the diff loaded. Reload and try again.",
        }
      : { path: file.path, outcome: "reverted" };
  } catch (error) {
    return {
      path: file.path,
      outcome: "failed",
      message: error instanceof Error ? error.message : String(error),
    };
  }
}

/** Reverts every file, continuing after failures, and reports each outcome. */
export async function revertFiles(
  request: ChangesRequest,
  thread: ThreadSummary,
  files: FsTurnFile[],
  diffs: Record<string, FsDiff | null>,
  against: DiffAgainst,
): Promise<FileRevert[]> {
  const results: FileRevert[] = [];
  for (const file of files) {
    results.push(await revertFile(request, thread, file, diffs[file.path] ?? null, against));
  }
  return results;
}
