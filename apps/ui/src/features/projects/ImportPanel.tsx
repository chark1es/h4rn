/**
 * The import pane inside Add project.
 *
 * It lists only what the daemon actually found, offers the recommended projects
 * checked, and shows a run's progress while it works. A person can close the
 * dialog and the daemon keeps importing; reopening asks for the current run.
 */
import { ArrowDownToLineIcon, CheckIcon, FolderGit2Icon, FolderIcon } from "lucide-react";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";

import { onImportFrame } from "@/lib/client/frames";
import { cn } from "@/lib/utils";
import {
  applyImportEvent,
  importErrorText,
  loadImportScan,
  selectAllImportProjects,
  selectNoImportProjects,
  setReviewAll,
  startImport,
  toggleImportProject,
  useImportState,
  visibleProjects,
} from "./importStore";
import { shortPath } from "./store";

export function ImportPanel() {
  const { t } = useTranslation("dialogs");
  const scan = useImportState((state) => state.scan);
  const loading = useImportState((state) => state.loading);
  const error = useImportState((state) => state.error);
  const selected = useImportState((state) => state.selected);
  const reviewAll = useImportState((state) => state.reviewAll);
  const progress = useImportState((state) => state.progress);

  useEffect(() => {
    void loadImportScan();
  }, []);
  useEffect(() => onImportFrame((event) => applyImportEvent(event.progress)), []);

  const projects = visibleProjects(scan, reviewAll);
  const groups = [
    {
      key: "repositories",
      label: t("import.repositories"),
      items: projects.filter((project) => project.git),
    },
    {
      key: "other",
      label: t("import.otherFolders"),
      items: projects.filter((project) => !project.git),
    },
  ].filter((group) => group.items.length > 0);
  const running = progress?.running ?? false;
  const shownTotal = scan?.projects.length ?? 0;
  const hiddenRecommended = (scan?.projects ?? []).filter((project) => !project.recommended).length;
  const percent =
    progress && progress.conversations_total > 0
      ? Math.round((progress.conversations_done / progress.conversations_total) * 100)
      : 0;

  return (
    <div className="q-imp">
      <div className="q-imp-bar">
        <span className="q-imp-count">
          {loading && !scan
            ? t("import.looking")
            : scan
              ? t("import.shown", { shown: projects.length, count: shownTotal })
              : t("import.noScan")}
        </span>
        <span className="grow" />
        <button
          type="button"
          className={cn("q-imp-link", reviewAll && "on")}
          aria-pressed={reviewAll}
          onClick={() => setReviewAll(!reviewAll)}
        >
          {reviewAll
            ? t("import.recommendedOnly")
            : hiddenRecommended > 0
              ? t("import.reviewAll")
              : t("import.reviewingAll")}
        </button>
      </div>

      <div className="q-imp-list" aria-busy={loading}>
        {error ? (
          <div className="q-fb-msg err" role="alert">
            {importErrorText(error)}
          </div>
        ) : loading && !scan ? (
          <div className="q-fb-msg">{t("import.readingSaved")}</div>
        ) : projects.length === 0 ? (
          <div className="q-fb-msg">
            {!scan
              ? t("import.noScanYet")
              : shownTotal === 0
                ? t("import.noneFound")
                : t("import.noRecommended")}
          </div>
        ) : (
          groups.map((group) => (
            <div key={group.key} className="q-imp-group">
              <div className="q-imp-group-h">{group.label}</div>
              {group.items.map((project) => {
                const checked = selected.includes(project.key);
                const exhausted = project.importable === 0;
                return (
                  <label
                    key={project.key}
                    className={cn("q-imp-row", checked && "on", exhausted && "done")}
                  >
                    <input
                      type="checkbox"
                      className="sr-only"
                      checked={checked}
                      disabled={exhausted || running}
                      onChange={() => toggleImportProject(project.key)}
                    />
                    <span className={cn("q-check", checked && "on")} aria-hidden>
                      {checked ? <CheckIcon className="q-icon s14" strokeWidth={2} /> : null}
                    </span>
                    {project.git ? (
                      <FolderGit2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                    ) : (
                      <FolderIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                    )}
                    <span className="q-imp-name">
                      <span className="q-imp-nm">{project.name}</span>
                      <span className="q-imp-path q-mono">{shortPath(project.path)}</span>
                    </span>
                    <span className="q-imp-meta">
                      {exhausted
                        ? t("import.alreadyImported")
                        : t("import.conversations", { count: project.importable })}
                    </span>
                  </label>
                );
              })}
            </div>
          ))
        )}
      </div>

      {scan && scan.warnings.length > 0 ? (
        <div className="q-imp-warn" role="status">
          {scan.warnings.map((warning) => (
            <p key={warning}>{warning}</p>
          ))}
        </div>
      ) : null}

      <div className="q-imp-foot">
        <button type="button" className="q-imp-link" onClick={selectAllImportProjects}>
          {t("import.selectAll")}
        </button>
        <button type="button" className="q-imp-link" onClick={selectNoImportProjects}>
          {t("import.selectNone")}
        </button>
        <span className="grow" />
        {progress ? (
          <span className="q-imp-status" aria-live="polite">
            {progress.running
              ? t("import.progress", {
                  done: progress.conversations_done,
                  total: progress.conversations_total,
                })
              : progress.imported > 0
                ? t("import.imported", { count: progress.imported })
                : t("import.nothingNew")}
          </span>
        ) : null}
      </div>

      {progress ? (
        <div className="q-imp-progress" aria-hidden>
          <div
            className="q-imp-progress-bar"
            style={{ width: `${progress.running ? Math.max(percent, 4) : 100}%` }}
          />
        </div>
      ) : null}

      {progress && progress.warnings.length > 0 ? (
        <div className="q-banner wrap" role="status">
          <ArrowDownToLineIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span className="grow">{progress.warnings.join(" ")}</span>
        </div>
      ) : null}
      {progress && progress.failed > 0 ? (
        <p className="q-faint q-imp-note">{t("import.failed", { count: progress.failed })}</p>
      ) : null}
    </div>
  );
}

/** Whether the dialog's primary button should run an import. */
export async function submitImport(): Promise<void> {
  await startImport();
}
