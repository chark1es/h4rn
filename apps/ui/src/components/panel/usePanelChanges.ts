import { useCallback, useEffect, useMemo, useState } from "react";

import type { DiffAgainst, FsDiff, FsTurnChanges, ThreadId, ThreadSummary, TurnId } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient, pushToast, useApp } from "@/lib/store/appStore";

export type ChangeMode = "agent" | "head";

export type PanelChanges = {
  turns: Array<{ id: TurnId; label: string }>;
  turnId: TurnId | null;
  selectTurn: (turnId: TurnId | null) => void;
  mode: ChangeMode;
  setMode: (mode: ChangeMode) => void;
  changes: FsTurnChanges | null;
  diffs: Record<string, FsDiff | null>;
  loading: boolean;
  diffsLoading: boolean;
  error: string | null;
  added: number;
  removed: number;
  revertAll: () => Promise<void>;
  reload: () => void;
};

/**
 * The right panel's Changes tab reads the same daemon data the IDE shows:
 * the files for one logged turn, plus a per-file diff against either the
 * agent's snapshot or HEAD. Both scopes are one click apart, as in the design.
 */
export function usePanelChanges(thread: ThreadSummary | null): PanelChanges {
  const threadId = thread?.thread_id ?? null;
  const log = useApp((state) => (threadId ? state.logs[threadId] : undefined));
  const turns = useMemo(
    () =>
      (log?.turns ?? []).map((turn, index) => ({
        id: turn.id,
        label: `Turn ${index + 1}`,
      })),
    [log?.turns],
  );

  const [pickedTurn, setPickedTurn] = useState<TurnId | null>(null);
  const [mode, setMode] = useState<ChangeMode>("agent");
  const [changes, setChanges] = useState<FsTurnChanges | null>(null);
  const [diffs, setDiffs] = useState<Record<string, FsDiff | null>>({});
  const [loading, setLoading] = useState(false);
  const [diffsLoading, setDiffsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const latestTurn = turns.at(-1)?.id ?? null;
  const turnId = pickedTurn ?? latestTurn;

  // A different thread starts from its latest logged turn again.
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset when the selection changes, not when the setter identity does.
  useEffect(() => {
    setPickedTurn(null);
  }, [threadId]);

  // `nonce` is the explicit reload trigger; it is intentionally not read here.
  // biome-ignore lint/correctness/useExhaustiveDependencies: nonce is a reload trigger
  useEffect(() => {
    if (!thread || !turnId) {
      setChanges(null);
      setDiffs({});
      setLoading(false);
      setError(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    getClient()
      .request({
        type: "fs_turn_changes",
        host: thread.host,
        workspace: thread.workspace_path,
        thread_id: thread.thread_id,
        turn_id: turnId,
      })
      .then((response) => {
        if (cancelled || response.type !== "fs_turn_changes") return;
        setChanges(response.changes);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setChanges(null);
        setLoading(false);
        setError(toProtoError(cause).message);
      });
    return () => {
      cancelled = true;
    };
  }, [thread, turnId, nonce]);

  useEffect(() => {
    if (!thread || !changes || changes.files.length === 0) {
      setDiffs({});
      setDiffsLoading(false);
      return;
    }
    let cancelled = false;
    const against: DiffAgainst =
      mode === "head"
        ? { kind: "head" }
        : {
            kind: "agent_turn",
            thread_id: thread.thread_id as ThreadId,
            turn_id: (turnId ?? changes.turn_id) as TurnId,
          };
    setDiffsLoading(true);
    void Promise.all(
      changes.files.map(async (file) => {
        try {
          const response = await getClient().request({
            type: "fs_diff",
            host: thread.host,
            workspace: thread.workspace_path,
            path: file.path,
            against,
          });
          return [file.path, response.type === "fs_diff" ? response.diff : null] as const;
        } catch {
          return [file.path, null] as const;
        }
      }),
    ).then((entries) => {
      if (cancelled) return;
      setDiffs(Object.fromEntries(entries));
      setDiffsLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [thread, changes, mode, turnId]);

  const reload = useCallback(() => setNonce((value) => value + 1), []);

  const revertAll = useCallback(async () => {
    if (!thread || !changes) return;
    let reverted = 0;
    for (const file of changes.files) {
      const diff = diffs[file.path];
      if (!diff || diff.before === null) continue;
      try {
        const response = await getClient().request({
          type: "fs_write",
          host: thread.host,
          workspace: thread.workspace_path,
          path: file.path,
          content: diff.before,
          base_etag: file.after_etag,
        });
        if (response.type === "fs_written" && !response.result.conflict) reverted += 1;
      } catch {
        // Keep reverting the rest; the toast reports the partial result.
      }
    }
    pushToast({
      title:
        reverted === changes.files.length
          ? `Reverted ${reverted} ${reverted === 1 ? "file" : "files"}.`
          : `Reverted ${reverted} of ${changes.files.length} files.`,
      variant: reverted > 0 ? "success" : "error",
    });
    reload();
  }, [thread, changes, diffs, reload]);

  const added = changes?.files.reduce((sum, file) => sum + file.added, 0) ?? 0;
  const removed = changes?.files.reduce((sum, file) => sum + file.removed, 0) ?? 0;

  return {
    turns,
    turnId,
    selectTurn: setPickedTurn,
    mode,
    setMode,
    changes,
    diffs,
    loading,
    diffsLoading,
    error,
    added,
    removed,
    revertAll,
    reload,
  };
}
