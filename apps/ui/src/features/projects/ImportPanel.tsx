/**
 * The import pane inside Add project.
 *
 * It lists only what the daemon actually found, offers the recommended projects
 * checked, and shows a run's progress while it works. A person can close the
 * dialog and the daemon keeps importing; reopening asks for the current run.
 */
import { ArrowDownToLineIcon, CheckIcon, FolderGit2Icon, FolderIcon } from "lucide-react";
import { useEffect } from "react";

import { onImportFrame } from "@/lib/client/frames";
import { cn } from "@/lib/utils";
import {
  applyImportEvent,
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
    { label: "Repositories", items: projects.filter((project) => project.git) },
    { label: "Other folders", items: projects.filter((project) => !project.git) },
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
            ? "Looking for conversations…"
            : scan
              ? `${projects.length} of ${shownTotal} ${shownTotal === 1 ? "project" : "projects"}`
              : "No scan yet"}
        </span>
        <span className="grow" />
        <button
          type="button"
          className={cn("q-imp-link", reviewAll && "on")}
          aria-pressed={reviewAll}
          onClick={() => setReviewAll(!reviewAll)}
        >
          {reviewAll ? "Recommended only" : hiddenRecommended > 0 ? "Review all" : "Reviewing all"}
        </button>
      </div>

      <div className="q-imp-list" aria-busy={loading}>
        {error ? (
          <div className="q-fb-msg err" role="alert">
            {error}
          </div>
        ) : loading && !scan ? (
          <div className="q-fb-msg">Reading the harnesses' saved conversations…</div>
        ) : projects.length === 0 ? (
          <div className="q-fb-msg">
            {!scan
              ? "No scan yet."
              : shownTotal === 0
                ? "No conversations worth importing were found on this host."
                : "No recommended projects. Select Review all to see everything found."}
          </div>
        ) : (
          groups.map((group) => (
            <div key={group.label} className="q-imp-group">
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
                      {checked ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
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
                        ? "Already imported"
                        : `${project.importable} ${project.importable === 1 ? "conversation" : "conversations"}`}
                    </span>
                  </label>
                );
              })}
            </div>
          ))
        )}
      </div>

      <div className="q-imp-foot">
        <button type="button" className="q-imp-link" onClick={selectAllImportProjects}>
          Select all
        </button>
        <button type="button" className="q-imp-link" onClick={selectNoImportProjects}>
          Select none
        </button>
        <span className="grow" />
        {progress ? (
          <span className="q-imp-status" aria-live="polite">
            {progress.running
              ? `Importing ${progress.imported} of ${progress.conversations_total}…`
              : progress.imported > 0
                ? `Imported ${progress.imported} ${progress.imported === 1 ? "conversation" : "conversations"}.`
                : "Nothing new to import."}
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
        <p className="q-faint q-imp-note">
          {progress.failed} {progress.failed === 1 ? "conversation" : "conversations"} could not be
          imported.
        </p>
      ) : null}
    </div>
  );
}

/** Whether the dialog's primary button should run an import. */
export async function submitImport(): Promise<void> {
  await startImport();
}
