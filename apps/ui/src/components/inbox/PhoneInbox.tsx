import { ChevronDownIcon, ClockIcon, FolderPlusIcon, SquarePenIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import { StateTabs } from "@/components/inbox/StateTabs";
import { ThreadRow } from "@/components/inbox/ThreadRow";
import { PhoneLargeTitle } from "@/components/shell/PhoneNav";
import { useNow } from "@/hooks/use-now";
import { groupThreads, monogram } from "@/lib/grouping";
import { formatNumber } from "@/lib/i18n";
import { needsAttention, snoozedThreads, tabCounts, threadsForTab } from "@/lib/inbox";
import {
  acknowledgeThread,
  openNewThread,
  openThread,
  setTab,
  setThreadTab,
  snoozeThread,
  togglePin,
  toggleProject,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

/**
 * Phone inbox: the same project grouping as the desktop sidebar, folded into
 * the narrow stacked navigation. Threads are the only content; the desktop
 * footer destinations live in the tab bar.
 */
export function PhoneInbox() {
  const { t } = useTranslation(["sidebar", "common"]);
  const state = useApp((s) => s);
  const now = useNow();
  const [snoozedOpen, setSnoozedOpen] = useState(false);
  const tab = state.ui.tab;
  const loading = state.loadingThreads;
  const counts = useMemo(() => tabCounts(state, now), [state, now]);
  const visible = useMemo(() => threadsForTab(state, tab, now), [state, tab, now]);
  const groups = useMemo(() => groupThreads(visible), [visible]);
  const snoozed = useMemo(() => snoozedThreads(state, now), [state, now]);
  const allEmpty = Object.keys(state.threads).length === 0 && !loading;

  return (
    <>
      <PhoneLargeTitle
        title={t("inbox")}
        actions={
          <button
            type="button"
            className="navbtn"
            aria-label={t("newThread")}
            onClick={() => openNewThread()}
          >
            <SquarePenIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
          </button>
        }
      />
      {allEmpty ? null : (
        <div className="q-ph-seg">
          <StateTabs tab={tab} counts={counts} loading={loading} onSelect={setTab} />
        </div>
      )}
      <div className="q-ph-body">
        {loading && Object.keys(state.threads).length === 0 ? (
          <div className="q-ph-pg" role="status" aria-busy="true" aria-label={t("loadingThreads")}>
            {[68, 52, 76, 60].map((width) => (
              <div key={width} className="q-row">
                <span className="q-row-st">
                  <span className="q-skel" style={{ width: 12, height: 12, borderRadius: "50%" }} />
                </span>
                <span className="q-row-main" style={{ gap: 8, paddingTop: 2 }}>
                  <span className="q-skel" style={{ height: 12, width: `${width}%` }} />
                  <span className="q-skel" style={{ height: 10, width: "44%" }} />
                </span>
              </div>
            ))}
          </div>
        ) : allEmpty ? (
          <div className="q-empty q-side-empty" style={{ paddingTop: 24 }}>
            <h3>{t("empty.noProjects")}</h3>
            <p>{t("empty.noProjectsBodyPhone")}</p>
            <button type="button" className="q-btn secondary" onClick={() => openNewThread()}>
              <FolderPlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("empty.addProject")}
            </button>
          </div>
        ) : groups.length === 0 ? (
          <div className="q-empty q-side-empty" style={{ paddingTop: 24 }}>
            <h3>{t(`empty.${tab}`)}</h3>
            <p>{t("empty.stateBody")}</p>
          </div>
        ) : (
          groups.map((group) => {
            const closed = state.ui.layout.closedProjects[group.key] === true;
            const you = group.threads.some((thread) => needsAttention(thread, now));
            return (
              <section
                key={group.key}
                className="q-ph-pg"
                aria-label={t("projects.groupLabel", { name: group.name, host: group.host })}
              >
                <button
                  type="button"
                  className="q-ph-pg-h"
                  aria-expanded={!closed}
                  onClick={() => toggleProject(group.key)}
                >
                  <span className="q-pmono" aria-hidden="true">
                    {monogram(group.name)}
                  </span>
                  <span className="nm">{group.name}</span>
                  <span className="hn">{state.hosts[group.host]?.name ?? group.host}</span>
                  <span className="grow" />
                  {closed ? (
                    <>
                      {you ? (
                        <span
                          className="q-dot you"
                          role="img"
                          aria-label={t("projects.needsYou")}
                        />
                      ) : null}
                      <span className="ct q-num">{formatNumber(group.threads.length)}</span>
                    </>
                  ) : null}
                  <ChevronDownIcon
                    className={cn("chev q-icon s16", closed && "closed")}
                    strokeWidth={1.5}
                    aria-hidden
                  />
                </button>
                {closed ? null : (
                  <div className="q-rows">
                    {group.threads.map((thread) => (
                      <ThreadRow
                        key={thread.thread_id}
                        thread={thread}
                        host={state.hosts[thread.host]}
                        index={0}
                        compact
                        selected={state.ui.selectedThreadId === thread.thread_id}
                        tabStop={state.ui.selectedThreadId === thread.thread_id}
                        resumeAt={thread.resume_at ?? thread.limit?.reset_at ?? null}
                        now={now}
                        onOpen={() => openThread(thread.thread_id)}
                        onAcknowledge={() => void acknowledgeThread(thread.thread_id)}
                        onSettle={() => void setThreadTab(thread.thread_id, "settled")}
                        onReopen={() => void setThreadTab(thread.thread_id, "done")}
                        onPin={() => void togglePin(thread.thread_id)}
                        onSnooze={(until) => void snoozeThread(thread.thread_id, until)}
                      />
                    ))}
                  </div>
                )}
              </section>
            );
          })
        )}
        {tab === "working" && snoozed.length > 0 ? (
          <button
            type="button"
            className="q-snoozed"
            aria-expanded={snoozedOpen}
            onClick={() => setSnoozedOpen((value) => !value)}
          >
            <ClockIcon className="q-icon s16" strokeWidth={1.5} aria-hidden />
            <span>{t("snoozed.title")}</span>
            <span className="q-num q-faint">{formatNumber(snoozed.length)}</span>
          </button>
        ) : null}
        <div style={{ height: 16 }} />
      </div>
    </>
  );
}
