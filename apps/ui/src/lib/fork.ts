import type { StreamTurn } from "@/lib/store/projection";

/**
 * The turn id a fork from this row should name, or `null` when the daemon has
 * no settled turn to copy.
 *
 * A user cue shows the person's message as its own row, so it names the daemon
 * turn it belongs to in `forkTurnId`. That turn's own row says whether it has
 * settled: a prompt whose turn is still running, or was queued behind another
 * turn and has not started, cannot be forked yet. Copied history has no live
 * daemon row and is settled by definition.
 */
export function forkableTurn(turn: StreamTurn, turns: StreamTurn[]): string | null {
  if (turn.forkTurnId) {
    if (turn.imported) return turn.forkTurnId;
    const daemonTurn = turns.find((candidate) => candidate.id === turn.forkTurnId);
    if (!daemonTurn || daemonTurn.status === "running") return null;
    return turn.forkTurnId;
  }
  if (turn.status === "running") return null;
  if (turn.items.length === 0) return null;
  return turn.id;
}

/**
 * The newest turn a fork can name, when there is one.
 *
 * Only the newest row counts: "fork from the latest turn" must never quietly
 * walk back to an older finished turn while the newest one is still running.
 */
export function latestForkableTurn(turns: StreamTurn[]): string | null {
  const newest = turns[turns.length - 1];
  if (!newest) return null;
  return forkableTurn(newest, turns);
}
