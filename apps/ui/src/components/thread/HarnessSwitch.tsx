import { ArrowLeftRightIcon, ChevronDownIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Popover } from "@/components/common/Popover";
import { SuggestInput } from "@/components/ui/autocomplete";
import { Listbox, ListboxItem } from "@/components/ui/listbox";
import { useAcpAgents, useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
import { proxyModelsFor, useProxyCatalog } from "@/features/proxy/proxyStore";
import type { DroppedSetting, HarnessKind, RuntimeMode, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessLabel, threadHarnessLabel } from "@/lib/entities";
import { formatList, i18n } from "@/lib/i18n";
import { pushToast, switchThreadHarness, useApp } from "@/lib/store/appStore";
import { activeThread } from "@/lib/thread-selection";
import { modeLabel, newThreadModel, threadModelChoices } from "@/lib/threadControls";
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
      // The same labels the permission picker shows, so "plan" reads as the
      // "Plan" the person chose rather than a slug with a suffix.
      return modeLabel(dropped.value as RuntimeMode);
    case "reasoning_effort":
      return i18n.t("thread:harnessSwitch.droppedEffort", { value: dropped.value });
    case "fast":
      return i18n.t("thread:harnessSwitch.droppedFast");
  }
}

type AgentCatalogue = {
  id: string;
  models: { id: string }[];
};

/** Models the host's threads on `harness` already reported to this daemon. */
function reportedModels(
  harness: HarnessKind,
  host: string,
  threads: readonly ThreadSummary[],
): string[] {
  const reported = threads
    .filter(
      (thread) => thread.harness === harness && thread.host === host && thread.models.length > 0,
    )
    .sort((left, right) => right.last_activity.localeCompare(left.last_activity))[0];
  return reported?.models ?? [];
}

/** The model a target starts on: its own first reported model, or the live one. */
function defaultModelFor(
  target: SwitchTarget,
  agents: AgentCatalogue[],
  host: string,
  threads: readonly ThreadSummary[],
): string {
  if (target.agentId) {
    const instance = agents.find((candidate) => candidate.id === target.agentId);
    return instance?.models[0]?.id ?? "default";
  }
  // A host's live catalogue is the truth: an installed Codex CLI can drop the
  // model the static table names, and the daemon refuses a model the agent does
  // not offer.
  return newThreadModel(target.harness, host, threads);
}

/** Everything a switch panel needs from one thread. */
function targetsFor(
  summary: ThreadSummary,
  rows: { harness: HarnessKind; installed: string | null }[],
  agents: { id: string; name: string; installed: boolean }[],
): SwitchTarget[] {
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
      label: threadHarnessLabel("acp", agent.name || agent.id),
    });
  }
  return found;
}

/**
 * The switch surface's body: where the thread moves, and with which model.
 *
 * Shared by the header's panel and the command palette's sheet, so both offer
 * the same targets, the same live model catalogue and the same refusal text.
 */
function SwitchPanel({ summary, onClose }: { summary: ThreadSummary; onClose: () => void }) {
  const { t } = useTranslation(["thread", "common"]);
  useProxyCatalog();
  useHarnessVersions();
  useAcpAgents();
  const rows = useHarnesses((state) => state.rows);
  const agents = useHarnesses((state) => state.agents);
  const threads = useApp((state) => state.threads);
  const threadValues = useMemo(() => Object.values(threads), [threads]);
  const [targetKey, setTargetKey] = useState<string | null>(null);
  const [modelDraft, setModelDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const initialized = useRef<string | null>(null);

  const targets = useMemo(() => targetsFor(summary, rows, agents), [rows, agents, summary]);

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
        // A host's reported models belong to whichever agent ran there. An ACP
        // target lists its own agent's models, never a sibling agent's.
        target.agentId ? [] : reportedModels(target.harness, summary.host, threadValues),
      )
    : [];

  // Opening the panel selects the first available target; changing target
  // picks that agent's default model, never the old harness's model name. The
  // model stays editable (including empty) while the panel is open.
  useEffect(() => {
    if (!target) return;
    if (initialized.current === target.key) return;
    initialized.current = target.key;
    setTargetKey(target.key);
    setModelDraft(defaultModelFor(target, agents, summary.host, threadValues));
  }, [target, agents, summary.host, threadValues]);

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
    onClose();
    if (result.dropped.length > 0) {
      pushToast({
        variant: "success",
        title: t("harnessSwitch.switched", { harness: target.label }),
        description: t("harnessSwitch.dropped", {
          settings: formatList(result.dropped.map(droppedLabel)),
        }),
      });
    }
  };

  if (targets.length === 0) {
    return (
      <div className="q-tctrl">
        <p className="q-tctrl-note">{t("harnessSwitch.noOther")}</p>
      </div>
    );
  }

  return (
    <div className="q-tctrl">
      <p className="q-tctrl-note">{t("harnessSwitch.note")}</p>

      <Listbox
        aria-label={t("harnessSwitch.harness")}
        activation="manual"
        value={target?.key ?? null}
        onValueChange={(key) => {
          const choice = targets.find((candidate) => candidate.key === key);
          if (!choice) return;
          initialized.current = choice.key;
          setTargetKey(choice.key);
          setModelDraft(defaultModelFor(choice, agents, summary.host, threadValues));
          setError(null);
        }}
      >
        {targets.map((choice) => (
          <ListboxItem key={choice.key} value={choice.key}>
            {choice.label}
          </ListboxItem>
        ))}
      </Listbox>

      <div className="q-tctrl-head">{t("harnessSwitch.model")}</div>
      <form
        className="q-tctrl-model"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <SuggestInput
          aria-label={t("harnessSwitch.modelLabel")}
          value={modelDraft}
          onValueChange={setModelDraft}
          suggestions={models}
          showLabel={t("common:showSuggestions")}
          mono
          className="flex-1"
        />
      </form>

      {error ? (
        <p className="q-tctrl-note q-danger" role="alert">
          {error}
        </p>
      ) : null}

      <div className="q-tctrl-actions">
        <button type="button" className="q-btn ghost" onClick={onClose}>
          {t("common:cancel")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={busy || !target || !modelDraft.trim()}
          onClick={() => void submit()}
        >
          <ArrowLeftRightIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {busy ? t("harnessSwitch.switching") : t("harnessSwitch.switch")}
        </button>
      </div>
    </div>
  );
}

/**
 * The header's harness control: where a thread moves to another provider
 * between turns.
 *
 * On a desktop it is a small anchored panel. A phone gets the same panel in a
 * viewport-bounded sheet with full-size rows, because an anchored panel of this
 * height does not fit above a 375 px screen's composer. Only installed
 * harnesses and configured ACP agents are offered, because the daemon refuses
 * an adapter it cannot start.
 */
export function HarnessSwitch({ summary }: { summary: ThreadSummary }) {
  const { t } = useTranslation("thread");
  const isPhone = useIsPhone();
  const [open, setOpen] = useState(false);
  const trigger = (
    <>
      <span className="sr-only">{t("harnessSwitch.trigger")}</span>
      <span>{threadHarnessLabel(summary.harness, summary.agent_id)}</span>
      <ChevronDownIcon
        className={cn("q-icon s12", open && "q-rot180")}
        strokeWidth={1.5}
        aria-hidden
      />
    </>
  );

  if (isPhone) {
    return (
      <>
        <button
          type="button"
          className="q-harness-switch"
          aria-haspopup="dialog"
          aria-expanded={open}
          onClick={() => setOpen(true)}
        >
          {trigger}
        </button>
        {open ? <HarnessSwitchSheet summary={summary} onClose={() => setOpen(false)} /> : null}
      </>
    );
  }

  return (
    <Popover
      label={t("harnessSwitch.title")}
      align="start"
      width={320}
      open={open}
      onOpenChange={setOpen}
      triggerClassName="q-harness-switch"
      trigger={() => trigger}
    >
      {({ close }) => (
        <div className="q-tctrl">
          <div className="q-tctrl-head">{t("harnessSwitch.title")}</div>
          <SwitchPanel summary={summary} onClose={close} />
        </div>
      )}
    </Popover>
  );
}

/**
 * The switch panel as a modal sheet.
 *
 * The command palette opens this for the thread in view, so the action works
 * from the IDE and every other surface where the thread header is not mounted.
 * On a phone it is a bottom sheet bounded by the viewport; on a desktop it is a
 * centered dialog.
 */
export function HarnessSwitchSheet({
  summary,
  onClose,
}: {
  summary: ThreadSummary;
  onClose: () => void;
}) {
  const { t } = useTranslation(["thread", "common"]);
  const isPhone = useIsPhone();
  const sheet = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    // Modal: take focus so a `/` command that opened it does not edit the
    // composer behind it.
    sheet.current?.focus({ preventScroll: true });
  }, []);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  return (
    <>
      <button
        type="button"
        className={isPhone ? "q-ph-scrim" : "q-switch-scrim"}
        aria-label={t("harnessSwitch.close")}
        onClick={onClose}
      />
      <div
        ref={sheet}
        className={isPhone ? "q-ph-sheet q-switch-sheet" : "q-switch-dialog"}
        role="dialog"
        aria-modal="true"
        aria-label={t("harnessSwitch.title")}
        tabIndex={-1}
      >
        {isPhone ? <div className="q-grab" /> : null}
        <div className="q-switch-head">
          <span className="q-tctrl-head">{t("harnessSwitch.title")}</span>
          <button type="button" className="q-btn ghost" onClick={onClose}>
            {t("common:done")}
          </button>
        </div>
        <SwitchPanel summary={summary} onClose={onClose} />
      </div>
    </>
  );
}

/**
 * The palette's switch surface, mounted once for the focused thread.
 *
 * The command is global, so it cannot rely on the thread header being on
 * screen: in IDE view there is no header to open. This listens for the palette
 * event and opens the sheet for whichever thread the shell is showing.
 */
export function HarnessSwitchCommand() {
  const threads = useApp((state) => state.threads);
  const selectedThreadId = useApp((state) => state.ui.selectedThreadId);
  const [open, setOpen] = useState(false);
  const thread = activeThread(threads, selectedThreadId);

  useEffect(() => {
    const handler = () => setOpen(true);
    window.addEventListener("h4rn:switch-harness", handler);
    return () => window.removeEventListener("h4rn:switch-harness", handler);
  }, []);

  if (!open || !thread) return null;
  return <HarnessSwitchSheet summary={thread} onClose={() => setOpen(false)} />;
}
