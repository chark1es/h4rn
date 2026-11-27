import { PaperclipIcon, SendIcon, UserRoundIcon, XIcon } from "lucide-react";
import { useTranslation } from "react-i18next";
import { formatBytes } from "@/features/attachments/upload";
import { dismissGuestRequest, sendGuestRequest, useGuest } from "@/features/guest/guestStore";
import type { GuestRequest, ThreadId } from "@/gen";
import { absoluteTime } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

/**
 * One guest message waiting for the owner (ADR-044).
 *
 * It is a request, not a turn: nothing has run. Send appends it as a normal
 * message with the thread's current permission mode; Dismiss deletes its files.
 */
export function GuestRequestCard({ threadId }: { threadId: ThreadId }) {
  const requests = useApp((state) => state.logs[threadId]?.guestRequests);
  const { t } = useTranslation("thread");
  const answering = useGuest().answering;
  if (!requests || requests.length === 0) return null;
  return (
    <section className="q-guest-reqs" aria-label={t("guest.requests")}>
      {requests.map((request) => (
        <GuestRequestRow
          key={request.request_id}
          threadId={threadId}
          request={request}
          busy={answering[request.request_id] ?? false}
        />
      ))}
    </section>
  );
}

function GuestRequestRow({
  threadId,
  request,
  busy,
}: {
  threadId: ThreadId;
  request: GuestRequest;
  busy: boolean;
}) {
  const { t } = useTranslation("thread");
  const attachments = request.attachments ?? [];
  return (
    <section className="q-guest-req" aria-label={t("guest.waiting")}>
      <div className="q-guest-req-head">
        <span className="q-guest-req-tag">
          <UserRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {t("guest.tag")}
        </span>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {t("guest.from", { time: absoluteTime(request.created_at) })}
        </span>
      </div>
      {request.text ? <p className="q-guest-req-text">{request.text}</p> : null}
      {attachments.length ? (
        <ul className="q-guest-req-files" aria-label={t("guest.files")}>
          {attachments.map((attachment) => (
            <li key={`${attachment.name}:${attachment.size}`} className="q-guest-req-file">
              <PaperclipIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              <span className="q-guest-req-name">{attachment.name}</span>
              <span className="q-faint">{formatBytes(attachment.size)}</span>
            </li>
          ))}
        </ul>
      ) : null}
      <div className="q-guest-req-foot">
        <span className="q-faint" style={{ fontSize: 12 }}>
          {t("guest.sendHint")}
        </span>
        <button
          type="button"
          className={cn("q-btn ghost")}
          disabled={busy}
          onClick={() => void dismissGuestRequest(threadId, request.request_id)}
        >
          <XIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("guest.dismiss")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={busy}
          onClick={() => void sendGuestRequest(threadId, request.request_id)}
        >
          <SendIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("guest.send")}
        </button>
      </div>
    </section>
  );
}
