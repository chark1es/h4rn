import { CheckIcon, FolderIcon, GitBranchIcon, Trash2Icon, TriangleAlertIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import type {
  HarnessKind,
  ProtoError,
  SkillDiff,
  SkillImportCandidate,
  SkillImportPreview,
  SkillSource,
  SkillSummary,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { formatDateTime, formatNumber, formatRelativeTime, i18n } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import {
  addSkill,
  commitSkillImport,
  diffSkill,
  getSkillsState,
  isUnmanagedCandidate,
  previewSkillImport,
  removeSkill,
  useSkills,
} from "./store";

const ALL_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];
const NO_AVAILABILITY: never[] = [];
/** An example repository for the placeholder: a URL, the same in every language. */
const EXAMPLE_REPO = "git@github.com:acme/skills.git";

function FieldLabel({
  children,
  htmlFor,
  hint,
}: {
  children: React.ReactNode;
  htmlFor?: string;
  hint?: string;
}) {
  return (
    <label htmlFor={htmlFor} className="flex items-baseline justify-between">
      <span className="text-[12px] text-[var(--fg-3)]">{children}</span>
      {hint ? <span className="text-[11.5px] text-[var(--fg-3)]">{hint}</span> : null}
    </label>
  );
}

/** A size with one decimal in the active language's digits; the unit stays B, KB or MB. */
function bytes(count: number): string {
  const tenth = { minimumFractionDigits: 1, maximumFractionDigits: 1 };
  if (count >= 1_048_576) return `${formatNumber(count / 1_048_576, tenth)} MB`;
  if (count >= 1024) return `${formatNumber(count / 1024, tenth)} KB`;
  return `${formatNumber(count)} B`;
}

export function sourceLabel(source: SkillSource): string {
  switch (source.type) {
    case "local_folder":
      return i18n.t("dialogs:skills.source.folder", { path: source.path });
    case "git":
      return source.commit && source.commit !== "HEAD"
        ? i18n.t("dialogs:skills.source.gitAt", {
            url: source.url,
            commit: source.commit.slice(0, 7),
          })
        : i18n.t("dialogs:skills.source.git", { url: source.url });
    case "harness_dir":
      return i18n.t("dialogs:skills.source.harness", { harness: HARNESS_LABELS[source.harness] });
    case "connect":
      return i18n.t("dialogs:skills.source.connect");
  }
}

// ---------------------------------------------------------------------------
// Add
// ---------------------------------------------------------------------------

export function SkillAddDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const availability = useSkills((state) => state.inventory?.harnesses) ?? NO_AVAILABILITY;
  const [mode, setMode] = useState<"local_folder" | "git">("local_folder");
  const [path, setPath] = useState("");
  const [url, setUrl] = useState("");
  const [commit, setCommit] = useState("");
  const [subdir, setSubdir] = useState("");
  const [name, setName] = useState("");
  const { t } = useTranslation(["dialogs", "common"]);
  const [harnesses, setHarnesses] = useState<HarnessKind[]>(ALL_HARNESSES);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<"noHarness" | "noFolder" | "noRepo" | "secret" | null>(
    null,
  );

  useEffect(() => {
    if (!open) return;
    setMode("local_folder");
    setPath("");
    setUrl("");
    setCommit("");
    setSubdir("");
    setName("");
    // Snapshot the detected harnesses once, when the dialog opens. Reading the
    // store lazily keeps a late inventory load from wiping what the user has
    // already typed into the form.
    const detected = (getSkillsState().inventory?.harnesses ?? NO_AVAILABILITY)
      .filter((entry) => entry.detected)
      .map((entry) => entry.harness);
    setHarnesses(detected.length > 0 ? detected : ALL_HARNESSES);
    setBusy(false);
    setProblem(null);
  }, [open]);

  const toggle = (harness: HarnessKind) => {
    setHarnesses((current) =>
      current.includes(harness)
        ? current.filter((entry) => entry !== harness)
        : [...current, harness],
    );
  };

  const submit = async () => {
    if (harnesses.length === 0) {
      setProblem("noHarness");
      return;
    }
    if (mode === "local_folder" && !path.trim()) {
      setProblem("noFolder");
      return;
    }
    if (mode === "git" && !url.trim()) {
      setProblem("noRepo");
      return;
    }
    const source: SkillSource =
      mode === "local_folder"
        ? { type: "local_folder", path: path.trim() }
        : {
            type: "git",
            url: url.trim(),
            commit: commit.trim() || "HEAD",
            subdir: subdir.trim() || null,
          };
    setBusy(true);
    setProblem(null);
    const added = await addSkill({
      source,
      harnesses,
      name: name.trim() || null,
    });
    setBusy(false);
    if (added) onClose();
    else setProblem("secret");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t("skills.add.title")}</DialogTitle>
          <DialogDescription>{t("skills.add.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-3">
            <div className="flex flex-col gap-1">
              <FieldLabel>{t("skills.add.source")}</FieldLabel>
              <div className="q-seg inline" role="tablist" aria-label={t("skills.add.sourceLabel")}>
                <button
                  type="button"
                  role="tab"
                  aria-selected={mode === "local_folder"}
                  onClick={() => setMode("local_folder")}
                >
                  <FolderIcon className="q-icon s14" aria-hidden /> {t("skills.add.folder")}
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={mode === "git"}
                  onClick={() => setMode("git")}
                >
                  <GitBranchIcon className="q-icon s14" aria-hidden /> {t("skills.add.git")}
                </button>
              </div>
            </div>

            {mode === "local_folder" ? (
              <div className="flex flex-col gap-1">
                <FieldLabel htmlFor="skill-path">{t("skills.add.folderField")}</FieldLabel>
                <Input
                  id="skill-path"
                  value={path}
                  onChange={(event) => setPath(event.target.value)}
                  placeholder="~/code/skills/release-notes"
                  spellCheck={false}
                  autoComplete="off"
                />
                <span className="text-[11.5px] text-[var(--fg-3)]">
                  {t("skills.add.folderHint")}
                </span>
              </div>
            ) : (
              <>
                <div className="flex flex-col gap-1">
                  <FieldLabel htmlFor="skill-url">{t("fields.repository")}</FieldLabel>
                  <Input
                    id="skill-url"
                    value={url}
                    onChange={(event) => setUrl(event.target.value)}
                    placeholder={EXAMPLE_REPO}
                    spellCheck={false}
                    autoComplete="off"
                  />
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="flex flex-col gap-1">
                    <FieldLabel htmlFor="skill-commit" hint={t("skills.add.commitHint")}>
                      {t("skills.add.commit")}
                    </FieldLabel>
                    <Input
                      id="skill-commit"
                      value={commit}
                      onChange={(event) => setCommit(event.target.value)}
                      placeholder="main"
                      spellCheck={false}
                      autoComplete="off"
                    />
                  </div>
                  <div className="flex flex-col gap-1">
                    <FieldLabel htmlFor="skill-subdir" hint={t("fields.optional")}>
                      {t("skills.add.subdir")}
                    </FieldLabel>
                    <Input
                      id="skill-subdir"
                      value={subdir}
                      onChange={(event) => setSubdir(event.target.value)}
                      placeholder="skills/release-notes"
                      spellCheck={false}
                      autoComplete="off"
                    />
                  </div>
                </div>
              </>
            )}

            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="skill-name" hint={t("fields.optional")}>
                {t("skills.add.nameOverride")}
              </FieldLabel>
              <Input
                id="skill-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder="release-notes"
                spellCheck={false}
                autoComplete="off"
              />
            </div>

            <div className="flex flex-col gap-1.5">
              <span className="text-[12px] text-[var(--fg-3)]">{t("skills.add.installFor")}</span>
              <div className="grid grid-cols-2 gap-1.5">
                {ALL_HARNESSES.map((harness) => {
                  const state = availability.find((entry) => entry.harness === harness);
                  const checked = harnesses.includes(harness);
                  return (
                    <label
                      key={harness}
                      className="flex flex-wrap items-center gap-x-2 rounded-[6px] bg-bg-raised px-2 py-1.5 text-[12.5px] text-fg shadow-[inset_0_0_0_1px_var(--line-2)]"
                    >
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={() => toggle(harness)}
                        aria-label={t("skills.add.installForOne", {
                          harness: HARNESS_LABELS[harness],
                        })}
                      />
                      <span className="grow">{HARNESS_LABELS[harness]}</span>
                      {state && !state.detected ? (
                        <span className="text-[11px] text-[var(--fg-3)]">
                          {t("skills.add.notDetected")}
                        </span>
                      ) : null}
                    </label>
                  );
                })}
              </div>
            </div>

            {problem ? (
              <div className="q-banner err">
                {problem === "secret"
                  ? t("skills.secretRefusal")
                  : t(`skills.add.problems.${problem}`)}
              </div>
            ) : null}
          </div>
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button onClick={() => void submit()} disabled={busy}>
            {busy ? t("skills.add.adding") : t("skills.add.submit")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

type CandidateKind = "new" | "unmanaged" | "managed" | "conflict";

function candidateKind(candidate: SkillImportCandidate): CandidateKind {
  if (candidate.conflict) return "conflict";
  if (candidate.managed) return "managed";
  return candidate.duplicate ? "unmanaged" : "new";
}

/** Each kind's badge tone; its label is `skills.import.kinds.<kind>`. */
const KIND_TONE: Record<CandidateKind, "warn" | "ok" | undefined> = {
  new: undefined,
  unmanaged: undefined,
  managed: "ok",
  conflict: "warn",
};

type ImportProblem =
  | { kind: "scanFailed"; message: string }
  | { kind: "scanFailedRetry" }
  | { kind: "partial" };

/**
 * Brings skills a harness already has into h4rn. New skills join the
 * registry (and so the h4rn-connect sync); with "Manage" on, h4rn also takes
 * over each folder so a newer version from another machine replaces it.
 */
export function SkillImportDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [preview, setPreview] = useState<SkillImportPreview | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const { t } = useTranslation(["dialogs", "common"]);
  const [manage, setManage] = useState(true);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<ImportProblem | null>(null);

  useEffect(() => {
    if (!open) return;
    setPreview(null);
    setSelected({});
    setProblem(null);
    setManage(true);
    let cancelled = false;
    setBusy(true);
    let reason: ProtoError | null = null;
    void previewSkillImport({
      quiet: true,
      onError: (error) => {
        reason = error;
      },
    }).then((result) => {
      if (cancelled) return;
      setBusy(false);
      setPreview(result);
      if (!result) {
        // The daemon's own message and hint are shown as sent.
        const failure = reason as ProtoError | null;
        setProblem(
          failure
            ? {
                kind: "scanFailed",
                message: failure.hint ? `${failure.message} ${failure.hint}` : failure.message,
              }
            : { kind: "scanFailedRetry" },
        );
        return;
      }
      setSelected(
        Object.fromEntries(
          result.candidates.filter(isUnmanagedCandidate).map((candidate) => [candidate.id, true]),
        ),
      );
    });
    return () => {
      cancelled = true;
    };
  }, [open]);

  const candidates = preview?.candidates ?? [];
  const selectable = (candidate: SkillImportCandidate) => {
    const kind = candidateKind(candidate);
    return kind === "new" || (kind === "unmanaged" && manage);
  };
  const chosen = candidates.filter((candidate) => selected[candidate.id] && selectable(candidate));
  const adding = chosen.filter((candidate) => !candidate.duplicate).length;
  const managing = manage ? chosen.length : 0;
  const pending = candidates.filter(isUnmanagedCandidate).length;

  const importNow = async () => {
    setBusy(true);
    const ok = await commitSkillImport(
      chosen.map((candidate) => candidate.id),
      { manage },
    );
    setBusy(false);
    if (ok) onClose();
    else setProblem({ kind: "partial" });
  };

  const actionLabel = busy
    ? t("skills.import.working")
    : managing > 0 && adding === 0
      ? t("skills.import.manage", { count: managing })
      : t("skills.import.add", { count: chosen.length });

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("skills.import.title")}</DialogTitle>
          <DialogDescription>{t("skills.import.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <label className="q-skill-manage">
            <input
              type="checkbox"
              className="sr-only"
              checked={manage}
              onChange={(event) => setManage(event.target.checked)}
            />
            <span className={cn("q-check", manage && "on")} aria-hidden>
              {manage ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
            </span>
            <span>
              <span className="q-skill-manage-t">{t("skills.import.manageTitle")}</span>
              <span className="q-skill-manage-s">{t("skills.import.manageBody")}</span>
            </span>
          </label>

          <div className="q-sec-h">
            <h2>{t("skills.import.found")}</h2>
            <span className="aside">
              {busy && !preview
                ? t("skills.import.scanning")
                : preview
                  ? t("skills.import.summary", {
                      pending,
                      handled: candidates.length - pending,
                    })
                  : null}
            </span>
          </div>

          <div className="q-list" style={{ maxHeight: 320, overflow: "auto" }}>
            {busy && !preview ? (
              <div className="q-li">
                <span className="q-faint">{t("skills.import.scanningFolders")}</span>
              </div>
            ) : !preview ? (
              <div className="q-li">
                <span className="q-faint">{t("skills.import.noResults")}</span>
              </div>
            ) : candidates.length === 0 ? (
              <div className="q-li">
                <span className="q-faint">{t("skills.import.none")}</span>
              </div>
            ) : (
              candidates.map((candidate) => {
                const kind = candidateKind(candidate);
                const enabled = selectable(candidate);
                const tone = KIND_TONE[kind];
                const on = enabled && Boolean(selected[candidate.id]);
                return (
                  <label
                    key={candidate.id}
                    className={cn("q-li", !enabled && "q-li-off")}
                    style={{ cursor: enabled ? "pointer" : "default" }}
                  >
                    <input
                      type="checkbox"
                      className="sr-only"
                      checked={on}
                      disabled={!enabled}
                      onChange={(event) =>
                        setSelected((current) => ({
                          ...current,
                          [candidate.id]: event.target.checked,
                        }))
                      }
                      aria-label={t("skills.import.bringIn", { name: candidate.name })}
                    />
                    <span className={cn("q-check", on && "on")} aria-hidden>
                      {on ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
                    </span>
                    <div className="grow" style={{ minWidth: 0 }}>
                      <div className="t">{candidate.name}</div>
                      <div className="s truncate">
                        {HARNESS_LABELS[candidate.harness]} ·{" "}
                        <span className="q-mono">{candidate.source_path}</span>
                      </div>
                      {candidate.conflict ? (
                        <div
                          className="q-warn"
                          style={{ display: "flex", gap: 4, fontSize: 12, marginTop: 3 }}
                        >
                          <TriangleAlertIcon
                            className="q-icon s12"
                            style={{ flex: "none", marginTop: 2 }}
                            aria-hidden
                          />
                          <span>
                            {t("skills.import.conflict", { conflict: candidate.conflict })}
                          </span>
                        </div>
                      ) : null}
                    </div>
                    <span className={cn("q-badge", tone)}>{t(`skills.import.kinds.${kind}`)}</span>
                  </label>
                );
              })
            )}
          </div>

          {problem ? (
            <div className="q-banner err">
              {problem.kind === "scanFailed"
                ? t("skills.import.problems.scanFailed", { message: problem.message })
                : t(`skills.import.problems.${problem.kind}`)}
            </div>
          ) : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button onClick={() => void importNow()} disabled={busy || chosen.length === 0}>
            {actionLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Details, versions and diff
// ---------------------------------------------------------------------------

export function SkillDetailsDialog({
  open,
  skill,
  onClose,
}: {
  open: boolean;
  skill: SkillSummary | null;
  onClose: () => void;
}) {
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [diff, setDiff] = useState<SkillDiff | null>(null);
  const [busy, setBusy] = useState(false);
  const { t } = useTranslation(["dialogs", "common"]);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [problem, setProblem] = useState<"compare" | "remove" | null>(null);

  const versions = useMemo(() => {
    if (!skill) return [];
    return Array.from(
      new Set([
        skill.version,
        ...skill.installs.flatMap((install) => (install.version ? [install.version] : [])),
      ]),
    );
  }, [skill]);

  useEffect(() => {
    if (!open || !skill) return;
    setFrom("");
    setTo(skill.version);
    setDiff(null);
    setBusy(false);
    setConfirmRemove(false);
    setProblem(null);
  }, [open, skill]);

  if (!skill) return null;

  const compare = async () => {
    setBusy(true);
    setProblem(null);
    const result = await diffSkill(skill.id, from || null, to || skill.version);
    setBusy(false);
    if (result) setDiff(result);
    else setProblem("compare");
  };

  const remove = async () => {
    setBusy(true);
    const ok = await removeSkill(skill.id);
    setBusy(false);
    if (ok) onClose();
    else setProblem("remove");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{skill.name}</DialogTitle>
          <DialogDescription>{skill.description}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-1 text-[12.5px]">
            <div className="flex items-baseline gap-2">
              <span className="w-24 text-[var(--fg-3)]">{t("skills.details.version")}</span>
              <span className="q-mono">{skill.version.slice(0, 12)}</span>
              <span className="q-faint">· {bytes(skill.total_bytes)}</span>
            </div>
            <div className="flex items-baseline gap-2">
              <span className="w-24 text-[var(--fg-3)]">{t("skills.details.source")}</span>
              <span className="q-mono break-all">{sourceLabel(skill.source)}</span>
            </div>
            <div className="flex items-baseline gap-2">
              <span className="w-24 text-[var(--fg-3)]">{t("skills.details.updated")}</span>
              <span>
                {formatDateTime(skill.updated_at)}{" "}
                <span className="q-faint">({formatRelativeTime(skill.updated_at)})</span>
              </span>
            </div>
          </div>

          <div className="q-sec-h" style={{ marginTop: 4 }}>
            <h2>{t("skills.details.installs")}</h2>
            <span className="aside">{t("skills.details.thisMachine")}</span>
          </div>
          <table className="q-tbl">
            <thead>
              <tr>
                <th style={{ width: "22%" }}>{t("fields.harness")}</th>
                <th style={{ width: "18%" }}>{t("skills.details.version")}</th>
                <th style={{ width: "16%" }}>{t("skills.details.state")}</th>
                <th>{t("skills.details.path")}</th>
              </tr>
            </thead>
            <tbody>
              {skill.installs.map((install) => (
                <tr key={install.harness}>
                  <td>{HARNESS_LABELS[install.harness]}</td>
                  <td className="q-mono">{install.version?.slice(0, 8) ?? "—"}</td>
                  <td>
                    {install.installed ? (
                      <span className={install.enabled ? "q-ok" : "q-faint"}>
                        {install.enabled
                          ? t("skills.details.enabled")
                          : t("skills.details.disabled")}
                        {" · "}
                        {install.managed
                          ? t("skills.details.managed")
                          : t("skills.details.userCopy")}
                      </span>
                    ) : (
                      <span className="q-faint">{t("skills.details.notInstalled")}</span>
                    )}
                  </td>
                  <td className="q-mono q-faint" style={{ maxWidth: 280, overflow: "hidden" }}>
                    {install.path ?? "—"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          <div className="q-sec-h" style={{ marginTop: 4 }}>
            <h2>{t("skills.details.compareTitle")}</h2>
            <span className="aside">{t("skills.details.compareAside")}</span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <ChipSelect
              label={t("skills.details.fromVersion")}
              width={200}
              value={from}
              options={[
                { value: "", label: t("skills.details.newSkill") },
                ...versions.map((version) => ({ value: version, label: version.slice(0, 12) })),
              ]}
              onChange={setFrom}
            />
            <span className="q-faint">→</span>
            <ChipSelect
              label={t("skills.details.toVersion")}
              width={200}
              value={to}
              options={versions.map((version) => ({ value: version, label: version.slice(0, 12) }))}
              onChange={setTo}
            />
            <Button variant="secondary" onClick={() => void compare()} disabled={busy}>
              {busy ? t("skills.details.comparing") : t("skills.details.compare")}
            </Button>
          </div>

          {diff ? (
            <div className="q-list" style={{ maxHeight: 190, overflow: "auto" }}>
              {diff.files.length === 0 ? (
                <div className="q-li">
                  <span className="q-faint">{t("skills.details.same")}</span>
                </div>
              ) : (
                diff.files.map((file) => (
                  <div className="q-li" key={file.path}>
                    <span
                      className={cn(
                        "q-badge",
                        file.change === "added" && "ok",
                        file.change === "removed" && "danger",
                      )}
                    >
                      {t(`skills.details.change.${file.change}`)}
                    </span>
                    <span className="grow q-mono" style={{ fontSize: 12 }}>
                      {file.path}
                    </span>
                    <span className="q-faint q-num" style={{ fontSize: 11.5 }}>
                      {file.old_size !== null ? bytes(file.old_size) : "—"} →{" "}
                      {file.new_size !== null ? bytes(file.new_size) : "—"}
                    </span>
                  </div>
                ))
              )}
            </div>
          ) : null}

          {problem ? (
            <div className="q-banner err">{t(`skills.details.problems.${problem}`)}</div>
          ) : null}

          {confirmRemove ? (
            <div className="q-conflict">
              <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
              <div className="grow">
                <div className="t">{t("skills.details.removeTitle", { name: skill.name })}</div>
                <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                  {t("skills.details.removeBody")}
                </div>
              </div>
              <Button variant="ghost" size="sm" onClick={() => setConfirmRemove(false)}>
                {t("common:cancel")}
              </Button>
              <Button variant="destructive" size="sm" onClick={() => void remove()} disabled={busy}>
                {busy ? t("removing") : t("common:remove")}
              </Button>
            </div>
          ) : null}
        </DialogBody>
        <DialogFooter>
          {confirmRemove ? null : (
            <Button variant="ghost" onClick={() => setConfirmRemove(true)}>
              <Trash2Icon className="q-icon s14" aria-hidden /> {t("skills.details.removeSkill")}
            </Button>
          )}
          <span className="grow" />
          <Button variant="secondary" onClick={onClose}>
            {t("common:close")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
