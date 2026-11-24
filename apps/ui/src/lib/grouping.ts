import type { ThreadSummary } from "@/gen";

/**
 * A project is a workspace folder on one host. Threads are grouped by that
 * pair only — the daemon carries both fields on every summary, so no extra
 * project registry is needed.
 *
 * A conversation imported from a linked worktree runs there, so its working
 * folder stays the worktree; `project_path` names the checkout it belongs to,
 * which is what the sidebar groups by.
 */
export type ProjectGroup = {
  key: string;
  name: string;
  host: string;
  threads: ThreadSummary[];
};

export function projectKey(
  thread: Pick<ThreadSummary, "host" | "workspace_path" | "project_path">,
): string {
  return `${thread.host}\u0000${thread.project_path ?? thread.workspace_path}`;
}

export function projectName(workspacePath: string): string {
  const parts = workspacePath.split("/").filter(Boolean);
  return parts.at(-1) ?? "Workspace";
}

export function monogram(name: string): string {
  const first = [...name].find((char) => /[a-z0-9]/i.test(char));
  return (first ?? "?").toUpperCase();
}

/** Groups keep the order in which their first thread appears in the list. */
export function groupThreads(threads: ThreadSummary[]): ProjectGroup[] {
  const groups = new Map<string, ProjectGroup>();
  for (const thread of threads) {
    const key = projectKey(thread);
    const existing = groups.get(key);
    if (existing) {
      existing.threads.push(thread);
      continue;
    }
    groups.set(key, {
      key,
      name: projectName(thread.workspace_path),
      host: thread.host,
      threads: [thread],
    });
  }
  return [...groups.values()];
}
