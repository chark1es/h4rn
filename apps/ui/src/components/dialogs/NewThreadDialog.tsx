import { ArrowRightIcon, GitBranchIcon, PlusIcon } from "lucide-react";
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
import type { HarnessKind, HostId } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { HARNESS_LABELS, HARNESS_SLUGS } from "@/lib/entities";
import { closeNewThread, createThread, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const DEFAULT_MODELS: Record<HarnessKind, string> = {
  claude_code: "claude-sonnet-5-5",
  codex: "gpt-5.1-codex",
  opencode: "claude-sonnet-5-5",
  antigravity: "gemini-3-pro",
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
      <span className="text-label text-ink-faint">{children}</span>
      {hint ? <span className="text-micro text-ink-faint">{hint}</span> : null}
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

  const submit = async () => {
    if (!host || !prompt.trim() || starting) return;
    setStarting(true);
    const ok = await createThread({
      host,
      harness,
      workspacePath: workspace.trim() || "~/code",
      model,
      prompt: prompt.trim(),
    });
    setStarting(false);
    if (ok) {
      closeNewThread();
      setPrompt("");
    }
  };

  const offline = connection !== "online";

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
              className="h-11 w-full rounded-sm border border-control-border bg-raised px-2 text-compact text-ink outline-none focus-visible:border-cue focus-visible:ring-2 focus-visible:ring-cue/40 md:h-[30px]"
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
              className="h-11 w-full rounded-sm border border-control-border bg-raised px-2 text-compact text-ink outline-none focus-visible:border-cue focus-visible:ring-2 focus-visible:ring-cue/40 md:h-[30px]"
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
              disabled
              className="h-11 w-full rounded-sm border border-line bg-panel px-2 text-compact text-ink-faint md:h-[30px]"
              value="approval"
            >
              <option value="approval">Approval required</option>
            </select>
            <span className="text-micro text-ink-faint">
              Runtime mode arrives when the daemon accepts it.
            </span>
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
          <FieldLabel
            htmlFor="new-thread-task"
            hint={prompt.trim() ? `${prompt.trim().length} chars` : undefined}
          >
            Task
          </FieldLabel>
          <Textarea
            id="new-thread-task"
            autoFocus
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            onKeyDown={(event) => {
              if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                event.preventDefault();
                void submit();
              }
            }}
            placeholder="Describe the task. Use @ for files and hosts, / for commands."
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
              disabled={offline || !host || !prompt.trim() || starting}
              onClick={() => void submit()}
              title={
                offline
                  ? "Reconnect to start a thread."
                  : !host
                    ? "No online host is available."
                    : !prompt.trim()
                      ? "Describe the task first."
                      : undefined
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
