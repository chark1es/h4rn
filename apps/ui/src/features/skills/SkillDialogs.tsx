import { FolderIcon, GitBranchIcon, Trash2Icon, TriangleAlertIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

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
import type { HarnessKind, SkillDiff, SkillImportPreview, SkillSource, SkillSummary } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { absoluteDateTime, relativeAge } from "@/lib/format";
import { cn } from "@/lib/utils";
import {
  addSkill,
  commitSkillImport,
  diffSkill,
  previewSkillImport,
  removeSkill,
  useSkills,
} from "./store";

const ALL_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];
const NO_AVAILABILITY: never[] = [];

const SECRET_REFUSAL =
  "h4rn scans every file in a skill and refuses bundles that contain credentials. Remove the secret (or read it from an environment variable at run time) and add it again.";

const selectClass =
  "h-7 w-full rounded-[6px] border-0 bg-bg-raised px-2 text-[12.5px] text-fg shadow-[inset_0_0_0_1px_var(--line-2)] outline-none focus-visible:shadow-[inset_0_0_0_1px_var(--accent)]";

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
    const detected = availability.filter((entry) => entry.detected).map((entry) => entry.harness);
    setHarnesses(detected.length > 0 ? detected : ALL_HARNESSES);
    setBusy(false);
    setProblem(null);
  }, [open, availability]);

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

export function SkillImportDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [preview, setPreview] = useState<SkillImportPreview | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setPreview(null);
    setSelected({});
    setProblem(null);
    setBusy(false);
    let cancelled = false;
    setBusy(true);
    void previewSkillImport().then((result) => {
      if (cancelled) return;
      setBusy(false);
      setPreview(result);
      if (!result) {
        setProblem("h4rn could not read the harness skill directories.");
        return;
      }
      setSelected(
        Object.fromEntries(
          result.candidates
            .filter((candidate) => !candidate.duplicate && !candidate.conflict)
            .map((candidate) => [candidate.id, true]),
        ),
      );
    });
    return () => {
      cancelled = true;
    };
  }, [open]);

  const candidates = preview?.candidates ?? [];
  const chosen = candidates.filter((candidate) => selected[candidate.id]).length;

  const importNow = async () => {
    const ids = candidates.filter((candidate) => selected[candidate.id]).map((c) => c.id);
    setBusy(true);
    const ok = await commitSkillImport(ids);
    setBusy(false);
    if (ok) onClose();
    else setProblem("Some candidates were not imported. Rescan and try the remaining ones.");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[640px]">
        <DialogHeader>
          <DialogTitle>Import from harness directories</DialogTitle>
          <DialogDescription>
            h4rn scans the skill folders of each harness on this machine. Duplicates are skipped; a
            name with different content needs a conflict resolution first.
          </DialogDescription>
        </DialogHeader>

        <div className="q-sec-h">
          <h2>Candidates</h2>
          <span className="aside">
            {chosen} selected · {preview?.conflicts ?? 0} conflicts · {preview?.duplicates ?? 0}{" "}
            duplicates
          </span>
        </div>

        <div className="q-list" style={{ maxHeight: 320, overflow: "auto" }}>
          {busy && !preview ? (
            <div className="q-li">
              <span className="q-faint">Scanning…</span>
            </div>
          ) : candidates.length === 0 ? (
            <div className="q-li">
              <span className="q-faint">
                No importable skills found in the harness directories.
              </span>
            </div>
          ) : (
            candidates.map((candidate) => {
              const disabled = candidate.duplicate || Boolean(candidate.conflict);
              return (
                <label
                  key={candidate.id}
                  className="q-li"
                  style={{ cursor: disabled ? "default" : "pointer" }}
                >
                  <span className="q-check" aria-hidden>
                    <input
                      type="checkbox"
                      checked={Boolean(selected[candidate.id])}
                      disabled={disabled}
                      onChange={(event) =>
                        setSelected((current) => ({
                          ...current,
                          [candidate.id]: event.target.checked,
                        }))
                      }
                      aria-label={`Import ${candidate.name}`}
                    />
                  </span>
                  <div className="grow">
                    <div className="t">{candidate.name}</div>
                    <div className="s">
                      {HARNESS_LABELS[candidate.harness]} ·{" "}
                      <span className="q-mono">{candidate.source_path}</span>
                    </div>
                    {candidate.conflict ? (
                      <div className="q-warn" style={{ fontSize: 12, marginTop: 3 }}>
                        <TriangleAlertIcon className="q-icon s12" aria-hidden />{" "}
                        {candidate.conflict}
                      </div>
                    ) : null}
                  </div>
                  {candidate.duplicate ? (
                    <span className="q-badge">Already present</span>
                  ) : candidate.conflict ? (
                    <span className="q-badge warn">Conflict</span>
                  ) : null}
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
          <Button onClick={() => void importNow()} disabled={busy || chosen === 0}>
            {busy ? "Importing…" : `Import ${chosen} ${chosen === 1 ? "skill" : "skills"}`}
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
          <select
            className={cn(selectClass, "max-w-[220px]")}
            aria-label="From version"
            value={from}
            onChange={(event) => setFrom(event.target.value)}
          >
            <option value="">New skill (no prior version)</option>
            {versions.map((version) => (
              <option key={version} value={version}>
                {version.slice(0, 12)}
              </option>
            ))}
          </select>
          <span className="q-faint">→</span>
          <select
            className={cn(selectClass, "max-w-[220px]")}
            aria-label="To version"
            value={to}
            onChange={(event) => setTo(event.target.value)}
          >
            {versions.map((version) => (
              <option key={version} value={version}>
                {version.slice(0, 12)}
              </option>
            ))}
          </select>
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
