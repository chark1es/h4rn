import type {
  DiffAgainst,
  FsDiffCommit,
  FsDiffCommitList,
  FsDiffFiles,
  ThreadId,
  TurnId,
} from "@/gen";

/**
 * What a diff pane is comparing.
 *
 * `head` and `agent` are the original two scopes. `branch` reviews the working
 * tree against the merge base of a base branch, and `commit` reviews one commit
 * against its first parent. The daemon resolves every one of them; the UI only
 * names the choice.
 */
export type DiffSource = "head" | "agent" | "branch" | "commit";

export const DIFF_SOURCES: Array<{ value: DiffSource; label: string; hint: string }> = [
  { value: "head", label: "Uncommitted", hint: "against HEAD" },
  { value: "agent", label: "Turn", hint: "this turn's changes" },
  { value: "branch", label: "Branch", hint: "against the merge base" },
  { value: "commit", label: "Commits", hint: "one commit at a time" },
];

export type DiffScope = {
  source: DiffSource;
  threadId: ThreadId | null;
  turnId: TurnId | null;
  base: string | null;
  commit: string | null;
};

/**
 * The daemon request for the current scope, or `null` when the scope needs
 * something this workspace does not have (an agent turn, for example).
 */
export function againstForScope(scope: DiffScope): DiffAgainst | null {
  switch (scope.source) {
    case "head":
      return { kind: "head" };
    case "agent":
      return scope.threadId && scope.turnId
        ? { kind: "agent_turn", thread_id: scope.threadId, turn_id: scope.turnId }
        : null;
    case "branch":
      return { kind: "base", base: scope.base };
    case "commit":
      return scope.commit ? { kind: "commit", sha: scope.commit } : null;
  }
}

/**
 * The identity of a diff scope: what the pane asked the daemon for.
 *
 * A review comment is stored against this key, never against the display
 * label. The daemon prints a label of its own (the trailing characters of the
 * turn id, for example), and that text is not unique: it is for people to read.
 * The full request, turn id and commit sha included, is what keeps two
 * comments apart, so the label can change without moving a comment.
 */
export function diffScopeKey(against: DiffAgainst | null | undefined): string {
  if (!against) return "none";
  switch (against.kind) {
    case "head":
      return "head";
    case "agent_turn":
      return `turn:${against.thread_id}:${against.turn_id}`;
    case "base":
      return `base:${against.base ?? ""}`;
    case "commit":
      return `commit:${against.sha}`;
  }
}

/**
 * The scope a diff reply describes, with a branch's base filled in.
 *
 * A branch request may ask for the repository default (`base: null`), and the
 * daemon echoes back exactly that. The file list also reports which branch the
 * default resolved to. The default and that branch picked by name are one
 * comparison, so they must produce one comment key.
 */
export function resolvedAgainst(
  against: DiffAgainst | null | undefined,
  resolvedBase: string | null,
): DiffAgainst | null {
  if (!against) return null;
  if (against.kind === "base" && against.base === null && resolvedBase) {
    return { kind: "base", base: resolvedBase };
  }
  return against;
}

/**
 * The phrase a review comment carries: "branch vs main", "commit 1a2b3c4".
 *
 * This is display text only; `diffScopeKey` decides which diff a comment
 * belongs to.
 */
export function sourceDescription(source: DiffSource, label: string | null): string {
  switch (source) {
    case "head":
      return "uncommitted (HEAD)";
    case "agent":
      return label ? label.toLowerCase() : "agent turn";
    case "branch":
      return label ? `branch ${label}` : "branch vs base";
    case "commit":
      return label ? label.toLowerCase() : "commit";
  }
}

/** Keep the open file when it is still in the list, otherwise choose the first. */
export function firstDiffPath(files: FsDiffFiles | null, preferred: string | null): string | null {
  if (!files || files.files.length === 0) return null;
  if (preferred && files.files.some((file) => file.path === preferred)) return preferred;
  return files.files[0]?.path ?? null;
}

/** A commit row's label: short sha and subject, kept to one line. */
export function commitLabel(commit: Pick<FsDiffCommit, "short_sha" | "subject">): string {
  const subject = commit.subject.trim();
  return subject ? `${commit.short_sha} ${subject}` : commit.short_sha;
}

/** The commit a commit scope should show: the open one, else the newest. */
export function chooseCommit(
  commits: Pick<FsDiffCommitList, "commits"> | null,
  current: string | null,
): string | null {
  if (current && (commits?.commits.some((commit) => commit.sha === current) ?? true))
    return current;
  return commits?.commits[0]?.sha ?? null;
}

/** The base a branch scope should request; an empty list means the default. */
export function baseOptions(
  branches: string[],
  current: string | null,
): Array<{ value: string; label: string; hint?: string }> {
  const options: Array<{ value: string; label: string; hint?: string }> = branches.map((name) => ({
    value: name,
    label: name,
  }));
  if (current && !branches.includes(current)) {
    options.unshift({ value: current, label: current, hint: "current base" });
  }
  return options;
}
