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
import { Trans, useTranslation } from "react-i18next";

import { ChipSelect } from "@/components/common/ChipSelect";
import { LibraryErrorBanner } from "@/components/pages/LibraryErrorBanner";
import { PhoneLargeTitle, PhoneNavButton } from "@/components/shell/PhoneNav";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import type { HarnessKind, SkillConflict, SkillInstallState, SkillSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { HARNESS_LABELS } from "@/lib/entities";
import { formatRelativeTime } from "@/lib/i18n";
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

/** Product names, shortened so four fit across a phone row. */
const SHORT_HARNESS: Record<HarnessKind, string> = {
  claude_code: "Claude",
  codex: "Codex",
  opencode: "OpenCode",
  antigravity: "Anti",
  acp: "ACP",
};

function shortVersion(version: string | null): string {
  return version ? version.slice(0, 6) : "—";
}

function SyncBanner() {
  const sync = useSkills((state) => state.sync);
  const syncing = useSkills((state) => state.syncing);
  const connection = useApp((state) => state.connection.status);
  const now = useNow();
  const { t } = useTranslation(["library", "common"]);

  const offline = connection === "offline";
  const label = syncing
    ? t("skills.sync.syncing")
    : !sync
      ? offline
        ? t("skills.sync.offline")
        : t("skills.sync.notSynced")
      : sync.state === "synced"
        ? t("skills.sync.synced")
        : sync.state === "blocked"
          ? t("skills.sync.blocked")
          : sync.state === "offline"
            ? t("skills.sync.offline")
            : sync.state === "syncing"
              ? t("skills.sync.syncing")
              : t("skills.sync.idle");

  const detail = sync
    ? [
        sync.last_sync
          ? t("skills.sync.lastSync", { when: formatRelativeTime(sync.last_sync, now) })
          : t("skills.sync.noSync"),
        t("skills.sync.pushed", { pushed: sync.pushed }),
        t("skills.sync.pulled", { pulled: sync.pulled }),
      ].join(" · ")
    : offline
      ? t("skills.sync.offlineDetail")
      : t("skills.sync.idleDetail");
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
            · {sync.hint ?? t("skills.sync.conflicts", { count: sync.conflicts })}
          </span>
        ) : null}
      </span>
      <Button
        variant="ghost"
        size="sm"
        className="q-inline-end"
        onClick={() => void syncSkillsNow()}
        disabled={syncing}
      >
        {syncing ? t("skills.sync.syncing") : t("skills.sync.syncNow")}
      </Button>
    </div>
  );
}

function Conflicts({ conflicts }: { conflicts: SkillConflict[] }) {
  const isPhone = useIsPhone();
  const { t } = useTranslation(["library", "dialogs", "common"]);
  const [choices, setChoices] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  if (conflicts.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>{t("skills.conflicts.title")}</h2>
        <span className="aside">{t("skills.conflicts.aside")}</span>
      </div>
      <div className="flex flex-col gap-2">
        {conflicts.map((conflict) => {
          const choice = choices[conflict.id] ?? "keep_local";
          return (
            <div className="q-conflict flex-wrap gap-y-2" key={conflict.id}>
              <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
              <div className="grow" style={{ minWidth: isPhone ? "calc(100% - 28px)" : undefined }}>
                <div className="t">{conflict.name}</div>
                <div className="q-muted text-xs mt-0.5">
                  <Trans
                    t={t}
                    i18nKey="skills.conflicts.versions"
                    values={{
                      reason: conflict.reason,
                      local: conflict.local_version
                        ? shortVersion(conflict.local_version)
                        : t("skills.conflicts.none"),
                      remote: shortVersion(conflict.remote_version),
                    }}
                    components={{ v: <span className="q-mono" /> }}
                  />
                </div>
              </div>
              <ChipSelect
                label={t("library:skills.conflicts.resolutionFor", { name: conflict.name })}
                value={choice}
                options={[
                  { value: "keep_local", label: t("skills.conflicts.keepLocal") },
                  { value: "keep_remote", label: t("skills.conflicts.keepRemote") },
                  { value: "keep_both", label: t("skills.conflicts.keepBoth") },
                ]}
                onChange={(next) => setChoices((current) => ({ ...current, [conflict.id]: next }))}
              />
              <Button
                variant="secondary"
                disabled={Boolean(busy[conflict.id])}
                onClick={() => {
                  setBusy((current) => ({ ...current, [conflict.id]: true }));
                  void resolveSkillConflict(
                    conflict.id,
                    choice as "keep_local" | "keep_remote" | "keep_both",
                  ).finally(() => setBusy((current) => ({ ...current, [conflict.id]: false })));
                }}
              >
                {busy[conflict.id] ? t("skills.conflicts.settling") : t("skills.conflicts.resolve")}
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
  const { t } = useTranslation(["library", "common"]);
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
            <Switch
              checked={enabled}
              disabled={!install || Boolean(pending[`${skill.id}:${harness}`])}
              aria-label={t("skills.row.toggle", {
                skill: skill.name,
                harness: HARNESS_LABELS[harness],
              })}
              title={
                !install
                  ? t("skills.row.noDirectory", { harness: HARNESS_LABELS[harness] })
                  : t("skills.row.version", {
                      harness: HARNESS_LABELS[harness],
                      version: install.version ?? t("skills.row.notInstalled"),
                    })
              }
              onCheckedChange={() => {
                if (!install) return;
                void setSkillEnabled(skill.id, harness, !enabled);
              }}
            />
            <span className="text-xs whitespace-nowrap text-[var(--fg-3)]">
              {isPhone ? SHORT_HARNESS[harness] : HARNESS_LABELS[harness]}
            </span>
            <span
              className={cn("q-mono text-xs", failed ? "q-warn" : "text-[var(--fg-3)]")}
              title={failed ? t("skills.row.missingCopy") : undefined}
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
  const { t } = useTranslation(["library", "common"]);
  const installed = skill.installs.filter((install) => install.installed).length;
  return (
    <div className="q-li" style={{ flexWrap: isPhone ? "wrap" : undefined }}>
      <span
        className="grid place-items-center rounded-md bg-[var(--fill)]"
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
              <TriangleAlertIcon className="q-icon s14" aria-hidden />{" "}
              {t("skills.row.conflicts", { count: skill.conflicts })}
            </span>
          ) : null}
        </div>
        <div className="s">{skill.description}</div>
        <div className="q-faint mt-1 text-xs">
          {t("skills.row.installs", {
            installed,
            count: skill.installs.length,
            source: sourceLabel(skill.source),
          })}
        </div>
      </button>
      <HarnessMatrix skill={skill} isPhone={isPhone} />
      {isPhone ? null : <ChevronRightIcon className="q-icon s14 q-faint" aria-hidden />}
    </div>
  );
}

function SkeletonList({ label }: { label: string }) {
  return (
    <div className="q-list" role="status" aria-busy="true" aria-label={label}>
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
          <div className="q-skel w-45 h-6" />
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
  const errorAction = useSkills((state) => state.errorAction);
  const { t } = useTranslation(["library", "settings", "dialogs", "common"]);
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
      <main className="q-main" aria-label={t("settings:nav.skills")}>
        {isPhone ? (
          <PhoneLargeTitle
            title={t("settings:nav.skills")}
            summary={
              loading && !inventory
                ? t("skills.loadingRegistry")
                : t("skills.summaryShort", { installed: skills.length, count: detected })
            }
            actions={
              <>
                <PhoneNavButton
                  label={t("skills.findExisting")}
                  onClick={() => setImportOpen(true)}
                >
                  <DownloadIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
                </PhoneNavButton>
                <PhoneNavButton
                  label={t("dialogs:skills.add.submit")}
                  onClick={() => setAddOpen(true)}
                >
                  <PlusIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
                </PhoneNavButton>
              </>
            }
          />
        ) : null}
        <div className="q-page">
          <div className="q-form wide">
            {isPhone ? null : (
              <div className="mb-5 flex flex-wrap items-start gap-3">
                <div className="grow basis-0">
                  <h1 className="text-xl font-semibold">{t("settings:nav.skills")}</h1>
                  <div className="q-muted mt-1">
                    {loading && !inventory
                      ? t("skills.loadingRegistry")
                      : t("skills.summary", { installed: skills.length, count: detected })}
                  </div>
                </div>
                <Button variant="secondary" onClick={() => setImportOpen(true)}>
                  <DownloadIcon className="q-icon s14" aria-hidden /> {t("skills.findExisting")}
                </Button>
                <Button onClick={() => setAddOpen(true)}>
                  <PlusIcon className="q-icon s14" aria-hidden /> {t("dialogs:skills.add.submit")}
                </Button>
              </div>
            )}

            <SyncBanner />

            {unmanaged.length > 0 && !foundDismissed ? (
              <div className="q-banner" style={{ margin: "0 0 18px" }} role="status">
                <span className="ic">
                  <BlocksIcon className="q-icon s14" aria-hidden />
                </span>
                <span className="grow">
                  <b>
                    {unmanaged.length === 1
                      ? t("skills.found.one", { name: unmanaged[0]?.name ?? "" })
                      : t("skills.found.many", { count: unmanaged.length })}
                  </b>{" "}
                  {unmanaged.length === 1
                    ? t("skills.found.bringOne")
                    : t("skills.found.bringMany")}
                </span>
                <Button variant="ghost" size="sm" onClick={() => setFoundDismissed(true)}>
                  {t("skills.found.notNow")}
                </Button>
                <Button size="sm" onClick={() => setImportOpen(true)}>
                  {t("skills.found.review")}
                </Button>
              </div>
            ) : null}

            <LibraryErrorBanner
              error={error}
              action={errorAction}
              style={{ margin: "0 0 18px" }}
              onRetry={() => void loadSkills({ force: true })}
            />

            <Conflicts conflicts={conflicts} />

            <div className="q-sec" style={{ marginTop: conflicts.length > 0 ? 24 : 0 }}>
              <div className="q-sec-h">
                <h2>{t("skills.installed.title")}</h2>
                {isPhone ? null : <span className="aside">{t("skills.installed.aside")}</span>}
              </div>
              {loading && !inventory ? (
                <SkeletonList label={t("skills.loading")} />
              ) : skills.length === 0 ? (
                <div className="q-empty">
                  <h3>{t("skills.installed.emptyTitle")}</h3>
                  <p>{t("skills.installed.emptyBody")}</p>
                  <div className="flex gap-2">
                    <Button onClick={() => setAddOpen(true)}>
                      <PlusIcon className="q-icon s14" aria-hidden />{" "}
                      {t("dialogs:skills.add.submit")}
                    </Button>
                    <Button variant="secondary" onClick={() => setImportOpen(true)}>
                      <DownloadIcon className="q-icon s14" aria-hidden /> {t("skills.import")}
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
                  <h2>{t("skills.directories.title")}</h2>
                  <span className="aside">{t("skills.directories.aside")}</span>
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
                          {entry.user_root ?? t("skills.directories.noUserRoot")}
                        </div>
                      </div>
                      <span className="q-faint" style={{ fontSize: 12 }}>
                        {entry.detected
                          ? t("skills.directories.detected")
                          : t("skills.directories.notInstalled")}
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
