import { useEffect } from "react";
import { threadsForTab } from "@/lib/inbox";
import { type CommandId, chordFromEvent, commandSpec, matchCommand } from "@/lib/keybindings";
import {
  acknowledgeThread,
  getState,
  interruptThread,
  navigate,
  openNewThread,
  openShortcuts,
  openThread,
  pendingApprovalIn,
  requestComposerFocus,
  requestFilesFocus,
  respondApproval,
  setPanelTab,
  setTab,
  setThreadTab,
  setUi,
  snoozeThread,
  togglePanel,
  togglePin,
  toggleSidebar,
  toggleTerminal,
  toggleTheme,
  toggleView,
} from "@/lib/store/appStore";

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable;
}

type State = ReturnType<typeof getState>;

/** The thread a thread command acts on: the open one, else the selected row. */
function targetThread(state: State) {
  const route = state.ui.route;
  const id = route.kind === "thread" ? route.threadId : state.ui.selectedThreadId;
  return id ? state.threads[id] : undefined;
}

function openThreadRoute(state: State): string | null {
  const route = state.ui.route;
  return route.kind === "thread" ? route.threadId : null;
}

function stepThread(state: State, direction: 1 | -1): boolean {
  const visible = threadsForTab(state, state.ui.tab, Date.now());
  if (visible.length === 0) return false;
  const currentId = openThreadRoute(state) ?? state.ui.selectedThreadId;
  const index = visible.findIndex((thread) => thread.thread_id === currentId);
  const nextIndex =
    index < 0
      ? direction === 1
        ? 0
        : visible.length - 1
      : Math.min(visible.length - 1, Math.max(0, index + direction));
  const next = visible[nextIndex];
  if (!next || next.thread_id === currentId) return true;
  openThread(next.thread_id);
  return true;
}

/**
 * Runs one command. Returns false when it does not apply right now, so the
 * key press is left alone (for example Cmd+Enter with nothing to approve).
 */
export function runCommand(id: CommandId, state: State = getState()): boolean {
  const thread = targetThread(state);
  const openId = openThreadRoute(state);
  const inThreadView = state.ui.route.kind === "thread" || state.ui.route.kind === "ide";
  switch (id) {
    case "palette.toggle":
      setUi({ paletteOpen: !state.ui.paletteOpen, newThreadOpen: false });
      return true;
    case "shortcuts.open":
      if (state.ui.shortcutsOpen) setUi({ shortcutsOpen: false });
      else openShortcuts();
      return true;
    case "thread.new":
      openNewThread();
      return true;
    case "settings.open":
      navigate({ kind: "placeholder", surface: "settings", path: "/settings/general" });
      return true;
    case "theme.toggle":
      toggleTheme();
      return true;
    case "nav.inbox":
      navigate({ kind: "inbox" });
      return true;
    case "nav.pulls":
      navigate({ kind: "placeholder", surface: "pulls", path: "/pulls" });
      return true;
    case "nav.usage":
      navigate({ kind: "placeholder", surface: "usage", path: "/usage" });
      return true;
    case "nav.tools":
      navigate({ kind: "placeholder", surface: "library", path: "/library" });
      return true;
    case "tab.working":
      setTab("working");
      return true;
    case "tab.done":
      setTab("done");
      return true;
    case "tab.settled":
      setTab("settled");
      return true;
    case "thread.next":
      return stepThread(state, 1);
    case "thread.previous":
      return stepThread(state, -1);
    case "thread.reply":
      if (!openId) return false;
      requestComposerFocus();
      return true;
    case "approval.approve":
    case "approval.deny": {
      if (!openId) return false;
      const approvalId = pendingApprovalIn(state.logs[openId]);
      if (!approvalId) return false;
      void respondApproval(openId, approvalId, id === "approval.approve" ? "approve" : "deny");
      return true;
    }
    case "turn.interrupt":
      if (!openId) return false;
      void interruptThread(openId);
      return true;
    case "turn.next":
    case "turn.previous":
      if (!openId) return false;
      window.dispatchEvent(
        new CustomEvent("h4rn:turn", { detail: { direction: id === "turn.next" ? 1 : -1 } }),
      );
      return true;
    case "thread.settle":
      if (!thread || thread.tab === "settled") return false;
      void setThreadTab(thread.thread_id, "settled");
      return true;
    case "thread.acknowledge":
      if (thread?.tab !== "done") return false;
      void acknowledgeThread(thread.thread_id);
      return true;
    case "thread.snooze":
      if (!thread) return false;
      void snoozeThread(thread.thread_id, new Date(Date.now() + 3_600_000).toISOString());
      return true;
    case "thread.pin":
      if (!thread) return false;
      void togglePin(thread.thread_id);
      return true;
    case "thread.reopen":
      if (thread?.tab !== "settled") return false;
      void setThreadTab(thread.thread_id, "done");
      return true;
    case "composer.attach":
      // The focused composer owns its file picker and listens for this event.
      window.dispatchEvent(new CustomEvent("h4rn:attach"));
      return true;
    case "sidebar.toggle":
      toggleSidebar();
      return true;
    case "panel.toggle":
      togglePanel();
      return true;
    case "view.toggle":
      toggleView();
      return true;
    case "terminal.toggle":
      if (!inThreadView) return false;
      toggleTerminal();
      return true;
    case "panel.changes":
      if (!inThreadView) return false;
      setPanelTab("changes");
      return true;
    case "panel.subagents":
      if (!inThreadView) return false;
      setPanelTab("subagents");
      return true;
    case "panel.files":
      if (!thread) return false;
      requestFilesFocus();
      return true;
  }
}

/** Global keyboard map. Every command is a Cmd/Ctrl chord (see `lib/keybindings`). */
export function useKeyboardShortcuts(): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) return;
      const state = getState();

      if (event.key === "Escape" && !event.metaKey && !event.ctrlKey) {
        if (state.ui.paletteOpen) {
          event.preventDefault();
          setUi({ paletteOpen: false });
          return;
        }
        if (state.ui.shortcutsOpen) {
          event.preventDefault();
          setUi({ shortcutsOpen: false });
          return;
        }
        const route = state.ui.route;
        if (
          !isTypingTarget(event.target) &&
          (route.kind === "hosts" ||
            (route.kind === "placeholder" &&
              (route.surface === "settings" || route.surface === "library")))
        ) {
          event.preventDefault();
          navigate({ kind: "inbox" });
        }
        return;
      }

      // The Shortcuts settings page records the next chord itself.
      if (state.ui.recordingShortcut) return;

      const chord = chordFromEvent(event);
      if (!chord) return;
      const id = matchCommand(chord);
      if (!id) return;
      if (isTypingTarget(event.target) && !commandSpec(id).inInputs) return;
      if (runCommand(id, state)) event.preventDefault();
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
}
