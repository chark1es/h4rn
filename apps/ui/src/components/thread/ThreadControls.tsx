import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

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
  const { t } = useTranslation(["composer", "common"]);
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
      label={t("controls.label")}
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
          <div className="q-tctrl-head">{t("controls.thread")}</div>
          <p className="q-tctrl-note">
            {t("controls.nextTurn", { harness: harnessLabel(summary.harness) })}
          </p>

          <div className="q-tctrl-head">{t("controls.model")}</div>
          <form
            className="q-tctrl-model"
            onSubmit={(event) => {
              event.preventDefault();
              applyModel();
            }}
          >
            <input
              aria-label={t("controls.model")}
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
              {t("controls.use")}
            </button>
          </form>

          {modes.length > 0 ? (
            <>
              <div className="q-tctrl-head">{t("controls.permissions")}</div>
              <div role="menu" aria-label={t("controls.permissionMode")} className="q-tctrl-list">
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
              <div className="q-tctrl-head">{t("controls.permissions")}</div>
              <p className="q-tctrl-note">
                {t("controls.agentManagesMode", { mode: modeLabel(mode) })}
              </p>
            </>
          )}

          {efforts.length > 0 ? (
            <>
              <div className="q-tctrl-head">{t("controls.effort")}</div>
              <div role="menu" aria-label={t("controls.effort")} className="q-tctrl-list">
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
                    label={t("controls.harnessDefault")}
                    hint={t("controls.harnessDefaultHint")}
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
              <div className="q-tctrl-head">{t("controls.effort")}</div>
              <p className="q-tctrl-note">{effortLabel(summary.reasoning_effort)}</p>
            </>
          ) : null}

          {fast || (summary.fast ?? false) ? (
            <>
              <div className="q-tctrl-head">{t("controls.speed")}</div>
              <Choice
                checked={summary.fast ?? false}
                label={t("common:fast")}
                hint={
                  fast
                    ? t("controls.fastHint")
                    : summary.fast_available === false
                      ? t("controls.noFastTier")
                      : t("controls.unknownFastTier")
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
