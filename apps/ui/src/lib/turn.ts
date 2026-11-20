import type { Attachment } from "@/gen";
import type { MessageItem, StreamTurn, TurnStatusOrRunning } from "@/lib/store/projection";

/**
 * The plain-language footer for a turn the agent stopped before finishing.
 *
 * Returns `null` for every other status so the turn footer keeps its normal
 * completion and failure wording.
 */
export function stoppedNotice(status: TurnStatusOrRunning): string | null {
  switch (status) {
    case "token_limit":
      return "Stopped: the agent hit its token limit.";
    case "turn_limit":
      return "Stopped: the agent hit its turn-request limit.";
    default:
      return null;
  }
}

/**
 * The prompt that started a turn, so a stopped turn can be resent as-is.
 *
 * Returns `null` when the turn holds no user message (nothing to resend).
 */
export function turnPrompt(turn: StreamTurn): { text: string; attachments: Attachment[] } | null {
  const userItems = turn.items.filter(
    (item): item is MessageItem => item.kind === "message" && item.role === "user",
  );
  if (userItems.length === 0) return null;
  const text = userItems
    .map((item) => item.text)
    .filter((part) => part.trim().length > 0)
    .join("\n\n");
  const attachments = userItems.flatMap((item) => item.attachments);
  if (text.length === 0 && attachments.length === 0) return null;
  return { text, attachments };
}
