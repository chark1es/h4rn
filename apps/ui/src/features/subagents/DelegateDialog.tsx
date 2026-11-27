import { LoaderIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { SuggestInput } from "@/components/ui/autocomplete";
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
import { HARNESS_MODELS } from "@/lib/models";
import { useApp } from "@/lib/store/appStore";
import { localOnlyHostOptions } from "@/lib/thread-hosts";

import { closeDelegate, spawnSubagent, useSubagentStore } from "./subagentStore";

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/**
 * The model a delegated child starts with.
 *
 * A child inherits its parent's model only while it keeps the parent's
 * harness: an ACP thread's model (`default`, or an id only that configured
 * agent knows) means nothing to Claude Code, so a fallback starts on that
 * harness's own default.
 */
export function delegateModel(
  parent: { harness: HarnessKind; model: string },
  harness: HarnessKind,
): string {
  return parent.harness === harness ? parent.model : HARNESS_MODELS[harness].default;
}

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
  const { t } = useTranslation(["dialogs", "common"]);
  const open = useSubagentStore((state) => state.delegateOpen);
  const spawning = useSubagentStore((state) => state.spawning);
  const hosts = useApp((state) => state.hosts);
  const [task, setTask] = useState("");
  const [harness, setHarness] = useState<HarnessKind>(
    thread.harness === "acp" ? "claude_code" : thread.harness,
  );
  const [host, setHost] = useState<HostId>(thread.host);
  const [model, setModel] = useState(() =>
    delegateModel(thread, thread.harness === "acp" ? "claude_code" : thread.harness),
  );
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

  /**
   * Stable facts about the parent, for the seed.
   *
   * A parent summary is refreshed on every event (title, agent state, usage,
   * last activity), so the raw `thread` object changes constantly while the
   * dialog is open. Seeding from these scalars means only a new open, or a
   * different parent identity, resets the draft: an erased task, summary or
   * budget would otherwise vanish mid-typing because the daemon sent a tick.
   */
  const parentThreadId = thread.thread_id;
  const parentHarness = thread.harness;
  const parentHost = thread.host;
  const parentModel = thread.model;
  const seeded = useRef<string | null>(null);

  useEffect(() => {
    if (!open) {
      seeded.current = null;
      return;
    }
    const seed = [parentThreadId, parentHarness, parentHost, parentModel].join("\u0000");
    if (seeded.current === seed) return;
    seeded.current = seed;
    const nextHarness: HarnessKind = parentHarness === "acp" ? "claude_code" : parentHarness;
    setTask("");
    setHarness(nextHarness);
    setHost(parentHost);
    setModel(delegateModel({ harness: parentHarness, model: parentModel }, nextHarness));
    setIsolation("worktree");
    setMaxTurns("");
    setMaxMinutes("");
    setMaxTokens("");
    setSummary("");
  }, [open, parentThreadId, parentHarness, parentHost, parentModel]);

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

  const harnessChoices = (hosts[host]?.harnesses.length ? hosts[host].harnesses : HARNESSES).filter(
    // A child has no configured agent instance, so the bare ACP kind is not
    // startable here.
    (kind) => kind !== "acp",
  );

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeDelegate())}>
      <DialogContent className="gap-3 sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>{t("delegate.title")}</DialogTitle>
          <DialogDescription>{t("delegate.description")}</DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-1">
          <label htmlFor="delegate-task" className="text-[12px] text-[var(--fg-3)]">
            {t("fields.task")}
          </label>
          <Textarea
            id="delegate-task"
            value={task}
            autoFocus
            placeholder={t("delegate.taskPlaceholder")}
            className="min-h-[84px] resize-none"
            onChange={(event) => setTask(event.target.value)}
          />
        </div>

        <div className="grid gap-3 sm:grid-cols-3">
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-host" className="text-[12px] text-[var(--fg-3)]">
              {t("fields.host")}
            </label>
            <ChipSelect
              id="delegate-host"
              label={t("fields.host")}
              value={host}
              placeholder={t("fields.noOnlineHosts")}
              className="w-40"
              options={localOnlyHostOptions(onlineHosts, thread.host, t("delegate.hostLocalOnly"))}
              onChange={(next) => {
                setHost(next);
                const options = hosts[next]?.harnesses.filter(
                  (kind): kind is Exclude<HarnessKind, "acp"> => kind !== "acp",
                );
                const fallback = options?.[0];
                if (fallback && !options?.includes(harness as Exclude<HarnessKind, "acp">)) {
                  setHarness(fallback);
                  setModel(delegateModel(thread, fallback));
                }
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-harness" className="text-[12px] text-[var(--fg-3)]">
              {t("fields.harness")}
            </label>
            <ChipSelect
              id="delegate-harness"
              label={t("fields.harness")}
              value={harness}
              className="w-40"
              options={harnessChoices.map((option) => ({
                value: option,
                label: HARNESS_LABELS[option],
              }))}
              onChange={(next) => {
                setHarness(next);
                // A model belongs to one harness; switching starts from the
                // new harness's own default rather than sending its CLI an id
                // it never issued.
                setModel(HARNESS_MODELS[next].default);
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-model" className="text-[12px] text-[var(--fg-3)]">
              {t("fields.model")}
            </label>
            <SuggestInput
              id="delegate-model"
              value={model}
              onValueChange={setModel}
              suggestions={HARNESS_MODELS[harness].models}
              showLabel={t("common:showSuggestions")}
              mono
            />
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-3">
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-isolation" className="text-[12px] text-[var(--fg-3)]">
              {t("fields.workspace")}
            </label>
            <ChipSelect<SubagentIsolation>
              id="delegate-isolation"
              label={t("fields.workspace")}
              value={isolation}
              className="w-40"
              options={[
                { value: "worktree", label: t("delegate.isolated") },
                { value: "shared", label: t("delegate.shared") },
              ]}
              onChange={setIsolation}
            />
          </div>
          <div className="flex flex-col gap-1">
            <label htmlFor="delegate-turns" className="text-[12px] text-[var(--fg-3)]">
              {t("delegate.maxTurns")}
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
              {t("delegate.maxMinutes")}
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
              {t("delegate.maxTokens")}
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
              {t("delegate.summary")}
            </label>
            <Input
              id="delegate-summary"
              placeholder={t("delegate.summaryPlaceholder")}
              value={summary}
              onChange={(event) => setSummary(event.target.value)}
            />
          </div>
        </div>

        <p className="text-[11.5px] leading-[16px] text-[var(--fg-3)]">{t("delegate.note")}</p>

        <DialogFooter>
          <button type="button" className="q-btn ghost" onClick={closeDelegate}>
            {t("common:cancel")}
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
            {t("delegate.submit")}
          </button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export { HARNESS_SLUGS };
