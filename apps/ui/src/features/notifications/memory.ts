/**
 * Per-thread memory for the notifier.
 *
 * A thread summary says a turn ended, but not how it ended; the wording comes
 * from the last `turn_completed` and the error that preceded it. A new turn
 * clears both, so a later failure can never show an older error.
 */

import type { ThreadId, TurnStatus } from "@/gen";

import type { LastTurn } from "./logic";

/** How long an error remains the explanation for a failed turn. */
export const ERROR_MEMORY_MS = 2 * 60_000;

export class NotificationMemory {
  private readonly errors = new Map<ThreadId, { message: string; at: number }>();
  private readonly turns = new Map<ThreadId, LastTurn>();
  private readonly failures = new Set<ThreadId>();

  /** Remembers an error row; an error with no thread says nothing about one. */
  rememberError(threadId: ThreadId | null, message: string, at: number): void {
    if (!threadId) return;
    this.errors.set(threadId, { message, at });
  }

  /** A new attempt is not explained by the previous attempt's failure. */
  startTurn(threadId: ThreadId): void {
    this.errors.delete(threadId);
    this.turns.delete(threadId);
    this.failures.delete(threadId);
  }

  completeTurn(threadId: ThreadId, status: TurnStatus, at: number): void {
    this.turns.set(threadId, { status, error: this.lastError(threadId, at) });
  }

  lastTurn(threadId: ThreadId): LastTurn | null {
    return this.turns.get(threadId) ?? null;
  }

  lastError(threadId: ThreadId, now: number): string | null {
    const remembered = this.errors.get(threadId);
    if (!remembered) return null;
    if (now - remembered.at > ERROR_MEMORY_MS) {
      this.errors.delete(threadId);
      return null;
    }
    return remembered.message;
  }

  /** Whether this thread's current turn's failure was already announced. */
  failureAnnounced(threadId: ThreadId): boolean {
    return this.failures.has(threadId);
  }

  announceFailure(threadId: ThreadId): void {
    this.failures.add(threadId);
  }
}
