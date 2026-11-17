import {
  BlocksIcon,
  CheckCircle2Icon,
  ChevronRightIcon,
  DownloadIcon,
  PlusIcon,
  RotateCwIcon,
  TriangleAlertIcon,
  WifiOffIcon,
} from "lucide-react";
import { useEffect, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { Button } from "@/components/ui/button";
import type { HarnessKind, SkillConflict, SkillInstallState, SkillSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { HARNESS_LABELS } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { SkillAddDialog, SkillDetailsDialog, SkillImportDialog, sourceLabel } from "./SkillDialogs";
import {
  ensureSkillEvents,
  isUnmanagedCandidate,
  loadSkills,
  previewSkillImport,
  resolveSkillConflict,
  setSkillEnabled,
  syncSkillsNow,
  useSkills,
} from "./store";

const ALL_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

const SHORT_HARNESS: Record<HarnessKind, string> = {
  claude_code: "Claude",
  codex: "Codex",
  opencode: "OpenCode",
  antigravity: "Anti",
};

function shortVersion(version: string | null): string {
  return version ? version.slice(0, 6) : "—";
}

function SyncBanner() {
  const sync = useSkills((state) => state.sync);
  const syncing = useSkills((state) => state.syncing);
  const connection = useApp((state) => state.connection.status);
  const now = useNow();

  const offline = connection === "offline";
  const label = syncing
    ? "Syncing…"
    : !sync
      ? offline
        ? "Offline"
        : "Not synced yet"
      : sync.state === "synced"
        ? "Synced"
        : sync.state === "blocked"
          ? "Needs attention"
          : sync.state === "offline"
            ? "Offline"
            : sync.state === "syncing"
              ? "Syncing…"
              : "Idle";

  const detail = sync
    ? [
        sync.last_sync ? `last sync ${relativeAge(sync.last_sync, now)} ago` : "no completed sync",
        `${sync.pushed} pushed`,
        `${sync.pulled} pulled`,
      ].join(" · ")
    : offline
      ? "Local skills keep working; sync resumes when the daemon is online."
      : "Run a cycle to publish local changes and pull the shared registry.";
  const summary = sync?.message?.trim() || detail;

  return (
    <div
      className={cn("q-banner", "wrap")}
      style={{ margin: "0 0 18px", height: "auto", minHeight: 34 }}
    >
      {offline ? (
        <span className="ic">
          <WifiOffIcon className="q-icon s14" aria-hidden />
        </span>
      ) : sync?.state === "blocked" || (sync?.conflicts ?? 0) > 0 ? (
        <span className="ic q-warn">
          <TriangleAlertIcon className="q-icon s14" aria-hidden />
        </span>
      ) : (
        <span className={sync?.state === "synced" ? "ic q-ok" : "ic q-faint"}>
          <RotateCwIcon className="q-icon s14" aria-hidden />
        </span>
      )}
      <span className="grow">
        <b>{label}</b> · {summary}
        {sync && (sync.conflicts > 0 || sync.hint) ? (
          <span className="q-faint">
            {" "}
            ·{" "}
            {sync.hint ??
              `${sync.conflicts} ${sync.conflicts === 1 ? "conflict" : "conflicts"} to settle.`}
          </span>
        ) : null}
      </span>
      <Button variant="ghost" size="sm" onClick={() => void syncSkillsNow()} disabled={syncing}>
        {syncing ? "Syncing…" : "Sync now"}
      </Button>
    </div>
  );
}

function Conflicts({ conflicts }: { conflicts: SkillConflict[] }) {
  const isPhone = useIsPhone();
  const [choices, setChoices] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  if (conflicts.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>Conflicts</h2>
        <span className="aside">Settle these before the next sync</span>
      </div>
      <div className="flex flex-col gap-2">
        {conflicts.map((conflict) => {
          const choice = choices[conflict.id] ?? "keep_local";
          return (
            <div className="q-conflict" key={conflict.id} style={{ flexWrap: "wrap", rowGap: 8 }}>
              <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
              <div className="grow" style={{ minWidth: isPhone ? "calc(100% - 28px)" : undefined }}>
                <div className="t">{conflict.name}</div>
                <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                  {conflict.reason} · local{" "}
                  <span className="q-mono">
                    {conflict.local_version ? shortVersion(conflict.local_version) : "none"}
                  </span>{" "}
                  → remote <span className="q-mono">{shortVersion(conflict.remote_version)}</span>
                </div>
              </div>
              <ChipSelect
                label={`Resolution for ${conflict.name}`}
                value={choice}
                options={[
                  { value: "keep_local", label: "Keep local" },
                  { value: "keep_remote", label: "Keep remote" },
                  { value: "keep_both", label: "Keep both" },
                ]}
                onChange={(next) => setChoices((current) => ({ ...current, [conflict.id]: next }))}
              />
              <Button
                variant="secondary"
                size="sm"
                disabled={Boolean(busy[conflict.id])}
                onClick={() => {
                  setBusy((current) => ({ ...current, [conflict.id]: true }));
                  void resolveSkillConflict(
                    conflict.id,
                    choice as "keep_local" | "keep_remote" | "keep_both",
                  ).finally(() => setBusy((current) => ({ ...current, [conflict.id]: false })));
                }}
              >
                {busy[conflict.id] ? "Settling…" : "Resolve"}
              </Button>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function HarnessMatrix({ skill, isPhone }: { skill: SkillSummary; isPhone: boolean }) {
  const pending = useSkills((state) => state.pending);
  const installs = new Map<HarnessKind, SkillInstallState>(
    skill.installs.map((install) => [install.harness, install]),
  );
  return (
    <div
      className={cn(
        isPhone ? "mt-2 grid w-full grid-cols-4 gap-2" : "flex shrink-0 items-start gap-4",
      )}
      style={isPhone ? undefined : { minWidth: 244 }}
    >
      {ALL_HARNESSES.map((harness) => {
        const install = installs.get(harness);
        const enabled = install?.enabled ?? false;
        const failed = enabled && !install?.installed;
        return (
          <div className="flex min-w-0 flex-col items-center gap-1" key={harness}>
            <button
              type="button"
              role="switch"
              className="q-switch"
              aria-checked={enabled}
              aria-disabled={pending[`${skill.id}:${harness}`] ? "true" : undefined}
              disabled={!install || Boolean(pending[`${skill.id}:${harness}`])}
              aria-label={`${skill.name} for ${HARNESS_LABELS[harness]}`}
              title={
                !install
                  ? `${HARNESS_LABELS[harness]} has no skill directory on this host`
                  : `${HARNESS_LABELS[harness]} · ${install.version ?? "not installed"}`
              }
              onClick={() => {
                if (!install) return;
                void setSkillEnabled(skill.id, harness, !enabled);
              }}
            />
            <span className="text-[10.5px] whitespace-nowrap text-[var(--fg-3)]">
              {isPhone ? SHORT_HARNESS[harness] : HARNESS_LABELS[harness]}
            </span>
            <span
              className={cn("q-mono text-[10px]", failed ? "q-warn" : "text-[var(--fg-3)]")}
              title={failed ? "Enabled in the registry but no managed copy was found" : undefined}
            >
              {shortVersion(install?.version ?? null)}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function SkillRow({
  skill,
  isPhone,
  onOpen,
}: {
  skill: SkillSummary;
  isPhone: boolean;
  onOpen: () => void;
}) {
  const installed = skill.installs.filter((install) => install.installed).length;
  return (
    <div className="q-li" style={{ flexWrap: isPhone ? "wrap" : undefined }}>
      <span
        className="grid place-items-center rounded-[6px] bg-[var(--fill)]"
        style={{ width: 26, height: 26, color: "var(--fg-2)" }}
      >
        <BlocksIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <button type="button" className="grow min-w-0 text-left" onClick={onOpen}>
        <div className="t flex flex-wrap items-center gap-2">
          {skill.name}
          <span className="q-badge q-mono" style={{ fontWeight: 400 }}>
            v{shortVersion(skill.version)}
          </span>
          {skill.conflicts > 0 ? (
            <span className="q-badge warn">
              <TriangleAlertIcon className="q-icon s12" aria-hidden /> {skill.conflicts}{" "}
              {skill.conflicts === 1 ? "conflict" : "conflicts"}
            </span>
          ) : null}
        </div>
        <div className="s">{skill.description}</div>
        <div className="q-faint mt-1 text-[11.5px]">
          {installed} of {skill.installs.length} harnesses · {sourceLabel(skill.source)}
        </div>
      </button>
      <HarnessMatrix skill={skill} isPhone={isPhone} />
      {isPhone ? null : <ChevronRightIcon className="q-icon s14 q-faint" aria-hidden />}
    </div>
  );
}

function SkeletonList() {
  return (
    <div className="q-list" role="status" aria-busy="true" aria-label="Loading skills">
      {[0, 1, 2].map((row) => (
        <div className="q-li" key={row}>
          <div className="q-skel" style={{ width: 26, height: 26, borderRadius: 6 }} />
          <div className="grow">
            <div className="q-skel" style={{ width: `${44 + row * 10}%`, height: 12 }} />
            <div
              className="q-skel"
              style={{ width: `${62 - row * 8}%`, height: 9, marginTop: 6 }}
            />
            <div className="q-skel" style={{ width: "38%", height: 8, marginTop: 6 }} />
          </div>
          <div className="q-skel" style={{ width: 180, height: 24 }} />
        </div>
      ))}
    </div>
  );
}

export function SkillsPage() {
  const isPhone = useIsPhone();
  const localHost = useApp((state) => state.localHostId);
  const inventory = useSkills((state) => state.inventory);
  const loading = useSkills((state) => state.loading);
  const error = useSkills((state) => state.error);
  const conflicts = useSkills((state) => state.conflicts);
  const [addOpen, setAddOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [details, setDetails] = useState<SkillSummary | null>(null);
  const preview = useSkills((state) => state.preview);
  const [foundDismissed, setFoundDismissed] = useState(false);

  useEffect(() => {
    ensureSkillEvents();
  }, []);

  // Look for skills the harnesses already have, so the page can offer to
  // bring them in. The scan only reads.
  useEffect(() => {
    if (localHost) void previewSkillImport({ quiet: true });
  }, [localHost]);
  const unmanaged = preview?.candidates.filter(isUnmanagedCandidate) ?? [];

  // A deep link can mount this page before the daemon's hello reports the
  // local host; retry as soon as it arrives instead of leaving the error up.
  useEffect(() => {
    if (localHost) void loadSkills();
  }, [localHost]);

  const skills = inventory?.skills ?? [];
  const detected = inventory?.harnesses.filter((entry) => entry.detected).length ?? 0;

  return (
    <>
      <main className="q-main" aria-label="Skills">
        <div className="q-page">
          <div className="q-wrap" style={{ maxWidth: 920 }}>
            <div className="mb-5 flex items-start gap-3">
              <div className="grow">
                <h1 className="text-[20px] leading-[26px] font-semibold tracking-[-0.015em]">
                  Skills
                </h1>
                <div className="q-muted mt-1">
                  {loading && !inventory
                    ? "Reading the skill registry…"
                    : isPhone
                      ? `${skills.length} installed · ${detected} harnesses`
                      : `${skills.length} installed · ${detected} harnesses detected · available wherever you choose`}
                </div>
              </div>
              <Button variant="secondary" onClick={() => setImportOpen(true)}>
                <DownloadIcon className="q-icon s14" aria-hidden /> Find existing
              </Button>
              <Button onClick={() => setAddOpen(true)}>
                <PlusIcon className="q-icon s14" aria-hidden /> Add skill
              </Button>
            </div>

            <SyncBanner />

            {unmanaged.length > 0 && !foundDismissed ? (
              <div className="q-banner" style={{ margin: "0 0 18px" }} role="status">
                <span className="ic">
                  <BlocksIcon className="q-icon s14" aria-hidden />
                </span>
                <span className="grow">
                  <b>
                    {unmanaged.length === 1
                      ? `${unmanaged[0]?.name} is in a harness folder`
                      : `${unmanaged.length} skills are in your harness folders`}{" "}
                    but not managed by h4rn.
                  </b>{" "}
                  Bring {unmanaged.length === 1 ? "it" : "them"} in to sync across your machines.
                </span>
                <Button variant="ghost" size="sm" onClick={() => setFoundDismissed(true)}>
                  Not now
                </Button>
                <Button size="sm" onClick={() => setImportOpen(true)}>
                  Review
                </Button>
              </div>
            ) : null}

            {error ? (
              <div className="q-banner err" style={{ margin: "0 0 18px" }}>
                <span className="ic">
                  <TriangleAlertIcon className="q-icon s14" aria-hidden />
                </span>
                <span className="grow">
                  <b>{error.message}</b> {error.hint ?? "The daemon did not answer."}
                </span>
                <Button variant="ghost" size="sm" onClick={() => void loadSkills({ force: true })}>
                  Retry
                </Button>
              </div>
            ) : null}

            <Conflicts conflicts={conflicts} />

            <div className="q-sec" style={{ marginTop: conflicts.length > 0 ? 24 : 0 }}>
              <div className="q-sec-h">
                <h2>Installed skills</h2>
                {isPhone ? null : (
                  <span className="aside">
                    Each harness gets a managed copy; versions can differ until the next sync
                  </span>
                )}
              </div>
              {loading && !inventory ? (
                SkeletonList()
              ) : skills.length === 0 ? (
                <div className="q-empty">
                  <h3>No skills yet</h3>
                  <p>
                    Add a folder with a SKILL.md, clone one from git, or import what your harnesses
                    already installed.
                  </p>
                  <div className="flex gap-2">
                    <Button onClick={() => setAddOpen(true)}>
                      <PlusIcon className="q-icon s14" aria-hidden /> Add skill
                    </Button>
                    <Button variant="secondary" onClick={() => setImportOpen(true)}>
                      <DownloadIcon className="q-icon s14" aria-hidden /> Import
                    </Button>
                  </div>
                </div>
              ) : (
                <div className="q-list">
                  {skills.map((skill) => (
                    <SkillRow
                      key={skill.id}
                      skill={skill}
                      isPhone={isPhone}
                      onOpen={() => setDetails(skill)}
                    />
                  ))}
                </div>
              )}
            </div>

            {inventory && inventory.harnesses.length > 0 ? (
              <div className="q-sec">
                <div className="q-sec-h">
                  <h2>Harness directories</h2>
                  <span className="aside">Where h4rn installs managed copies on this machine</span>
                </div>
                <div className="q-list">
                  {inventory.harnesses.map((entry) => (
                    <div className="q-li" key={entry.harness}>
                      <span
                        className={cn("inline-flex", entry.detected ? "q-ok" : "q-faint")}
                        aria-hidden
                      >
                        {entry.detected ? (
                          <CheckCircle2Icon className="q-icon s14" />
                        ) : (
                          <WifiOffIcon className="q-icon s14" />
                        )}
                      </span>
                      <div className="grow">
                        <div className="t">{HARNESS_LABELS[entry.harness]}</div>
                        <div className="s q-mono">
                          {entry.user_root ?? "No user skill directory"}
                        </div>
                      </div>
                      <span className="q-faint" style={{ fontSize: 12 }}>
                        {entry.detected ? "Detected" : "Not installed"}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            ) : null}
          </div>
        </div>
      </main>

      <SkillAddDialog open={addOpen} onClose={() => setAddOpen(false)} />
      <SkillImportDialog open={importOpen} onClose={() => setImportOpen(false)} />
      <SkillDetailsDialog
        open={Boolean(details)}
        skill={details}
        onClose={() => setDetails(null)}
      />
    </>
  );
}
