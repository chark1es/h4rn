import { useCallback, useEffect, useMemo, useState } from "react";

import {
  againstFor,
  CHANGES_SELECT_EVENT,
  type FileRevert,
  loadFileDiff,
  loadTurnChanges,
  revertFiles,
  revertFile as revertOne,
} from "@/features/changes/store";
import type { FsDiff, FsTurnChanges, ThreadSummary, TurnId } from "@/gen";
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
  revertFile: (path: string) => Promise<void>;
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
      (log?.turns ?? [])
        .filter((turn) => !turn.id.startsWith("cue:user:"))
        .map((turn, index) => ({
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

  const latestTurn =
    [...turns].reverse().find((turn) => !turn.id.startsWith("cue:user:"))?.id ?? null;
  const turnId = pickedTurn ?? latestTurn;

  // A different thread starts from its latest logged turn again.
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset when the selection changes, not when the setter identity does.
  useEffect(() => {
    setPickedTurn(null);
  }, [threadId]);

  // The stream's change-set card asks for one turn through a window event.
  useEffect(() => {
    const select = (event: Event) => {
      const detail = (event as CustomEvent<{ turnId: TurnId | null }>).detail;
      setPickedTurn(detail?.turnId ?? null);
    };
    window.addEventListener(CHANGES_SELECT_EVENT, select);
    return () => window.removeEventListener(CHANGES_SELECT_EVENT, select);
  }, []);

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
    loadTurnChanges((request) => getClient().request(request), thread, turnId)
      .then((next) => {
        if (cancelled) return;
        setChanges(next);
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
    const against = againstFor(mode, thread, turnId ?? changes.turn_id);
    setDiffsLoading(true);
    void Promise.all(
      changes.files.map(async (file) => {
        try {
          return [
            file.path,
            await loadFileDiff(
              (request) => getClient().request(request),
              thread,
              file.path,
              against,
            ),
          ] as const;
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

  const report = useCallback(
    (results: FileRevert[], total: number) => {
      const done = results.filter(
        (result) => result.outcome === "reverted" || result.outcome === "deleted",
      ).length;
      const failed = results.filter((result) => result.outcome === "failed");
      pushToast({
        title:
          done === total
            ? `Reverted ${done} ${done === 1 ? "file" : "files"}.`
            : `Reverted ${done} of ${total} files.`,
        ...(failed.length > 0
          ? {
              description: failed
                .slice(0, 2)
                .map((result) => `${result.path}: ${result.message ?? "failed"}`)
                .join(" · "),
            }
          : {}),
        variant: done > 0 ? "success" : "error",
      });
      reload();
    },
    [reload],
  );

  const revertAll = useCallback(async () => {
    if (!thread || !changes) return;
    const against = againstFor(mode, thread, turnId ?? changes.turn_id);
    const results = await revertFiles(
      (request) => getClient().request(request),
      thread,
      changes.files,
      diffs,
      against,
    );
    report(results, changes.files.length);
  }, [thread, changes, diffs, mode, turnId, report]);

  const revertFile = useCallback(
    async (path: string) => {
      if (!thread || !changes) return;
      const file = changes.files.find((candidate) => candidate.path === path);
      if (!file) return;
      const against = againstFor(mode, thread, turnId ?? changes.turn_id);
      const result = await revertOne(
        (request) => getClient().request(request),
        thread,
        file,
        diffs[path] ?? null,
        against,
      );
      report([result], 1);
    },
    [thread, changes, diffs, mode, turnId, report],
  );

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
    revertFile,
    reload,
  };
}
