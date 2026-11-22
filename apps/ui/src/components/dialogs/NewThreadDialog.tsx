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
import { useHarnesses, useHarnessVersions } from "@/features/harnesses/harnessStore";
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
import { HARNESS_LABELS, HARNESS_SLUGS } from "@/lib/entities";
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
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, []);

  useEffect(() => {
    if (!open) {
      sessionRef.current = null;
      appliedPreset.current = false;
      hostResolved.current = false;
      hostChosen.current = false;
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
        setModel(proxyDefaultModel(nextHarness) ?? DEFAULT_MODELS[nextHarness]);
      } else if (!hostChosen.current && session.snapshot().host !== first.host_id) {
        showDraft(session.rebindHost(first.host_id));
      }
    }
    setStarting(false);
  }, [open, onlineHosts, localHostId, installed, preset, draftSession, showDraft]);

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

  const selectedHost = host ? hosts[host] : undefined;
  const harnessChoices = detectedHarnesses(HARNESSES, installed, selectedHost?.harnesses);
  const modelInfo = HARNESS_MODELS[harness];

  // If detection settles and the chosen harness is not among those found,
  // move to one that is rather than leave an undetected harness selected.
  const firstChoice = harnessChoices[0];
  const harnessMissing = harnessChoices.length > 0 && !harnessChoices.includes(harness);
  useEffect(() => {
    if (!open || !harnessMissing || !firstChoice) return;
    setHarness(firstChoice);
    setModel(proxyDefaultModel(firstChoice) ?? DEFAULT_MODELS[firstChoice]);
  }, [open, harnessMissing, firstChoice]);
  const offline = connection !== "online";
  const modelChoices = [
    ...new Set([...modelInfo.models, ...proxyModels.map((candidate) => candidate.id)]),
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

  const commitWorkspace = useCallback(() => {
    const draft = draftSession().commitWorkspace(workspace);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, [draftSession, workspace]);

  const submit = async () => {
    if (starting || attachments.blockedReason) return;
    // Cmd+Enter with no host yet: nothing has been committed or saved.
    const session = draftSession();
    const draft = session.submission(workspace);
    if (!draft) return;
    if (!draft.text.trim() && attachments.attachments.length === 0) return;
    // The folder on screen is the one the request runs in, but the visible
    // text stays where it is: Start never swaps the box for another slot.
    setWorkspace(draft.workspace);
    setStarting(true);
    const ok = await createThread({
      host: draft.host,
      harness,
      workspacePath: draft.workspace,
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
                const draft = draftSession().selectHost(value);
                setHost(draft.host);
                setPrompt(draft.text);
                setWorkspace(draft.workspace);
                const nextHarness = detectedHarnesses(HARNESSES, installed, nextHost?.harnesses)[0];
                if (nextHarness) {
                  setHarness(nextHarness);
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
              value={harnessChoices.includes(harness) ? harness : ""}
              options={harnessChoices.map((option) => ({
                value: option,
                label: HARNESS_LABELS[option],
              }))}
              onChange={(next) => {
                setHarness(next);
                setModel(proxyDefaultModel(next) ?? DEFAULT_MODELS[next]);
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
            {modelInfo.thinking ? (
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
            disabled={!host}
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
              host
                ? "Describe the task. Use @ for files and hosts, / for commands. Paste or drop images too."
                : "Connecting…"
            }
            className="min-h-32"
          />
        </div>

        <p className="rounded-sm border border-line bg-bg-side px-2.5 py-1.5 text-small text-fg-2">
          {selectedHost ? (
            <>
              Starts <span className="font-mono">{HARNESS_SLUGS[harness]}</span> on{" "}
              <span className="font-medium text-fg">{selectedHost.name}</span> in{" "}
              <span className="font-mono">{workspace || DEFAULT_NEW_THREAD_WORKSPACE}</span>.
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
              onPointerDown={() => {
                startPressed.current = true;
              }}
              onPointerUp={() => {
                startPressed.current = false;
              }}
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
