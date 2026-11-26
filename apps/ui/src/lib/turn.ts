import { i18n } from "@/lib/i18n";
import type { TurnStatusOrRunning } from "@/lib/store/projection";

/**
 * The plain-language footer for a turn the agent stopped before finishing.
 *
 * Returns `null` for every other status so the turn footer keeps its normal
 * completion and failure wording.
 */
export function stoppedNotice(status: TurnStatusOrRunning): string | null {
  switch (status) {
    case "token_limit":
      return i18n.t("thread:turn.stopped.token_limit");
    case "turn_limit":
      return i18n.t("thread:turn.stopped.turn_limit");
    default:
      return null;
  }
}
