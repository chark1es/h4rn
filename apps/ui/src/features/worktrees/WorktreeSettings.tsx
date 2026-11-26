import { RefreshCwIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import type { TrackedWorktree } from "@/gen";
import { tildePath } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import {
  type ClearConfirmation,
  captureClear,
  clearAllDescription,
  clearableWorktrees,
  clearWorktrees,
  dismissWorktreeOutcome,
  forceCanFix,
  forceRemoveDescription,
  inUseWorktrees,
  loadWorktreeSettings,
  loadWorktrees,
  removeDescription,
  saveWorktreeSettings,
  settledWorktrees,
  splitMiddlePath,
  useWorktreeList,
  useWorktrees,
  worktreeClientCurrent,
  worktreeOutcomeForClient,
  worktreePage,
  worktreeSubline,
} from "./worktreeStore";

/** The branch, or the folder name when the checkout is detached. */
function worktreeTitle(row: TrackedWorktree): string {
  if (row.branch) return row.branch;
  const parts = row.path.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) ?? row.path;
}

/** A path that keeps its last two folders when the row is narrow. */
function MiddlePath({ path }: { path: string }) {
  const [head, tail] = splitMiddlePath(tildePath(path));
  return (
    <span
      className="q-mono q-muted"
      title={path}
      style={{ display: "flex", minWidth: 0, flex: "0 1 280px", fontSize: 12 }}
    >
      <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
        {head}
      </span>
      <span style={{ flex: "none", whiteSpace: "nowrap" }}>{tail}</span>
    </span>
  );
}

function WorktreeRow({
  row,
  action,
}: {
  row: TrackedWorktree;
  action?: { disabled: boolean; onClick: () => void };
}) {
  const { t } = useTranslation(["settings", "common"]);
  const title = worktreeTitle(row);
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t q-mono" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
          {title}
        </div>
        <div className="s">{worktreeSubline(row)}</div>
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <MiddlePath path={row.path} />
        {action ? (
          <button
            type="button"
            className="q-btn secondary"
            disabled={action.disabled}
            aria-label={t("worktrees.removeLabel", { name: title })}
            onClick={action.onClick}
          >
            {t("common:remove")}
          </button>
        ) : null}
      </div>
    </div>
  );
}

/** Settings → Worktrees: what h4rn tracks, what is in use, and a way to clear the rest. */
export function WorktreeSettings() {
  const { t } = useTranslation(["settings", "common"]);
  const online = useApp((state) => state.connection.status) === "online";
  useWorktreeList(online);
  // Intentionally discard the last clear's result when the page is left.
  useEffect(() => dismissWorktreeOutcome, []);

  const source = useWorktrees((state) => state.source);
  const current = worktreeClientCurrent(source);
  const storedRows = useWorktrees((state) => state.rows);
  const rows = current ? storedRows : [];
  const loaded = useWorktrees((state) => state.loaded) && current;
  const loading = useWorktrees((state) => state.loading);
  const clearing = useWorktrees((state) => state.clearing);
  const error = useWorktrees((state) => state.error);
  const outcome = useWorktrees(worktreeOutcomeForClient);
  const clearError = useWorktrees((state) => state.clearError);
  const settings = useWorktrees((state) => state.settings);
  const settingsSource = useWorktrees((state) => state.settingsSource);
  const settingsLoading = useWorktrees((state) => state.settingsLoading);
  const settingsSaving = useWorktrees((state) => state.settingsSaving);
  const settingsError = useWorktrees((state) => state.settingsError);
  const settingsCurrent = worktreeClientCurrent(settingsSource);

  const [location, setLocation] = useState(settings?.location ?? "");
  const [prefix, setPrefix] = useState(settings?.branch_prefix ?? "h4rn/");

  useEffect(() => {
    if (online) void loadWorktreeSettings();
  }, [online]);

  // The daemon is the source of truth: adopt what it returned, including the
  // default prefix it substitutes for an empty one.
  useEffect(() => {
    if (!settings) return;
    setLocation(settings.location ?? "");
    setPrefix(settings.branch_prefix);
  }, [settings]);

  const settingsDirty =
    settings !== null &&
    (location.trim() !== (settings.location ?? "") || prefix !== settings.branch_prefix);

  // Each confirmation keeps the machine and paths it was opened for, so a
  // refresh behind the dialog cannot change its targets.
  const [confirmAll, setConfirmAll] = useState<ClearConfirmation | null>(null);
  const [removing, setRemoving] = useState<{
    row: TrackedWorktree;
    confirmation: ClearConfirmation;
  } | null>(null);
  const [forcing, setForcing] = useState<ClearConfirmation | null>(null);

  // A rebuilt client means the open dialogs and their outcome describe
  // another machine: close them rather than let a confirm act there.
  useEffect(() => {
    if (current) return;
    setConfirmAll(null);
    setRemoving(null);
    setForcing(null);
    dismissWorktreeOutcome();
  }, [current]);

  const inUse = inUseWorktrees(rows);
  const settled = settledWorktrees(rows);
  const clearable = clearableWorktrees(rows);
  const page = worktreePage(loaded, rows, current ? error : null);
  const keptThreads = settled.length - clearable.length;
  const clearAllCount = confirmAll?.paths.length ?? 0;
  const forceCount = forcing?.paths.length ?? 0;
  const busy = clearing || !online || !current;
  const skipped = outcome?.skipped ?? [];
  // Only the skips a forced removal can actually fix are offered again: an
  // in-use, untracked or vanished entry is skipped either way.
  const forceable = skipped.filter(
    (item) => forceCanFix(item.reason) && settled.some((row) => row.path === item.path),
  );
  // A thread workspace belongs to whoever made it, so a forced removal says so.
  const forceableThreads = forceable.filter((item) =>
    settled.some((row) => row.path === item.path && row.origin === "thread"),
  );
  const threadNote =
    forceableThreads.length === 0
      ? ""
      : t("worktrees.force.threadNote", { count: forceableThreads.length });

  return (
    <div className="q-form">
      <h1>{t("worktrees.title")}</h1>
      <p className="lead">{t("worktrees.lead")}</p>

      <div className="q-group">
        <div className="q-group-head">
          <h2>{t("worktrees.newWorktrees")}</h2>
        </div>
        <p className="desc">{t("worktrees.newWorktreesHint")}</p>
        <div className="q-field">
          <div className="lab">
            <label className="t" htmlFor="worktree-location">
              {t("worktrees.location")}
            </label>
            <div className="s">
              <Trans
                t={t}
                i18nKey="worktrees.locationHint"
                components={{ code: <span className="q-mono" /> }}
              />
            </div>
          </div>
          <div className="ctl">
            <input
              id="worktree-location"
              className="q-input q-mono"
              value={location}
              disabled={!online || !settingsCurrent || settingsLoading || settingsSaving}
              placeholder="~/worktrees"
              spellCheck={false}
              onChange={(event) => setLocation(event.target.value)}
              style={{ maxWidth: 320, fontSize: 12 }}
            />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <label className="t" htmlFor="worktree-branch-prefix">
              {t("worktrees.branchPrefix")}
            </label>
            <div className="s">{t("worktrees.branchPrefixHint")}</div>
          </div>
          <div className="ctl">
            <input
              id="worktree-branch-prefix"
              className="q-input q-mono"
              value={prefix}
              disabled={!online || !settingsCurrent || settingsLoading || settingsSaving}
              placeholder="h4rn/"
              spellCheck={false}
              onChange={(event) => setPrefix(event.target.value)}
              style={{ maxWidth: 220, fontSize: 12 }}
            />
          </div>
        </div>
        <div className="q-field">
          <div className="lab" />
          <div className="ctl" style={{ justifyContent: "flex-start", gap: 8 }}>
            <button
              type="button"
              className="q-btn secondary"
              disabled={
                !online || !settingsCurrent || settingsLoading || settingsSaving || !settingsDirty
              }
              onClick={() =>
                void saveWorktreeSettings(
                  {
                    location: location.trim() === "" ? null : location.trim(),
                    branch_prefix: prefix,
                  },
                  settingsSource ?? undefined,
                )
              }
            >
              {settingsSaving ? t("common:saving") : t("common:save")}
            </button>
            {settingsError ? (
              <span className="q-field-error" role="alert">
                {settingsError.message}
                {settingsError.hint ? ` ${settingsError.hint}` : ""}
              </span>
            ) : null}
            {!settingsError && settings !== null && !settingsDirty ? (
              <span className="q-muted" style={{ fontSize: 12 }}>
                {t("worktrees.saved")}
              </span>
            ) : null}
          </div>
        </div>
      </div>

      {page.kind === "groups" && inUse.length > 0 ? (
        <div className="q-group">
          <div className="q-group-head">
            <h2>{t("worktrees.inUse")}</h2>
          </div>
          {inUse.map((row) => (
            <WorktreeRow key={row.path} row={row} />
          ))}
        </div>
      ) : null}

      <div className="q-group">
        <div className="q-group-head">
          <h2>{t("worktrees.settled")}</h2>
          <div style={{ display: "flex", gap: 8 }}>
            <button
              type="button"
              className="q-btn ghost"
              disabled={loading || !online || !current}
              onClick={() => void loadWorktrees()}
            >
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("common:refresh")}
            </button>
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy || clearable.length === 0}
              onClick={() =>
                setConfirmAll(
                  captureClear(
                    clearable.map((row) => row.path),
                    true,
                  ),
                )
              }
            >
              {t("worktrees.clearAll.button")}
            </button>
          </div>
        </div>

        {page.kind === "loading" ? (
          online ? (
            <div className="q-skel" style={{ height: 120 }} />
          ) : (
            <p className="q-muted">{t("worktrees.offline")}</p>
          )
        ) : (
          <>
            {page.notice ? <p className="q-muted">{page.notice}</p> : null}

            {outcome || clearError ? (
              <div className="q-harness-out" role="status" aria-live="polite">
                {outcome ? (
                  <p className="res">{t("worktrees.removed", { count: outcome.removed })}</p>
                ) : null}
                {clearError ? <p className="res bad">{clearError}</p> : null}
                {skipped.map((item) => (
                  <p className="res" key={item.path}>
                    <span className="q-mono" title={item.path}>
                      {tildePath(item.path)}
                    </span>{" "}
                    · {item.reason}
                  </p>
                ))}
                {forceable.length > 0 ? (
                  <button
                    type="button"
                    className="q-btn secondary"
                    style={{ marginTop: 8 }}
                    disabled={busy}
                    onClick={() =>
                      setForcing(
                        captureClear(
                          forceable.map((item) => item.path),
                          outcome?.bulk ?? false,
                        ),
                      )
                    }
                  >
                    {t("worktrees.force.button")}
                  </button>
                ) : null}
              </div>
            ) : null}

            {settled.length === 0 ? (
              page.empty ? (
                <p className="q-muted">{t("worktrees.empty")}</p>
              ) : rows.length > 0 ? (
                <p className="q-muted">{t("worktrees.nothingSettled")}</p>
              ) : null
            ) : (
              settled.map((row) => (
                <WorktreeRow
                  key={row.path}
                  row={row}
                  action={{
                    disabled: busy,
                    onClick: () => setRemoving({ row, confirmation: captureClear([row.path]) }),
                  }}
                />
              ))
            )}
          </>
        )}
      </div>

      <ConfirmDialog
        open={confirmAll !== null}
        title={t("worktrees.clearAll.title", { count: clearAllCount })}
        description={clearAllDescription(keptThreads)}
        confirmLabel={t("worktrees.clearAll.button")}
        onOpenChange={(open) => {
          if (!open) setConfirmAll(null);
        }}
        onConfirm={() => {
          if (confirmAll) {
            void clearWorktrees(confirmAll.paths, false, confirmAll.client, confirmAll.bulk);
          }
          setConfirmAll(null);
        }}
      />

      <ConfirmDialog
        open={removing !== null}
        title={t("worktrees.remove.title")}
        description={removing ? removeDescription(removing.row) : ""}
        confirmLabel={t("common:remove")}
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => {
          if (removing) {
            void clearWorktrees(
              removing.confirmation.paths,
              false,
              removing.confirmation.client,
              removing.confirmation.bulk,
            );
          }
          setRemoving(null);
        }}
      />

      <ConfirmDialog
        open={forcing !== null}
        title={t("worktrees.force.title", { count: forceCount })}
        description={forceRemoveDescription(threadNote)}
        confirmLabel={t("worktrees.force.button")}
        onOpenChange={(open) => {
          if (!open) setForcing(null);
        }}
        onConfirm={() => {
          if (forcing) {
            void clearWorktrees(forcing.paths, true, forcing.client, forcing.bulk);
          }
          setForcing(null);
        }}
      />
    </div>
  );
}
