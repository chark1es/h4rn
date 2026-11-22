import { CheckIcon, FolderIcon, GitBranchIcon, Trash2Icon, TriangleAlertIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { ChipSelect } from "@/components/common/ChipSelect";

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
import { absoluteDateTime, relativeAge } from "@/lib/format";
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

const SECRET_REFUSAL =
  "h4rn scans every file in a skill and refuses bundles that contain credentials. Remove the secret (or read it from an environment variable at run time) and add it again.";

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

function bytes(count: number): string {
  if (count >= 1_048_576) return `${(count / 1_048_576).toFixed(1)} MB`;
  if (count >= 1024) return `${(count / 1024).toFixed(1)} KB`;
  return `${count} B`;
}

export function sourceLabel(source: SkillSource): string {
  switch (source.type) {
    case "local_folder":
      return `Folder · ${source.path}`;
    case "git":
      return `Git · ${source.url}${source.commit && source.commit !== "HEAD" ? ` @ ${source.commit.slice(0, 7)}` : ""}`;
    case "harness_dir":
      return `Imported from ${HARNESS_LABELS[source.harness]}`;
    case "connect":
      return "Synced registry";
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
  const [harnesses, setHarnesses] = useState<HarnessKind[]>(ALL_HARNESSES);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

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
      setProblem("Pick at least one harness. A skill is only useful where a harness can read it.");
      return;
    }
    if (mode === "local_folder" && !path.trim()) {
      setProblem("Enter the folder that contains SKILL.md.");
      return;
    }
    if (mode === "git" && !url.trim()) {
      setProblem("Enter a git repository URL.");
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
    else setProblem(SECRET_REFUSAL);
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[600px]">
        <DialogHeader>
          <DialogTitle>Add a skill</DialogTitle>
          <DialogDescription>
            h4rn copies the bundle into each harness&apos;s skill directory and keeps the registry
            row. The original folder is never modified.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="flex flex-col gap-1">
            <FieldLabel>Source</FieldLabel>
            <div className="q-seg inline" role="tablist" aria-label="Skill source">
              <button
                type="button"
                role="tab"
                aria-selected={mode === "local_folder"}
                onClick={() => setMode("local_folder")}
              >
                <FolderIcon className="q-icon s14" aria-hidden /> Folder
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={mode === "git"}
                onClick={() => setMode("git")}
              >
                <GitBranchIcon className="q-icon s14" aria-hidden /> Git
              </button>
            </div>
          </div>

          {mode === "local_folder" ? (
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="skill-path">Skill folder</FieldLabel>
              <Input
                id="skill-path"
                value={path}
                onChange={(event) => setPath(event.target.value)}
                placeholder="~/code/skills/release-notes"
                spellCheck={false}
                autoComplete="off"
              />
              <span className="text-[11.5px] text-[var(--fg-3)]">
                The folder must contain SKILL.md.
              </span>
            </div>
          ) : (
            <>
              <div className="flex flex-col gap-1">
                <FieldLabel htmlFor="skill-url">Repository</FieldLabel>
                <Input
                  id="skill-url"
                  value={url}
                  onChange={(event) => setUrl(event.target.value)}
                  placeholder="git@github.com:acme/skills.git"
                  spellCheck={false}
                  autoComplete="off"
                />
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <div className="flex flex-col gap-1">
                  <FieldLabel htmlFor="skill-commit" hint="Defaults to HEAD">
                    Commit
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
                  <FieldLabel htmlFor="skill-subdir" hint="Optional">
                    Subdirectory
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
            <FieldLabel htmlFor="skill-name" hint="Optional">
              Name override
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
            <span className="text-[12px] text-[var(--fg-3)]">Install for</span>
            <div className="grid grid-cols-2 gap-1.5">
              {ALL_HARNESSES.map((harness) => {
                const state = availability.find((entry) => entry.harness === harness);
                const checked = harnesses.includes(harness);
                return (
                  <label
                    key={harness}
                    className="flex items-center gap-2 rounded-[6px] bg-bg-raised px-2 py-1.5 text-[12.5px] text-fg shadow-[inset_0_0_0_1px_var(--line-2)]"
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => toggle(harness)}
                      aria-label={`Install for ${HARNESS_LABELS[harness]}`}
                    />
                    <span className="grow">{HARNESS_LABELS[harness]}</span>
                    {state && !state.detected ? (
                      <span className="text-[11px] text-[var(--fg-3)]">not detected</span>
                    ) : null}
                  </label>
                );
              })}
            </div>
          </div>

          {problem ? <div className="q-banner err">{problem}</div> : null}
        </div>

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={busy}>
            {busy ? "Adding…" : "Add skill"}
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

const KIND_BADGE: Record<CandidateKind, { label: string; tone?: "warn" | "ok" }> = {
  new: { label: "New" },
  unmanaged: { label: "In h4rn, not managed" },
  managed: { label: "Managed", tone: "ok" },
  conflict: { label: "Conflict", tone: "warn" },
};

/**
 * Brings skills a harness already has into h4rn. New skills join the
 * registry (and so the h4rn-connect sync); with "Manage" on, h4rn also takes
 * over each folder so a newer version from another machine replaces it.
 */
export function SkillImportDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [preview, setPreview] = useState<SkillImportPreview | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [manage, setManage] = useState(true);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

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
        setProblem(
          reason
            ? `The scan did not finish: ${reason.message}${reason.hint ? ` ${reason.hint}` : ""}`
            : "The scan did not finish. Try again.",
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
    else setProblem("Some skills were not brought in. Rescan and try the remaining ones.");
  };

  const actionLabel = busy
    ? "Working…"
    : managing > 0 && adding === 0
      ? `Manage ${managing} ${managing === 1 ? "skill" : "skills"}`
      : `Add ${chosen.length} ${chosen.length === 1 ? "skill" : "skills"}`;

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[680px]">
        <DialogHeader>
          <DialogTitle>Skills already on this machine</DialogTitle>
          <DialogDescription>
            h4rn scanned each harness's skill folder. Adding a skill puts it in your registry, which
            syncs to your other machines through h4rn-connect.
          </DialogDescription>
        </DialogHeader>

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
            <span className="q-skill-manage-t">Let h4rn manage these folders</span>
            <span className="q-skill-manage-s">
              h4rn marks each folder as its own without changing the skill's files. A newer version
              synced from another machine then replaces it, and removing the skill in h4rn deletes
              the folder. Off, the folders stay yours and only the registry copy syncs.
            </span>
          </span>
        </label>

        <div className="q-sec-h">
          <h2>Found</h2>
          <span className="aside">
            {busy && !preview
              ? "Scanning…"
              : preview
                ? `${pending} to bring in · ${candidates.length - pending} already handled`
                : null}
          </span>
        </div>

        <div className="q-list" style={{ maxHeight: 320, overflow: "auto" }}>
          {busy && !preview ? (
            <div className="q-li">
              <span className="q-faint">Scanning the harness folders…</span>
            </div>
          ) : !preview ? (
            <div className="q-li">
              <span className="q-faint">No results. The reason is below.</span>
            </div>
          ) : candidates.length === 0 ? (
            <div className="q-li">
              <span className="q-faint">No skills found in the harness folders.</span>
            </div>
          ) : (
            candidates.map((candidate) => {
              const kind = candidateKind(candidate);
              const enabled = selectable(candidate);
              const badge = KIND_BADGE[kind];
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
                    aria-label={`Bring in ${candidate.name}`}
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
                        <span>{candidate.conflict}. Settle it under Conflicts first.</span>
                      </div>
                    ) : null}
                  </div>
                  <span className={cn("q-badge", badge.tone)}>{badge.label}</span>
                </label>
              );
            })
          )}
        </div>

        {problem ? <div className="q-banner err">{problem}</div> : null}

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
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

function changeLabel(change: "added" | "removed" | "modified"): string {
  return change === "added" ? "Added" : change === "removed" ? "Removed" : "Modified";
}

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
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

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
    else setProblem("These versions could not be compared.");
  };

  const remove = async () => {
    setBusy(true);
    const ok = await removeSkill(skill.id);
    setBusy(false);
    if (ok) onClose();
    else setProblem("The skill is still in the registry.");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[700px]">
        <DialogHeader>
          <DialogTitle>{skill.name}</DialogTitle>
          <DialogDescription>{skill.description}</DialogDescription>
        </DialogHeader>

        <div className="grid gap-1 text-[12.5px]">
          <div className="flex items-baseline gap-2">
            <span className="w-24 text-[var(--fg-3)]">Version</span>
            <span className="q-mono">{skill.version.slice(0, 12)}</span>
            <span className="q-faint">· {bytes(skill.total_bytes)}</span>
          </div>
          <div className="flex items-baseline gap-2">
            <span className="w-24 text-[var(--fg-3)]">Source</span>
            <span className="q-mono break-all">{sourceLabel(skill.source)}</span>
          </div>
          <div className="flex items-baseline gap-2">
            <span className="w-24 text-[var(--fg-3)]">Updated</span>
            <span>
              {absoluteDateTime(skill.updated_at)}{" "}
              <span className="q-faint">({relativeAge(skill.updated_at)} ago)</span>
            </span>
          </div>
        </div>

        <div className="q-sec-h" style={{ marginTop: 4 }}>
          <h2>Installs</h2>
          <span className="aside">This machine</span>
        </div>
        <table className="q-tbl">
          <thead>
            <tr>
              <th style={{ width: "22%" }}>Harness</th>
              <th style={{ width: "18%" }}>Version</th>
              <th style={{ width: "16%" }}>State</th>
              <th>Path</th>
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
                      {install.enabled ? "Enabled" : "Disabled"}
                      {install.managed ? " · managed" : " · user copy"}
                    </span>
                  ) : (
                    <span className="q-faint">Not installed</span>
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
          <h2>Compare versions</h2>
          <span className="aside">Path-by-path digest diff</span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <ChipSelect
            label="From version"
            width={200}
            value={from}
            options={[
              { value: "", label: "New skill (no prior version)" },
              ...versions.map((version) => ({ value: version, label: version.slice(0, 12) })),
            ]}
            onChange={setFrom}
          />
          <span className="q-faint">→</span>
          <ChipSelect
            label="To version"
            width={200}
            value={to}
            options={versions.map((version) => ({ value: version, label: version.slice(0, 12) }))}
            onChange={setTo}
          />
          <Button variant="secondary" onClick={() => void compare()} disabled={busy}>
            {busy ? "Comparing…" : "Compare"}
          </Button>
        </div>

        {diff ? (
          <div className="q-list" style={{ maxHeight: 190, overflow: "auto" }}>
            {diff.files.length === 0 ? (
              <div className="q-li">
                <span className="q-faint">These versions contain the same files.</span>
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
                    {changeLabel(file.change)}
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

        {problem ? <div className="q-banner err">{problem}</div> : null}

        {confirmRemove ? (
          <div className="q-conflict">
            <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
            <div className="grow">
              <div className="t">Remove {skill.name}?</div>
              <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                The registry row and every h4rn-managed copy are deleted. A user-owned copy with the
                same name stays where it is.
              </div>
            </div>
            <Button variant="ghost" size="sm" onClick={() => setConfirmRemove(false)}>
              Cancel
            </Button>
            <Button variant="destructive" size="sm" onClick={() => void remove()} disabled={busy}>
              {busy ? "Removing…" : "Remove"}
            </Button>
          </div>
        ) : null}

        <DialogFooter>
          {confirmRemove ? null : (
            <Button variant="ghost" onClick={() => setConfirmRemove(true)}>
              <Trash2Icon className="q-icon s14" aria-hidden /> Remove skill
            </Button>
          )}
          <span className="grow" />
          <Button variant="secondary" onClick={onClose}>
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
