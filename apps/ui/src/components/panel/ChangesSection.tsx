import { ChevronDownIcon, Code2Icon, FileIcon, RotateCcwIcon } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ShortcutCaps } from "@/components/ui/kbd";
import { Tip } from "@/components/ui/tooltip";
import type { ThreadSummary } from "@/gen";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { openInIde, setView } from "@/lib/store/appStore";
import { FileDiff } from "./FileDiff";
import type { PanelChanges } from "./usePanelChanges";

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/** The Changes tab, shared by the right panel and the phone sheet's body. */
export function ChangesSection({
  thread,
  changes,
  ide,
  onFooterAction,
}: {
  thread: ThreadSummary;
  changes: PanelChanges;
  ide: boolean;
  onFooterAction?: () => void;
}) {
  const { t } = useTranslation(["thread", "common"]);
  const { mode, setMode, turns, turnId, changes: turnChanges, diffs, loading, error } = changes;
  const files = turnChanges?.files ?? [];
  const turnLabel = pickedLabel(turns, turnId, t("changes.noTurns"));
  const [confirm, setConfirm] = useState<
    { kind: "all" } | { kind: "file"; path: string; name: string } | null
  >(null);

  return (
    <>
      <div className="q-rp-bar">
        <div className="q-seg inline" role="tablist" aria-label={t("changes.compare")}>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "agent"}
            onClick={() => setMode("agent")}
          >
            {t("changes.vsAgent")}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "head"}
            onClick={() => setMode("head")}
          >
            {t("changes.vsHead")}
          </button>
        </div>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button type="button" className="q-btn ghost" style={{ height: 26 }}>
              {turnLabel} <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-44">
            <DropdownMenuLabel>{t("changes.turn")}</DropdownMenuLabel>
            {turns.map((turn) => (
              <DropdownMenuItem key={turn.id} onSelect={() => changes.selectTurn(turn.id)}>
                {turn.label}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
        <span className="grow" />
        <span className="q-num q-pm">
          <span className="q-add">+{changes.added}</span>{" "}
          <span className="q-del">−{changes.removed}</span>
        </span>
      </div>

      <div className="q-rp-body q-scroll">
        {error ? (
          <div className="q-sub-empty">
            <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>
              {t("changes.loadFailed")}
            </p>
            <p style={{ margin: "4px 0 10px" }}>{error}</p>
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={changes.reload}
            >
              {t("common:retry")}
            </button>
          </div>
        ) : loading && !turnChanges ? (
          <div
            role="status"
            aria-busy="true"
            aria-label={t("changes.loading")}
            style={{ padding: "10px 12px" }}
          >
            <div className="q-skel" style={{ height: 14, width: "52%", marginBottom: 12 }} />
            {[0, 1, 2, 3].map((row) => (
              <div key={row} className="q-skel" style={{ height: 28, marginBottom: 6 }} />
            ))}
          </div>
        ) : files.length === 0 ? (
          <div className="q-empty q-rp-empty">
            <h3>{t("changes.emptyTitle")}</h3>
            <p>{t("changes.emptyBody", { harness: harnessLabel(thread.harness) })}</p>
          </div>
        ) : (
          <>
            <div className="q-rp-by">
              <EntityAvatar
                seed={harnessSeed(thread.host, thread.harness)}
                kind="harness"
                label={harnessLabel(thread.harness)}
                size={14}
              />
              <span>
                {t("changes.by", {
                  harness: harnessLabel(thread.harness),
                  host: thread.host,
                  count: files.length,
                  time: absoluteTime(thread.last_activity),
                })}
              </span>
            </div>
            <div className="q-flist">
              {files.map((file) => {
                const { directory, name } = splitPath(file.path);
                return (
                  <div key={file.path} style={{ display: "flex", alignItems: "center", gap: 2 }}>
                    <button type="button" className="q-fr" onClick={() => openInIde(file.path)}>
                      <FileIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                      <span className="p q-mono">
                        <span>{directory}</span>
                        {name}
                      </span>
                      {file.before_etag === null ? (
                        <span className="q-badge ok" style={{ height: 18 }}>
                          {t("changes.new")}
                        </span>
                      ) : null}
                      <span className="q-num q-pm">
                        <span className="q-add">+{file.added}</span>
                        {file.removed > 0 ? <span className="q-del"> −{file.removed}</span> : null}
                      </span>
                    </button>
                    <Tip label={t("changes.revertFile", { name: file.path })}>
                      <button
                        type="button"
                        className="q-ibtn"
                        aria-label={t("changes.revertFile", { name })}
                        onClick={() => setConfirm({ kind: "file", path: file.path, name })}
                      >
                        <RotateCcwIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      </button>
                    </Tip>
                  </div>
                );
              })}
            </div>
            {ide ? (
              <p className="q-rp-note">{t("changes.ideNote")}</p>
            ) : (
              files.map((file) => (
                <FileDiff
                  key={file.path}
                  path={file.path}
                  diff={diffs[file.path] ?? null}
                  added={file.added}
                  removed={file.removed}
                  loading={changes.diffsLoading}
                  onOpen={() => openInIde(file.path)}
                />
              ))
            )}
          </>
        )}
      </div>

      <div className="q-rp-foot">
        <button
          type="button"
          className="q-btn ghost"
          disabled={files.length === 0}
          onClick={() => setConfirm({ kind: "all" })}
        >
          {t("changes.revertAll")}
        </button>
        <span className="grow" />
        {ide ? (
          <button type="button" className="q-btn secondary" onClick={() => setView("thread")}>
            {t("changes.backToThread")} <ShortcutCaps command="view.toggle" />
          </button>
        ) : (
          <button
            type="button"
            className="q-btn secondary"
            onClick={onFooterAction ?? (() => setView("ide"))}
          >
            <Code2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("changes.openInIde")} <ShortcutCaps command="view.toggle" />
          </button>
        )}
      </div>
      <ConfirmDialog
        open={confirm !== null}
        title={
          confirm?.kind === "file"
            ? t("changes.confirm.fileTitle", { name: confirm.name })
            : t("changes.confirm.allTitle", { count: files.length })
        }
        description={
          confirm?.kind === "file"
            ? t("changes.confirm.fileBody")
            : t("changes.confirm.allBody", { count: files.length })
        }
        confirmLabel={t("changes.confirm.confirm")}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        onConfirm={() => {
          if (confirm?.kind === "file") void changes.revertFile(confirm.path);
          else if (confirm?.kind === "all") void changes.revertAll();
        }}
      />
    </>
  );
}

function pickedLabel(
  turns: Array<{ id: string; label: string }>,
  turnId: string | null,
  none: string,
): string {
  if (turns.length === 0) return none;
  const picked = turns.find((turn) => turn.id === turnId);
  return picked?.label ?? turns.at(-1)?.label ?? none;
}
