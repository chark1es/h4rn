import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { Popover } from "@/components/common/Popover";
import { useHarnesses } from "@/features/harnesses/harnessStore";
import { proxyModelsFor, useProxyCatalog } from "@/features/proxy/proxyStore";
import { useThreadSettings } from "@/features/threads/settings";
import type { ThreadSummary } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import {
  setThreadFast,
  setThreadModel,
  setThreadReasoningEffort,
  setThreadRuntimeMode,
} from "@/lib/store/appStore";
import {
  effortLabel,
  modeLabel,
  nonDefaultSettings,
  threadEffortChoices,
  threadFastSupported,
  threadModeChoices,
  threadModelChoices,
} from "@/lib/threadControls";
import { cn } from "@/lib/utils";

function Choice({
  checked,
  label,
  hint,
  onSelect,
}: {
  checked: boolean;
  label: string;
  hint?: string;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      role="menuitemradio"
      aria-checked={checked}
      className="q-tctrl-row"
      onClick={onSelect}
    >
      <span className="q-tctrl-check" aria-hidden>
        {checked ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
      </span>
      <span className="grow min-w-0">
        <span className="block truncate">{label}</span>
        {hint ? <span className="q-tctrl-hint truncate">{hint}</span> : null}
      </span>
    </button>
  );
}

/**
 * The compact thread controls in the composer: the current model, permission
 * mode and effort, and a picker for each the harness really offers.
 *
 * A harness without a real effort control or fast tier shows no row for it, an
 * ACP agent shows only the modes it advertised, and Fast follows the thread's
 * current model: a model with no tier, or one nothing has described, hides the
 * row unless Fast is already on, so it can be turned off.
 */
export function ThreadControls({ summary }: { summary: ThreadSummary }) {
  useProxyCatalog();
  const agents = useHarnesses((value) => value.agents);
  const threadSettings = useThreadSettings();
  const [open, setOpen] = useState(false);
  const [modelDraft, setModelDraft] = useState(summary.model);
  useEffect(() => setModelDraft(summary.model), [summary.model]);

  // The permission shortcut opens this picker on the thread in view.
  useEffect(() => {
    const handler = () => setOpen(true);
    window.addEventListener("h4rn:permissions", handler);
    return () => window.removeEventListener("h4rn:permissions", handler);
  }, []);

  const agent = summary.agent_id
    ? agents.find((candidate) => candidate.id === summary.agent_id)
    : undefined;
  const modes = threadModeChoices(
    summary.harness,
    agent?.modes.map((mode) => mode.id),
  );
  const efforts = threadEffortChoices(summary.harness, summary.reasoning_efforts ?? []);
  const fast = threadFastSupported(summary);
  const models = threadModelChoices(
    summary.harness,
    summary.model,
    agent?.models.map((model) => model.id) ?? [],
    proxyModelsFor(summary.harness).map((model) => model.id),
    summary.models ?? [],
  );
  const changed = nonDefaultSettings(summary, threadSettings.defaultPermissionMode);
  const mode = summary.runtime_mode ?? "approval_required";

  const applyModel = () => {
    const next = modelDraft.trim();
    if (!next || next === summary.model) return;
    void setThreadModel(summary.thread_id, next).then((ok) => {
      if (ok) setOpen(false);
    });
  };

  return (
    <Popover
      label="Thread settings"
      align="end"
      width={320}
      placement="above"
      open={open}
      onOpenChange={setOpen}
      triggerClassName="q-model"
      trigger={({ open: expanded }) => (
        <>
          <span className="truncate" style={{ maxWidth: 210 }}>
            {summary.model}
          </span>
          {changed.length > 0 ? (
            <span className="q-tctrl-quiet truncate">{changed.join(" · ")}</span>
          ) : null}
          <ChevronDownIcon
            className={cn("q-icon s12", expanded && "q-rot180")}
            strokeWidth={1.5}
            aria-hidden
          />
        </>
      )}
    >
      {() => (
        <div className="q-tctrl">
          <div className="q-tctrl-head">Thread</div>
          <p className="q-tctrl-note">
            {harnessLabel(summary.harness)} · these apply to the next turn.
          </p>

          <div className="q-tctrl-head">Model</div>
          <form
            className="q-tctrl-model"
            onSubmit={(event) => {
              event.preventDefault();
              applyModel();
            }}
          >
            <input
              aria-label="Model"
              list={`h4rn-thread-models-${summary.thread_id}`}
              value={modelDraft}
              onChange={(event) => setModelDraft(event.target.value)}
              className="q-mono"
            />
            <datalist id={`h4rn-thread-models-${summary.thread_id}`}>
              {models.map((model) => (
                <option key={model} value={model} />
              ))}
            </datalist>
            <button
              type="submit"
              className="q-btn secondary"
              disabled={modelDraft.trim() === summary.model}
            >
              Use
            </button>
          </form>

          {modes.length > 0 ? (
            <>
              <div className="q-tctrl-head">Permissions</div>
              <div role="menu" aria-label="Permission mode" className="q-tctrl-list">
                {modes.map((choice) => (
                  <Choice
                    key={choice.value}
                    checked={choice.value === mode}
                    label={choice.label}
                    onSelect={() => {
                      void setThreadRuntimeMode(summary.thread_id, choice.value).then((ok) => {
                        if (ok) setOpen(false);
                      });
                    }}
                  />
                ))}
              </div>
            </>
          ) : (
            <>
              <div className="q-tctrl-head">Permissions</div>
              <p className="q-tctrl-note">{modeLabel(mode)} · the agent manages its own mode.</p>
            </>
          )}

          {efforts.length > 0 ? (
            <>
              <div className="q-tctrl-head">Reasoning effort</div>
              <div role="menu" aria-label="Reasoning effort" className="q-tctrl-list">
                {efforts.map((effort) => (
                  <Choice
                    key={effort}
                    checked={summary.reasoning_effort === effort}
                    label={effortLabel(effort)}
                    onSelect={() => {
                      void setThreadReasoningEffort(summary.thread_id, effort).then((ok) => {
                        if (ok) setOpen(false);
                      });
                    }}
                  />
                ))}
                {summary.reasoning_effort ? (
                  <Choice
                    checked={false}
                    label="Harness default"
                    hint="Use the harness's own setting"
                    onSelect={() => {
                      void setThreadReasoningEffort(summary.thread_id, null).then((ok) => {
                        if (ok) setOpen(false);
                      });
                    }}
                  />
                ) : null}
              </div>
            </>
          ) : summary.reasoning_effort ? (
            <>
              <div className="q-tctrl-head">Reasoning effort</div>
              <p className="q-tctrl-note">{effortLabel(summary.reasoning_effort)}</p>
            </>
          ) : null}

          {fast || (summary.fast ?? false) ? (
            <>
              <div className="q-tctrl-head">Speed</div>
              <Choice
                checked={summary.fast ?? false}
                label="Fast"
                hint={
                  fast
                    ? "Priority service tier, higher usage"
                    : summary.fast_available === false
                      ? "This model has no fast tier"
                      : "No fast tier is known for this model"
                }
                onSelect={() => {
                  void setThreadFast(summary.thread_id, !(summary.fast ?? false)).then((ok) => {
                    if (ok) setOpen(false);
                  });
                }}
              />
            </>
          ) : null}
        </div>
      )}
    </Popover>
  );
}
