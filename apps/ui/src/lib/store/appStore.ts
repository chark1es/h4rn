import { useSyncExternalStore } from "react";
import { isPlacementMove, noteManualPlacement } from "@/features/notifications/placement";
import { leftAloneList } from "@/features/rewind/left-alone";
import type {
  ApprovalDecision,
  Attachment,
  ClientRequest,
  DroppedSetting,
  HarnessKind,
  HostId,
  HostSummary,
  LimitStatus,
  ProtoError,
  QueueAction,
  QueuedMessage,
  ResumeSchedule,
  RewindPlan,
  RewoundPrompt,
  RuntimeMode,
  ThreadId,
  ThreadSummary,
  ThreadTab,
  TurnId,
  WorktreeRequest,
} from "@/gen";
import { isTurnRunning } from "@/lib/agent";
import type { H4rnClient } from "@/lib/client/client";
import { createClientFromEnv, readBootConfig } from "@/lib/client/config";
import { toProtoError } from "@/lib/client/errors";
import type { ConnectionSnapshot, EventEnvelope } from "@/lib/client/types";
import { drafts, moveLocalThreadDraft } from "@/lib/drafts";
import { i18n } from "@/lib/i18n";
import { joinSentences } from "@/lib/i18n/sentences";
import { pushRoute, type Route, readRoute, replaceRoute, routePath } from "@/lib/router";
import { applyTerminalTheme } from "@/lib/terminalTheme";
import { createThreadLog, foldEvent, isRealTurn, type ThreadLog } from "./projection";

export type Theme = "light" | "dark" | "system";

export type UiView = "thread" | "ide";
export type PanelTab = "changes" | "files" | "subagents" | "preview";

export type ThreadFilterState = {
  host: HostId | null;
  harness: HarnessKind | null;
  needsAttention: boolean;
  unread: boolean;
};

export type LayoutState = {
  sidebarOpen: boolean;
  sidebarWidth: number;
  panelOpen: boolean;
  panelWidth: number;
  panelTab: PanelTab;
  view: UiView;
  terminalOpen: boolean;
  closedProjects: Record<string, boolean>;
  librarySelected: string;
};

export type UiState = {
  route: Route;
  tab: ThreadTab;
  query: string;
  selectedThreadId: ThreadId | null;
  paletteOpen: boolean;
  shortcutsOpen: boolean;
  newThreadOpen: boolean;
  /** The Link pull request dialog is open for the focused thread. */
  linkPrOpen: boolean;
  theme: Theme;
  snoozedOpen: boolean;
  filter: ThreadFilterState;
  filterOpen: boolean;
  moreOpen: boolean;
  limitsOpen: boolean;
  /** Settings → Shortcuts is capturing a chord; global shortcuts stand down. */
  recordingShortcut: boolean;
  /** Host and folder the next New thread dialog starts with. */
  newThreadPreset: { host: string; workspace: string } | null;
  /** The Add project dialog is open. */
  addProjectOpen: boolean;
  /** The queued message currently open in the composer for editing. */
  queuedEdit: { threadId: ThreadId; messageId: string } | null;
  focusSearchNonce: number;
  focusComposerNonce: number;
  focusFilesNonce: number;
  pendingIdePath: string | null;
  /** The file the IDE editor currently shows, for the panel tree's selection. */
  ideActivePath: string | null;
  /** The HTML file the Preview tab shows, scoped to its thread. */
  preview: { threadId: ThreadId; path: string } | null;
  /** The preview is open full-width in the main area instead of the panel. */
  previewExpanded: boolean;
  /**
   * A prompt a rewind returned, waiting for its composer to adopt it.
   *
   * The draft that was already in the box stays above it; the token lets the
   * composer consume exactly one restore and lets a newer one win.
   */
  composerRestore: {
    threadId: ThreadId;
    token: number;
    prompt: string;
    attachments: Attachment[];
  } | null;
  pendingRouteFocus: boolean;
  layout: LayoutState;
};

const LAYOUT_KEY = "h4rn.layout";
const DEFAULT_LAYOUT: LayoutState = {
  sidebarOpen: true,
  sidebarWidth: 288,
  panelOpen: false,
  panelWidth: 400,
  panelTab: "changes",
  view: "thread",
  terminalOpen: false,
  closedProjects: { "orion\u0000/home/you/code/infra": true },
  librarySelected: "playwright",
};

function readLayout(): LayoutState {
  try {
    const raw = window.localStorage.getItem(LAYOUT_KEY);
    if (!raw) return { ...DEFAULT_LAYOUT, closedProjects: { ...DEFAULT_LAYOUT.closedProjects } };
    const parsed = JSON.parse(raw) as Partial<LayoutState>;
    const stored = parsed.panelTab;
    // A preview is scoped to a file and a live thread; after a reload there is
    // nothing to show, so return to the file tree instead.
    const panelTab: PanelTab =
      stored === "files" || stored === "subagents"
        ? stored
        : stored === "preview"
          ? "files"
          : "changes";
    return {
      ...DEFAULT_LAYOUT,
      ...parsed,
      panelTab,
      sidebarWidth: clampNumber(parsed.sidebarWidth, 240, 420, DEFAULT_LAYOUT.sidebarWidth),
      panelWidth: clampNumber(parsed.panelWidth, 320, 640, DEFAULT_LAYOUT.panelWidth),
      closedProjects: { ...DEFAULT_LAYOUT.closedProjects, ...(parsed.closedProjects ?? {}) },
    };
  } catch {
    return { ...DEFAULT_LAYOUT, closedProjects: { ...DEFAULT_LAYOUT.closedProjects } };
  }
}

function clampNumber(value: unknown, min: number, max: number, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.max(min, Math.min(max, value))
    : fallback;
}

function writeLayout(layout: LayoutState): void {
  try {
    // The view (Thread/IDE) is a per-visit choice; the rest of the shell is
    // remembered the way the prototype remembers panel and width.
    window.localStorage.setItem(LAYOUT_KEY, JSON.stringify({ ...layout, view: "thread" }));
  } catch {
    // Layout memory is a convenience; the session still works without it.
  }
}

export type ToastVariant = "error" | "info" | "success";

export type Toast = {
  id: number;
  title: string;
  description?: string;
  variant: ToastVariant;
};

export type PendingCreate = {
  tempId: ThreadId;
  prompt: string;
  at: number;
};

/**
 * The queue rows this client has for one thread.
 *
 * The thread summary only counts the queue, because a full queue can be larger
 * than a paired transport's frame. The rows are fetched separately and paged;
 * `revision` says which version of the queue they are, so a late reply cannot
 * put back a message that already started.
 */
export type ThreadQueue = {
  queued: QueuedMessage[];
  total: number;
  revision: number;
};

export type AppState = {
  connection: ConnectionSnapshot;
  hosts: Record<HostId, HostSummary>;
  threads: Record<ThreadId, ThreadSummary>;
  /**
   * Journal sequence of the newest summary this client has applied per thread.
   *
   * A reply that carries a summary without one (a create response) must not
   * overwrite a summary projected from a newer event.
   */
  threadSeqs: Record<ThreadId, number>;
  /** Queue rows per thread, fetched separately from the summary. */
  queues: Record<ThreadId, ThreadQueue>;
  logs: Record<ThreadId, ThreadLog>;
  threadErrors: Record<ThreadId, ProtoError>;
  loadingThreads: boolean;
  threadsError: ProtoError | null;
  lastSyncAt: number | null;
  serverVersion: string | null;
  /** Daemon host id of the machine this client is talking to. */
  localHostId: HostId | null;
  /**
   * The daemon machine's IANA time zone, or `null` when an older daemon did
   * not report one. Schedule editors default to it.
   */
  daemonTimezone: string | null;
  /**
   * The connect machine the connected daemon owns, from its `hello`.
   *
   * A direct socket reaches exactly this daemon, so this is the machine the
   * notifier must keep online. It is never this UI's own device row.
   */
  daemonMachineId: string | null;
  pendingCreates: PendingCreate[];
  /** What is in flight on a thread, as a kind (`shell:threadActions.*`). */
  pendingActions: Record<ThreadId, ThreadActionKind | undefined>;
  toasts: Toast[];
  ui: UiState;
};

const initialTheme = (): Theme => {
  const config = readBootConfig();
  if (config.theme) return config.theme;
  try {
    const stored = window.localStorage.getItem("h4rn.theme");
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    // localStorage may be unavailable in a locked-down webview.
  }
  return "system";
};

function createInitialState(): AppState {
  const route = readRoute();
  return {
    connection: {
      status: "connecting",
      attempt: 0,
      transport: "direct",
      lastConnectedAt: null,
      lastEventAt: null,
      latencyMs: null,
      error: null,
    },
    hosts: {},
    threads: {},
    threadSeqs: {},
    queues: {},
    logs: {},
    threadErrors: {},
    loadingThreads: true,
    threadsError: null,
    lastSyncAt: null,
    serverVersion: null,
    localHostId: null,
    daemonTimezone: null,
    daemonMachineId: null,
    pendingCreates: [],
    pendingActions: {},
    toasts: [],
    ui: {
      route,
      tab: "working",
      query: "",
      selectedThreadId: route.kind === "thread" ? route.threadId : null,
      paletteOpen: false,
      shortcutsOpen: false,
      newThreadOpen: false,
      linkPrOpen: false,
      theme: initialTheme(),
      snoozedOpen: false,
      filter: { host: null, harness: null, needsAttention: false, unread: false },
      filterOpen: false,
      moreOpen: false,
      limitsOpen: false,
      recordingShortcut: false,
      newThreadPreset: null,
      addProjectOpen: false,
      queuedEdit: null,
      focusSearchNonce: 0,
      focusComposerNonce: 0,
      focusFilesNonce: 0,
      pendingIdePath: null,
      ideActivePath: null,
      preview: null,
      previewExpanded: false,
      composerRestore: null,
      pendingRouteFocus: true,
      layout: {
        ...readLayout(),
        view: route.kind === "ide" ? "ide" : "thread",
        // The /ide deep link is an editor first: open its file tree.
        ...(route.kind === "ide" ? { panelOpen: true, panelTab: "files" as PanelTab } : {}),
      },
    },
  };
}

type Store = {
  getState: () => AppState;
  setState: (next: AppState) => void;
  subscribe: (listener: () => void) => () => void;
};

function createStore(initial: AppState): Store {
  let state = initial;
  const listeners = new Set<() => void>();
  return {
    getState: () => state,
    setState: (next) => {
      state = next;
      for (const listener of listeners) listener();
    },
    subscribe: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

export const store = createStore(createInitialState());

export function useApp<T>(selector: (state: AppState) => T): T {
  return useSyncExternalStore(
    store.subscribe,
    () => selector(store.getState()),
    () => selector(store.getState()),
  );
}

export function getState(): AppState {
  return store.getState();
}

function setState(next: AppState): void {
  store.setState(next);
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

export function resolvedTheme(theme: Theme): "light" | "dark" {
  if (theme !== "system") return theme;
  if (typeof window === "undefined") return "light";
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function applyTheme(theme: Theme): void {
  if (typeof document === "undefined") return;
  const resolved = resolvedTheme(theme);
  document.documentElement.classList.toggle("dark", resolved === "dark");
  document.documentElement.classList.toggle("light", resolved === "light");
  document.documentElement.dataset.theme = resolved;
}

export function setTheme(theme: Theme): void {
  try {
    window.localStorage.setItem("h4rn.theme", theme);
  } catch {
    // Non-fatal: the theme still applies for this session.
  }
  applyTheme(theme);
  setState({ ...getState(), ui: { ...getState().ui, theme } });
}

export function toggleTheme(): void {
  setTheme(resolvedTheme(getState().ui.theme) === "dark" ? "light" : "dark");
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

let toastId = 0;

export function pushToast(input: {
  title: string;
  description?: string;
  variant?: ToastVariant;
  ttlMs?: number;
}): void {
  const toast: Toast = {
    id: ++toastId,
    title: input.title,
    variant: input.variant ?? "info",
    ...(input.description ? { description: input.description } : {}),
  };
  const state = getState();
  setState({ ...state, toasts: [...state.toasts.slice(-2), toast] });
  const ttl = input.ttlMs ?? 4_500;
  setTimeout(() => dismissToast(toast.id), ttl);
}

export function dismissToast(id: number): void {
  const state = getState();
  setState({ ...state, toasts: state.toasts.filter((toast) => toast.id !== id) });
}

// ---------------------------------------------------------------------------
// UI actions
// ---------------------------------------------------------------------------

export function navigate(route: Route): void {
  pushRoute(route);
  const state = getState();
  // Thread links keep whichever view is open; every other destination is a
  // page, so it only makes sense on the Thread side of the shell.
  const view: UiView =
    route.kind === "ide" ? "ide" : route.kind === "thread" ? state.ui.layout.view : "thread";
  const layout: LayoutState = { ...state.ui.layout, view };
  if (layout.view !== state.ui.layout.view) writeLayout(layout);
  const threadId = route.kind === "thread" ? route.threadId : (state.ui.selectedThreadId ?? null);
  const keepPreview =
    (route.kind === "thread" || route.kind === "ide") &&
    state.ui.preview !== null &&
    state.ui.preview.threadId === threadId;
  setState({
    ...state,
    ui: {
      ...state.ui,
      route,
      selectedThreadId: route.kind === "thread" ? route.threadId : state.ui.selectedThreadId,
      preview: keepPreview ? state.ui.preview : null,
      previewExpanded: keepPreview ? state.ui.previewExpanded : false,
      pendingRouteFocus: true,
      paletteOpen: false,
      layout,
    },
  });
  if (route.kind === "thread") void ensureThread(route.threadId);
}

export function openThread(threadId: ThreadId): void {
  navigate({ kind: "thread", threadId });
}

export function setTab(tab: ThreadTab): void {
  const state = getState();
  setState({ ...state, ui: { ...state.ui, tab, filterOpen: false } });
}

export function setQuery(query: string): void {
  const state = getState();
  setState({ ...state, ui: { ...state.ui, query } });
}

export function setSelectedThread(threadId: ThreadId | null): void {
  const state = getState();
  const keepPreview = state.ui.preview !== null && state.ui.preview.threadId === threadId;
  setState({
    ...state,
    ui: {
      ...state.ui,
      selectedThreadId: threadId,
      preview: keepPreview ? state.ui.preview : null,
      previewExpanded: keepPreview ? state.ui.previewExpanded : false,
    },
  });
}

export function setUi(patch: Partial<UiState>): void {
  const state = getState();
  setState({ ...state, ui: { ...state.ui, ...patch } });
}

export function togglePalette(): void {
  const state = getState();
  setState({ ...state, ui: { ...state.ui, paletteOpen: !state.ui.paletteOpen } });
}

export function closePalette(): void {
  setUi({ paletteOpen: false });
}

export function openShortcuts(): void {
  setUi({ shortcutsOpen: true, paletteOpen: false });
}

export function closeShortcuts(): void {
  setUi({ shortcutsOpen: false });
}

/** Opens New thread, optionally starting in one project's folder. */
export function openNewThread(preset?: { host: string; workspace: string }): void {
  setUi({ newThreadOpen: true, paletteOpen: false, newThreadPreset: preset ?? null });
}

/** Opens the "Link pull request" dialog for the focused thread. */
export function openLinkPullRequest(): void {
  setUi({ linkPrOpen: true, paletteOpen: false });
}

export function closeLinkPullRequest(): void {
  setUi({ linkPrOpen: false });
}

export function openAddProject(): void {
  setUi({ addProjectOpen: true, paletteOpen: false });
}

export function closeAddProject(): void {
  setUi({ addProjectOpen: false });
}

export function closeNewThread(): void {
  setUi({ newThreadOpen: false });
}

export function requestSearchFocus(): void {
  setUi({ focusSearchNonce: getState().ui.focusSearchNonce + 1 });
}

export function requestComposerFocus(): void {
  setUi({ focusComposerNonce: getState().ui.focusComposerNonce + 1 });
}

export function setFilter(patch: Partial<ThreadFilterState>): void {
  const state = getState();
  setState({ ...state, ui: { ...state.ui, filter: { ...state.ui.filter, ...patch } } });
}

export function clearFilter(): void {
  setFilter({ host: null, harness: null, needsAttention: false, unread: false });
}

// ---------------------------------------------------------------------------
// Layout: one sidebar, one right panel, and the Thread/IDE view
// ---------------------------------------------------------------------------

/** DESIGN §Layout 6: main keeps at least this much room; the right panel yields first. */
export const MAIN_MIN_WIDTH = 560;

/** Whether the right panel fits beside the sidebar and a 560 px main. */
export function panelFits(layout: LayoutState, viewportWidth: number): boolean {
  const sidebar = layout.sidebarOpen ? layout.sidebarWidth : 0;
  return viewportWidth - sidebar - layout.panelWidth >= MAIN_MIN_WIDTH;
}

/**
 * A person asked for the panel. When it would leave main under the floor,
 * the sidebar closes to make room instead of the request doing nothing.
 * Automatic resizes never come through here: they hide the panel first.
 */
function claimPanelRoom(layout: LayoutState): LayoutState {
  if (!layout.panelOpen || !layout.sidebarOpen || typeof window === "undefined") return layout;
  if (panelFits(layout, window.innerWidth)) return layout;
  return { ...layout, sidebarOpen: false };
}

export function setLayout(patch: Partial<LayoutState>): void {
  const state = getState();
  const merged = { ...state.ui.layout, ...patch };
  const layout = patch.panelOpen === true ? claimPanelRoom(merged) : merged;
  writeLayout(layout);
  setState({ ...state, ui: { ...state.ui, layout } });
}

export function setView(view: UiView): void {
  const state = getState();
  if (state.ui.layout.view === view && (view !== "thread" || state.ui.route.kind !== "ide")) {
    return;
  }
  const route =
    view === "thread" && state.ui.route.kind === "ide"
      ? { kind: "inbox" as const }
      : state.ui.route;
  if (route !== state.ui.route) pushRoute(route);
  const revealed = view === "ide" && !state.ui.layout.panelOpen;
  const base: LayoutState = {
    ...state.ui.layout,
    view,
    // Entering the IDE with the panel closed reveals the file tree; the panel
    // is part of the editor rather than an overlay.
    ...(revealed ? { panelOpen: true, panelTab: "files" as PanelTab } : {}),
  };
  const layout = revealed ? claimPanelRoom(base) : base;
  writeLayout(layout);
  setState({ ...state, ui: { ...state.ui, route, layout } });
}

export function toggleView(): void {
  setView(getState().ui.layout.view === "ide" ? "thread" : "ide");
}

export function toggleSidebar(): void {
  setLayout({ sidebarOpen: !getState().ui.layout.sidebarOpen });
}

export function setSidebarWidth(width: number): void {
  setLayout({ sidebarWidth: Math.max(240, Math.min(420, Math.round(width))) });
}

export function togglePanel(): void {
  setLayout({ panelOpen: !getState().ui.layout.panelOpen });
}

export function setPanelTab(panelTab: PanelTab): void {
  setLayout({ panelOpen: true, panelTab });
}

/** The custom event a phone listens for to open the preview sheet. */
export const PREVIEW_EVENT = "h4rn:preview";

/**
 * Opens one HTML file in the Preview tab.
 *
 * The Files action and an `.html` link in an agent message both come here. A
 * phone hears the event and opens its panel sheet instead.
 */
export function openPreview(path: string, threadId?: ThreadId): void {
  const state = getState();
  // A link in a PR body or another page is not about the selected thread.
  if (!threadId && state.ui.route.kind !== "thread") return;
  const target =
    threadId ??
    (state.ui.route.kind === "thread" ? state.ui.route.threadId : null) ??
    state.ui.selectedThreadId;
  if (!target) return;
  const layout = claimPanelRoom({ ...state.ui.layout, panelOpen: true, panelTab: "preview" });
  writeLayout(layout);
  setState({
    ...state,
    ui: {
      ...state.ui,
      preview: { threadId: target, path },
      previewExpanded: false,
      layout,
    },
  });
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent(PREVIEW_EVENT, { detail: { threadId: target, path } }));
  }
}

/** Closes the preview and returns the panel to Files. */
export function closePreview(): void {
  const state = getState();
  const layout: LayoutState =
    state.ui.layout.panelTab === "preview"
      ? { ...state.ui.layout, panelTab: "files" }
      : state.ui.layout;
  writeLayout(layout);
  setState({
    ...state,
    ui: { ...state.ui, preview: null, previewExpanded: false, layout },
  });
}

export function expandPreview(): void {
  const state = getState();
  if (!state.ui.preview) return;
  // The main area is the full-width view; the panel would only repeat it.
  const layout: LayoutState = { ...state.ui.layout, panelOpen: false };
  writeLayout(layout);
  setState({
    ...state,
    ui: { ...state.ui, previewExpanded: true, layout },
  });
}

export function collapsePreview(): void {
  const state = getState();
  if (!state.ui.previewExpanded) return;
  const layout = claimPanelRoom({
    ...state.ui.layout,
    panelOpen: true,
    panelTab: "preview",
  });
  writeLayout(layout);
  setState({
    ...state,
    ui: { ...state.ui, previewExpanded: false, layout },
  });
}

export function setPanelWidth(width: number): void {
  setLayout({ panelWidth: Math.max(320, Math.min(640, Math.round(width))) });
}

export function toggleProject(key: string): void {
  const closed = { ...getState().ui.layout.closedProjects };
  if (closed[key]) delete closed[key];
  else closed[key] = true;
  setLayout({ closedProjects: closed });
}

export function setLibrarySelected(librarySelected: string): void {
  setLayout({ librarySelected });
}

export function toggleTerminal(): void {
  setLayout({ terminalOpen: !getState().ui.layout.terminalOpen });
}

export function requestFilesFocus(): void {
  const state = getState();
  setState({
    ...state,
    ui: {
      ...state.ui,
      layout: claimPanelRoom({ ...state.ui.layout, panelOpen: true, panelTab: "files" }),
      focusFilesNonce: state.ui.focusFilesNonce + 1,
    },
  });
  writeLayout(getState().ui.layout);
}

export function openInIde(path: string): void {
  const state = getState();
  const opening = !state.ui.layout.panelOpen;
  const base: LayoutState = {
    ...state.ui.layout,
    view: "ide",
    ...(opening ? { panelOpen: true, panelTab: "files" as PanelTab } : {}),
  };
  const layout = opening ? claimPanelRoom(base) : base;
  writeLayout(layout);
  setState({ ...state, ui: { ...state.ui, layout, pendingIdePath: path } });
}

export function setIdeActivePath(path: string | null): void {
  const state = getState();
  if (state.ui.ideActivePath === path) return;
  setState({ ...state, ui: { ...state.ui, ideActivePath: path } });
}

export function takePendingIdePath(): string | null {
  const state = getState();
  const path = state.ui.pendingIdePath;
  if (path !== null) setState({ ...state, ui: { ...state.ui, pendingIdePath: null } });
  return path;
}

// ---------------------------------------------------------------------------
// Connection bootstrap
// ---------------------------------------------------------------------------

let client: H4rnClient | null = null;
let started = false;
let windowHooked = false;
const clientListeners = new Set<(client: H4rnClient) => void>();

/**
 * Notifies whenever the transport client is created or replaced.
 *
 * A sign-in, pairing or host switch rebuilds the whole client, so anything
 * bound to the previous connection (such as the workspace watch) has to move
 * to the new one. Listeners run before the new client starts.
 */
export function onClientChange(listener: (client: H4rnClient) => void): () => void {
  clientListeners.add(listener);
  return () => clientListeners.delete(listener);
}

export function getClient(): H4rnClient {
  if (!client) {
    client = createClientFromEnv();
    for (const listener of [...clientListeners]) listener(client);
  }
  return client;
}

/**
 * Replaces the client and reconnects.
 *
 * Signing in, pairing and switching hosts all change the transport (plain WS,
 * Noise direct, or the relay), so the whole client is rebuilt rather than
 * trying to swap a live transport underneath an established session.
 */
export function restartConnection(): void {
  const previous = client;
  client = null;
  started = false;
  try {
    previous?.stop();
  } catch {
    // A client that never started has nothing to stop.
  }
  startConnection();
}

const eventQueue: EventEnvelope[] = [];
const appEventListeners = new Set<(event: EventEnvelope) => void>();
let flushScheduled = false;

/**
 * Subscribes to raw protocol events before projection.
 *
 * Feature stores use this for data that the thread projection summarises away,
 * such as a subagent's live budget counters. Listeners survive a reconnect
 * because `startConnection` keeps its set across client rebuilds.
 */
export function onAppEvent(listener: (event: EventEnvelope) => void): () => void {
  appEventListeners.add(listener);
  return () => appEventListeners.delete(listener);
}

function scheduleFlush(): void {
  if (flushScheduled) return;
  flushScheduled = true;
  const run = () => {
    flushScheduled = false;
    flushEvents();
  };
  if (typeof requestAnimationFrame === "function") {
    requestAnimationFrame(run);
  } else {
    setTimeout(run, 16);
  }
}

/** Exposed for tests and for the pause/resume path. */
export function flushEvents(): void {
  if (eventQueue.length === 0) return;
  const batch = eventQueue.splice(0, eventQueue.length);
  const before = getState();
  let next = before;
  for (const envelope of batch) {
    next = applyEvent(next, envelope);
  }
  setState(next);
  if (routePath(before.ui.route) !== routePath(next.ui.route)) replaceRoute(next.ui.route);
  // The summary only counts the queue; it never carries the rows, because a
  // full queue can be larger than a paired transport's frame.
  for (const envelope of batch) {
    if (envelope.event.type === "thread_upserted") syncQueueWithSummary(envelope.event);
  }
}

function enqueueEvent(envelope: EventEnvelope): void {
  eventQueue.push(envelope);
  scheduleFlush();
}

/**
 * Says a rewind fell back to a handoff, once and only for live news.
 *
 * The daemon journals the fallback, so a replay folds it into the history
 * without repeating a toast the person already saw. `replayed` is the one
 * signal that tells the two apart.
 */
export function noticeRewindFallback(envelope: EventEnvelope): void {
  if (envelope.replayed || envelope.event.type !== "rewind_fell_back") return;
  const harness = getState().threads[envelope.event.thread_id]?.harness;
  pushToast({
    variant: "info",
    title: i18n.t("shell:rewind.fellBack"),
    description: harness
      ? i18n.t("shell:rewind.fellBackNamed", { harness: harnessName(harness) })
      : i18n.t("shell:rewind.fellBackUnnamed"),
  });
}

export function startConnection(): void {
  if (started) return;
  started = true;
  const app = getClient();

  app.onStatus((connection) => {
    setState({ ...getState(), connection });
  });

  app.onEvent((envelope) => {
    enqueueEvent(envelope);
    // A planned native rewind can still be refused when the next session
    // starts. The daemon journals that moment; say it once, live, and let a
    // replay fold the event without repeating the news.
    noticeRewindFallback(envelope);
    for (const listener of appEventListeners) listener(envelope);
  });

  app.onReady((info) => {
    const hosts = Object.fromEntries(info.hosts.map((host) => [host.host_id, host]));
    const state = getState();
    setState({
      ...state,
      hosts,
      serverVersion: info.serverVersion,
      localHostId: info.localHostId,
      daemonTimezone: info.daemonTimezone || null,
      daemonMachineId: info.daemonMachineId,
      connection: { ...state.connection, error: null },
    });
    // Queue rows are keyed by the daemon's revision counter, which starts over
    // with a new daemon: a reconnect refetches them from scratch.
    setState({ ...getState(), queues: {} });
    void loadThreads().then(() => {
      const current = getState();
      if (info.reconnected) {
        for (const [threadId, log] of Object.entries(current.logs)) {
          if (log.loaded) void loadThread(threadId, log.nextSeq);
        }
      }
      if (current.ui.route.kind === "thread") {
        const threadId = current.ui.route.threadId;
        const log = current.logs[threadId];
        if (!log?.loaded) void loadThread(threadId, log?.nextSeq ?? 0);
      }
    });
  });

  if (typeof window !== "undefined" && !windowHooked) {
    windowHooked = true;
    window.addEventListener("popstate", () => {
      const route = readRoute();
      const state = getState();
      const view: UiView =
        route.kind === "ide" ? "ide" : route.kind === "thread" ? state.ui.layout.view : "thread";
      setState({
        ...state,
        ui: {
          ...state.ui,
          route,
          selectedThreadId: route.kind === "thread" ? route.threadId : state.ui.selectedThreadId,
          pendingRouteFocus: true,
          layout: { ...state.ui.layout, view },
        },
      });
      if (route.kind === "thread") void ensureThread(route.threadId);
    });
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    media.addEventListener("change", () => {
      if (getState().ui.theme === "system") applyTheme("system");
    });
    // A warm direct-pairing deep link stored its URI in the secret store; the
    // transport is chosen at client construction, so rebuild it.
    window.addEventListener("h4rn-direct-pair", () => {
      restartConnection();
    });
  }

  applyTheme(getState().ui.theme);
  applyTerminalTheme();
  app.start();
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

export async function loadThreads(): Promise<void> {
  const state = getState();
  if (!state.loadingThreads) setState({ ...state, loadingThreads: true });
  try {
    const response = await getClient().request({
      type: "list_threads",
      filter: {
        tabs: [],
        host: null,
        harness: null,
        search: null,
        include_snoozed: true,
        include_children: false,
      },
    });
    if (response.type !== "threads") return;
    const current = getState();
    const threads: Record<ThreadId, ThreadSummary> = {
      ...response.threads.reduce<Record<ThreadId, ThreadSummary>>((acc, thread) => {
        acc[thread.thread_id] = thread;
        return acc;
      }, {}),
    };
    for (const pending of current.pendingCreates) {
      const optimistic = current.threads[pending.tempId];
      if (optimistic) threads[pending.tempId] = optimistic;
    }
    setState({
      ...current,
      threads,
      loadingThreads: false,
      threadsError: null,
      lastSyncAt: Date.now(),
    });
    // A reload clears the cached rows (the daemon's revision counter starts
    // over), so every thread that still has queued messages fetches them now.
    // Waiting for an unrelated event would hide the queue for as long as a
    // rate-limit pause lasts.
    for (const summary of Object.values(threads)) {
      if (summary.queued_count > 0) void loadThreadQueue(summary.thread_id);
    }
  } catch (error) {
    const current = getState();
    setState({
      ...current,
      loadingThreads: false,
      threadsError: toProtoError(error),
    });
  }
}

export async function loadThread(threadId: ThreadId, sinceSeq = 0): Promise<void> {
  const state = getState();
  const existing = state.logs[threadId] ?? createThreadLog();
  if (existing.loading) return;
  setState({
    ...state,
    logs: { ...state.logs, [threadId]: { ...existing, loading: true } },
    threadErrors: omitKey(state.threadErrors, threadId),
  });

  try {
    let log = getState().logs[threadId] ?? createThreadLog();
    let pages = 0;
    // A paired session receives a long replay in pages that each fit one
    // transport frame. Keep asking from the last sequence until the daemon
    // says there is nothing more; an older daemon answers everything at once.
    for (;;) {
      const page = await getClient().request({
        type: "get_thread",
        thread_id: threadId,
        since_seq: pages === 0 ? sinceSeq : log.nextSeq,
      });
      for (const { seq, event } of page.events) {
        // The global replay may already have applied an event while this
        // request was in flight; sequence numbers make the load idempotent.
        if (seq <= log.nextSeq) continue;
        log = foldEvent(log, event);
        log = { ...log, nextSeq: seq };
      }
      log = { ...log, nextSeq: Math.max(log.nextSeq, page.next_seq) };
      pages += 1;
      if (!page.more || pages > 10_000) {
        log = { ...log, loading: false, loaded: true };
        const current = getState();
        setState({
          ...current,
          threads: { ...current.threads, [threadId]: page.summary },
          threadSeqs: {
            ...current.threadSeqs,
            [threadId]: Math.max(current.threadSeqs[threadId] ?? 0, log.nextSeq),
          },
          logs: { ...current.logs, [threadId]: log },
          threadErrors: omitKey(current.threadErrors, threadId),
        });
        // The summary only counts the queue; a thread opened while messages
        // wait needs its rows even before another event arrives.
        if (page.summary.queued_count > 0) void loadThreadQueue(threadId);
        return;
      }
    }
  } catch (error) {
    const current = getState();
    const proto = toProtoError(error);
    setState({
      ...current,
      logs: {
        ...current.logs,
        [threadId]: { ...(current.logs[threadId] ?? existing), loading: false },
      },
      threadErrors: { ...current.threadErrors, [threadId]: proto },
    });
  }
}

export function ensureThread(threadId: ThreadId): void {
  const state = getState();
  const log = state.logs[threadId];
  if (log?.loaded || log?.loading) return;
  // ThreadPane mounts before the first hello; onReady loads the active thread.
  if (state.connection.status !== "online") return;
  if (state.threadErrors[threadId]) return;
  void loadThread(threadId, log?.nextSeq ?? 0);
}

export function retryThread(threadId: ThreadId): void {
  const state = getState();
  setState({ ...state, threadErrors: omitKey(state.threadErrors, threadId) });
  void loadThread(threadId, 0);
}

// ---------------------------------------------------------------------------
// Live usage frames
//
// Usage ticks and schedules arrive outside the journal. Feature stores own
// those streams, but the thread list still needs a schedule to show "Resumes
// 14:32" without a full reload.
// ---------------------------------------------------------------------------

export function patchThreadSummary(threadId: ThreadId, patch: Partial<ThreadSummary>): void {
  const state = getState();
  const summary = state.threads[threadId];
  if (!summary) return;
  setState({ ...state, threads: { ...state.threads, [threadId]: { ...summary, ...patch } } });
}

export function applyResumeSchedule(schedule: ResumeSchedule | null): void {
  if (!schedule) {
    // A cleared schedule names no thread, so the cheap path is a fresh list.
    void loadThreads();
    return;
  }
  const summary = getState().threads[schedule.thread_id];
  if (!summary) return;
  patchThreadSummary(schedule.thread_id, {
    resume_at: schedule.resume_at,
    limit: summary.limit
      ? { ...summary.limit, reset_at: schedule.resume_at }
      : {
          harness: null,
          reset_at: schedule.resume_at,
          message: schedule.reason,
        },
  });
}

export function applyLimitObservation(status: LimitStatus): void {
  if (!status.thread_id) return;
  patchThreadSummary(status.thread_id, {
    agent_state: "rate_limited",
    limit: {
      harness: status.harness,
      reset_at: status.reset_at,
      message: status.message,
    },
  });
}

// ---------------------------------------------------------------------------
// Event projection
// ---------------------------------------------------------------------------

function omitKey<T>(record: Record<string, T>, key: string): Record<string, T> {
  if (!(key in record)) return record;
  const next = { ...record };
  delete next[key];
  return next;
}

/** Folds one server event into a state; exposed for tests. */
export function applyEvent(state: AppState, envelope: EventEnvelope): AppState {
  const { event } = envelope;

  if (event.type === "thread_upserted") {
    const { type: _type, ...summary } = event;
    let threads = { ...state.threads, [summary.thread_id]: summary };
    const threadSeqs = { ...state.threadSeqs, [summary.thread_id]: envelope.seq };
    let logs = state.logs;
    let pendingCreates = state.pendingCreates;
    let ui = state.ui;

    if (!state.threads[summary.thread_id]) {
      const match = pendingCreates.find((pending) =>
        summary.title.toLowerCase().startsWith(pending.prompt.slice(0, 24).toLowerCase()),
      );
      const consumed = match ?? pendingCreates[0];
      if (consumed) {
        threads = { ...threads };
        logs = { ...logs };
        const optimisticLog = logs[consumed.tempId];
        delete threads[consumed.tempId];
        delete logs[consumed.tempId];
        if (optimisticLog) logs[summary.thread_id] = optimisticLog;
        pendingCreates = pendingCreates.filter((pending) => pending.tempId !== consumed.tempId);
        // Text typed while the thread was still optimistic belongs to the
        // real id now; the temp id is never written to storage.
        moveLocalThreadDraft(summary.host, consumed.tempId, summary.thread_id);
        if (ui.route.kind === "thread" && ui.route.threadId === consumed.tempId) {
          ui = {
            ...ui,
            route: { kind: "thread", threadId: summary.thread_id },
            selectedThreadId: summary.thread_id,
          };
        }
      }
    }

    return {
      ...state,
      threads,
      threadSeqs,
      logs,
      pendingCreates,
      ui,
    };
  }

  // Every thread-scoped event names its thread; there is no "last event"
  // ownership inference left to go wrong on a busy daemon.
  const threadId = "thread_id" in event ? event.thread_id : null;
  if (!threadId) {
    return state;
  }

  const currentLog = state.logs[threadId] ?? createThreadLog();
  // Do not materialise a partial transcript for a thread that was never
  // loaded: `ensureThread` asks for its full history and the sequence numbers
  // keep that load idempotent.
  const folded =
    currentLog.loaded || currentLog.loading
      ? {
          ...foldEvent(currentLog, event),
          nextSeq: Math.max(currentLog.nextSeq, envelope.seq),
        }
      : null;

  const summary = state.threads[threadId];
  let nextSummary = summary;

  if (summary) {
    nextSummary = { ...summary };
    if (event.type === "agent_state_changed") {
      nextSummary.agent_state = event.state;
      if (event.state === "awaiting_approval") nextSummary.needs_attention = true;
    }
    if (event.type === "turn_started") {
      nextSummary.tab = "working";
      nextSummary.unread = false;
      nextSummary.last_activity = event.started_at;
    }
    if (event.type === "approval_requested") {
      nextSummary.agent_state = "awaiting_approval";
      nextSummary.needs_attention = true;
    }
    if (event.type === "usage_limit_hit") {
      nextSummary.agent_state = "rate_limited";
      nextSummary.needs_attention = !event.auto_resume;
    }
    if (event.type === "error") {
      nextSummary.needs_attention = true;
    }
    if (event.type === "turn_completed") {
      nextSummary.last_activity = new Date().toISOString();
    }
    if (event.type === "guest_requested") {
      nextSummary.pending_guest_requests = (nextSummary.pending_guest_requests ?? 0) + 1;
      nextSummary.needs_attention = true;
    }
    if (event.type === "guest_request_resolved") {
      nextSummary.pending_guest_requests = Math.max(
        0,
        (nextSummary.pending_guest_requests ?? 0) - 1,
      );
    }
  }

  return {
    ...state,
    threads: nextSummary ? { ...state.threads, [threadId]: nextSummary } : state.threads,
    logs: folded ? { ...state.logs, [threadId]: folded } : state.logs,
  };
}

// ---------------------------------------------------------------------------
// Thread actions
// ---------------------------------------------------------------------------

/** A thread action; its failure toast is `shell:threadActions.<kind>`. */
export type ThreadActionKind =
  | "moveToSettled"
  | "moveToDone"
  | "moveToWorking"
  | "acknowledge"
  | "settle"
  | "pin"
  | "unpin"
  | "snooze"
  | "wake"
  | "interrupt"
  | "mode"
  | "model"
  | "effort"
  | "fastOn"
  | "fastOff"
  | "rename"
  | "switchHarness"
  | "rewind";

/** The host a thread runs on, or the words for "the host" when it is gone. */
function threadHost(threadId: ThreadId): string {
  return getState().threads[threadId]?.host ?? i18n.t("shell:threadActions.theHost");
}

async function runThreadAction(
  threadId: ThreadId,
  label: ThreadActionKind,
  body: Extract<
    ClientRequest,
    {
      type:
        | "set_thread_tab"
        | "set_thread_runtime_mode"
        | "set_thread_model"
        | "set_thread_reasoning_effort"
        | "set_thread_fast"
        | "snooze"
        | "interrupt"
        | "rename_thread";
    }
  >,
): Promise<boolean> {
  const placement = body.type === "set_thread_tab" && isPlacementMove(body.tab);
  const state = getState();
  setState({
    ...state,
    pendingActions: { ...state.pendingActions, [threadId]: label },
  });
  try {
    await getClient().request(body);
    // Moving a thread into Done or Settled is the person's placement, not the
    // daemon finishing a turn: tell the notifier so it cannot pin a remembered
    // live completion on the Done update that follows. A pin toggle only moves
    // through Working and is not a placement. The note is written only after
    // the daemon accepted the move; a failed move changed nothing to explain.
    if (placement) noteManualPlacement(threadId);
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      variant: "error",
      title: i18n.t(`shell:threadActions.${label}`, { host: threadHost(threadId) }),
      description: proto.hint ?? proto.message,
    });
    return false;
  } finally {
    const current = getState();
    setState({
      ...current,
      pendingActions: { ...current.pendingActions, [threadId]: undefined },
    });
  }
}

export async function setThreadTab(
  threadId: ThreadId,
  tab: ThreadTab,
  pinned = false,
): Promise<void> {
  const label =
    tab === "settled" ? "moveToSettled" : tab === "done" ? "moveToDone" : "moveToWorking";
  await runThreadAction(threadId, label, {
    type: "set_thread_tab",
    thread_id: threadId,
    tab,
    pinned,
  });
}

export async function acknowledgeThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "acknowledge", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "settled",
    pinned: false,
  });
}

export async function settleThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "settle", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "settled",
    pinned: false,
  });
}

export async function togglePin(threadId: ThreadId): Promise<void> {
  const summary = getState().threads[threadId];
  if (!summary) return;
  await runThreadAction(threadId, summary.pinned ? "unpin" : "pin", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "working",
    pinned: !summary.pinned,
  });
}

export async function snoozeThread(threadId: ThreadId, until: string | null): Promise<void> {
  await runThreadAction(threadId, until ? "snooze" : "wake", {
    type: "snooze",
    thread_id: threadId,
    until,
  });
}

export async function interruptThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "interrupt", { type: "interrupt", thread_id: threadId });
}

/** Changes a thread's permission mode; the daemon applies it in place or next turn. */
export async function setThreadRuntimeMode(
  threadId: ThreadId,
  mode: RuntimeMode,
): Promise<boolean> {
  return runThreadAction(threadId, "mode", {
    type: "set_thread_runtime_mode",
    thread_id: threadId,
    mode,
  });
}

/** Changes a thread's model; the conversation is kept. */
export async function setThreadModel(threadId: ThreadId, model: string): Promise<boolean> {
  return runThreadAction(threadId, "model", {
    type: "set_thread_model",
    thread_id: threadId,
    model,
  });
}

/** Changes a thread's reasoning effort; `null` returns to the harness default. */
export async function setThreadReasoningEffort(
  threadId: ThreadId,
  effort: string | null,
): Promise<boolean> {
  return runThreadAction(threadId, "effort", {
    type: "set_thread_reasoning_effort",
    thread_id: threadId,
    effort,
  });
}

/** Turns a thread's fast tier on or off. */
export async function setThreadFast(threadId: ThreadId, fast: boolean): Promise<boolean> {
  return runThreadAction(threadId, fast ? "fastOn" : "fastOff", {
    type: "set_thread_fast",
    thread_id: threadId,
    fast,
  });
}

/** What a harness switch did, including the settings the new harness dropped. */
export type SwitchHarnessResult =
  | { ok: true; dropped: DroppedSetting[] }
  | { ok: false; message: string; hint: string | null };

/**
 * Moves a thread to another harness between turns.
 *
 * A successful answer is acked through the event stream like every other
 * thread change; this returns the dropped settings so the caller can say which
 * ones the new harness could not keep. A refusal is a toast plus a returned
 * error, so the control that made the request can also show it in place.
 */
export async function switchThreadHarness(
  threadId: ThreadId,
  harness: HarnessKind,
  model: string,
  agentId: string | null = null,
): Promise<SwitchHarnessResult> {
  const state = getState();
  setState({
    ...state,
    pendingActions: { ...state.pendingActions, [threadId]: "switchHarness" },
  });
  try {
    const response = await getClient().request({
      type: "switch_thread_harness",
      thread_id: threadId,
      harness,
      agent_id: agentId,
      model,
    });
    return {
      ok: true,
      dropped: response.type === "thread_switched" ? response.dropped : [],
    };
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      variant: "error",
      title: i18n.t("shell:threadActions.switchHarness", { host: threadHost(threadId) }),
      description: proto.hint ?? proto.message,
    });
    return { ok: false, message: proto.message, hint: proto.hint ?? null };
  } finally {
    const current = getState();
    setState({
      ...current,
      pendingActions: { ...current.pendingActions, [threadId]: undefined },
    });
  }
}

/** Renames a thread by hand. The daemon never replaces a name you typed. */
export async function renameThread(threadId: ThreadId, title: string): Promise<boolean> {
  const trimmed = title.trim();
  if (!trimmed) return false;
  return runThreadAction(threadId, "rename", {
    type: "rename_thread",
    thread_id: threadId,
    title: trimmed,
  });
}

/** The first approval of a thread that still needs an answer, if any. */
export function pendingApprovalIn(log: ThreadLog | undefined): string | null {
  if (!log) return null;
  for (const turn of log.turns) {
    for (const item of turn.items) {
      if (item.kind !== "approval") continue;
      const status = log.approvals[item.approvalId]?.status ?? "pending";
      if (status === "pending" || status === "error") return item.approvalId;
    }
  }
  return null;
}

export async function respondApproval(
  threadId: ThreadId,
  approvalId: string,
  decision: ApprovalDecision,
  optionId?: string | null,
): Promise<void> {
  const state = getState();
  const log = state.logs[threadId];
  if (!log) return;
  setState({
    ...state,
    logs: {
      ...state.logs,
      [threadId]: {
        ...log,
        approvals: {
          ...log.approvals,
          [approvalId]: { status: "sending" },
        },
      },
    },
  });

  try {
    // The daemon broadcasts `approval_resolved` once the harness accepted the
    // answer, so every client (including this one) resolves from the stream.
    await getClient().request({
      type: "respond_approval",
      thread_id: threadId,
      approval_id: approvalId,
      decision,
      option_id: optionId ?? null,
    });
  } catch (error) {
    const proto = toProtoError(error);
    const current = getState();
    const currentLog = current.logs[threadId];
    if (currentLog) {
      setState({
        ...current,
        logs: {
          ...current.logs,
          [threadId]: {
            ...currentLog,
            approvals: { ...currentLog.approvals, [approvalId]: { status: "error" } },
          },
        },
      });
    }
    pushToast({
      variant: "error",
      title: i18n.t(
        decision === "approve" ? "shell:approval.approveFailed" : "shell:approval.denyFailed",
        { host: threadHost(threadId) },
      ),
      description: joinSentences([proto.message, i18n.t("shell:approval.stillPending")]),
    });
  }
}

let localCounter = 0;

let composerRestoreToken = 0;

/**
 * What rewinding one prompt would really do.
 *
 * The thread row only says what the harness *can* do. A prompt recorded before
 * its provider boundary was kept, and a conversation with no provider session,
 * can only use a handoff, so the dialog asks the daemon for the selected
 * prompt's plan instead of promising the thread-wide method.
 *
 * Throws when the plan cannot be read; the dialog shows the reason.
 */
export async function planRewind(threadId: ThreadId, beforeItemId: string): Promise<RewindPlan> {
  const response = await getClient().request({
    type: "rewind_plan",
    thread_id: threadId,
    before_item_id: beforeItemId,
  });
  if (response.type !== "rewind_plan") {
    // i18n-ignore-next-line: a reply of the wrong type is a bug, not a state
    throw new Error("The daemon answered a rewind plan with the wrong shape.");
  }
  return response.plan;
}

function rewindFailedTitle(title: string | undefined): string {
  return title ? i18n.t("shell:rewind.failed", { title }) : i18n.t("shell:rewind.failedUntitled");
}

/**
 * Edits an earlier prompt: the daemon removes it and everything after it.
 *
 * On success the prompt and its attachments wait in `ui.composerRestore`, and
 * the composer keeps whatever draft was already there above them.
 */
export async function rewindThread(
  threadId: ThreadId,
  beforeItemId: string,
  restoreFiles: boolean,
): Promise<RewoundPrompt | null> {
  const state = getState();
  setState({
    ...state,
    pendingActions: { ...state.pendingActions, [threadId]: "rewind" },
  });
  try {
    const response = await getClient().request({
      type: "rewind_thread",
      thread_id: threadId,
      before_item_id: beforeItemId,
      restore_files: restoreFiles,
    });
    if (response.type !== "rewound") return null;
    const prompt = response.prompt;
    composerRestoreToken += 1;
    const current = getState();
    setState({
      ...current,
      ui: {
        ...current.ui,
        composerRestore: {
          threadId,
          token: composerRestoreToken,
          prompt: prompt.prompt,
          attachments: prompt.attachments,
        },
        focusComposerNonce: current.ui.focusComposerNonce + 1,
      },
    });
    // Say what the harness really did, not what the thread row advertises: the
    // answer names the method the daemon recorded for this cut.
    const harness = current.threads[threadId]?.harness;
    const leftAlone = leftAloneList(prompt.left_alone);
    pushToast({
      title: i18n.t("shell:rewind.done"),
      description: joinSentences([
        prompt.method === "native" && harness
          ? i18n.t("shell:rewind.native", { harness: harnessName(harness) })
          : i18n.t("shell:rewind.handoff"),
        leftAlone ? i18n.t("shell:rewind.leftAlone", { files: leftAlone }) : null,
      ]),
    });
    return prompt;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      variant: "error",
      title: rewindFailedTitle(getState().threads[threadId]?.title),
      description: proto.hint ?? proto.message,
    });
    return null;
  } finally {
    const current = getState();
    setState({
      ...current,
      pendingActions: { ...current.pendingActions, [threadId]: undefined },
    });
  }
}

/** Marks one restored prompt as adopted by its composer. */
export function consumeComposerRestore(token: number): void {
  const state = getState();
  if (state.ui.composerRestore?.token !== token) return;
  setState({ ...state, ui: { ...state.ui, composerRestore: null } });
}

/** The prompt and everything after it leave the active thread. */
export function rewindRefusal(
  summary: ThreadSummary | undefined,
  log: ThreadLog | undefined,
  queued?: QueuedMessage[] | undefined,
): string | null {
  if (!summary) return i18n.t("thread:rewind.notLoaded");
  if (summary.rewind === "unsupported") {
    return i18n.t("thread:rewind.unsupported", { harness: harnessName(summary.harness) });
  }
  if (summary.agent_state === "awaiting_approval" || pendingApprovalIn(log)) {
    return i18n.t("thread:rewind.approval");
  }
  // A message the daemon is unsure it delivered may already be in the
  // provider conversation, and the rewind would remove the turn that holds it.
  if (queued?.some((message) => message.delivery_unknown)) {
    return i18n.t("thread:rewind.delivery");
  }
  if (summary.agent_state === "queued") {
    return i18n.t("thread:rewind.queued");
  }
  if (
    summary.agent_state === "thinking" ||
    summary.agent_state === "searching" ||
    summary.agent_state === "reading" ||
    summary.agent_state === "editing" ||
    summary.agent_state === "running_tool"
  ) {
    return i18n.t("thread:rewind.running");
  }
  // Only a real armed resume blocks the edit. A cancelled resume can leave the
  // thread's last state saying "rate_limited" with nothing scheduled.
  if (summary.resume_at != null || summary.agent_state === "scheduled") {
    return i18n.t("thread:rewind.resume");
  }
  return null;
}

/**
 * Whether every removed turn has a complete pre-turn file checkpoint.
 *
 * The checkpoint, not the diff list, decides whether "Revert files too" can
 * cover a shell command or another change no tool call named. A turn without
 * one refuses the file choice rather than quietly restoring a partial set.
 */
export function filesRewindAvailable(
  log: ThreadLog | undefined,
  beforeItemId: string | null,
): boolean {
  if (!log || !beforeItemId) return false;
  const index = log.turns.findIndex((turn) =>
    turn.items.some((item) => item.kind === "message" && item.itemId === beforeItemId),
  );
  if (index < 0) return false;
  // The row holding the message is its cue, a synthetic row that never carries
  // a change set (`changes: null`), and its own daemon turn usually started
  // before it. The removed turns are that daemon turn and every real turn
  // after the cue; only they can report a checkpoint.
  const cue = log.turns[index];
  const removed = log.turns.slice(index).filter(isRealTurn);
  const own = cue?.forkTurnId
    ? log.turns.find((turn) => turn.id === cue.forkTurnId && isRealTurn(turn))
    : undefined;
  if (own && !removed.includes(own)) removed.unshift(own);
  return removed.length > 0 && removed.every((turn) => turn.changes?.complete === true);
}

function harnessName(harness: ThreadSummary["harness"]): string {
  switch (harness) {
    case "claude_code":
      return "Claude Code";
    case "codex":
      return "Codex";
    case "opencode":
      return "OpenCode";
    case "antigravity":
      return "Antigravity";
    case "acp":
      return i18n.t("common:thisAgent");
  }
}
export type SendOptions = {
  /** Deliver into the running turn instead of queuing for the next one. */
  steer?: boolean;
};

/**
 * What the daemon could say about a send.
 *
 * `unclear` means the frame was written and the adapter never answered: the
 * turn may hold the text, so the composer keeps the draft instead of clearing
 * it while the person decides.
 */
export type SendResult = "sent" | "unclear" | "failed";

export async function sendMessage(
  threadId: ThreadId,
  text: string,
  attachments: Attachment[] = [],
  options: SendOptions = {},
): Promise<SendResult> {
  const body = text.trim();
  if (!body && attachments.length === 0) return "failed";
  const state = getState();
  const log = state.logs[threadId] ?? createThreadLog();
  const localId = `local-${++localCounter}`;
  const at = new Date().toISOString();
  setState({
    ...state,
    logs: {
      ...state.logs,
      [threadId]: {
        ...log,
        pending: [...log.pending, { id: localId, text: body, attachments, at, sending: true }],
      },
    },
  });

  // A steer names the turn the client saw, so a turn that ended in the
  // meantime is refused instead of catching a newer one.
  const observedTurn = options.steer ? getState().threads[threadId]?.active_turn_id : undefined;
  try {
    const response = await getClient().request({
      type: "send_message",
      thread_id: threadId,
      text: body,
      attachments: attachments.map((attachment) => attachment.id),
      steer: options.steer ?? false,
      ...(observedTurn ? { turn_id: observedTurn } : {}),
    });
    // A queued message has no transcript item yet: the durable queue row from
    // `thread_upserted` replaces the optimistic one, so drop the local row.
    const queuedId = response.type === "message_accepted" ? response.queued_message_id : null;
    if (response.type === "message_accepted" && response.unrecorded) {
      // The agent has the words but the transcript row was not saved. The send
      // succeeded, so the row stays sent and nothing invites a resend.
      const host = getState().threads[threadId]?.host;
      pushToast({
        variant: "info",
        title: i18n.t("composer:toasts.unrecorded.title"),
        description: host
          ? i18n.t("composer:toasts.unrecorded.description", { host })
          : i18n.t("composer:toasts.unrecorded.descriptionAnyHost"),
      });
    }
    const current = getState();
    const currentLog = current.logs[threadId];
    if (currentLog) {
      setState({
        ...current,
        logs: {
          ...current.logs,
          [threadId]: {
            ...currentLog,
            pending: queuedId
              ? currentLog.pending.filter((message) => message.id !== localId)
              : currentLog.pending.map((message) =>
                  message.id === localId ? { ...message, sending: false } : message,
                ),
          },
        },
      });
    }
    return "sent";
  } catch (error) {
    const proto = toProtoError(error);
    if (proto.code === "delivery_unclear") {
      // The daemon wrote the steer before the connection failed, so the turn
      // may already hold it. The composer keeps the draft (it is the person's
      // only other copy until the recovered row appears, and clears it when
      // that row does), and the row stays sent here so the transcript never
      // invites a resend automatically.
      const current = getState();
      const currentLog = current.logs[threadId];
      if (currentLog) {
        setState({
          ...current,
          logs: {
            ...current.logs,
            [threadId]: {
              ...currentLog,
              pending: currentLog.pending.map((message) =>
                message.id === localId ? { ...message, sending: false } : message,
              ),
            },
          },
        });
      }
      pushToast({
        variant: "info",
        title: i18n.t("composer:toasts.unclear.title"),
        description: i18n.t("composer:toasts.unclear.draft", { message: proto.message }),
      });
      return "unclear";
    }
    const current = getState();
    const currentLog = current.logs[threadId];
    if (currentLog) {
      setState({
        ...current,
        logs: {
          ...current.logs,
          [threadId]: {
            ...currentLog,
            pending: currentLog.pending.filter((message) => message.id !== localId),
          },
        },
      });
    }
    const host = current.threads[threadId]?.host;
    pushToast({
      variant: "error",
      title: host
        ? i18n.t("composer:toasts.sendFailed.title", { host })
        : i18n.t("composer:toasts.sendFailed.titleAnyHost"),
      description: i18n.t("composer:toasts.sendFailed.description", { message: proto.message }),
    });
    return "failed";
  }
}

/**
 * Applies one queue change and reflects the daemon's answer immediately.
 *
 * The daemon also broadcasts a new summary; using the response keeps the row
 * from flickering while that event travels. A reply can, however, arrive after
 * a newer summary (or after another client's change): it names the revision it
 * came from, and anything older than what this client already holds is ignored
 * rather than putting back a message whose turn has started.
 */
async function runQueueAction(
  threadId: ThreadId,
  messageId: string,
  action: QueueAction,
  verb: "edit" | "remove" | "move" | "send" | "steer",
): Promise<boolean> {
  try {
    const response = await getClient().request({
      type: "queue_message",
      thread_id: threadId,
      message_id: messageId,
      action,
    });
    if (response.type === "queue_updated") {
      // A no-op (a move at the end of the queue) changed nothing and carries no
      // event; replacing newer rows with it would only lose state.
      if (response.seq === 0) return true;
      const state = getState();
      const latest = state.threadSeqs[threadId] ?? 0;
      const held = state.queues[threadId]?.revision ?? 0;
      if (response.seq < latest || response.revision < held) return true;
      if (response.queued.length < response.total) {
        // More messages wait than one reply can carry: it is the first page,
        // not the queue. Fetch every page rather than replacing the cached
        // rows with a short list.
        void loadThreadQueue(threadId);
        return true;
      }
      applyQueue(threadId, response.queued, response.total, response.revision);
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    const unclear = proto.code === "delivery_unclear";
    pushToast(
      unclear
        ? {
            variant: "info",
            title: i18n.t("composer:toasts.unclear.title"),
            description: i18n.t("composer:toasts.unclear.queue", { message: proto.message }),
          }
        : {
            variant: "error",
            title: i18n.t(`composer:toasts.queueFailed.${verb}`),
            description: proto.message,
          },
    );
    return false;
  }
}

/**
 * Replaces the cached queue rows for one thread.
 *
 * A reply older than the rows already held, or older than the summary daemon's
 * own revision, is dropped: the queue it describes has already moved on.
 */
function applyQueue(
  threadId: ThreadId,
  queued: QueuedMessage[],
  total: number,
  revision: number,
): void {
  const state = getState();
  const held = state.queues[threadId];
  if (held && revision < held.revision) return;
  if (revision < (state.threads[threadId]?.queue_revision ?? 0)) return;
  setState({
    ...state,
    queues: { ...state.queues, [threadId]: { queued, total, revision } },
  });
}

/**
 * Fetches a thread's queue, following pages until `total` is covered.
 *
 * A queue larger than one frame comes back in bounded pages. Pages are only
 * joined while every one names the same revision: a message that started or was
 * edited between two pages would otherwise be stitched into a queue that never
 * existed, so the read restarts from the first page.
 */
export async function loadThreadQueue(threadId: ThreadId): Promise<void> {
  let offset = 0;
  let rows: QueuedMessage[] = [];
  let revision: number | null = null;
  let restarts = 0;
  for (;;) {
    let page: { queued: QueuedMessage[]; total: number; next_offset: number; revision: number };
    try {
      const response = await getClient().request({
        type: "get_thread_queue",
        thread_id: threadId,
        offset,
      });
      if (response.type !== "thread_queue") return;
      page = response;
    } catch {
      // The next summary with a newer revision retries this; a toast for a
      // queue nobody is looking at would be noise.
      return;
    }
    if (revision !== null && page.revision !== revision) {
      // The queue changed between two pages. Start over with the revision it
      // has now; a few restarts in a row means the queue is churning, and the
      // latest summary will ask again.
      if (restarts >= 2) return;
      restarts += 1;
      offset = 0;
      rows = [];
      revision = null;
      continue;
    }
    revision = page.revision;
    rows = rows.concat(page.queued);
    if (page.next_offset <= offset || page.next_offset >= page.total) {
      applyQueue(threadId, rows, page.total, page.revision);
      return;
    }
    offset = page.next_offset;
  }
}

/**
 * Keeps the cached rows in step with a summary.
 *
 * The summary only counts the queue. A count that does not match what this
 * client holds, or a revision it has not applied, means the rows are stale.
 * A count of zero keeps an empty entry at the summary's revision rather than
 * dropping the entry: a reply still in flight must be recognised as older than
 * the summary and dropped, instead of putting back a message that has started.
 *
 * Exposed for tests; the live path calls it for each `thread_upserted`.
 */
export function syncQueueWithSummary(summary: ThreadSummary): void {
  const state = getState();
  const held = state.queues[summary.thread_id];
  // A summary the daemon wrote before the rows this client already holds is
  // delayed, not news: applying its count would hide (or clear) a queue that
  // has moved on, and end an edit of a row that still exists.
  if (held && summary.queue_revision < held.revision) return;
  if (summary.queued_count === 0) {
    if (held && held.total === 0 && held.revision === summary.queue_revision) return;
    setState({
      ...state,
      queues: {
        ...state.queues,
        [summary.thread_id]: {
          queued: [],
          total: 0,
          revision: summary.queue_revision,
        },
      },
    });
    return;
  }
  if (held && held.revision === summary.queue_revision && held.total === summary.queued_count) {
    return;
  }
  void loadThreadQueue(summary.thread_id);
}

export function editQueuedMessage(
  threadId: ThreadId,
  messageId: string,
  text: string,
  attachments: Attachment[],
): Promise<boolean> {
  return runQueueAction(
    threadId,
    messageId,
    { type: "edit", text, attachments: attachments.map((attachment) => attachment.id) },
    "edit",
  );
}

export function removeQueuedMessage(threadId: ThreadId, messageId: string): Promise<boolean> {
  return runQueueAction(threadId, messageId, { type: "remove" }, "remove");
}

export function moveQueuedMessage(
  threadId: ThreadId,
  messageId: string,
  direction: "up" | "down",
): Promise<boolean> {
  return runQueueAction(threadId, messageId, { type: "move", direction }, "move");
}

/** Sends a queued message now: steer it in, or move it to the front. */
export function sendQueuedNow(threadId: ThreadId, messageId: string): Promise<boolean> {
  return runQueueAction(
    threadId,
    messageId,
    { type: "send_now", turn_id: observedTurn(threadId) },
    "send",
  );
}

/** Steers one queued message, refusing rather than queuing it again. */
export function steerQueuedMessage(threadId: ThreadId, messageId: string): Promise<boolean> {
  return runQueueAction(
    threadId,
    messageId,
    { type: "steer", turn_id: observedTurn(threadId) },
    "steer",
  );
}

/**
 * The turn this client last saw running for a thread.
 *
 * A queue action names it so the daemon steers only into the turn the person
 * meant; a turn that ended in the meantime is refused or moved instead of
 * catching whatever runs next.
 */
function observedTurn(threadId: ThreadId): TurnId | null {
  return getState().threads[threadId]?.active_turn_id ?? null;
}

/**
 * Steers the oldest queued message into the running turn.
 *
 * The shortcut leaves the composer's draft alone. It needs both a running turn
 * and a harness that really takes mid-turn input: without either, "steer" would
 * refuse or mean something else, so the key does nothing instead of guessing.
 */
export function steerOldestQueued(threadId: ThreadId): boolean {
  const state = getState();
  const summary = state.threads[threadId];
  if (!summary?.steer || !isTurnRunning(summary)) return false;
  const oldest = state.queues[threadId]?.queued[0];
  if (!oldest?.steerable) return false;
  void steerQueuedMessage(threadId, oldest.message_id);
  return true;
}

export type CreateThreadInput = {
  host: HostId;
  harness: HarnessKind;
  /** Configured ACP agent instance, when `harness` is `acp`. */
  agentId?: string | null;
  workspacePath: string;
  model: string;
  prompt: string;
  attachments?: Attachment[];
  runtimeMode?: RuntimeMode;
  /** Reasoning effort in the harness's vocabulary, when it has one. */
  reasoningEffort?: string | null;
  /** Ask for a harness's fast tier (Codex). */
  fast?: boolean;
  worktree?: WorktreeRequest | null;
};

/**
 * Replaces the optimistic row with the ids the daemon returned.
 *
 * The response may arrive after `thread_upserted` already reconciled the row,
 * so every step tolerates a missing temp entry.
 */
function reconcileCreatedThread(
  tempId: ThreadId,
  threadId: ThreadId,
  created: ThreadSummary,
): void {
  const state = getState();
  const tempLog = state.logs[tempId];
  const existingLog = state.logs[threadId];
  const threads = omitKey(state.threads, tempId);
  // The create reply carries no sequence, and it was written before the first
  // event. If an upsert for this thread is already projected — a generated
  // title can land this fast — that summary is the newer one and stays.
  threads[threadId] =
    state.threadSeqs[threadId] !== undefined ? (state.threads[threadId] ?? created) : created;
  const logs = omitKey(state.logs, tempId);
  logs[threadId] = tempLog
    ? { ...tempLog, loaded: true, loading: false }
    : (existingLog ?? { ...createThreadLog(), loaded: true });
  const pendingCreates = state.pendingCreates.filter((pending) => pending.tempId !== tempId);

  moveLocalThreadDraft(created.host, tempId, threadId);

  let ui = state.ui;
  if (ui.route.kind === "thread" && ui.route.threadId === tempId) {
    ui = {
      ...ui,
      route: { kind: "thread", threadId },
      selectedThreadId: threadId,
    };
    replaceRoute(ui.route);
  }

  setState({ ...state, threads, logs, pendingCreates, ui });
}

export async function createThread(input: CreateThreadInput): Promise<boolean> {
  const state = getState();
  const tempId = `local-thread-${++localCounter}`;
  // Shown until the daemon names the thread; the prompt's own words win.
  const title = input.prompt.split("\n")[0]?.slice(0, 56) || i18n.t("sidebar:newThread");
  const now = new Date().toISOString();
  const optimistic: ThreadSummary = {
    thread_id: tempId,
    parent_thread_id: null,
    host: input.host,
    harness: input.harness,
    ...(input.agentId ? { agent_id: input.agentId } : {}),
    title,
    workspace_path: input.workspacePath,
    model: input.model,
    runtime_mode: input.runtimeMode ?? "approval_required",
    reasoning_efforts: [],
    models: [],
    ...(input.reasoningEffort ? { reasoning_effort: input.reasoningEffort } : {}),
    fast: input.fast ?? false,
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: now,
    agent_state: "thinking",
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
    needs_attention: false,
    pending_guest_requests: 0,
    unread: false,
    resume_at: null,
    rewind: "handoff",
    file_rewind: false,
    limit: null,
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    queued_count: 0,
    queue_revision: 0,
    steer: false,
  };

  setState({
    ...state,
    threads: { ...state.threads, [tempId]: optimistic },
    logs: { ...state.logs, [tempId]: { ...createThreadLog(), loaded: true } },
    pendingCreates: [...state.pendingCreates, { tempId, prompt: input.prompt, at: Date.now() }],
  });
  navigate({ kind: "thread", threadId: tempId });

  try {
    const response = await getClient().request({
      type: "create_thread",
      host: input.host,
      harness: input.harness,
      agent_id: input.agentId ?? null,
      workspace_path: input.workspacePath,
      model: input.model,
      prompt: input.prompt,
      attachments: (input.attachments ?? []).map((attachment) => attachment.id),
      runtime_mode: input.runtimeMode ?? "approval_required",
      reasoning_effort: input.reasoningEffort ?? null,
      fast: input.fast ?? false,
      worktree: input.worktree ?? null,
    });
    if (response.type === "thread_created") {
      reconcileCreatedThread(tempId, response.thread_id, response.summary);
    }
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    const current = getState();
    setState({
      ...current,
      threads: omitKey(current.threads, tempId),
      logs: omitKey(current.logs, tempId),
      pendingCreates: current.pendingCreates.filter((pending) => pending.tempId !== tempId),
    });
    // The optimistic thread never existed, so nothing will ever ask for its
    // draft again. Drop the memory-only text typed into it rather than leaving
    // it under a temporary id that another session could reuse.
    drafts.clearThreadDraft(input.host, tempId);
    navigate({ kind: "inbox" });
    pushToast({
      variant: "error",
      title: i18n.t("shell:start.failed", {
        host: getState().hosts[input.host]?.name ?? input.host,
      }),
      description: joinSentences([proto.message, i18n.t("shell:start.nothingRunning")]),
    });
    return false;
  }
}

/**
 * Forks one thread at a finished turn and opens the new thread.
 *
 * The daemon copies the source transcript; the client only records the row it
 * returns (an event for the same thread may already have arrived) and loads the
 * fork's history through the ordinary thread route. A refusal — a turn still
 * running or queued — names its recovery in the toast.
 */
export async function forkThread(threadId: ThreadId, turnId: string): Promise<boolean> {
  try {
    const response = await getClient().request({
      type: "fork_thread",
      thread_id: threadId,
      turn_id: turnId,
    });
    if (response.type !== "thread_forked") return false;
    const state = getState();
    setState({
      ...state,
      threads: { ...state.threads, [response.thread_id]: response.summary },
    });
    openThread(response.thread_id);
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      variant: "error",
      title: i18n.t("shell:fork.failed"),
      description: joinSentences([proto.message, proto.hint]),
    });
    return false;
  }
}

export async function refreshHosts(): Promise<void> {
  try {
    const response = await getClient().request({ type: "list_hosts" });
    if (response.type !== "hosts") return;
    const state = getState();
    setState({
      ...state,
      hosts: Object.fromEntries(response.hosts.map((host) => [host.host_id, host])),
    });
  } catch {
    // Presence is refreshed again on reconnect.
  }
}
