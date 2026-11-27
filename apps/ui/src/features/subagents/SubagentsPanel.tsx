import {
  CheckIcon,
  CircleXIcon,
  KeyRoundIcon,
  LoaderIcon,
  PlusIcon,
  RefreshCwIcon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Tip } from "@/components/ui/tooltip";
import type { SubagentState, SubagentView, ThreadSummary } from "@/gen";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed, subagentSeed } from "@/lib/entities";
import { formatTokens } from "@/lib/format";
import { formatNumber, i18n } from "@/lib/i18n";
import { openThread } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { DelegateDialog } from "./DelegateDialog";
import {
  cancelSubagent,
  loadSubagents,
  openDelegate,
  selectSubagent,
  subagentStore,
  useSelectedSubagent,
  useSubagentError,
  useSubagentLoading,
  useSubagentStore,
  useSubagentViews,
  waitingSubagentCount,
} from "./subagentStore";

function stateLabel(view: SubagentView): string {
  return i18n.t(`thread:subagents.state.${view.state}`);
}

function isTerminal(state: SubagentState): boolean {
  return (
    state === "completed" ||
    state === "failed" ||
    state === "cancelled" ||
    state === "budget_exceeded"
  );
}

function elapsedText(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1_000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function progressLine(view: SubagentView): string {
  const tokenText = i18n.t("thread:subagents.tokens", {
    tokens: formatTokens(view.progress.tokens),
  });
  if (view.state === "awaiting_approval") {
    const cap = view.budget.max_turns;
    const turn = view.progress.turns_started + 1;
    return cap
      ? `${i18n.t("thread:subagents.turnOf", { turn, cap })} · ${tokenText}`
      : `${tokenText} · ${i18n.t("thread:subagents.waitingOnYou")}`;
  }
  if (view.state === "failed" || view.state === "budget_exceeded") {
    return view.result?.error ?? stateLabel(view);
  }
  if (view.state === "starting") return view.progress.current ?? stateLabel(view);
  return `${tokenText}${view.progress.current ? ` · ${view.progress.current}` : ""}`;
}

function budgetPercent(view: SubagentView): number {
  const ratios: number[] = [];
  if (view.budget.max_turns && view.budget.max_turns > 0) {
    ratios.push(view.progress.turns_started / view.budget.max_turns);
  }
  if (view.budget.max_tokens && view.budget.max_tokens > 0) {
    ratios.push(view.progress.tokens / view.budget.max_tokens);
  }
  if (view.budget.max_wall_time_secs && view.budget.max_wall_time_secs > 0) {
    ratios.push(view.progress.elapsed_ms / 1_000 / view.budget.max_wall_time_secs);
  }
  if (ratios.length === 0) return 0;
  return Math.min(100, Math.max(0, Math.round(Math.max(...ratios) * 100)));
}

function subagentGlyph(view: SubagentView) {
  if (view.state === "failed" || view.state === "budget_exceeded") {
    return <CircleXIcon className="q-icon s14 q-danger" strokeWidth={1.5} aria-hidden />;
  }
  if (view.state === "completed") {
    return <CheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />;
  }
  if (view.state === "cancelled") {
    return <CircleXIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />;
  }
  return <AgentGlyph glyph={glyphForAgentState(view.agent_state)} size={14} />;
}

function SubagentNode({
  view,
  parentThreadId,
  selected,
  onSelect,
}: {
  view: SubagentView;
  parentThreadId: string;
  selected: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation("thread");
  const percent = budgetPercent(view);
  const waiting = view.state === "awaiting_approval";
  return (
    <button
      type="button"
      className={cn("q-node", "q-sub-node", selected && "sel", view.depth > 1 && "kid2")}
      style={{ ["--d" as string]: view.depth - 1 }}
      aria-pressed={selected}
      onClick={onSelect}
    >
      <span className="gl">{subagentGlyph(view)}</span>
      <div className="bd">
        <div className="t">
          <span>{view.task}</span>
          <span className="q-faint q-num" style={{ fontSize: 12 }}>
            {elapsedText(view.progress.elapsed_ms)}
          </span>
        </div>
        <div className="w">
          <EntityAvatar
            seed={subagentSeed(view.host, parentThreadId, view.subagent_id)}
            kind="subagent"
            label={t("subagents.avatar")}
            size={14}
          />
          <EntityAvatar seed={machineSeed(view.host)} kind="machine" label={view.host} size={14} />
          {view.host} ·{" "}
          <EntityAvatar
            seed={harnessSeed(view.host, view.harness)}
            kind="harness"
            label={harnessLabel(view.harness)}
            size={12}
          />{" "}
          {harnessLabel(view.harness)}
        </div>
        <div className="s q-num">
          {waiting ? (
            <span className="q-accent" style={{ fontWeight: 500 }}>
              {stateLabel(view)}
            </span>
          ) : (
            stateLabel(view)
          )}
          {` · ${progressLine(view)}`}
        </div>
        {percent > 0 && view.state === "running" ? (
          <div
            className="prog"
            role="progressbar"
            aria-label={t("subagents.budgetUsed", {
              percent: formatNumber(percent / 100, { style: "percent" }),
            })}
            aria-valuenow={percent}
            aria-valuemin={0}
            aria-valuemax={100}
          >
            <i style={{ width: `${percent}%` }} />
          </div>
        ) : null}
      </div>
    </button>
  );
}

function Details({ view }: { view: SubagentView }) {
  const { t } = useTranslation("thread");
  const [confirmingId, setConfirmingId] = useState<string | null>(null);
  const confirming = confirmingId === view.subagent_id;
  const cancelling = useSubagentStore((state) => state.cancelling);

  const budget = [
    view.budget.max_turns ? t("subagents.details.turns", { count: view.budget.max_turns }) : null,
    view.budget.max_wall_time_secs
      ? t("subagents.details.minutes", { count: Math.round(view.budget.max_wall_time_secs / 60) })
      : null,
    view.budget.max_tokens
      ? t("subagents.tokens", { tokens: formatTokens(view.budget.max_tokens) })
      : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <div className="q-insp-sec">
      <h3>{view.task}</h3>
      <dl className="q-kv">
        <dt>{t("subagents.details.runsOn")}</dt>
        <dd>
          {view.host} · {harnessLabel(view.harness)}
        </dd>
        <dt>{t("subagents.details.model")}</dt>
        <dd className="q-mono">{view.model || t("subagents.details.parentDefault")}</dd>
        <dt>{t("subagents.details.context")}</dt>
        <dd>
          {view.context.kind === "summary"
            ? t("subagents.details.summary")
            : view.context.kind === "files"
              ? t("subagents.details.files", { count: view.context.paths.length })
              : t("subagents.details.none")}
        </dd>
        <dt>{t("subagents.details.workspace")}</dt>
        <dd>
          {view.isolation === "worktree"
            ? (view.branch ?? t("subagents.details.worktree"))
            : t("subagents.details.shared")}
          <span className="q-faint"> · {view.workspace_path}</span>
        </dd>
        <dt>{t("subagents.details.budget")}</dt>
        <dd className="q-num">{budget || t("subagents.details.defaults")}</dd>
        <dt>{t("subagents.details.progress")}</dt>
        <dd className="q-num">
          {t("subagents.details.turns", { count: view.progress.turns_completed })} ·{" "}
          {t("subagents.tokens", { tokens: formatTokens(view.progress.tokens) })} ·{" "}
          {elapsedText(view.progress.elapsed_ms)}
        </dd>
        <dt>{t("subagents.details.approvals")}</dt>
        <dd>{t("subagents.details.inherited")}</dd>
        <dt>{t("subagents.details.credential")}</dt>
        <dd style={{ display: "flex", alignItems: "center", gap: 4 }}>
          <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />{" "}
          {view.credential.shared_with_parent
            ? t("subagents.details.sharedWithParent", { label: view.credential.label })
            : view.credential.label}
        </dd>
      </dl>
      <div style={{ display: "flex", gap: 6, marginTop: 14, flexWrap: "wrap" }}>
        <button
          type="button"
          className="q-btn secondary"
          disabled={!view.child_thread_id}
          onClick={() => {
            if (view.child_thread_id) openThread(view.child_thread_id);
          }}
        >
          {t("subagents.details.openThread")}
        </button>
        {!isTerminal(view.state) ? (
          confirming ? (
            <>
              <button type="button" className="q-btn ghost" onClick={() => setConfirmingId(null)}>
                {t("subagents.details.keepRunning")}
              </button>
              <button
                type="button"
                className="q-btn danger"
                disabled={cancelling === view.subagent_id}
                onClick={() => void cancelSubagent(view)}
              >
                {cancelling === view.subagent_id ? (
                  <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
                ) : null}
                {t("subagents.details.cancel")}
              </button>
            </>
          ) : (
            <button
              type="button"
              className="q-btn ghost"
              onClick={() => setConfirmingId(view.subagent_id)}
            >
              {t("subagents.details.cancel")}
            </button>
          )
        ) : null}
      </div>
    </div>
  );
}

function SubagentSkeleton() {
  const { t } = useTranslation("thread");
  return (
    <div className="q-tree" role="status" aria-busy="true" aria-label={t("subagents.loading")}>
      {[0, 1, 2].map((row) => (
        <div className="q-node" key={row} style={{ pointerEvents: "none" }}>
          <span className="gl">
            <span className="q-skel" style={{ width: 12, height: 12, borderRadius: "50%" }} />
          </span>
          <div className="bd">
            <span className="q-skel" style={{ height: 11, width: `${68 - row * 8}%` }} />
            <span className="q-skel" style={{ height: 9, width: `${44 + row * 5}%` }} />
          </div>
        </div>
      ))}
    </div>
  );
}

/** Desktop right-panel tab: live delegation tree plus the selected child. */
export function SubagentsPanel({ thread }: { thread: ThreadSummary }) {
  const { t } = useTranslation(["thread", "common"]);
  const views = useSubagentViews(thread.thread_id);
  const loading = useSubagentLoading(thread.thread_id);
  const error = useSubagentError(thread.thread_id);
  const selected = useSelectedSubagent();
  const waiting = waitingSubagentCount(thread.thread_id);

  useEffect(() => {
    void loadSubagents(thread.thread_id);
  }, [thread.thread_id]);

  useEffect(() => {
    if (views.length === 0) {
      if (selected) selectSubagent(null);
      return;
    }
    if (!selected || !views.some((view) => view.subagent_id === selected.subagent_id)) {
      selectSubagent(views[0]?.subagent_id ?? null);
    }
  }, [views, selected]);

  return (
    <>
      <div className="q-rp-bar">
        <span className="q-faint" style={{ fontSize: 12 }}>
          {views.length === 0
            ? t("subagents.none")
            : waiting > 0
              ? `${t("subagents.count", { count: views.length })} · ${t("subagents.waiting", { count: waiting })}`
              : t("subagents.count", { count: views.length })}
        </span>
        <span className="grow" />
        <Tip label={t("subagents.refresh")}>
          <button
            type="button"
            className="q-ibtn"
            aria-label={t("subagents.refresh")}
            onClick={() => void loadSubagents(thread.thread_id, true)}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
        </Tip>
        <button type="button" className="q-btn ghost" onClick={openDelegate}>
          <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("subagents.delegate")}
        </button>
      </div>
      <div className="q-rp-body q-scroll">
        {error ? (
          <div className="q-empty q-sub-empty">
            <h3>{t("subagents.loadFailed")}</h3>
            <p>{error}</p>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => void loadSubagents(thread.thread_id, true)}
            >
              {t("common:retry")}
            </button>
          </div>
        ) : loading && views.length === 0 ? (
          <SubagentSkeleton />
        ) : views.length === 0 ? (
          <div className="q-empty q-sub-empty">
            <h3>{t("subagents.emptyTitle")}</h3>
            <p>{t("subagents.emptyBody", { harness: harnessLabel(thread.harness) })}</p>
            <button type="button" className="q-btn secondary" onClick={openDelegate}>
              <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("subagents.delegate")}
            </button>
          </div>
        ) : (
          <>
            <div className="q-tree">
              <div className="q-node parent">
                <span className="gl">
                  <AgentGlyph glyph={glyphForAgentState(thread.agent_state)} size={14} />
                </span>
                <div className="bd">
                  <div className="t">
                    <span>{thread.title}</span>
                    <span className="q-faint q-num" style={{ fontSize: 12 }}>
                      {t("subagents.parent")}
                    </span>
                  </div>
                  <div className="w">
                    <EntityAvatar
                      seed={machineSeed(thread.host)}
                      kind="machine"
                      label={thread.host}
                      size={14}
                    />
                    {thread.host} ·{" "}
                    <EntityAvatar
                      seed={harnessSeed(thread.host, thread.harness)}
                      kind="harness"
                      label={harnessLabel(thread.harness)}
                      size={12}
                    />{" "}
                    {harnessLabel(thread.harness)}
                  </div>
                  <div className="s q-num">{glyphForAgentState(thread.agent_state).label}</div>
                </div>
              </div>
              <div className="q-kids">
                {views.map((view) => (
                  <SubagentNode
                    key={view.subagent_id}
                    view={view}
                    parentThreadId={thread.thread_id}
                    selected={selected?.subagent_id === view.subagent_id}
                    onSelect={() => selectSubagent(view.subagent_id)}
                  />
                ))}
              </div>
            </div>
            {selected ? <Details view={selected} /> : null}
          </>
        )}
      </div>
      <DelegateDialog thread={thread} />
    </>
  );
}

export function subagentCount(threadId: string): number {
  return Object.keys(subagentStore.getState().byThread[threadId]?.views ?? {}).length;
}
