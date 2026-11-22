import { useSyncExternalStore } from "react";
import type {
  ApprovalDecision,
  Attachment,
  ClientRequest,
  HarnessKind,
  HostId,
  HostSummary,
  LimitStatus,
  ProtoError,
  ResumeSchedule,
  RuntimeMode,
  ThreadId,
  ThreadSummary,
  ThreadTab,
  WorktreeRequest,
} from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { createClientFromEnv, readBootConfig } from "@/lib/client/config";
import { toProtoError } from "@/lib/client/errors";
import type { ConnectionSnapshot, EventEnvelope } from "@/lib/client/types";
import { pruneThreadDrafts } from "@/lib/drafts";
import { pushRoute, type Route, readRoute, replaceRoute, routePath } from "@/lib/router";
import { applyTerminalTheme } from "@/lib/terminalTheme";
import { createThreadLog, foldEvent, type ThreadLog } from "./projection";

export type Theme = "light" | "dark" | "system";

export type UiView = "thread" | "ide";
export type PanelTab = "changes" | "files" | "subagents";

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
  focusSearchNonce: number;
  focusComposerNonce: number;
  focusFilesNonce: number;
  pendingIdePath: string | null;
  /** The file the IDE editor currently shows, for the panel tree's selection. */
  ideActivePath: string | null;
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
    const panelTab: PanelTab =
      parsed.panelTab === "files" || parsed.panelTab === "subagents" ? parsed.panelTab : "changes";
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

export type AppState = {
  connection: ConnectionSnapshot;
  hosts: Record<HostId, HostSummary>;
  threads: Record<ThreadId, ThreadSummary>;
  logs: Record<ThreadId, ThreadLog>;
  threadErrors: Record<ThreadId, ProtoError>;
  loadingThreads: boolean;
  threadsError: ProtoError | null;
  lastSyncAt: number | null;
  serverVersion: string | null;
  /** Daemon host id of the machine this client is talking to. */
  localHostId: HostId | null;
  pendingCreates: PendingCreate[];
  pendingActions: Record<ThreadId, string | undefined>;
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
    logs: {},
    threadErrors: {},
    loadingThreads: true,
    threadsError: null,
    lastSyncAt: null,
    serverVersion: null,
    localHostId: null,
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
      theme: initialTheme(),
      snoozedOpen: false,
      filter: { host: null, harness: null, needsAttention: false, unread: false },
      filterOpen: false,
      moreOpen: false,
      limitsOpen: false,
      recordingShortcut: false,
      newThreadPreset: null,
      addProjectOpen: false,
      focusSearchNonce: 0,
      focusComposerNonce: 0,
      focusFilesNonce: 0,
      pendingIdePath: null,
      ideActivePath: null,
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
  setState({
    ...state,
    ui: {
      ...state.ui,
      route,
      selectedThreadId: route.kind === "thread" ? route.threadId : state.ui.selectedThreadId,
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
  setState({ ...state, ui: { ...state.ui, selectedThreadId: threadId } });
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

export function getClient(): H4rnClient {
  client ??= createClientFromEnv();
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
}

function enqueueEvent(envelope: EventEnvelope): void {
  eventQueue.push(envelope);
  scheduleFlush();
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
      connection: { ...state.connection, error: null },
    });
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
        // The full inventory (child threads included) decides which drafts
        // survive; only root threads go into the visible list below.
        include_children: true,
      },
    });
    if (response.type !== "threads") return;
    const current = getState();
    const threads: Record<ThreadId, ThreadSummary> = {
      ...response.threads.reduce<Record<ThreadId, ThreadSummary>>((acc, thread) => {
        if (thread.parent_thread_id !== null) return acc;
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
    // A thread that is gone takes its draft with it; this also runs at startup
    // so drafts left by deleted threads do not accumulate. The inventory is
    // every thread the daemon reported, child threads included, plus any child
    // the subagent panel had already opened. Only hosts this daemon knows are
    // considered, so switching machines or signing out leaves other drafts be.
    const retained = new Map<string, { host: HostId; threadId: ThreadId }>();
    for (const thread of response.threads) {
      retained.set(`${thread.host}\u0000${thread.thread_id}`, {
        host: thread.host,
        threadId: thread.thread_id,
      });
    }
    for (const thread of Object.values(current.threads)) {
      if (thread.parent_thread_id === null) continue;
      retained.set(`${thread.host}\u0000${thread.thread_id}`, {
        host: thread.host,
        threadId: thread.thread_id,
      });
    }
    pruneThreadDrafts([...retained.values()], Object.keys(current.hosts));
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
    const response = await getClient().request({
      type: "get_thread",
      thread_id: threadId,
      since_seq: sinceSeq,
    });
    if (response.type !== "thread") return;

    let log = getState().logs[threadId] ?? createThreadLog();
    for (const { seq, event } of response.events) {
      // The global replay may already have applied an event while this
      // request was in flight; sequence numbers make the load idempotent.
      if (seq <= log.nextSeq) continue;
      log = foldEvent(log, event);
      log = { ...log, nextSeq: seq };
    }
    log = {
      ...log,
      loading: false,
      loaded: true,
      nextSeq: Math.max(log.nextSeq, response.next_seq),
    };

    const current = getState();
    setState({
      ...current,
      threads: { ...current.threads, [threadId]: response.summary },
      logs: { ...current.logs, [threadId]: log },
      threadErrors: omitKey(current.threadErrors, threadId),
    });
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

function applyEvent(state: AppState, envelope: EventEnvelope): AppState {
  const { event } = envelope;

  if (event.type === "thread_upserted") {
    const { type: _type, ...summary } = event;
    let threads = { ...state.threads, [summary.thread_id]: summary };
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

async function runThreadAction(
  threadId: ThreadId,
  label: string,
  body: Extract<ClientRequest, { type: "set_thread_tab" | "snooze" | "interrupt" }>,
): Promise<boolean> {
  const state = getState();
  setState({
    ...state,
    pendingActions: { ...state.pendingActions, [threadId]: label },
  });
  try {
    await getClient().request(body);
    return true;
  } catch (error) {
    const proto = toProtoError(error);
    pushToast({
      variant: "error",
      title: `${label} failed on ${getState().threads[threadId]?.host ?? "the host"}.`,
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
    tab === "settled" ? "Settling" : tab === "done" ? "Moving to Done" : "Moving to Working";
  await runThreadAction(threadId, label, {
    type: "set_thread_tab",
    thread_id: threadId,
    tab,
    pinned,
  });
}

export async function acknowledgeThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "Acknowledging", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "settled",
    pinned: false,
  });
}

export async function settleThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "Settling", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "settled",
    pinned: false,
  });
}

export async function togglePin(threadId: ThreadId): Promise<void> {
  const summary = getState().threads[threadId];
  if (!summary) return;
  await runThreadAction(threadId, summary.pinned ? "Unpinning" : "Pinning", {
    type: "set_thread_tab",
    thread_id: threadId,
    tab: "working",
    pinned: !summary.pinned,
  });
}

export async function snoozeThread(threadId: ThreadId, until: string | null): Promise<void> {
  await runThreadAction(threadId, until ? "Snoozing" : "Waking", {
    type: "snooze",
    thread_id: threadId,
    until,
  });
}

export async function interruptThread(threadId: ThreadId): Promise<void> {
  await runThreadAction(threadId, "Interrupting", { type: "interrupt", thread_id: threadId });
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
      title: `Couldn't send ${decision === "approve" ? "Go" : "Hold"} to ${current.threads[threadId]?.host ?? "the host"}.`,
      description: `${proto.message} The approval is still pending.`,
    });
  }
}

let localCounter = 0;

export async function sendMessage(
  threadId: ThreadId,
  text: string,
  attachments: Attachment[] = [],
): Promise<boolean> {
  const body = text.trim();
  if (!body && attachments.length === 0) return false;
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

  try {
    await getClient().request({
      type: "send_message",
      thread_id: threadId,
      text: body,
      attachments: attachments.map((attachment) => attachment.id),
    });
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
    return true;
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
            pending: currentLog.pending.filter((message) => message.id !== localId),
          },
        },
      });
    }
    pushToast({
      variant: "error",
      title: `Couldn't send the reply to ${current.threads[threadId]?.host ?? "the host"}.`,
      description: `${proto.message} Your draft is still in the composer.`,
    });
    return false;
  }
}

export type CreateThreadInput = {
  host: HostId;
  harness: HarnessKind;
  workspacePath: string;
  model: string;
  prompt: string;
  attachments?: Attachment[];
  runtimeMode?: RuntimeMode;
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
  summary: ThreadSummary,
): void {
  const state = getState();
  const tempLog = state.logs[tempId];
  const existingLog = state.logs[threadId];
  const threads = omitKey(state.threads, tempId);
  threads[threadId] = summary;
  const logs = omitKey(state.logs, tempId);
  logs[threadId] = tempLog
    ? { ...tempLog, loaded: true, loading: false }
    : (existingLog ?? { ...createThreadLog(), loaded: true });
  const pendingCreates = state.pendingCreates.filter((pending) => pending.tempId !== tempId);

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
  const title = input.prompt.split("\n")[0]?.slice(0, 56) || "New thread";
  const now = new Date().toISOString();
  const optimistic: ThreadSummary = {
    thread_id: tempId,
    parent_thread_id: null,
    host: input.host,
    harness: input.harness,
    title,
    workspace_path: input.workspacePath,
    model: input.model,
    tab: "working",
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
    unread: false,
    resume_at: null,
    limit: null,
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
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
      workspace_path: input.workspacePath,
      model: input.model,
      prompt: input.prompt,
      attachments: (input.attachments ?? []).map((attachment) => attachment.id),
      runtime_mode: input.runtimeMode ?? "approval_required",
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
    navigate({ kind: "inbox" });
    pushToast({
      variant: "error",
      title: `Couldn't start the thread on ${getState().hosts[input.host]?.name ?? input.host}.`,
      description: `${proto.message} Nothing is running.`,
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
