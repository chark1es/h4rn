import { ChevronDownIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { Popover } from "@/components/common/Popover";
import { SuggestInput } from "@/components/ui/autocomplete";
import { Listbox, ListboxItem } from "@/components/ui/listbox";
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

const HARNESS_DEFAULT = "";
const FAST = "fast";

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
            <SuggestInput
              aria-label={t("controls.model")}
              value={modelDraft}
              onValueChange={setModelDraft}
              suggestions={models}
              showLabel={t("common:showSuggestions")}
              mono
              className="flex-1"
            />
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
              <Listbox
                aria-label={t("controls.permissionMode")}
                activation="manual"
                value={mode}
                onValueChange={(next) => {
                  if (next === null) return;
                  void setThreadRuntimeMode(summary.thread_id, next).then((ok) => {
                    if (ok) setOpen(false);
                  });
                }}
              >
                {modes.map((choice) => (
                  <ListboxItem key={choice.value} value={choice.value}>
                    {choice.label}
                  </ListboxItem>
                ))}
              </Listbox>
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
              <Listbox
                aria-label={t("controls.effort")}
                activation="manual"
                value={summary.reasoning_effort ?? null}
                onValueChange={(next) => {
                  if (next === null) return;
                  const effort = next === HARNESS_DEFAULT ? null : next;
                  void setThreadReasoningEffort(summary.thread_id, effort).then((ok) => {
                    if (ok) setOpen(false);
                  });
                }}
              >
                {efforts.map((effort) => (
                  <ListboxItem key={effort} value={effort}>
                    {effortLabel(effort)}
                  </ListboxItem>
                ))}
                {summary.reasoning_effort ? (
                  <ListboxItem
                    value={HARNESS_DEFAULT}
                    description={t("controls.harnessDefaultHint")}
                  >
                    {t("controls.harnessDefault")}
                  </ListboxItem>
                ) : null}
              </Listbox>
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
              <Listbox
                aria-label={t("controls.speed")}
                activation="manual"
                deselectable
                value={summary.fast ? FAST : null}
                onValueChange={(next) => {
                  void setThreadFast(summary.thread_id, next === FAST).then((ok) => {
                    if (ok) setOpen(false);
                  });
                }}
              >
                <ListboxItem
                  value={FAST}
                  description={
                    fast
                      ? t("controls.fastHint")
                      : summary.fast_available === false
                        ? t("controls.noFastTier")
                        : t("controls.unknownFastTier")
                  }
                >
                  {t("common:fast")}
                </ListboxItem>
              </Listbox>
            </>
          ) : null}
        </div>
      )}
    </Popover>
  );
}
