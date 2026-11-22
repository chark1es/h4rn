import { LoaderIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { ChipSelect } from "@/components/common/ChipSelect";
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
import type { HarnessKind, HostId, SubagentIsolation, ThreadSummary } from "@/gen";
import { HARNESS_LABELS, HARNESS_SLUGS } from "@/lib/entities";
import { useApp } from "@/lib/store/appStore";
import { localOnlyHostOptions } from "@/lib/thread-hosts";

import { closeDelegate, spawnSubagent, useSubagentStore } from "./subagentStore";

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

function parseCount(value: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) && parsed > 0 ? Math.floor(parsed) : null;
}

/**
 * Delegate form.
 *
 * Defaults follow the parent thread: same host and harness, the parent's model,
 * and an isolated worktree so the child cannot edit the parent's tree. Other
 * hosts are shown but disabled until cross-host delegation exists, so the form
 * never offers a spawn the daemon will reject. Blank limits mean the daemon's
 * defaults, which are not "unlimited".
 */
export function DelegateDialog({ thread }: { thread: ThreadSummary }) {
  const open = useSubagentStore((state) => state.delegateOpen);
  const spawning = useSubagentStore((state) => state.spawning);
  const hosts = useApp((state) => state.hosts);
  const [task, setTask] = useState("");
  const [harness, setHarness] = useState<HarnessKind>(
    thread.harness === "acp" ? "claude_code" : thread.harness,
  );
  const [host, setHost] = useState<HostId>(thread.host);
  const [model, setModel] = useState(thread.model);
  const [isolation, setIsolation] = useState<SubagentIsolation>("worktree");
  const [maxTurns, setMaxTurns] = useState("");
  const [maxMinutes, setMaxMinutes] = useState("");
  const [maxTokens, setMaxTokens] = useState("");
  const [summary, setSummary] = useState("");

  const onlineHosts = useMemo(
    () =>
      Object.values(hosts)
        .filter((candidate) => candidate.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );

  useEffect(() => {
    if (!open) return;
    setTask("");
    setHarness(thread.harness === "acp" ? "claude_code" : thread.harness);
    setHost(thread.host);
    setModel(thread.model);
    setIsolation("worktree");
    setMaxTurns("");
    setMaxMinutes("");
    setMaxTokens("");
    setSummary("");
  }, [open, thread.harness, thread.host, thread.model]);

  const submit = async () => {
    if (!task.trim() || spawning) return;
    const minutes = parseCount(maxMinutes);
    const ok = await spawnSubagent({
      parentThread: thread.thread_id,
      task,
      harness,
      host: host || null,
      model: model.trim() || null,
      isolation,
      maxTurns: parseCount(maxTurns),
      maxWallTimeSecs: minutes === null ? null : minutes * 60,
      maxTokens: parseCount(maxTokens),
      contextSummary: summary.trim() || null,
    });
    if (ok) closeDelegate();
  };

  const harnessChoices = hosts[host]?.harnesses.length ? hosts[host].harnesses : HARNESSES;

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeDelegate())}>
      <DialogContent className="gap-3 sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>Delegate a subagent</DialogTitle>
          <DialogDescription>
            The child gets its own thread on this thread's host and reports to this one. Approvals
            still come to this thread.
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-1">
          <label htmlFor="delegate-task" className="text-[12px] text-[var(--fg-3)]">
            Task
          </label>
          <Textarea
            id="delegate-task"
            value={task}
            autoFocus
            placeholder="Reproduce the failing auth test and report the race."
            className="min-h-[84px] resize-none"
            onChange={(event) => setTask(event.target.value)}
          />
        </div>

        <div className="grid gap-3 sm:grid-cols-3">
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-host" className="text-[12px] text-[var(--fg-3)]">
              Host
            </label>
            <ChipSelect
              id="delegate-host"
              label="Host"
              value={host}
              placeholder="No online hosts"
              width={160}
              options={localOnlyHostOptions(
                onlineHosts,
                thread.host,
                "Not yet: delegation runs on this thread's host",
              )}
              onChange={(next) => {
                setHost(next);
                const options = hosts[next]?.harnesses;
                if (options?.length && !options.includes(harness))
                  setHarness(options[0] ?? harness);
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-harness" className="text-[12px] text-[var(--fg-3)]">
              Harness
            </label>
            <ChipSelect
              id="delegate-harness"
              label="Harness"
              value={harness}
              width={160}
              options={harnessChoices.map((option) => ({
                value: option,
                label: HARNESS_LABELS[option],
              }))}
              onChange={setHarness}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-model" className="text-[12px] text-[var(--fg-3)]">
              Model
            </label>
            <Input
              id="delegate-model"
              value={model}
              className="font-mono"
              onChange={(event) => setModel(event.target.value)}
            />
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-3">
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-isolation" className="text-[12px] text-[var(--fg-3)]">
              Workspace
            </label>
            <ChipSelect<SubagentIsolation>
              id="delegate-isolation"
              label="Workspace"
              value={isolation}
              width={160}
              options={[
                { value: "worktree", label: "Isolated worktree" },
                { value: "shared", label: "Shared workspace" },
              ]}
              onChange={setIsolation}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-turns" className="text-[12px] text-[var(--fg-3)]">
              Max turns
            </label>
            <Input
              id="delegate-turns"
              inputMode="numeric"
              placeholder="20"
              value={maxTurns}
              onChange={(event) => setMaxTurns(event.target.value)}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-minutes" className="text-[12px] text-[var(--fg-3)]">
              Max minutes
            </label>
            <Input
              id="delegate-minutes"
              inputMode="numeric"
              placeholder="15"
              value={maxMinutes}
              onChange={(event) => setMaxMinutes(event.target.value)}
            />
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-tokens" className="text-[12px] text-[var(--fg-3)]">
              Max tokens
            </label>
            <Input
              id="delegate-tokens"
              inputMode="numeric"
              placeholder="50000"
              value={maxTokens}
              onChange={(event) => setMaxTokens(event.target.value)}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-summary" className="text-[12px] text-[var(--fg-3)]">
              Context summary
            </label>
            <Input
              id="delegate-summary"
              placeholder="Optional handoff"
              value={summary}
              onChange={(event) => setSummary(event.target.value)}
            />
          </div>
        </div>

        <p className="text-[11.5px] leading-[16px] text-[var(--fg-3)]">
          Cross-host delegation is not implemented yet: the child runs on this thread's host. Blank
          limits use the daemon defaults (20 turns, 15 minutes, 50k tokens). Cancelling the child
          cleans up its worktree.
        </p>

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={closeDelegate}>
            Cancel
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={!task.trim() || spawning || !host}
            onClick={() => void submit()}
          >
            {spawning ? (
              <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
            ) : null}
            Delegate
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export { HARNESS_SLUGS };
