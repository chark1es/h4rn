import { ArrowLeftRightIcon, CheckIcon, ChevronDownIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { Popover } from "@/components/common/Popover";
import { useAcpAgents, useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
import { proxyModelsFor, useProxyCatalog } from "@/features/proxy/proxyStore";
import type { DroppedSetting, HarnessKind, ThreadSummary } from "@/gen";
import { harnessLabel, threadHarnessLabel } from "@/lib/entities";
import { pushToast, switchThreadHarness } from "@/lib/store/appStore";
import { newThreadModel, threadModelChoices } from "@/lib/threadControls";
import { cn } from "@/lib/utils";

type SwitchTarget = {
  key: string;
  harness: HarnessKind;
  agentId: string | null;
  label: string;
};

/** A dropped value in the words the thread control already uses. */
function droppedLabel(dropped: DroppedSetting): string {
  switch (dropped.setting) {
    case "runtime_mode":
      return `${dropped.value === "plan" ? "Plan mode" : `${dropped.value} mode`}`;
    case "reasoning_effort":
      return `reasoning effort ${dropped.value}`;
    case "fast":
      return "fast mode";
  }
}

type AgentCatalogue = {
  id: string;
  models: { id: string }[];
};

/** The model a target starts on: its own first reported model, or the table. */
function defaultModelFor(target: SwitchTarget, agents: AgentCatalogue[], host: string): string {
  if (target.agentId) {
    const instance = agents.find((candidate) => candidate.id === target.agentId);
    return instance?.models[0]?.id ?? "default";
  }
  return newThreadModel(target.harness, host, []);
}

/**
 * The thread header's harness control: where a thread moves to another
 * provider between turns.
 *
 * Only installed harnesses and configured ACP agents are offered, because the
 * daemon refuses an adapter it cannot start. A successful switch appears in the
 * transcript as a quiet divider; the control names any setting the new harness
 * had to drop.
 */
export function HarnessSwitch({ summary }: { summary: ThreadSummary }) {
  useProxyCatalog();
  useHarnessVersions();
  useAcpAgents();
  const rows = useHarnesses((state) => state.rows);
  const agents = useHarnesses((state) => state.agents);
  const [open, setOpen] = useState(false);
  const [targetKey, setTargetKey] = useState<string | null>(null);
  const [modelDraft, setModelDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const initialized = useRef<string | null>(null);

  // The palette command opens this control on the thread in view.
  useEffect(() => {
    const handler = () => setOpen(true);
    window.addEventListener("h4rn:switch-harness", handler);
    return () => window.removeEventListener("h4rn:switch-harness", handler);
  }, []);

  const targets = useMemo(() => {
    const found: SwitchTarget[] = [];
    for (const row of rows) {
      if (!row.installed || row.harness === summary.harness) continue;
      found.push({
        key: row.harness,
        harness: row.harness,
        agentId: null,
        label: harnessLabel(row.harness),
      });
    }
    for (const agent of agents) {
      if (!agent.installed) continue;
      if (summary.harness === "acp" && summary.agent_id === agent.id) continue;
      found.push({
        key: `acp:${agent.id}`,
        harness: "acp",
        agentId: agent.id,
        label: `ACP · ${agent.name || agent.id}`,
      });
    }
    return found;
  }, [rows, agents, summary.harness, summary.agent_id]);

  const target = targets.find((candidate) => candidate.key === targetKey) ?? targets[0];
  const agent = target?.agentId
    ? agents.find((candidate) => candidate.id === target.agentId)
    : undefined;

  const models = target
    ? threadModelChoices(
        target.harness,
        modelDraft,
        agent?.models.map((model) => model.id) ?? [],
        proxyModelsFor(target.harness).map((model) => model.id),
      )
    : [];

  // Opening the panel selects the first available target; changing target
  // picks that agent's default model, never the old harness's model name. The
  // model stays editable (including empty) while the panel is open.
  useEffect(() => {
    if (!open || !target) return;
    if (initialized.current === target.key) return;
    initialized.current = target.key;
    setTargetKey(target.key);
    setModelDraft(defaultModelFor(target, agents, summary.host));
  }, [open, target, agents, summary.host]);

  const submit = async () => {
    if (!target || !modelDraft.trim() || busy) return;
    setBusy(true);
    setError(null);
    const result = await switchThreadHarness(
      summary.thread_id,
      target.harness,
      modelDraft.trim(),
      target.agentId,
    );
    setBusy(false);
    if (!result.ok) {
      setError(result.hint ? `${result.message} ${result.hint}` : result.message);
      return;
    }
    setOpen(false);
    if (result.dropped.length > 0) {
      pushToast({
        variant: "success",
        title: `Switched to ${target.label}.`,
        description: `Dropped ${result.dropped.map(droppedLabel).join(", ")}; the new harness has no such setting.`,
      });
    }
  };

  return (
    <Popover
      label="Switch harness"
      align="start"
      width={320}
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) {
          setError(null);
          initialized.current = null;
        }
      }}
      triggerClassName="q-harness-switch"
      trigger={({ open: expanded }) => (
        <>
          <span className="sr-only">Switch harness: </span>
          <span>{threadHarnessLabel(summary.harness, summary.agent_id)}</span>
          <ChevronDownIcon
            className={cn("q-icon s12", expanded && "q-rot180")}
            strokeWidth={1.5}
            aria-hidden
          />
        </>
      )}
    >
      {({ close }) => (
        <div className="q-tctrl">
          <div className="q-tctrl-head">Switch harness</div>
          <p className="q-tctrl-note">
            The thread keeps its history, workspace and every setting the new harness offers. It
            continues from a summary of the conversation.
          </p>

          {targets.length === 0 ? (
            <p className="q-tctrl-note">
              No other harness is installed on this host. Install one in Settings → Harnesses, then
              come back.
            </p>
          ) : (
            <>
              <div role="menu" aria-label="Harness" className="q-tctrl-list">
                {targets.map((choice) => (
                  <button
                    key={choice.key}
                    type="button"
                    role="menuitemradio"
                    aria-checked={choice.key === target?.key}
                    className="q-tctrl-row"
                    onClick={() => {
                      initialized.current = choice.key;
                      setTargetKey(choice.key);
                      setModelDraft(defaultModelFor(choice, agents, summary.host));
                      setError(null);
                    }}
                  >
                    <span className="q-tctrl-check" aria-hidden>
                      {choice.key === target?.key ? (
                        <CheckIcon className="q-icon s12" strokeWidth={2} />
                      ) : null}
                    </span>
                    <span className="grow min-w-0 truncate">{choice.label}</span>
                  </button>
                ))}
              </div>

              <div className="q-tctrl-head">Model</div>
              <form
                className="q-tctrl-model"
                onSubmit={(event) => {
                  event.preventDefault();
                  void submit();
                }}
              >
                <input
                  aria-label="Model for the new harness"
                  list={`h4rn-switch-models-${summary.thread_id}`}
                  value={modelDraft}
                  onChange={(event) => setModelDraft(event.target.value)}
                  className="q-mono"
                />
                <datalist id={`h4rn-switch-models-${summary.thread_id}`}>
                  {models.map((model) => (
                    <option key={model} value={model} />
                  ))}
                </datalist>
              </form>

              {error ? (
                <p className="q-tctrl-note q-danger" role="alert">
                  {error}
                </p>
              ) : null}

              <div className="q-tctrl-actions">
                <button type="button" className="q-btn ghost" onClick={close}>
                  Cancel
                </button>
                <button
                  type="button"
                  className="q-btn primary"
                  disabled={busy || !target || !modelDraft.trim()}
                  onClick={() => void submit()}
                >
                  <ArrowLeftRightIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  {busy ? "Switching…" : "Switch"}
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </Popover>
  );
}
