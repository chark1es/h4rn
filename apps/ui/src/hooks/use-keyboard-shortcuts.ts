import { useEffect, useRef } from "react";

import { threadsForTab } from "@/lib/inbox";
import type { Route } from "@/lib/router";
import {
  acknowledgeThread,
  getState,
  interruptThread,
  navigate,
  openNewThread,
  openShortcuts,
  openThread,
  requestComposerFocus,
  requestSearchFocus,
  respondApproval,
  setSelectedThread,
  setTab,
  setThreadTab,
  setUi,
  snoozeThread,
  togglePin,
  toggleTheme,
} from "@/lib/store/appStore";

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable;
}

function routeOf(state: ReturnType<typeof getState>): Route {
  return state.ui.route;
}

function selectedThread(state: ReturnType<typeof getState>) {
  const id = state.ui.selectedThreadId;
  return id ? state.threads[id] : undefined;
}

function pendingApproval(threadId: string): string | null {
  const log = getState().logs[threadId];
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

/** Global keyboard map from `docs/ux.md`. Sequences never fire inside inputs. */
export function useKeyboardShortcuts(): void {
  const sequence = useRef<{ key: string; timer: ReturnType<typeof setTimeout> | null }>({
    key: "",
    timer: null,
  });

  useEffect(() => {
    const clearSequence = () => {
      sequence.current.key = "";
      if (sequence.current.timer) clearTimeout(sequence.current.timer);
      sequence.current.timer = null;
      setUi({ sequenceHint: null });
    };

    const onKeyDown = (event: KeyboardEvent) => {
      const state = getState();
      const route = routeOf(state);
      const meta = event.metaKey || event.ctrlKey;
      const key = event.key;
      const lower = key.toLowerCase();
      const typing = isTypingTarget(event.target);

      if (meta && lower === "k") {
        event.preventDefault();
        setUi({ paletteOpen: !state.ui.paletteOpen, newThreadOpen: false });
        return;
      }

      if (meta && event.shiftKey && lower === "n") {
        event.preventDefault();
        openNewThread();
        return;
      }

      if (meta && event.shiftKey && lower === "l") {
        event.preventDefault();
        toggleTheme();
        return;
      }

      if (meta && key === ",") {
        event.preventDefault();
        navigate({ kind: "placeholder", surface: "settings", path: "/settings" });
        return;
      }

      if (meta && key === "Enter") {
        if (route.kind === "thread") {
          const approvalId = pendingApproval(route.threadId);
          if (approvalId) {
            event.preventDefault();
            void respondApproval(route.threadId, approvalId, "approve");
          }
        }
        return;
      }

      if (meta && key === "Backspace") {
        if (route.kind === "thread") {
          const approvalId = pendingApproval(route.threadId);
          if (approvalId) {
            event.preventDefault();
            void respondApproval(route.threadId, approvalId, "deny");
          }
        }
        return;
      }

      if (meta && key === ".") {
        if (route.kind === "thread") {
          event.preventDefault();
          void interruptThread(route.threadId);
        }
        return;
      }

      if (typing || meta) return;

      if (event.altKey) {
        if (key === "1" || key === "2" || key === "3") {
          event.preventDefault();
          setTab(key === "1" ? "working" : key === "2" ? "done" : "settled");
        }
        return;
      }

      if (key === "?") {
        event.preventDefault();
        if (state.ui.shortcutsOpen) setUi({ shortcutsOpen: false });
        else openShortcuts();
        return;
      }

      if (key === "Escape") {
        clearSequence();
        return;
      }

      if (route.kind === "thread") {
        if (key === "]") {
          event.preventDefault();
          window.dispatchEvent(new CustomEvent("h4rn:cue", { detail: { direction: 1 } }));
          return;
        }
        if (key === "[") {
          event.preventDefault();
          window.dispatchEvent(new CustomEvent("h4rn:cue", { detail: { direction: -1 } }));
          return;
        }
      }

      if (sequence.current.key === "g") {
        clearSequence();
        const targets: Record<string, Route> = {
          i: { kind: "inbox" },
          h: { kind: "hosts" },
          l: { kind: "placeholder", surface: "library", path: "/library" },
          u: { kind: "placeholder", surface: "usage", path: "/usage" },
          e: { kind: "placeholder", surface: "ide", path: "/ide" },
          p: { kind: "placeholder", surface: "pulls", path: "/pulls" },
        };
        const target = targets[lower];
        if (target) {
          event.preventDefault();
          navigate(target);
        }
        return;
      }

      if (lower === "g") {
        event.preventDefault();
        sequence.current.key = "g";
        if (sequence.current.timer) clearTimeout(sequence.current.timer);
        sequence.current.timer = setTimeout(clearSequence, 900);
        setUi({ sequenceHint: "G …" });
        return;
      }

      if (key === "/") {
        event.preventDefault();
        if (route.kind !== "inbox") navigate({ kind: "inbox" });
        requestSearchFocus();
        return;
      }

      if (lower === "n" && route.kind !== "thread") {
        event.preventDefault();
        openNewThread();
        return;
      }

      if (lower === "r" && route.kind === "thread") {
        event.preventDefault();
        requestComposerFocus();
        return;
      }

      if (route.kind !== "inbox") return;

      const now = Date.now();
      const visible = threadsForTab(state, state.ui.tab, now);
      const current = selectedThread(state);
      const index = current
        ? visible.findIndex((thread) => thread.thread_id === current.thread_id)
        : -1;

      if (lower === "j" || lower === "k") {
        event.preventDefault();
        const nextIndex =
          visible.length === 0
            ? -1
            : lower === "j"
              ? Math.min(visible.length - 1, index + 1)
              : Math.max(0, index <= 0 ? 0 : index - 1);
        const next = nextIndex >= 0 ? visible[nextIndex] : undefined;
        if (next) setSelectedThread(next.thread_id);
        return;
      }

      if (key === "Enter" && current) {
        event.preventDefault();
        openThread(current.thread_id);
        return;
      }

      if (!current) return;

      if (lower === "a") {
        event.preventDefault();
        if (current.tab === "done") void acknowledgeThread(current.thread_id);
        return;
      }
      if (lower === "p") {
        event.preventDefault();
        void togglePin(current.thread_id);
        return;
      }
      if (lower === "s" && !event.shiftKey) {
        event.preventDefault();
        void snoozeThread(current.thread_id, new Date(Date.now() + 3_600_000).toISOString());
        return;
      }
      if (event.shiftKey && lower === "s") {
        event.preventDefault();
        void setThreadTab(current.thread_id, "settled");
        return;
      }
      if (lower === "u" && current.tab === "settled") {
        event.preventDefault();
        void setThreadTab(current.thread_id, "done");
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      if (sequence.current.timer) clearTimeout(sequence.current.timer);
    };
  }, []);
}
