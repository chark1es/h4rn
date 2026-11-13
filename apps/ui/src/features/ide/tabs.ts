import type { ProtoError } from "@/gen";

/**
 * One open editor tab.
 *
 * The tab owns the buffer, not the editor: CodeMirror instances are cheap and
 * the buffer must survive remounts (phone sheet -> full pane) and optimistic
 * saves. `savedContent` is the last text the daemon acknowledged; `etag` is the
 * optimistic-concurrency token that text was read or written with.
 */
export type IdeTab = {
  path: string;
  name: string;
  content: string;
  savedContent: string;
  etag: string | null;
  encoding: string;
  loading: boolean;
  binary: boolean;
  truncated: boolean;
  hint: string | null;
  error: ProtoError | null;
  /** Line to reveal after a search hit or a diff jump. */
  revealLine: number | null;
  /** Increments when `revealLine` should be re-applied to an unchanged line. */
  revealNonce: number;
};

export type TabState = {
  tabs: IdeTab[];
  activePath: string | null;
};

export function tabName(path: string): string {
  const parts = path.split("/");
  return parts.at(-1) || path;
}

export function createTab(path: string): IdeTab {
  return {
    path,
    name: tabName(path),
    content: "",
    savedContent: "",
    etag: null,
    encoding: "utf-8",
    loading: true,
    binary: false,
    truncated: false,
    hint: null,
    error: null,
    revealLine: null,
    revealNonce: 0,
  };
}

export function isDirty(tab: IdeTab): boolean {
  return !tab.loading && !tab.error && tab.content !== tab.savedContent;
}

/** Opens a tab or focuses the existing one. Never duplicates a path. */
export function openTab(state: TabState, path: string): TabState {
  const existing = state.tabs.find((tab) => tab.path === path);
  if (existing) return { ...state, activePath: path };
  return { tabs: [...state.tabs, createTab(path)], activePath: path };
}

/** Closes a tab and chooses the neighbour that should take focus. */
export function closeTab(state: TabState, path: string): TabState {
  const index = state.tabs.findIndex((tab) => tab.path === path);
  if (index === -1) return state;
  const tabs = state.tabs.filter((tab) => tab.path !== path);
  if (state.activePath !== path) return { ...state, tabs };
  const next = tabs[Math.min(index, tabs.length - 1)];
  return { tabs, activePath: next?.path ?? null };
}

/** Moves a tab to a new index, keeping the dragged tab active. */
export function reorderTabs(state: TabState, path: string, toIndex: number): TabState {
  const from = state.tabs.findIndex((tab) => tab.path === path);
  if (from === -1) return state;
  const clamped = Math.max(0, Math.min(toIndex, state.tabs.length - 1));
  if (from === clamped) return state;
  const tabs = [...state.tabs];
  const [moved] = tabs.splice(from, 1);
  if (!moved) return state;
  tabs.splice(clamped, 0, moved);
  return { ...state, tabs, activePath: path };
}

export function updateTab(state: TabState, path: string, patch: Partial<IdeTab>): TabState {
  let changed = false;
  const tabs = state.tabs.map((tab) => {
    if (tab.path !== path) return tab;
    const next = { ...tab, ...patch };
    if (sameTab(tab, next)) return tab;
    changed = true;
    return next;
  });
  return changed ? { ...state, tabs } : state;
}

function sameTab(left: IdeTab, right: IdeTab): boolean {
  return (
    left.content === right.content &&
    left.savedContent === right.savedContent &&
    left.etag === right.etag &&
    left.loading === right.loading &&
    left.binary === right.binary &&
    left.truncated === right.truncated &&
    left.hint === right.hint &&
    left.error === right.error &&
    left.revealLine === right.revealLine &&
    left.revealNonce === right.revealNonce
  );
}

export function activeTab(state: TabState): IdeTab | null {
  return state.tabs.find((tab) => tab.path === state.activePath) ?? null;
}

export function dirtyTabs(state: TabState): IdeTab[] {
  return state.tabs.filter(isDirty);
}
