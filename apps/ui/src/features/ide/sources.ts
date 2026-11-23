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
 * The phrase a review comment carries: "branch vs main", "commit 1a2b3c4".
 *
 * A turn is named by its own label ("turn 3"), so a comment written during one
 * turn is never mistaken for a comment on another.
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
