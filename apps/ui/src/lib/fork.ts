import type { StreamTurn } from "@/lib/store/projection";

/**
 * The turn id a fork from this row should name, or `null` when the row is not
 * a finished turn the daemon can copy.
 *
 * A running turn and a live user-message cue have no settled daemon turn to
 * copy; an imported cue carries its real turn id in `forkTurnId`.
 */
export function forkableTurn(turn: StreamTurn): string | null {
  if (turn.status === "running") return null;
  if (turn.items.length === 0) return null;
  if (turn.forkTurnId) return turn.forkTurnId;
  return turn.id.startsWith("cue:") ? null : turn.id;
}

/** The newest finished turn a fork can name, when there is one. */
export function latestForkableTurn(turns: StreamTurn[]): string | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const turn = turns[index];
    if (!turn) continue;
    const id = forkableTurn(turn);
    if (id) return id;
  }
  return null;
}
