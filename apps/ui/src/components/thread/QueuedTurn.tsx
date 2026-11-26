import {
  ChevronDownIcon,
  ChevronUpIcon,
  CornerDownRightIcon,
  PencilIcon,
  XIcon,
} from "lucide-react";
import { useTranslation } from "react-i18next";

import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import type { QueuedMessage, ThreadSummary } from "@/gen";
import { isTurnRunning } from "@/lib/agent";
import { i18n } from "@/lib/i18n";
import { moveQueuedMessage, removeQueuedMessage, sendQueuedNow } from "@/lib/store/appStore";

/**
 * The copy for what Send now will really do with this row right now.
 *
 * The daemon publishes `steerable` per row: a message with its own permission
 * ceiling, or one that may already have been delivered, is moved to the front
 * instead. The hint says exactly that instead of promising a steer.
 */
export function sendNowHint(summary: ThreadSummary, message: QueuedMessage): string {
  if (message.delivery_unknown) return i18n.t("composer:queue.hint.maybeDelivered");
  if (isTurnRunning(summary)) {
    return message.steerable
      ? i18n.t("composer:queue.hint.steers")
      : i18n.t("composer:queue.hint.waits");
  }
  return i18n.t("composer:queue.hint.next");
}

/** One queued message under the composer, with Send now, Edit, Remove and order. */
export function QueuedTurn({
  threadId,
  summary,
  message,
  index,
  total,
  editing,
}: {
  threadId: string;
  summary: ThreadSummary;
  message: QueuedMessage;
  index: number;
  /**
   * How many messages wait in total.
   *
   * The rows are a bounded page, so the last visible row is not necessarily the
   * queue's last; the caller knows the full count.
   */
  total: number;
  editing: boolean;
}) {
  const { t } = useTranslation(["composer", "common"]);
  const steers = isTurnRunning(summary) && message.steerable;
  return (
    <div className="q-queued" data-editing={editing || undefined}>
      <div className="q-queued-h">
        <span className="q-queued-label">
          {message.delivery_unknown ? t("queue.maybeDelivered") : t("queue.queued")}
        </span>
        <span className="q-faint">{sendNowHint(summary, message)}</span>
      </div>
      <div className="q-queued-text">{message.text}</div>
      {message.attachments.length > 0 ? (
        <MessageAttachments attachments={message.attachments} align="start" />
      ) : null}
      <div className="q-queued-a">
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => void sendQueuedNow(threadId, message.message_id)}
          title={
            message.delivery_unknown
              ? t("queue.sendAgainTitle")
              : steers
                ? t("queue.steerTitle")
                : t("queue.frontTitle")
          }
        >
          <CornerDownRightIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {message.delivery_unknown ? t("queue.sendAgain") : t("queue.sendNow")}
        </button>
        <button
          type="button"
          className="q-btn ghost"
          onClick={() =>
            window.dispatchEvent(
              new CustomEvent("h4rn:edit-queued", {
                detail: { threadId, message },
              }),
            )
          }
          title={t("queue.editTitle")}
        >
          <PencilIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {t("common:edit")}
        </button>
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => void removeQueuedMessage(threadId, message.message_id)}
          title={message.delivery_unknown ? t("queue.discardTitle") : t("queue.removeTitle")}
        >
          <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {message.delivery_unknown ? t("queue.discard") : t("common:remove")}
        </button>
        <span className="grow" />
        <button
          type="button"
          className="q-ibtn"
          disabled={index === 0}
          aria-label={t("queue.moveUp")}
          title={t("queue.moveUp")}
          onClick={() => void moveQueuedMessage(threadId, message.message_id, "up")}
        >
          <ChevronUpIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
        <button
          type="button"
          className="q-ibtn"
          disabled={index === total - 1}
          aria-label={t("queue.moveDown")}
          title={t("queue.moveDown")}
          onClick={() => void moveQueuedMessage(threadId, message.message_id, "down")}
        >
          <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </div>
    </div>
  );
}
