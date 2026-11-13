import { ArrowRightIcon, GitBranchIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import type * as React from "react";
import { useEffect, useMemo, useState } from "react";

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
import { useAttachmentDrafts } from "@/features/attachments/use-attachments";
import type { HarnessKind, HostId, RuntimeMode } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { HARNESS_LABELS, HARNESS_SLUGS } from "@/lib/entities";
import { closeNewThread, createThread, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const DEFAULT_MODELS: Record<HarnessKind, string> = {
  claude_code: "claude-sonnet-5-5",
  codex: "gpt-5.1-codex",
  opencode: "claude-sonnet-5-5",
  antigravity: "gemini-4-argon-medium",
};

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
  const connection = useApp((state) => state.connection.status);
  const isPhone = useIsPhone();

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
  const [workspace, setWorkspace] = useState("~/code");
  const [runtimeMode, setRuntimeMode] = useState<RuntimeMode>("approval_required");
  const [prompt, setPrompt] = useState("");
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!open) return;
    const first = onlineHosts[0];
    if (first) {
      setHost(first.host_id);
      const nextHarness = first.harnesses[0] ?? "claude_code";
      setHarness(nextHarness);
      setModel(DEFAULT_MODELS[nextHarness]);
    }
    setStarting(false);
  }, [open, onlineHosts]);

  const selectedHost = host ? hosts[host] : undefined;
  const harnessChoices = selectedHost?.harnesses.length ? selectedHost.harnesses : HARNESSES;
  const offline = connection !== "online";
  const attachments = useAttachmentDrafts("new-thread", {
    enabled: !offline && Boolean(host),
    disabledReason: offline ? "Reconnect to attach files." : "Pick an online host first.",
  });

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
            <select
              id="new-thread-host"
              value={host}
              onChange={(event) => {
                const nextHost = hosts[event.target.value];
                setHost(event.target.value);
                const nextHarness = nextHost?.harnesses[0];
                if (nextHarness) {
                  setHarness(nextHarness);
                  setModel(DEFAULT_MODELS[nextHarness]);
                }
              }}
              className="h-7 w-full rounded-[6px] border-0 bg-raised px-2 text-[12.5px] text-ink shadow-[inset_0_0_0_1px_var(--line-2)] outline-none focus-visible:shadow-[inset_0_0_0_1px_var(--accent)]"
            >
              {onlineHosts.length === 0 ? <option value="">No online hosts</option> : null}
              {onlineHosts.map((option) => (
                <option key={option.host_id} value={option.host_id}>
                  {option.name}
                </option>
              ))}
            </select>
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-harness">Harness</FieldLabel>
            <select
              id="new-thread-harness"
              value={harness}
              onChange={(event) => {
                const next = event.target.value as HarnessKind;
                setHarness(next);
                setModel(DEFAULT_MODELS[next]);
              }}
              className="h-7 w-full rounded-[6px] border-0 bg-raised px-2 text-[12.5px] text-ink shadow-[inset_0_0_0_1px_var(--line-2)] outline-none focus-visible:shadow-[inset_0_0_0_1px_var(--accent)]"
            >
              {harnessChoices.map((option) => (
                <option key={option} value={option}>
                  {HARNESS_LABELS[option]}
                </option>
              ))}
            </select>
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-model">Model</FieldLabel>
            <Input
              id="new-thread-model"
              value={model}
              onChange={(event) => setModel(event.target.value)}
              list="h4rn-models"
              className="font-mono"
            />
            <datalist id="h4rn-models">
              {Object.entries(DEFAULT_MODELS).map(([kind, option]) => (
                <option key={`${kind}:${option}`} value={option} />
              ))}
            </datalist>
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-[1fr_1.2fr]">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="new-thread-mode">Mode</FieldLabel>
            <select
              id="new-thread-mode"
              className="h-7 w-full rounded-[6px] border-0 bg-raised px-2 text-[12.5px] text-ink shadow-[inset_0_0_0_1px_var(--line-2)] outline-none focus-visible:shadow-[inset_0_0_0_1px_var(--accent)]"
              value={runtimeMode}
              onChange={(event) => setRuntimeMode(event.target.value as RuntimeMode)}
            >
              <option value="approval_required">Approval required</option>
              <option value="auto_accept_edits">Auto-accept edits</option>
              <option value="full_access">Full access</option>
            </select>
            <span className="text-micro text-ink-faint">Fixed for the life of the thread.</span>
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
            <span className="flex items-center gap-1 text-micro text-ink-faint">
              <GitBranchIcon className="size-3" aria-hidden />
              The worktree is chosen by the harness for now.
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
              className="flex h-8 items-center gap-1 rounded-sm px-1.5 text-micro text-ink-muted transition-colors duration-[var(--dur-instant)] hover:bg-panel hover:text-ink focus-visible:ring-2 focus-visible:ring-cue/40 focus-visible:outline-none"
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

          {attachments.tasks.length > 0 ? (
            <div className="rounded-sm border border-line">
              <AttachmentTray
                tasks={attachments.tasks}
                onRemove={attachments.remove}
                onRetry={attachments.retry}
              />
            </div>
          ) : null}

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

        <p className="rounded-sm border border-line bg-panel px-2.5 py-1.5 text-micro text-ink-muted">
          {selectedHost ? (
            <>
              Starts <span className="font-mono">{HARNESS_SLUGS[harness]}</span> on{" "}
              <span className="font-medium text-ink">{selectedHost.name}</span> in{" "}
              <span className="font-mono">{workspace || "~/code"}</span>.
            </>
          ) : (
            "Pair an online host before starting a thread."
          )}
        </p>

        <DialogFooter className="items-center sm:justify-between">
          <span className="hidden items-center gap-1 text-micro text-ink-faint sm:flex">
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
