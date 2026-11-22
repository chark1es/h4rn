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
  /** Threads whose current turn reported completion on the live stream. */
  private readonly live = new Set<ThreadId>();

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
    this.live.delete(threadId);
  }

  /**
   * Remembers how a turn ended.
   *
   * `live` is false for a completion delivered from the subscribe replay: the
   * wording may still describe a Done that follows, but replayed history must
   * never be the reason a notification fires.
   */
  completeTurn(threadId: ThreadId, status: TurnStatus, at: number, live = true): void {
    this.turns.set(threadId, { status, error: this.lastError(threadId, at) });
    if (live) this.live.add(threadId);
  }

  lastTurn(threadId: ThreadId): LastTurn | null {
    return this.turns.get(threadId) ?? null;
  }

  /** Whether a live completion for this thread's current turn is unannounced. */
  hasLiveCompletion(threadId: ThreadId): boolean {
    return this.live.has(threadId);
  }

  /** The completion was consumed; a later summary tick is not news again. */
  announceFinished(threadId: ThreadId): void {
    this.live.delete(threadId);
  }

  /**
   * A fresh baseline: no remembered completion may explain a later summary.
   *
   * Called when the thread list re-seeds after a ready handshake or a healed
   * gap, and when a person places a thread by hand.
   */
  clearCompletions(): void {
    this.live.clear();
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
