import { ArrowRightIcon, CheckIcon, GitBranchIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import type * as React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ENTER, Kbd, MOD } from "@/components/ui/kbd";
import { Textarea } from "@/components/ui/textarea";
import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import { acpModeChoices, acpModelChoices, acpPickerOptions } from "@/features/harnesses/acpForm";
import {
  probeAcpAgent,
  useAcpAgents,
  useHarnesses,
  useHarnessVersions,
} from "@/features/harnesses/harnessStore";
import { useThreadModels } from "@/features/harnesses/use-thread-models";
import { useProjects } from "@/features/projects/store";
import {
  proxyDefaultModel,
  proxyModelsFor,
  proxyOverrideFor,
  useProxy,
  useProxyCatalog,
} from "@/features/proxy/proxyStore";
import { useThreadSettings } from "@/features/threads/settings";
import type { HarnessKind, HostId, RuntimeMode } from "@/gen";
import { DEFAULT_NEW_THREAD_WORKSPACE, drafts } from "@/lib/drafts";
import { HARNESS_LABELS, HARNESS_SLUGS, threadHarnessLabel } from "@/lib/entities";
import { detectedHarnesses, HARNESS_MODELS } from "@/lib/models";
import { NewThreadDraftSession, type NewThreadDraftSnapshot } from "@/lib/newThreadDraft";
import { closeNewThread, createThread, useApp } from "@/lib/store/appStore";
import { defaultThreadHostId, localOnlyHostOptions } from "@/lib/thread-hosts";
import {
  effortLabel,
  newThreadFastSupported,
  newThreadModel,
  threadEffortChoices,
  threadModeChoices,
} from "@/lib/threadControls";
import { cn } from "@/lib/utils";

const DEFAULT_MODELS = Object.fromEntries(
  Object.entries(HARNESS_MODELS).map(([harness, info]) => [harness, info.default]),
) as Record<HarnessKind, string>;

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/** How each harness names its thinking control, under the model field. */
const THINKING_KEYS = {
  claude_code: "newThread.thinking.effort",
  codex: "newThread.thinking.reasoningEffort",
  antigravity: "newThread.thinking.modelLevel",
} as const;

/** A path with one separator style and no trailing separators, for comparison. */
function folderKey(path: string): string {
  return path.trim().replace(/\\/g, "/").replace(/\/+$/, "");
}

/**
 * A typed path with a leading `~` expanded against the host's home folder, the
 * way the daemon expands it. An unknown home leaves the path alone: guessing
 * could offer a worktree for a folder that is not the project.
 */
function expandHome(path: string, home: string | null): string {
  const trimmed = path.trim();
  if (!home) return trimmed;
  if (trimmed === "~") return folderKey(home);
  if (trimmed.startsWith("~/")) return folderKey(`${home}/${trimmed.slice(2)}`);
  return trimmed;
}

function Field({
  label,
  htmlFor,
  className,
  children,
}: {
  label: React.ReactNode;
  htmlFor: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <div className={cn("flex min-w-0 flex-col gap-1.5", className)}>
      <label htmlFor={htmlFor} className="text-xs font-medium text-fg-2">
        {label}
      </label>
      {children}
    </div>
  );
}

function FieldNote({ className, ...props }: React.ComponentProps<"span">) {
  return <span className={cn("text-xs text-muted-foreground", className)} {...props} />;
}

export function NewThreadDialog() {
  const { t } = useTranslation(["dialogs", "common"]);
  const open = useApp((state) => state.ui.newThreadOpen);
  const hosts = useApp((state) => state.hosts);
  const localHostId = useApp((state) => state.localHostId);
  const connection = useApp((state) => state.connection.status);
  useProxyCatalog();
  useHarnessVersions();
  useAcpAgents();
  const agents = useHarnesses((value) => value.agents);
  const probes = useHarnesses((value) => value.probes);
  const probeErrors = useHarnesses((value) => value.probeErrors);
  const probingAgents = useHarnesses((value) => value.probing);
  const harnessRows = useHarnesses((value) => (value.loaded ? value.rows : null));
  const threadList = useApp((state) => state.threads);
  const threadValues = useMemo(() => Object.values(threadList), [threadList]);
  const installed = useMemo(
    () => harnessRows?.filter((row) => row.installed !== null).map((row) => row.harness) ?? null,
    [harnessRows],
  );
  useProxy((state) => state.profiles);
  useProxy((state) => state.catalogs);

  const onlineHosts = useMemo(
    () =>
      Object.values(hosts)
        .filter((host) => host.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );

  const [host, setHost] = useState<HostId | "">("");
  /** The host the draft session is bound to; empty until one resolves. */
  const [boundHost, setBoundHost] = useState<HostId | "">("");
  const [harness, setHarness] = useState<HarnessKind>("claude_code");
  const [agentId, setAgentId] = useState<string | null>(null);
  const [model, setModel] = useState(DEFAULT_MODELS.claude_code);
  const [workspace, setWorkspace] = useState(DEFAULT_NEW_THREAD_WORKSPACE);
  const threadSettings = useThreadSettings();
  const [runtimeMode, setRuntimeMode] = useState<RuntimeMode>(threadSettings.defaultPermissionMode);
  const [reasoningEffort, setReasoningEffort] = useState<string>("default");
  const [fast, setFast] = useState(false);
  /** Run in a new checkout instead of the project's main folder. */
  const [worktreeEnabled, setWorktreeEnabled] = useState(false);
  /** Optional branch name; empty lets the daemon generate one. */
  const [worktreeBranch, setWorktreeBranch] = useState("");
  const [prompt, setPrompt] = useState("");
  const [starting, setStarting] = useState(false);
  /** Host + folder slot the prompt box is showing; one per dialog open. */
  const sessionRef = useRef<NewThreadDraftSession | null>(null);
  /** The preset is applied once per open; later host-list refreshes must not reset the form. */
  const appliedPreset = useRef(false);
  /** The first host and its draft are adopted once per open. */
  const hostResolved = useRef(false);
  /** The user picked the host; the dialog must not rebind it underneath them. */
  const hostChosen = useRef(false);
  /** Start is being pressed: its blur must not swap the text before it submits. */
  const startPressed = useRef(false);
  const preset = useApp((state) => state.ui.newThreadPreset);

  const draftSession = useCallback((): NewThreadDraftSession => {
    sessionRef.current ??= new NewThreadDraftSession(drafts);
    return sessionRef.current;
  }, []);

  const showDraft = useCallback((draft: NewThreadDraftSnapshot) => {
    setHost(draft.host);
    setBoundHost(draft.host);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, []);

  /** A host is usable only while it is online and this daemon allows it. */
  const hostEligible = useCallback(
    (hostId: HostId | "") =>
      hostId !== "" &&
      onlineHosts.some((candidate) => candidate.host_id === hostId) &&
      (localHostId === null || hostId === localHostId),
    [onlineHosts, localHostId],
  );

  useEffect(() => {
    if (!open) {
      sessionRef.current = null;
      appliedPreset.current = false;
      hostResolved.current = false;
      hostChosen.current = false;
      startPressed.current = false;
      // Nothing is bound until a host resolves again, so the box and Start
      // must not stay enabled from the previous open.
      setHost("");
      setBoundHost("");
      setWorkspace(DEFAULT_NEW_THREAD_WORKSPACE);
      setPrompt("");
      setWorktreeEnabled(false);
      setWorktreeBranch("");
      return;
    }
    if (sessionRef.current === null) {
      sessionRef.current = new NewThreadDraftSession(drafts, { workspace: preset?.workspace });
    }
    // A new thread can only run on this daemon's host for now; prefer it even
    // when another online host sorts first. Recompute it whenever the host
    // list or the daemon's own host changes, so Start never points at a host
    // this daemon would reject.
    const defaultId = defaultThreadHostId(onlineHosts, localHostId);
    const first = defaultId
      ? onlineHosts.find((candidate) => candidate.host_id === defaultId)
      : undefined;
    if (first) {
      const session = draftSession();
      if (!hostResolved.current) {
        hostResolved.current = true;
        showDraft(session.adoptHost(first.host_id));
        const nextHarness =
          detectedHarnesses(HARNESSES, installed, first.harnesses)[0] ?? "claude_code";
        setHarness(nextHarness);
        setAgentId(null);
        setModel(
          proxyDefaultModel(nextHarness) ??
            newThreadModel(nextHarness, first.host_id, threadValues),
        );
      } else if (
        // A host the user picked stays only while it is still a valid choice;
        // when it is gone (another daemon, or the host went offline) fall back
        // to the default host and show that slot's draft.
        !hostEligible(session.snapshot().host) ||
        (!hostChosen.current && session.snapshot().host !== first.host_id)
      ) {
        hostChosen.current = false;
        showDraft(session.rebindHost(first.host_id));
      }
    }
    setStarting(false);
  }, [
    open,
    onlineHosts,
    localHostId,
    installed,
    preset,
    draftSession,
    showDraft,
    hostEligible,
    threadValues,
  ]);

  // "New thread in <project>" starts in that project's folder.
  useEffect(() => {
    if (!open) {
      appliedPreset.current = false;
      return;
    }
    if (!preset || appliedPreset.current) return;
    appliedPreset.current = true;
    const session = draftSession();
    if (hosts[preset.host]?.online && preset.host !== session.snapshot().host) {
      hostChosen.current = true;
      showDraft(session.selectHost(preset.host));
    }
    showDraft(session.commitWorkspace(preset.workspace));
  }, [open, preset, hosts, draftSession, showDraft]);

  // The live catalogue can arrive after the dialog opens; adopt its default
  // only while the user has not chosen a model of their own. The static (or,
  // once a thread reported one on this host, the live) default counts as
  // unchosen; anything else is a value the person typed and is left alone.
  // This is the field's one source of truth, so a stale static default can
  // never be what Start sends.
  const proxyActive = proxyOverrideFor(harness);
  const proxyModels = proxyModelsFor(harness);
  const proxyProfileId = proxyActive?.profile.id ?? null;
  const proxyDefault = proxyDefaultModel(harness);
  const harnessDefault = newThreadModel(harness, boundHost, threadValues);
  const nativeModels = useThreadModels(
    boundHost,
    harness,
    open && connection === "online" && !proxyActive,
  );
  const defaultModel =
    proxyDefault ??
    (nativeModels && !nativeModels.includes(harnessDefault) ? "default" : harnessDefault);
  useEffect(() => {
    if (!open) return;
    setModel((current) =>
      current === DEFAULT_MODELS[harness] ||
      ((proxyProfileId !== null || nativeModels !== null) && current === harnessDefault)
        ? defaultModel
        : current,
    );
  }, [open, harness, harnessDefault, defaultModel, proxyProfileId, nativeModels]);

  const selectedHost = boundHost ? hosts[boundHost] : undefined;
  const harnessChoices = detectedHarnesses(HARNESSES, installed, selectedHost?.harnesses);
  const selectedAgent = agentId ? agents.find((agent) => agent.id === agentId) : undefined;
  const probe = agentId ? probes[agentId] : undefined;
  const probeError = agentId ? probeErrors[agentId] : undefined;
  const probing = agentId ? probingAgents[agentId] === true : false;
  const modelInfo = HARNESS_MODELS[harness];
  // Built-in harnesses and configured agents share one picker. A value is a
  // harness kind, or `acp:<instance>` for a local command.
  const harnessValue = harness === "acp" && agentId ? `acp:${agentId}` : harness;

  // If detection settles and the chosen harness is not among those found,
  // move to one that is rather than leave an undetected harness selected.
  const firstChoice = harnessChoices[0];
  const agentGone = harness === "acp" && !selectedAgent;
  // Until a host is bound the dialog has not chosen a harness yet, so the
  // first host's harness (picked above) is not "missing".
  const harnessMissing =
    boundHost !== "" &&
    harness !== "acp" &&
    harnessChoices.length > 0 &&
    !harnessChoices.includes(harness);
  useEffect(() => {
    if (!open) return;
    if (!harnessMissing && !agentGone) return;
    if (!firstChoice) return;
    setHarness(firstChoice);
    setAgentId(null);
    setModel(
      proxyDefaultModel(firstChoice) ?? newThreadModel(firstChoice, boundHost, threadValues),
    );
  }, [open, harnessMissing, agentGone, firstChoice, boundHost, threadValues]);

  // The model list comes from the agent's own handshake. Probe once when the
  // picker selects it; the daemon starts the command once and stops it. A
  // device without admin cannot probe, and the failure is remembered so the
  // picker does not retry on every render or selection. One probe at a time:
  // while a handshake is in flight a second start would only race it.
  useEffect(() => {
    if (!open || !agentId || probes[agentId] || probeErrors[agentId]) return;
    if (probingAgents[agentId] === true) return;
    void probeAcpAgent(agentId);
  }, [open, agentId, probes, probeErrors, probingAgents]);

  // The probe fills from an admin device; every device still sees the last
  // successful result the daemon stored in the list. A failed probe (a rejected
  // request, or an answer that carries an error) falls back to that stored
  // result instead of emptying the picker.
  const probeFailed = Boolean(probeError) || Boolean(probe?.error);
  const agentModels = acpModelChoices(probe, probeFailed, selectedAgent?.models);
  // Only a mode h4rn can actually select is offered. An agent that names its
  // own modes is left alone: the dialog says so instead of promising an
  // "Approval required" mode the agent never advertised. The same mapping the
  // composer uses reads the agent's ids, so `acceptEdits` and
  // `bypassPermissions` are the modes they name, and a plan mode is offered
  // wherever the harness or agent really has one.
  const agentModes = acpModeChoices(probe, probeFailed, selectedAgent?.modes);
  const runtimeModeChoices =
    harness === "acp" ? threadModeChoices("acp", agentModes) : threadModeChoices(harness);
  const modeManagedByAgent = harness === "acp" && runtimeModeChoices.length === 0;
  // Open on this device's default for new threads; a harness that cannot honour
  // it falls back to the first mode it offers below.
  useEffect(() => {
    if (!open) return;
    setRuntimeMode(threadSettings.defaultPermissionMode);
  }, [open, threadSettings.defaultPermissionMode]);
  // Effort and fast belong to one harness: switching harnesses keeps a value
  // the new one offers and otherwise returns to its default.
  const effortKey = threadEffortChoices(harness).join(",");
  useEffect(() => {
    const offered = effortKey.split(",").filter(Boolean);
    setReasoningEffort((current) =>
      current === "default" || offered.includes(current) ? current : "default",
    );
    setFast(false);
  }, [effortKey]);
  const efforts = threadEffortChoices(harness);
  // Fast belongs to the chosen model: the threads on this host are the only
  // per-model answers this device has, so an unknown model shows no row.
  const fastAvailable = newThreadFastSupported(harness, boundHost, model, threadValues);
  useEffect(() => {
    // Never carry Fast into a model that does not offer it: the daemon would
    // refuse the start, and the row is hidden so it could not be turned off.
    if (!fastAvailable) setFast(false);
  }, [fastAvailable]);
  useEffect(() => {
    if (!open) return;
    // An agent that manages its own mode gets no choice from this dialog, so
    // don't carry over one picked for another harness.
    if (modeManagedByAgent) {
      if (runtimeMode !== "approval_required") setRuntimeMode("approval_required");
      return;
    }
    if (runtimeModeChoices.length === 0) return;
    if (runtimeModeChoices.some((choice) => choice.value === runtimeMode)) return;
    const first = runtimeModeChoices[0];
    if (first) setRuntimeMode(first.value);
  }, [open, modeManagedByAgent, runtimeMode, runtimeModeChoices]);
  // A fixed working directory is where the agent runs, whatever path the
  // thread workspace would otherwise be: show it before Start and use it.
  const fixedWorkspace =
    harness === "acp" && selectedAgent?.working_dir.kind === "fixed_path"
      ? selectedAgent.working_dir.path
      : null;
  const effectiveWorkspace = fixedWorkspace ?? workspace;
  // A worktree needs a repository, so the option is offered only for a
  // registered git project on this host. The daemon still checks again and
  // answers honestly when a folder is not one.
  const projects = useProjects((value) => value.projects);
  const hostHome = useProjects((value) => value.home);
  const workspacePath = expandHome(effectiveWorkspace, hostHome);
  const gitProject =
    fixedWorkspace === null &&
    projects.some((project) => {
      if (!project.git || project.host !== boundHost) return false;
      // The daemon resolves the folder to its repository's top level, so a
      // subfolder of a registered project is a valid place to start one too.
      const projectPath = folderKey(project.path);
      const folder = folderKey(workspacePath);
      return folder === projectPath || folder.startsWith(`${projectPath}/`);
    });
  useEffect(() => {
    if (!gitProject) {
      setWorktreeEnabled(false);
      setWorktreeBranch("");
    }
  }, [gitProject]);
  const offline = connection !== "online";
  const modelChoices = [
    ...new Set([
      ...(harness === "acp"
        ? agentModels
        : proxyActive
          ? [defaultModel, ...proxyModels.map((candidate) => candidate.id)]
          : nativeModels
            ? ["default", ...nativeModels]
            : modelInfo.models),
    ]),
  ];
  const attachments = useAttachmentDrafts("new-thread", {
    enabled: !offline && Boolean(boundHost),
    disabledReason: offline ? t("newThread.reconnectToAttach") : t("newThread.pickHostFirst"),
  });

  useAttachShortcut(attachments.openPicker, open && !offline && Boolean(boundHost));

  useEffect(() => {
    if (!open) attachments.clear();
    // Clearing on close drops stale drafts between dialog sessions.
  }, [open, attachments.clear]);

  const commitWorkspace = useCallback(() => {
    const draft = draftSession().commitWorkspace(workspace);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, [draftSession, workspace]);

  const submit = async () => {
    // A press is over by the time Start runs; its flag must not outlive it.
    startPressed.current = false;
    if (starting || attachments.blockedReason) return;
    // Cmd+Enter with no host yet: nothing has been committed or saved.
    const session = draftSession();
    const draft = session.submission(workspace);
    if (!draft || !hostEligible(draft.host)) return;
    if (!draft.text.trim() && attachments.attachments.length === 0) return;
    // The folder on screen is the one the request runs in, but the visible
    // text stays where it is: Start never swaps the box for another slot.
    setWorkspace(draft.workspace);
    setStarting(true);
    const ok = await createThread({
      host: draft.host,
      harness,
      agentId: harness === "acp" ? agentId : null,
      // A fixed directory replaces the thread workspace: the thread row and
      // Files/Changes must name where the agent really runs.
      workspacePath: fixedWorkspace ?? draft.workspace,
      model,
      prompt: draft.text.trim(),
      attachments: attachments.attachments,
      runtimeMode,
      reasoningEffort: reasoningEffort === "default" ? null : reasoningEffort,
      fast,
      worktree:
        worktreeEnabled && gitProject
          ? worktreeBranch.trim() === ""
            ? true
            : worktreeBranch.trim()
          : null,
    });
    setStarting(false);
    if (ok) {
      // Only the text's own slot, and only while it still holds the revision
      // that was sent: newer text from another tab survives.
      session.clearSubmitted(draft);
      // The folder on screen was used, even if the field was never committed.
      drafts.writeLastWorkspace(draft.host, draft.workspace);
      closeNewThread();
      setPrompt("");
      attachments.clear();
    }
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeNewThread())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("newThread.title")}</DialogTitle>
          <DialogDescription>{t("newThread.description")}</DialogDescription>
        </DialogHeader>

        <DialogBody className="gap-4">
          <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
            <Field htmlFor="new-thread-host" label={t("fields.host")}>
              <ChipSelect
                id="new-thread-host"
                block
                label={t("fields.host")}
                placeholder={t("fields.noOnlineHosts")}
                value={host}
                options={localOnlyHostOptions(
                  onlineHosts,
                  localHostId,
                  t("newThread.hostLocalOnly"),
                )}
                onChange={(value) => {
                  if (value === "") return;
                  const nextHost = hosts[value];
                  const nextHarness = detectedHarnesses(
                    HARNESSES,
                    installed,
                    nextHost?.harnesses,
                  )[0];
                  // Reselecting the host that is already bound changes nothing:
                  // ChipSelect fires on every pick, including the current one, and
                  // the harness and model the person chose there (even one that
                  // is not the host's first harness) must survive it. A real host
                  // change starts from what that host's live catalogue offers,
                  // never the static table.
                  const hostChanged = value !== boundHost;
                  hostChosen.current = true;
                  showDraft(draftSession().selectHost(value));
                  if (nextHarness && harness !== "acp" && hostChanged) {
                    setHarness(nextHarness);
                    setAgentId(null);
                    setModel(
                      proxyDefaultModel(nextHarness) ??
                        newThreadModel(nextHarness, value, threadValues),
                    );
                  }
                }}
              />
            </Field>

            <Field htmlFor="new-thread-harness" label={t("fields.harness")}>
              <ChipSelect
                id="new-thread-harness"
                block
                label={t("fields.harness")}
                placeholder={t("newThread.noHarness")}
                value={
                  harnessValue === harness && !harnessChoices.includes(harness) ? "" : harnessValue
                }
                options={acpPickerOptions(
                  harnessChoices.map((option) => ({
                    value: option as string,
                    label: HARNESS_LABELS[option],
                  })),
                  agents,
                )}
                onChange={(next) => {
                  if (next.startsWith("acp:")) {
                    const id = next.slice(4);
                    setHarness("acp");
                    setAgentId(id);
                    setModel("default");
                    return;
                  }
                  const kind = next as HarnessKind;
                  setHarness(kind);
                  setAgentId(null);
                  setModel(
                    proxyDefaultModel(kind) ?? newThreadModel(kind, boundHost, threadValues),
                  );
                }}
              />
            </Field>

            <Field htmlFor="new-thread-model" label={t("fields.model")} className="sm:col-span-2">
              <Input
                id="new-thread-model"
                value={model}
                onChange={(event) => setModel(event.target.value)}
                list={`h4rn-models-${harness}`}
                className="font-mono"
              />
              <datalist id={`h4rn-models-${harness}`}>
                {modelChoices.map((option) => (
                  <option key={option} value={option} />
                ))}
              </datalist>
              {harness === "acp" ? (
                <FieldNote>
                  {probing
                    ? t("newThread.agent.readingModels")
                    : probe?.error
                      ? probe.error
                      : probeError
                        ? t("newThread.agent.probeFailed", { error: probeError })
                        : agentModels.length > 0
                          ? t("newThread.agent.models", {
                              count: agentModels.length,
                              agent: selectedAgent?.name ?? t("newThread.agent.theAgent"),
                            })
                          : selectedAgent && !selectedAgent.installed
                            ? t("newThread.agent.notInstalled")
                            : t("newThread.agent.noModels")}
                </FieldNote>
              ) : modelInfo.thinking && efforts.length === 0 && harness in THINKING_KEYS ? (
                <FieldNote>{t(THINKING_KEYS[harness as keyof typeof THINKING_KEYS])}</FieldNote>
              ) : null}
              {proxyActive ? (
                <FieldNote>
                  {t("newThread.proxyModels", {
                    count: proxyModels.length,
                    profile: proxyActive.profile.name,
                  })}
                </FieldNote>
              ) : null}
            </Field>

            {efforts.length > 0 ? (
              <Field htmlFor="new-thread-effort" label={t("newThread.effort")}>
                <ChipSelect
                  id="new-thread-effort"
                  block
                  label={t("newThread.effort")}
                  value={reasoningEffort}
                  options={[
                    { value: "default", label: t("common:default") },
                    ...efforts.map((effort) => ({ value: effort, label: effortLabel(effort) })),
                  ]}
                  onChange={setReasoningEffort}
                />
                <FieldNote>{t("newThread.effortHint")}</FieldNote>
              </Field>
            ) : null}
            {fastAvailable ? (
              <Field htmlFor="new-thread-fast" label={t("newThread.speed")}>
                <button
                  type="button"
                  id="new-thread-fast"
                  className="q-switch"
                  role="switch"
                  aria-checked={fast}
                  aria-label={t("newThread.fastMode")}
                  onClick={() => setFast((value) => !value)}
                />
                <FieldNote>{t("newThread.fastHint")}</FieldNote>
              </Field>
            ) : null}

            <Field htmlFor="new-thread-mode" label={t("newThread.mode")} className="sm:col-start-1">
              {modeManagedByAgent ? (
                <>
                  <span id="new-thread-mode" className="flex h-7 items-center text-sm text-fg-2">
                    {t("newThread.modeManaged")}
                  </span>
                  <FieldNote>
                    {probing ? t("newThread.readingModes") : t("newThread.agentDecides")}
                  </FieldNote>
                </>
              ) : (
                <>
                  <ChipSelect
                    id="new-thread-mode"
                    block
                    label={t("newThread.mode")}
                    value={runtimeMode}
                    options={runtimeModeChoices}
                    onChange={setRuntimeMode}
                  />
                  <FieldNote>{t("newThread.modeLater")}</FieldNote>
                </>
              )}
            </Field>

            <Field htmlFor="new-thread-workspace" label={t("newThread.where")}>
              <Input
                id="new-thread-workspace"
                value={effectiveWorkspace}
                disabled={fixedWorkspace !== null}
                onChange={(event) => setWorkspace(event.target.value)}
                onBlur={() => {
                  // Pressing Start blurs this field on the way to the button.
                  // Start sends what is on screen to the folder as typed; it
                  // must not swap the box to that folder's draft first.
                  if (startPressed.current) return;
                  commitWorkspace();
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    commitWorkspace();
                  }
                }}
                className="font-mono"
                placeholder="~/code/api"
              />
              <FieldNote className="inline-flex items-center gap-1.5">
                <GitBranchIcon className="size-3" aria-hidden />
                {fixedWorkspace
                  ? t("newThread.whereNote.fixed")
                  : gitProject
                    ? t("newThread.whereNote.git")
                    : t("newThread.whereNote.plain")}
              </FieldNote>
            </Field>
          </div>

          {gitProject ? (
            <div className="flex flex-col gap-1.5">
              <label className="q-addp-check">
                <input
                  type="checkbox"
                  className="sr-only"
                  checked={worktreeEnabled}
                  onChange={(event) => {
                    setWorktreeEnabled(event.target.checked);
                    if (!event.target.checked) setWorktreeBranch("");
                  }}
                />
                <span className={cn("q-check", worktreeEnabled && "on")} aria-hidden>
                  {worktreeEnabled ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
                </span>
                {t("newThread.worktree.toggle")}
              </label>
              {worktreeEnabled ? (
                <Input
                  id="new-thread-worktree-branch"
                  value={worktreeBranch}
                  onChange={(event) => setWorktreeBranch(event.target.value)}
                  placeholder={t("newThread.worktree.branchPlaceholder")}
                  className="max-w-[360px] font-mono"
                  spellCheck={false}
                  aria-label={t("newThread.worktree.branchLabel")}
                />
              ) : null}
              <FieldNote>
                {worktreeEnabled
                  ? worktreeBranch.trim()
                    ? t("newThread.worktree.named", {
                        branch: worktreeBranch.trim(),
                        workspace: effectiveWorkspace,
                      })
                    : t("newThread.worktree.daemonNames")
                  : t("newThread.worktree.off")}
              </FieldNote>
            </div>
          ) : null}

          <div className="flex flex-col gap-1">
            <div className="flex items-center justify-between">
              <label htmlFor="new-thread-task" className="text-xs font-medium text-fg-2">
                {t("fields.task")}
                {prompt.trim() ? (
                  <span className="ml-2 font-normal text-muted-foreground tabular-nums">
                    {t("newThread.chars", { count: prompt.trim().length })}
                  </span>
                ) : null}
              </label>
              <button
                type="button"
                disabled={offline || !boundHost}
                onClick={attachments.openPicker}
                aria-label={t("newThread.attachFiles")}
                title={offline ? t("newThread.reconnectToAttach") : t("newThread.attachTitle")}
                className="hit-area relative -my-1 inline-flex h-6 items-center gap-1.5 rounded-md px-1.5 text-xs text-fg-2 transition-colors duration-micro hover:bg-accent hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
              >
                <PaperclipIcon className="size-3.5" aria-hidden />
                {attachments.attachments.length > 0
                  ? t("newThread.attached", { count: attachments.attachments.length })
                  : t("newThread.attach")}
              </button>
            </div>

            <input
              ref={attachments.inputRef}
              type="file"
              multiple
              accept={FILE_ACCEPT}
              aria-label={t("newThread.attachFiles")}
              className="sr-only"
              tabIndex={-1}
              onChange={(event) => attachments.onInputChange(event.target.files)}
            />

            <AttachmentTray
              tasks={attachments.tasks}
              onRemove={attachments.remove}
              onRetry={attachments.retry}
            />

            <Textarea
              id="new-thread-task"
              data-autofocus
              disabled={!boundHost}
              value={prompt}
              onChange={(event) => {
                const value = event.target.value;
                setPrompt(value);
                draftSession().setText(value);
              }}
              onPaste={(event) => {
                const files = Array.from(event.clipboardData.files);
                if (files.length > 0) {
                  event.preventDefault();
                  attachments.addFiles(files);
                  return;
                }
                const pasted = event.clipboardData.getData("text/plain");
                if (attachments.addPastedText(pasted)) event.preventDefault();
              }}
              onKeyDown={(event) => {
                if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                  event.preventDefault();
                  void submit();
                }
              }}
              placeholder={boundHost ? t("newThread.placeholder") : t("newThread.connecting")}
              className="min-h-32"
            />
          </div>

          <p className="m-0 rounded-md border border-border bg-bg-side px-2.5 py-2 text-xs text-fg-2">
            {selectedHost ? (
              <Trans
                t={t}
                i18nKey={harness === "acp" ? "newThread.startsAgent" : "newThread.starts"}
                values={{
                  command:
                    harness === "acp"
                      ? (selectedAgent?.command ?? HARNESS_SLUGS.acp)
                      : HARNESS_SLUGS[harness],
                  agent: threadHarnessLabel(harness, agentId),
                  host: selectedHost.name,
                  path: effectiveWorkspace || DEFAULT_NEW_THREAD_WORKSPACE,
                }}
                components={{
                  cmd: <span className="font-mono" />,
                  host: <span className="font-medium text-fg" />,
                  path: <span className="font-mono" />,
                }}
              />
            ) : (
              t("newThread.pairHost")
            )}
          </p>
        </DialogBody>

        <DialogFooter>
          <span className="mr-auto hidden items-center gap-1.5 text-xs text-muted-foreground sm:inline-flex">
            <Kbd>
              {MOD} {ENTER}
            </Kbd>{" "}
            {t("newThread.startShortcut")}
          </span>
          <Button variant="ghost" onClick={closeNewThread}>
            {t("common:cancel")}
          </Button>
          <Button
            disabled={
              offline ||
              !boundHost ||
              !hostEligible(boundHost) ||
              starting ||
              Boolean(attachments.blockedReason) ||
              (!prompt.trim() && attachments.attachments.length === 0)
            }
            onPointerDown={() => {
              startPressed.current = true;
            }}
            onPointerUp={() => {
              startPressed.current = false;
            }}
            onPointerCancel={() => {
              startPressed.current = false;
            }}
            onPointerLeave={() => {
              startPressed.current = false;
            }}
            onClick={() => void submit()}
            title={
              attachments.blockedReason ??
              (offline
                ? t("newThread.reconnectToStart")
                : !boundHost || !hostEligible(boundHost)
                  ? t("newThread.noHostAvailable")
                  : !prompt.trim() && attachments.attachments.length === 0
                    ? t("newThread.describeFirst")
                    : undefined)
            }
          >
            {starting ? <PlusIcon aria-hidden /> : <ArrowRightIcon aria-hidden />}
            {offline
              ? t("newThread.offline")
              : starting
                ? t("newThread.starting")
                : t("newThread.start")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
