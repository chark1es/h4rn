import { ArrowRightIcon, GitBranchIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import type * as React from "react";
import { useEffect, useMemo, useState } from "react";
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
import { acpPickerOptions } from "@/features/harnesses/acpForm";
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
import { HARNESS_LABELS, HARNESS_SLUGS, threadHarnessLabel } from "@/lib/entities";
import { detectedHarnesses, HARNESS_MODELS } from "@/lib/models";
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
  const [harness, setHarness] = useState<HarnessKind>("claude_code");
  const [agentId, setAgentId] = useState<string | null>(null);
  const [model, setModel] = useState(DEFAULT_MODELS.claude_code);
  const [workspace, setWorkspace] = useState("~/code");
  const [runtimeMode, setRuntimeMode] = useState<RuntimeMode>("approval_required");
  const [prompt, setPrompt] = useState("");
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!open) return;
    // A new thread can only run on this daemon's host for now; prefer it even
    // when another online host sorts first.
    const firstId = defaultThreadHostId(onlineHosts, localHostId);
    const first = firstId
      ? onlineHosts.find((candidate) => candidate.host_id === firstId)
      : undefined;
    if (first) {
      setHost(first.host_id);
      const nextHarness =
        detectedHarnesses(HARNESSES, installed, first.harnesses)[0] ?? "claude_code";
      setHarness(nextHarness);
      setAgentId(null);
      setModel(proxyDefaultModel(nextHarness) ?? DEFAULT_MODELS[nextHarness]);
    }
    setStarting(false);
  }, [open, onlineHosts, localHostId, installed]);

  // "New thread in <project>" starts in that project's folder.
  const preset = useApp((state) => state.ui.newThreadPreset);
  useEffect(() => {
    if (!open || !preset) return;
    if (hosts[preset.host]?.online) setHost(preset.host);
    setWorkspace(preset.workspace);
  }, [open, preset, hosts]);

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

  const selectedHost = host ? hosts[host] : undefined;
  const harnessChoices = detectedHarnesses(HARNESSES, installed, selectedHost?.harnesses);
  const selectedAgent = agentId ? agents.find((agent) => agent.id === agentId) : undefined;
  const probe = agentId ? probes[agentId] : undefined;
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
  // picker selects it; the daemon starts the command once and stops it.
  useEffect(() => {
    if (!open || !agentId || probes[agentId]) return;
    void probeAcpAgent(agentId);
  }, [open, agentId, probes]);

  // Adopt the agent's first advertised model while the user has not chosen.
  const agentModels = probe?.models.map((entry) => entry.id) ?? [];
  const firstAgentModel = agentModels[0];
  useEffect(() => {
    if (!open || harness !== "acp" || !firstAgentModel) return;
    setModel((current) => (current === "default" || current === "" ? firstAgentModel : current));
  }, [open, harness, firstAgentModel]);
  const offline = connection !== "online";
  const modelChoices = [
    ...new Set([
      ...(harness === "acp" ? agentModels : modelInfo.models),
      ...(harness === "acp" ? [] : proxyModels.map((candidate) => candidate.id)),
    ]),
  ];
  const attachments = useAttachmentDrafts("new-thread", {
    enabled: !offline && Boolean(host),
    disabledReason: offline ? "Reconnect to attach files." : "Pick an online host first.",
  });

  useAttachShortcut(attachments.openPicker, open && !offline && Boolean(host));

  useEffect(() => {
    if (!open) attachments.clear();
    // Clearing on close drops stale drafts between dialog sessions.
  }, [open, attachments.clear]);

  const submit = async () => {
    if (!host || starting || attachments.blockedReason) return;
    if (!prompt.trim() && attachments.attachments.length === 0) return;
    setStarting(true);
    const ok = await createThread({
      host,
      harness,
      agentId: harness === "acp" ? agentId : null,
      workspacePath: workspace.trim() || "~/code",
      model,
      prompt: prompt.trim(),
      attachments: attachments.attachments,
      runtimeMode,
    });
    setStarting(false);
    if (ok) {
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
                const nextHost = hosts[value];
                setHost(value);
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
                {probe?.error
                  ? probe.error
                  : probe
                    ? probe.models.length > 0
                      ? `${probe.models.length} models from ${selectedAgent?.name ?? "the agent"}.`
                      : "The agent advertises no models; type one or leave `default`."
                    : "Reading the agent's models…"}
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
            <ChipSelect
              id="new-thread-mode"
              block
              label="Mode"
              value={runtimeMode}
              options={[
                { value: "approval_required", label: "Approval required" },
                { value: "auto_accept_edits", label: "Auto-accept edits" },
                { value: "full_access", label: "Full access" },
              ]}
              onChange={setRuntimeMode}
            />
            <span className="text-small text-fg-3">Fixed for the life of the thread.</span>
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-workspace" hint="on the host">
              Where
            </FieldLabel>
            <Input
              id="new-thread-workspace"
              value={workspace}
              onChange={(event) => setWorkspace(event.target.value)}
              className="font-mono"
              placeholder="~/code/api"
            />
            <span className="flex items-center gap-1 text-small text-fg-3">
              <GitBranchIcon className="size-3" aria-hidden />
              The agent works in this directory. h4rn does not create a worktree for new threads
              yet.
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
              disabled={offline || !host}
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
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
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
            placeholder="Describe the task. Use @ for files and hosts, / for commands. Paste or drop images too."
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
              <span className="font-mono">{workspace || "~/code"}</span>.
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
                !host ||
                starting ||
                Boolean(attachments.blockedReason) ||
                (!prompt.trim() && attachments.attachments.length === 0)
              }
              onClick={() => void submit()}
              title={
                attachments.blockedReason ??
                (offline
                  ? "Reconnect to start a thread."
                  : !host
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
