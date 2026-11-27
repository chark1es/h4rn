import { CircleAlertIcon, CopyIcon, InboxIcon, RefreshCwIcon, ServerIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { PhonePanelSheet } from "@/components/panel/PhonePanelSheet";
import { toastClearance } from "@/components/ui/sonner";
import { TerminalPanel } from "@/features/terminal/TerminalPanel";
import { AutoResumeCards } from "@/features/usage/AutoResumeCard";
import { UsageFooter } from "@/features/usage/UsageFooter";
import { useIsPhone } from "@/hooks/use-media-query";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { onPhonePanel } from "@/lib/phonePanels";
import {
  ensureThread,
  interruptThread,
  navigate,
  PREVIEW_EVENT,
  requestComposerFocus,
  retryThread,
  rewindRefusal,
  setPanelTab,
  toggleTerminal,
  useApp,
} from "@/lib/store/appStore";
import type { ApprovalItem, MessageItem } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { ApprovalCard } from "./ApprovalCard";
import { Composer } from "./Composer";
import { GuestRequestCard } from "./GuestRequestCard";
import { PhoneApprovalSheet, PhoneApprovalSummary } from "./PhoneApproval";
import { RewindDialog } from "./RewindDialog";
import { ThreadHeader } from "./ThreadHeader";
import { ThreadStream, ThreadStreamSkeleton } from "./ThreadStream";

function HeaderSkeleton({ hostName }: { hostName?: string | undefined }) {
  const { t } = useTranslation("thread");
  return (
    <header className="q-th-head">
      <div className="q-th-titles">
        <div className="q-skel mb-2 h-3 w-60" />
        <div className="q-skel h-2.5 w-90 max-w-full" />
      </div>
      <div className="q-th-state q-faint" style={{ fontWeight: 400 }}>
        {hostName ? t("pane.connectingTo", { host: hostName }) : t("pane.connecting")}
      </div>
    </header>
  );
}

function WaitingForTurn({
  threadId,
  host,
  harness,
}: {
  threadId: string;
  host: string;
  harness: string;
}) {
  const { t } = useTranslation(["thread", "common"]);
  const glyph = glyphForAgentState("thinking");
  return (
    <div className="q-stream">
      <div className="q-col">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <AgentGlyph glyph={glyph} size={14} />
          <p className="q-faint" style={{ margin: 0, fontSize: 12 }}>
            {t("pane.waitingFor", { harness, host })}
          </p>
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24, marginLeft: "auto" }}
            onClick={() => void interruptThread(threadId)}
          >
            {t("common:cancel")}
          </button>
        </div>
        <div style={{ marginTop: 16, display: "grid", gap: 8 }} aria-hidden>
          <div className="q-skel" style={{ height: 10, width: "82%" }} />
          <div className="q-skel" style={{ height: 10, width: "64%" }} />
          <div className="q-skel" style={{ height: 10, width: "44%" }} />
        </div>
        <p className="q-faint" style={{ marginTop: 16, fontSize: 12 }}>
          {t("pane.waitingNote")}
        </p>
      </div>
    </div>
  );
}

export function ThreadPane({ threadId }: { threadId: string }) {
  const { t } = useTranslation(["thread", "common"]);
  const summary = useApp((state) => state.threads[threadId]);
  const hosts = useApp((state) => state.hosts);
  const host = useApp((state) => state.hosts[summary?.host ?? ""]);
  const log = useApp((state) => state.logs[threadId]);
  const queued = useApp((state) => state.queues[threadId]?.queued);
  const threadError = useApp((state) => state.threadErrors[threadId]);
  const connection = useApp((state) => state.connection.status);
  const lastSyncAt = useApp((state) => state.lastSyncAt);
  const terminalOpen = useApp((state) => state.ui.layout.terminalOpen);
  const isPhone = useIsPhone();
  // Mount on first open, then keep the panel (and its PTYs) alive while hidden.
  const [terminalMounted, setTerminalMounted] = useState(terminalOpen);
  const [phonePanel, setPhonePanel] = useState<"changes" | "subagents" | "preview" | null>(null);
  const [approvalSheetOpen, setApprovalSheetOpen] = useState(false);
  const [rewindTarget, setRewindTarget] = useState<{ itemId: string } | null>(null);
  // The shortcut and the palette open the terminal through the store, not
  // the header button, so mount on any open.
  useEffect(() => {
    if (terminalOpen) setTerminalMounted(true);
  }, [terminalOpen]);

  useEffect(() => {
    ensureThread(threadId);
    setApprovalSheetOpen(false);
    setRewindTarget(null);
  }, [threadId]);

  // The delegated row in the stream opens the right panel's Subagents tab on
  // desktop, or the sheet's Subagents segment on a phone.
  useEffect(() => {
    const handler = () => {
      if (isPhone) setPhonePanel("subagents");
      else setPanelTab("subagents");
    };
    window.addEventListener("h4rn:subagents", handler);
    return () => window.removeEventListener("h4rn:subagents", handler);
  }, [isPhone]);

  // An HTML link in the stream opens the preview; a phone shows it in the
  // panel sheet, a desktop has already opened the right panel's tab.
  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ threadId?: string }>).detail;
      if (detail?.threadId && detail.threadId !== threadId) return;
      if (isPhone) setPhonePanel("preview");
    };
    window.addEventListener(PREVIEW_EVENT, handler);
    return () => window.removeEventListener(PREVIEW_EVENT, handler);
  }, [isPhone, threadId]);

  // The composer's `/` menu asks for the phone sheet by event: it does not own
  // the sheet's state, and on a phone the desktop panel tab would go nowhere.
  useEffect(() => {
    if (!isPhone) return;
    return onPhonePanel(setPhonePanel);
  }, [isPhone]);

  // The stream's change-set card opens one turn in the Changes tab.
  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ tab?: string }>).detail;
      if (isPhone) setPhonePanel(detail?.tab === "subagents" ? "subagents" : "changes");
      else setPanelTab(detail?.tab === "subagents" ? "subagents" : "changes");
    };
    window.addEventListener("h4rn:open-panel", handler);
    return () => window.removeEventListener("h4rn:open-panel", handler);
  }, [isPhone]);

  if (!summary && !log && !threadError) {
    return (
      <main className="q-main" aria-busy="true" aria-label={t("pane.loading")}>
        <HeaderSkeleton hostName={Object.values(hosts)[0]?.name} />
        <ThreadStreamSkeleton />
        <div className="q-dock">
          <div className="q-col">
            <div className="q-skel" style={{ height: 84, borderRadius: 12, opacity: 0.6 }} />
          </div>
        </div>
      </main>
    );
  }

  if (!summary) {
    return (
      <main className="q-main" aria-label={t("header.thread")}>
        <div className="q-center-empty">
          <div className="q-empty" style={{ width: 460, maxWidth: "100%", padding: 0 }}>
            <span className="q-danger">
              <CircleAlertIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
            </span>
            <h3 style={{ fontSize: 16, lineHeight: "24px", marginTop: 10 }}>
              {t("pane.loadFailed")}
            </h3>
            <p style={{ margin: 0 }}>
              {threadError
                ? t("pane.stillActive", { message: threadError.message })
                : t("pane.notFound")}
            </p>
            <div style={{ display: "flex", gap: 8 }}>
              <button type="button" className="q-btn primary" onClick={() => retryThread(threadId)}>
                <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("common:retry")}
              </button>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => navigate({ kind: "hosts" })}
              >
                <ServerIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("pane.openHost")}
              </button>
            </div>
          </div>
        </div>
      </main>
    );
  }

  const hostOnline = host?.online ?? false;
  const stale = connection === "offline" || connection === "degraded";
  const offline = stale || !hostOnline;
  const seenAt =
    lastSyncAt !== null
      ? absoluteTime(new Date(lastSyncAt).toISOString())
      : absoluteTime(host?.last_seen ?? summary.last_activity);
  const turns = log?.turns ?? [];
  const pending = log?.pending ?? [];
  const loaded = log?.loaded ?? false;
  const waiting =
    loaded &&
    turns.length === 0 &&
    pending.length === 0 &&
    summary.queued_count === 0 &&
    summary.agent_state !== "done";
  const pendingApprovals = turns
    .flatMap((turn) => turn.items)
    .filter((item): item is ApprovalItem => item.kind === "approval")
    .filter((item) => {
      const status = log?.approvals?.[item.approvalId]?.status ?? "pending";
      return status === "pending" || status === "sending" || status === "error";
    });
  const firstApproval = pendingApprovals[0];
  const hostName = host?.name ?? summary.host;
  const rewindBlocked = rewindRefusal(summary, log, queued);

  const main = (
    <main
      className={cn(
        "q-main",
        terminalOpen && "q-term-open",
        isPhone && terminalOpen && "q-phone-term",
      )}
      aria-label={summary.title}
    >
      <ThreadHeader
        summary={summary}
        host={host}
        onBack={() => navigate({ kind: "inbox" })}
        terminalOpen={terminalOpen}
        onToggleTerminal={toggleTerminal}
        onOpenPanel={() => setPhonePanel("changes")}
        stale={stale}
      />

      {loaded && offline && summary.tab === "working" ? (
        <div
          className="q-banner wrap"
          style={{ margin: "12px auto 0", maxWidth: 656, width: "calc(100% - 64px)" }}
        >
          <span className="ic">
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </span>
          <span className="grow">
            <b>{t("pane.unreachable", { host: hostName })}</b>{" "}
            {t("pane.readingAsOf", { time: seenAt })}
          </span>
        </div>
      ) : null}

      {threadError && log?.loaded ? (
        <div
          className="q-banner err"
          style={{ margin: "12px auto 0", maxWidth: 656, width: "calc(100% - 64px)" }}
        >
          <span className="ic">
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </span>
          <span className="grow">
            <b>{t("pane.stale")}</b> {threadError.message}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24 }}
            onClick={() => retryThread(threadId)}
          >
            {t("common:retry")}
          </button>
        </div>
      ) : null}

      {!loaded && threadError && !log?.loading ? (
        <div className="q-center-empty">
          <div className="q-empty" style={{ width: 460, maxWidth: "100%", padding: 0 }}>
            <span className="q-danger">
              <CircleAlertIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
            </span>
            <h3 style={{ fontSize: 16, lineHeight: "24px", marginTop: 10 }}>
              {t("pane.loadFailed")}
            </h3>
            <p style={{ margin: 0 }}>
              {threadError.message} {threadError.hint ?? t("pane.nothingLost")}
            </p>
            <div style={{ display: "flex", gap: 8 }}>
              <button type="button" className="q-btn primary" onClick={() => retryThread(threadId)}>
                <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("common:retry")}
              </button>
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => navigate({ kind: "hosts" })}
              >
                <ServerIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("pane.openHost")}
              </button>
            </div>
            <details
              className="q-disclosure"
              style={{ fontSize: 12, color: "var(--fg-2)", alignSelf: "stretch" }}
            >
              <summary style={{ cursor: "pointer" }}>{t("pane.details")}</summary>
              <dl className="q-kv" style={{ marginTop: 10, textAlign: "left" }}>
                <dt>{t("pane.host")}</dt>
                <dd>{hostName}</dd>
                <dt>{t("pane.lastSeen")}</dt>
                <dd className="q-num">{absoluteTime(host?.last_seen ?? summary.last_activity)}</dd>
                <dt>{t("pane.status")}</dt>
                <dd>{threadError.code.replace(/_/g, " ")}</dd>
                <dt>{t("pane.lastEvent")}</dt>
                <dd className="q-num">{absoluteTime(summary.last_activity)}</dd>
              </dl>
              <button
                type="button"
                className="q-btn ghost"
                style={{ marginTop: 8, marginLeft: -10 }}
                onClick={() => {
                  void navigator.clipboard?.writeText(
                    [
                      `thread: ${threadId}`,
                      `host: ${hostName}`,
                      `status: ${threadError.code}`,
                      `message: ${threadError.message}`,
                    ].join("\n"),
                  );
                }}
              >
                <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("pane.copyDiagnostics")}
              </button>
            </details>
          </div>
        </div>
      ) : !loaded ? (
        <ThreadStreamSkeleton />
      ) : waiting ? (
        <WaitingForTurn
          threadId={threadId}
          host={host?.name ?? summary.host}
          harness={harnessLabel(summary.harness)}
        />
      ) : turns.length === 0 && summary.queued_count === 0 ? (
        <div className="q-center-empty">
          <div className="q-empty" style={{ width: 460, maxWidth: "100%", padding: 0 }}>
            <InboxIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
            <h3 style={{ fontSize: 16, lineHeight: "24px" }}>{t("pane.emptyTitle")}</h3>
            <p style={{ margin: 0 }}>
              {summary.agent_state === "done"
                ? t("pane.emptyFinished", { host: host?.name ?? summary.host })
                : t("pane.emptyIdle", { host: host?.name ?? summary.host })}
            </p>
          </div>
        </div>
      ) : (
        <ThreadStream
          threadId={threadId}
          summary={summary}
          turns={turns}
          pending={pending}
          // A stopped or failed turn has no single prompt to repeat safely: a
          // queued follow-up would be sent twice. Put the cursor back in the
          // composer instead.
          onReply={requestComposerFocus}
          onEditFromHere={(item: MessageItem) => setRewindTarget({ itemId: item.itemId })}
          editDisabledReason={rewindBlocked}
          staleAt={offline ? lastSyncAt : null}
        />
      )}

      {!threadError || loaded ? (
        <div ref={toastClearance} className="q-dock">
          <div className="q-col">
            {isPhone ? (
              <>
                <AutoResumeCards threadId={threadId} compact />
                {firstApproval ? (
                  <PhoneApprovalSummary
                    thread={summary}
                    hostName={hostName}
                    approval={firstApproval}
                    onReview={() => setApprovalSheetOpen(true)}
                  />
                ) : null}
                <GuestRequestCard threadId={threadId} />
                <Composer summary={summary} />
                <UsageFooter threadId={threadId} />
              </>
            ) : (
              <>
                <AutoResumeCards threadId={threadId} />
                <GuestRequestCard threadId={threadId} />
                <ApprovalCard threadId={threadId} />
                <Composer summary={summary} />
                <UsageFooter threadId={threadId} />
              </>
            )}
          </div>
        </div>
      ) : null}

      {terminalMounted ? (
        <TerminalPanel
          key={threadId}
          threadId={threadId}
          host={summary.host}
          cwd={summary.workspace_path}
          hidden={!terminalOpen}
          onClose={toggleTerminal}
        />
      ) : null}

      <RewindDialog
        thread={summary}
        log={log}
        queued={queued}
        itemId={rewindTarget?.itemId ?? null}
        open={rewindTarget !== null}
        onOpenChange={(open) => {
          if (!open) setRewindTarget(null);
        }}
      />

      {isPhone && approvalSheetOpen && firstApproval ? (
        <PhoneApprovalSheet
          thread={summary}
          hostName={hostName}
          approval={firstApproval}
          onClose={() => setApprovalSheetOpen(false)}
        />
      ) : null}
    </main>
  );

  if (isPhone) {
    return (
      <>
        {main}
        {phonePanel ? (
          <PhonePanelSheet
            thread={summary}
            initialTab={phonePanel}
            onClose={() => setPhonePanel(null)}
            onReview={() => setApprovalSheetOpen(true)}
          />
        ) : null}
      </>
    );
  }

  return main;
}
