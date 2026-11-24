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
 * Rows are not in logical order: a queued prompt's cue is written when it
 * arrives, while its daemon turn only appears when it starts, after later
 * cues. Each row is ordered by the position of the cue that introduced its
 * turn, so a promoted turn never hides a later prompt that is still waiting.
 * The cue is preferred on a tie because it is the row that also names a
 * prompt-only turn whose daemon row stays empty.
 */
export function latestForkableTurn(turns: StreamTurn[]): string | null {
  let best: StreamTurn | null = null;
  let bestPosition = -1;
  turns.forEach((turn, index) => {
    const cueIndex = turn.forkTurnId
      ? index
      : turns.findIndex((candidate) => candidate.forkTurnId === turn.id);
    const position = cueIndex >= 0 ? cueIndex : index;
    const cue = turn.id.startsWith("cue:");
    if (position > bestPosition || (position === bestPosition && cue)) {
      best = turn;
      bestPosition = position;
    }
  });
  if (!best) return null;
  return forkableTurn(best, turns);
}
