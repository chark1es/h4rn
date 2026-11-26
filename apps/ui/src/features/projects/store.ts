/**
 * Registered projects and the sidebar's project ordering.
 *
 * A project is a folder on a host. Threads already group by
 * `(host, workspace_path)`; registering the folder with the daemon adds a
 * name, a "date added" and a sidebar row before the first thread exists.
 * Sorting and the manual order are a per-device preference, like T3 Code's.
 */
import { useSyncExternalStore } from "react";

import type {
  ClientRequest,
  DirectoryListing,
  HostId,
  Project,
  ProjectSource,
  ResponseBody,
  ThreadSummary,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { type ProjectGroup, projectKey, projectName } from "@/lib/grouping";
import { i18n } from "@/lib/i18n";
import { getClient, getState, pushToast } from "@/lib/store/appStore";

export type ProjectSort = "activity" | "added" | "name" | "manual";
export type ThreadSort = "activity" | "name";

/** Every project order, in menu order; the sidebar words them (`sidebar:projects.projectSort.*`). */
export const PROJECT_SORTS: readonly ProjectSort[] = ["activity", "added", "name", "manual"];

/** Every thread order, in menu order (`sidebar:projects.threadSort.*`). */
export const THREAD_SORTS: readonly ThreadSort[] = ["activity", "name"];

export type Prefs = { projectSort: ProjectSort; threadSort: ThreadSort; order: string[] };

export type ProjectsState = {
  projects: Project[];
  loaded: boolean;
  /**
   * The local host's home folder, for expanding a `~` path exactly as the
   * daemon does. Null until the daemon has answered.
   */
  home: string | null;
  prefs: Prefs;
};

const PREFS_KEY = "h4rn.sidebar.projects";
const DEFAULT_PREFS: Prefs = { projectSort: "activity", threadSort: "activity", order: [] };

function loadPrefs(): Prefs {
  try {
    const raw = window.localStorage.getItem(PREFS_KEY);
    if (!raw) return DEFAULT_PREFS;
    const parsed = JSON.parse(raw) as Partial<Prefs>;
    return {
      projectSort:
        parsed.projectSort && PROJECT_SORTS.includes(parsed.projectSort)
          ? parsed.projectSort
          : DEFAULT_PREFS.projectSort,
      threadSort:
        parsed.threadSort && THREAD_SORTS.includes(parsed.threadSort)
          ? parsed.threadSort
          : DEFAULT_PREFS.threadSort,
      order: Array.isArray(parsed.order)
        ? parsed.order.filter((key): key is string => typeof key === "string")
        : [],
    };
  } catch {
    return DEFAULT_PREFS;
  }
}

let state: ProjectsState = {
  projects: [],
  loaded: false,
  home: null,
  prefs: typeof window === "undefined" ? DEFAULT_PREFS : loadPrefs(),
};
const listeners = new Set<() => void>();

function set(patch: Partial<ProjectsState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function setPrefs(patch: Partial<Prefs>): void {
  const prefs = { ...state.prefs, ...patch };
  try {
    window.localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // Non-fatal: the order still applies for this session.
  }
  set({ prefs });
}

export function useProjects<T>(selector: (value: ProjectsState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

export function getProjectsState(): ProjectsState {
  return state;
}

type ProjectRequest = Extract<
  ClientRequest,
  {
    type:
      | "list_projects"
      | "add_project"
      | "rename_project"
      | "remove_project"
      | "browse_directories";
  }
>;

async function send(request: ProjectRequest): Promise<ResponseBody> {
  return (await getClient().request(request as never)) as unknown as ResponseBody;
}

function projectHost(): HostId | null {
  return getState().localHostId;
}

function toastError(title: string, error: unknown, fallback: string): void {
  const proto = toProtoError(error);
  pushToast({
    title,
    description: `${proto.message}${proto.hint ? ` ${proto.hint}` : ` ${fallback}`}`,
    variant: "error",
  });
}

/** Loads the daemon's registered projects. Quiet on failure: threads still group. */
export async function loadProjects(): Promise<void> {
  const host = projectHost();
  if (!host) return;
  try {
    const response = await send({ type: "list_projects", host });
    if (response.type === "projects") set({ projects: response.projects, loaded: true });
  } catch {
    set({ loaded: true });
  }
  // The home folder is only needed to expand a typed `~` path, so it is read
  // once from any listing (the same answer the daemon itself expands with).
  // A failure leaves it unknown rather than guessing: a worktree option that
  // cannot be checked is not offered.
  if (state.home === null) {
    try {
      const response = await send({ type: "browse_directories", host, path: null });
      if (response.type === "directories") set({ home: response.listing.home });
    } catch {
      // Leave it unknown; the next load tries again.
    }
  }
}

/** Adds a project; returns it, or null after showing why it failed. */
export async function addProject(
  source: ProjectSource,
  name?: string,
): Promise<{ project: Project } | { error: string }> {
  const host = projectHost();
  if (!host) return { error: i18n.t("sidebar:projects.noHost") };
  try {
    const response = await send({
      type: "add_project",
      host,
      source,
      name: name?.trim() ? name.trim() : null,
    });
    if (response.type !== "project") throw new Error(`Unexpected answer \`${response.type}\`.`);
    const project = response.project;
    set({
      projects: [...state.projects.filter((entry) => entry.id !== project.id), project],
    });
    return { project };
  } catch (error) {
    const proto = toProtoError(error);
    return { error: proto.hint ? `${proto.message}. ${proto.hint}` : proto.message };
  }
}

export async function renameProject(projectId: string, name: string): Promise<boolean> {
  const host = projectHost();
  if (!host) return false;
  try {
    const response = await send({ type: "rename_project", host, project_id: projectId, name });
    if (response.type !== "project") return false;
    const project = response.project;
    set({ projects: state.projects.map((entry) => (entry.id === project.id ? project : entry)) });
    return true;
  } catch (error) {
    toastError(
      i18n.t("sidebar:projects.toasts.renameFailed"),
      error,
      i18n.t("sidebar:projects.toasts.tryAgain"),
    );
    return false;
  }
}

export async function removeProject(projectId: string): Promise<boolean> {
  const host = projectHost();
  if (!host) return false;
  try {
    await send({ type: "remove_project", host, project_id: projectId });
    set({ projects: state.projects.filter((entry) => entry.id !== projectId) });
    return true;
  } catch (error) {
    toastError(
      i18n.t("sidebar:projects.toasts.removeFailed"),
      error,
      i18n.t("sidebar:projects.toasts.tryAgain"),
    );
    return false;
  }
}

export async function browseDirectories(
  path: string | null,
): Promise<{ listing: DirectoryListing } | { error: string }> {
  const host = projectHost();
  if (!host) return { error: i18n.t("sidebar:projects.noHost") };
  try {
    const response = await send({ type: "browse_directories", host, path });
    if (response.type !== "directories") throw new Error(`Unexpected answer \`${response.type}\`.`);
    return { listing: response.listing };
  } catch (error) {
    const proto = toProtoError(error);
    return { error: proto.hint ? `${proto.message}. ${proto.hint}` : proto.message };
  }
}

export function setProjectSort(projectSort: ProjectSort): void {
  setPrefs({ projectSort });
}

export function setThreadSort(threadSort: ThreadSort): void {
  setPrefs({ threadSort });
}

/** Moves one project before another (or to the end) and switches to Manual. */
export function moveProject(keys: string[], moved: string, before: string | null): void {
  const order = keys.filter((key) => key !== moved);
  const index = before ? order.indexOf(before) : -1;
  if (index < 0) order.push(moved);
  else order.splice(index, 0, moved);
  setPrefs({ order, projectSort: "manual" });
}

// ---------------------------------------------------------------------------
// Sidebar groups
// ---------------------------------------------------------------------------

export type SidebarProject = ProjectGroup & {
  /** The registered project, when the daemon has one for this folder. */
  project: Project | null;
  /** Every thread in the folder, before the tab filter, for activity sorting. */
  lastActivity: number;
};

/** `~/code/api` for display when the path is under the host user's home. */
export function shortPath(path: string, home?: string | null): string {
  if (home && (path === home || path.startsWith(`${home}/`))) {
    return `~${path.slice(home.length)}`;
  }
  const match = /^\/(?:home|Users)\/[^/]+(\/.*)?$/.exec(path);
  return match ? `~${match[1] ?? ""}` : path;
}

/**
 * Builds the sidebar's project groups: every folder with a visible thread,
 * plus registered projects that have no threads yet, sorted by preference.
 */
export function sidebarProjects(input: {
  visible: ThreadSummary[];
  all: ThreadSummary[];
  projects: Project[];
  prefs: Prefs;
}): SidebarProject[] {
  const { visible, all, projects, prefs } = input;
  const registered = new Map(
    projects.map((project) => [
      projectKey({ host: project.host, workspace_path: project.path }),
      project,
    ]),
  );
  const activity = new Map<string, number>();
  for (const thread of all) {
    const key = projectKey(thread);
    const at = Date.parse(thread.last_activity) || 0;
    activity.set(key, Math.max(activity.get(key) ?? 0, at));
  }
  const groups = new Map<string, SidebarProject>();
  for (const thread of visible) {
    const key = projectKey(thread);
    let group = groups.get(key);
    if (!group) {
      const project = registered.get(key) ?? null;
      group = {
        key,
        name: project?.name ?? projectName(thread.project_path ?? thread.workspace_path),
        host: thread.host,
        threads: [],
        project,
        lastActivity: activity.get(key) ?? 0,
      };
      groups.set(key, group);
    }
    group.threads.push(thread);
  }
  for (const [key, project] of registered) {
    if (groups.has(key) || activity.has(key)) continue;
    groups.set(key, {
      key,
      name: project.name,
      host: project.host,
      threads: [],
      project,
      lastActivity: Date.parse(project.created_at) || 0,
    });
  }

  const byName = (left: SidebarProject, right: SidebarProject) =>
    left.name.localeCompare(right.name, undefined, { sensitivity: "base" });
  const byActivity = (left: SidebarProject, right: SidebarProject) =>
    right.lastActivity - left.lastActivity || byName(left, right);
  const added = (group: SidebarProject) =>
    group.project ? Date.parse(group.project.created_at) || 0 : group.lastActivity;

  const list = [...groups.values()];
  for (const group of list) {
    group.threads.sort(
      prefs.threadSort === "name"
        ? (left, right) => left.title.localeCompare(right.title, undefined, { sensitivity: "base" })
        : (left, right) =>
            Number(right.pinned) - Number(left.pinned) ||
            Date.parse(right.last_activity) - Date.parse(left.last_activity),
    );
  }
  switch (prefs.projectSort) {
    case "name":
      return list.sort(byName);
    case "added":
      return list.sort((left, right) => added(right) - added(left) || byName(left, right));
    case "manual": {
      const rank = new Map(prefs.order.map((key, index) => [key, index]));
      return list.sort((left, right) => {
        const a = rank.get(left.key);
        const b = rank.get(right.key);
        if (a !== undefined && b !== undefined) return a - b;
        if (a !== undefined) return -1;
        if (b !== undefined) return 1;
        return byActivity(left, right);
      });
    }
    default:
      return list.sort(byActivity);
  }
}
