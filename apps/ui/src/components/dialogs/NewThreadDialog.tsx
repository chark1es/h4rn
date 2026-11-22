import { ArrowRightIcon, GitBranchIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import type * as React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ChipSelect } from "@/components/common/ChipSelect";
import { ENTER, Kbd, MOD } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import {
  acpModeChoices,
  acpModelChoices,
  acpPickerOptions,
  acpRuntimeModeChoices,
  THREAD_RUNTIME_MODES,
} from "@/features/harnesses/acpForm";
import {
  probeAcpAgent,
  useAcpAgents,
  useHarnesses,
  useHarnessVersions,
} from "@/features/harnesses/harnessStore";
import {
  proxyDefaultModel,
  proxyModelsFor,
  proxyOverrideFor,
  useProxy,
  useProxyCatalog,
} from "@/features/proxy/proxyStore";
import type { HarnessKind, HostId, RuntimeMode } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { DEFAULT_NEW_THREAD_WORKSPACE, drafts } from "@/lib/drafts";
import { HARNESS_LABELS, HARNESS_SLUGS, threadHarnessLabel } from "@/lib/entities";
import { detectedHarnesses, HARNESS_MODELS } from "@/lib/models";
import { NewThreadDraftSession, type NewThreadDraftSnapshot } from "@/lib/newThreadDraft";
import { closeNewThread, createThread, useApp } from "@/lib/store/appStore";
import { defaultThreadHostId, localOnlyHostOptions } from "@/lib/thread-hosts";
import { cn } from "@/lib/utils";

const DEFAULT_MODELS = Object.fromEntries(
  Object.entries(HARNESS_MODELS).map(([harness, info]) => [harness, info.default]),
) as Record<HarnessKind, string>;

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

function FieldLabel({
  children,
  hint,
  htmlFor,
}: {
  children: React.ReactNode;
  hint?: string | undefined;
  htmlFor: string;
}) {
  return (
    <label htmlFor={htmlFor} className="flex items-baseline justify-between">
      <span className="text-[12px] text-[var(--fg-3)]">{children}</span>
      {hint ? <span className="text-[11.5px] text-[var(--fg-3)]">{hint}</span> : null}
    </label>
  );
}

export function NewThreadDialog() {
  const open = useApp((state) => state.ui.newThreadOpen);
  const hosts = useApp((state) => state.hosts);
  const localHostId = useApp((state) => state.localHostId);
  const connection = useApp((state) => state.connection.status);
  const isPhone = useIsPhone();
  useProxyCatalog();
  useHarnessVersions();
  useAcpAgents();
  const agents = useHarnesses((value) => value.agents);
  const probes = useHarnesses((value) => value.probes);
  const probeErrors = useHarnesses((value) => value.probeErrors);
  const probingAgents = useHarnesses((value) => value.probing);
  const harnessRows = useHarnesses((value) => (value.loaded ? value.rows : null));
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
  const [runtimeMode, setRuntimeMode] = useState<RuntimeMode>("approval_required");
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
        setModel(proxyDefaultModel(nextHarness) ?? DEFAULT_MODELS[nextHarness]);
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
  }, [open, onlineHosts, localHostId, installed, preset, draftSession, showDraft, hostEligible]);

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

  // The catalogue can arrive after the dialog opens; adopt its default only
  // while the user has not chosen a model of their own.
  const proxyActive = proxyOverrideFor(harness);
  const proxyModels = proxyModelsFor(harness);
  const proxyProfileId = proxyActive?.profile.id ?? null;
  const proxyDefault = proxyDefaultModel(harness);
  const proxyModelKey = proxyModels.map((model) => model.id).join("|");
  useEffect(() => {
    if (!open || proxyProfileId === null || proxyModelKey === "") return;
    setModel((current) =>
      current === DEFAULT_MODELS[harness] ? (proxyDefault ?? current) : current,
    );
  }, [open, harness, proxyDefault, proxyModelKey, proxyProfileId]);

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
  const harnessMissing =
    harness !== "acp" && harnessChoices.length > 0 && !harnessChoices.includes(harness);
  useEffect(() => {
    if (!open) return;
    if (!harnessMissing && !agentGone) return;
    if (!firstChoice) return;
    setHarness(firstChoice);
    setAgentId(null);
    setModel(proxyDefaultModel(firstChoice) ?? DEFAULT_MODELS[firstChoice]);
  }, [open, harnessMissing, agentGone, firstChoice]);

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
  // "Approval required" mode the agent never advertised.
  const agentModes = acpModeChoices(probe, probeFailed, selectedAgent?.modes);
  const runtimeModeChoices =
    harness === "acp" ? acpRuntimeModeChoices(agentModes) : THREAD_RUNTIME_MODES;
  const modeManagedByAgent = harness === "acp" && runtimeModeChoices.length === 0;
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
  const offline = connection !== "online";
  const modelChoices = [
    ...new Set([
      ...(harness === "acp" ? agentModels : modelInfo.models),
      ...(harness === "acp" ? [] : proxyModels.map((candidate) => candidate.id)),
    ]),
  ];
  const attachments = useAttachmentDrafts("new-thread", {
    enabled: !offline && Boolean(boundHost),
    disabledReason: offline ? "Reconnect to attach files." : "Pick an online host first.",
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
      <DialogContent
        className={cn(
          "gap-3 sm:max-w-[640px]",
          isPhone &&
            "top-0 left-0 h-dvh max-h-dvh w-full max-w-none translate-x-0 translate-y-0 rounded-none border-0",
        )}
        showCloseButton={!isPhone}
      >
        <DialogHeader>
          <DialogTitle>New thread</DialogTitle>
          <DialogDescription>
            Pick where it runs, then describe the task. Everything after Start is visible in the
            stream.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3 sm:grid-cols-3">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-host">Host</FieldLabel>
            <ChipSelect
              id="new-thread-host"
              block
              label="Host"
              placeholder="No online hosts"
              value={host}
              options={localOnlyHostOptions(
                onlineHosts,
                localHostId,
                "Not yet: new threads run on this host",
              )}
              onChange={(value) => {
                if (value === "") return;
                const nextHost = hosts[value];
                hostChosen.current = true;
                showDraft(draftSession().selectHost(value));
                const nextHarness = detectedHarnesses(HARNESSES, installed, nextHost?.harnesses)[0];
                if (nextHarness && harness !== "acp") {
                  setHarness(nextHarness);
                  setAgentId(null);
                  setModel(DEFAULT_MODELS[nextHarness]);
                }
              }}
            />
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-harness">Harness</FieldLabel>
            <ChipSelect
              id="new-thread-harness"
              block
              label="Harness"
              placeholder="No harness detected"
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
                setModel(proxyDefaultModel(kind) ?? DEFAULT_MODELS[kind]);
              }}
            />
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-model">Model</FieldLabel>
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
              <span className="text-small text-fg-3">
                {probing
                  ? "Reading the agent's models…"
                  : probe?.error
                    ? probe.error
                    : probeError
                      ? `${probeError} Type a model or leave \`default\`.`
                      : agentModels.length > 0
                        ? `${agentModels.length} models from ${selectedAgent?.name ?? "the agent"}.`
                        : selectedAgent && !selectedAgent.installed
                          ? "This command is not installed on the host yet. Type a model or leave `default`."
                          : "No models read yet. Type one or leave `default`."}
              </span>
            ) : modelInfo.thinking ? (
              <span className="text-small text-fg-3">
                {modelInfo.thinking.kind}: {modelInfo.thinking.levels.join(", ")}
              </span>
            ) : (
              <span className="text-small text-fg-3">No thinking modes.</span>
            )}
            {proxyActive ? (
              <span className="text-small text-fg-3">
                {proxyModels.length} models from {proxyActive.profile.name}; the harness routes
                through it.
              </span>
            ) : null}
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-[1fr_1.2fr]">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-mode">Mode</FieldLabel>
            {modeManagedByAgent ? (
              <>
                <span id="new-thread-mode" className="py-1.5 text-small text-fg-2">
                  Mode is managed by the agent.
                </span>
                <span className="text-small text-fg-3">
                  {probing ? "Reading the agent's modes…" : "The agent decides its own approvals."}
                </span>
              </>
            ) : (
              <>
                <ChipSelect
                  id="new-thread-mode"
                  block
                  label="Mode"
                  value={runtimeMode}
                  options={runtimeModeChoices}
                  onChange={setRuntimeMode}
                />
                <span className="text-small text-fg-3">Fixed for the life of the thread.</span>
              </>
            )}
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel
              htmlFor="new-thread-workspace"
              hint={fixedWorkspace ? "fixed by the agent" : "on the host"}
            >
              Where
            </FieldLabel>
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
            <span className="flex items-center gap-1 text-small text-fg-3">
              <GitBranchIcon className="size-3" aria-hidden />
              {fixedWorkspace
                ? "This agent always runs in its configured directory."
                : "The agent works in this directory. h4rn does not create a worktree for new threads yet."}
            </span>
          </div>
        </div>

        <div className="flex flex-col gap-1">
          <div className="flex items-center justify-between">
            <FieldLabel
              htmlFor="new-thread-task"
              hint={prompt.trim() ? `${prompt.trim().length} chars` : undefined}
            >
              Task
            </FieldLabel>
            <button
              type="button"
              disabled={offline || !boundHost}
              onClick={attachments.openPicker}
              aria-label="Attach files"
              title={offline ? "Reconnect to attach files." : "Attach files (Ctrl/Cmd U)"}
              className="flex h-8 items-center gap-1 rounded-sm px-1.5 text-small text-fg-2 transition-colors duration-[var(--dur-instant)] hover:bg-bg-side hover:text-fg focus-visible:ring-2 focus-visible:ring-focus focus-visible:outline-none"
            >
              <PaperclipIcon className="size-3.5" aria-hidden />
              {attachments.attachments.length > 0
                ? `${attachments.attachments.length} attached`
                : "Attach"}
            </button>
          </div>

          <input
            ref={attachments.inputRef}
            type="file"
            multiple
            accept={FILE_ACCEPT}
            aria-label="Attach files"
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
            autoFocus
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
            placeholder={
              boundHost
                ? "Describe the task. Use @ for files and hosts, / for commands. Paste or drop images too."
                : "Connecting…"
            }
            className="min-h-32"
          />
        </div>

        <p className="rounded-sm border border-line bg-bg-side px-2.5 py-1.5 text-small text-fg-2">
          {selectedHost ? (
            <>
              Starts{" "}
              <span className="font-mono">
                {harness === "acp"
                  ? (selectedAgent?.command ?? HARNESS_SLUGS.acp)
                  : HARNESS_SLUGS[harness]}
              </span>{" "}
              {harness === "acp" ? `(${threadHarnessLabel(harness, agentId)})` : null} on{" "}
              <span className="font-medium text-fg">{selectedHost.name}</span> in{" "}
              <span className="font-mono">
                {effectiveWorkspace || DEFAULT_NEW_THREAD_WORKSPACE}
              </span>
              .
            </>
          ) : (
            "Pair an online host before starting a thread."
          )}
        </p>

        <DialogFooter className="items-center sm:justify-between">
          <span className="hidden items-center gap-1 text-small text-fg-3 sm:flex">
            <Kbd>
              {MOD} {ENTER}
            </Kbd>{" "}
            start
          </span>
          <div className="flex items-center gap-2">
            <Button variant="ghost" onClick={closeNewThread}>
              Cancel
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
                  ? "Reconnect to start a thread."
                  : !boundHost || !hostEligible(boundHost)
                    ? "No online host is available."
                    : !prompt.trim() && attachments.attachments.length === 0
                      ? "Describe the task first."
                      : undefined)
              }
            >
              {starting ? <PlusIcon aria-hidden /> : <ArrowRightIcon aria-hidden />}
              {offline ? "Offline" : starting ? "Starting…" : "Start"}
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
