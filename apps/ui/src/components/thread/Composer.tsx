import {
  ArrowUpIcon,
  AtSignIcon,
  CheckIcon,
  CornerDownRightIcon,
  ListEndIcon,
  PaperclipIcon,
  PlusIcon,
  XIcon,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";

import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import { useWorkspaceFiles } from "@/features/composer/workspaceFiles";
import type { QueuedMessage, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { isTurnRunning } from "@/lib/agent";
import { useFollowUpBehavior, useMultilineEnter } from "@/lib/chatSettings";
import { composerCommands } from "@/lib/composerCommands";
import { caretOnFirstLine, sentPrompts, stepPromptHistory } from "@/lib/composerHistory";
import {
  applyCompletion,
  type ComposerTrigger,
  detectComposerTrigger,
  fileReference,
  insertMentionToken,
  rankCommands,
  rankFilePaths,
  stepSelection,
  triggerKey,
} from "@/lib/composerMenu";
import {
  clearThreadDraftIfRevision,
  type DraftRevision,
  flushDrafts,
  readThreadDraft,
  readThreadDraftEntry,
  subscribeDraftChanges,
  writeThreadDraft,
} from "@/lib/drafts";
import { harnessLabel } from "@/lib/entities";
import { chordFromEvent, matchCommand } from "@/lib/keybindings";
import { openPhonePanel } from "@/lib/phonePanels";
import { editQueuedMessage, getState, sendMessage, setUi, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { ComposerMenu, type ComposerMenuRow } from "./ComposerMenu";
import { ThreadControls } from "./ThreadControls";

type OpenMenu = { trigger: ComposerTrigger; activeIndex: number };

/** Where the desktop popover sits, relative to the input wrapper. */
function caretAnchor(
  element: HTMLTextAreaElement,
  position: number,
): { left: number; bottom: number; lineHeight: number } {
  const wrapper = element.parentElement;
  if (!wrapper) return { left: 0, bottom: 0, lineHeight: 21 };
  const width = wrapper.clientWidth || element.clientWidth;
  if (typeof window === "undefined" || !window.getComputedStyle) {
    return { left: 0, bottom: 0, lineHeight: 21 };
  }
  const computed = window.getComputedStyle(element);
  const mirror = document.createElement("div");
  // `getPropertyValue` only accepts CSS property names (kebab-case); camelCase
  // names silently return "" in Chromium.
  for (const property of [
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "letter-spacing",
    "line-height",
    "text-transform",
    "word-spacing",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
    "border-style",
    "box-sizing",
  ]) {
    mirror.style.setProperty(property, computed.getPropertyValue(property));
  }
  mirror.style.position = "absolute";
  mirror.style.visibility = "hidden";
  mirror.style.top = "0";
  mirror.style.left = "0";
  mirror.style.width = `${element.clientWidth || width}px`;
  mirror.style.whiteSpace = "pre-wrap";
  mirror.style.overflowWrap = "break-word";
  mirror.textContent = element.value.slice(0, position);
  const marker = document.createElement("span");
  marker.textContent = element.value.slice(position) || ".";
  mirror.append(marker);
  document.body.append(mirror);
  const markerLeft = marker.offsetLeft;
  const markerTop = marker.offsetTop;
  mirror.remove();

  const lineHeight = Number.parseFloat(computed.lineHeight) || 21;
  // The textarea scrolls internally once it reaches its max height. The
  // mirror measures the caret in the full text, so subtract the scroll to
  // point at the line the caret is actually on.
  const caretTop = markerTop - element.scrollTop;
  const above = wrapper.clientHeight - (element.offsetTop + caretTop);
  return {
    left: Math.max(0, markerLeft + element.offsetLeft),
    bottom: Math.max(lineHeight, above + 6),
    // The flip-below fallback needs this line's height; a fixed default puts
    // the menu inside the caret line when the CSS line height differs.
    lineHeight,
  };
}

export function Composer({ summary }: { summary: ThreadSummary }) {
  const focusNonce = useApp((state) => state.ui.focusComposerNonce);
  const connection = useApp((state) => state.connection.status);
  const hostOnline = useApp((state) => state.hosts[summary.host]?.online ?? false);
  const hostName = useApp((state) => state.hosts[summary.host]?.name ?? summary.host);
  const [text, setText] = useState(() => readThreadDraft(summary.host, summary.thread_id));
  const [sending, setSending] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [menu, setMenu] = useState<OpenMenu | null>(null);
  const [anchor, setAnchor] = useState({ left: 0, bottom: 0, lineHeight: 21 });
  /**
   * The queued message open for editing, with the draft it interrupted.
   *
   * While editing, typing goes to the queue message instead of the draft: the
   * stored draft stays exactly as it was, so Cancel can restore it.
   */
  const [editing, setEditing] = useState<{
    messageId: string;
    draftText: string;
    draftRevision: DraftRevision | null;
  } | null>(null);
  const followUp = useFollowUpBehavior();
  const multiline = useMultilineEnter();
  const running = isTurnRunning(summary);
  const canSteer = summary.steer;
  const defaultAction = running && canSteer ? followUp : "queue";
  const beginEditRef = useRef<(message: QueuedMessage) => void>(() => {});
  const queued = summary.queued ?? [];
  const ref = useRef<HTMLTextAreaElement | null>(null);
  /** True from compositionstart to compositionend; IME never drives the menu. */
  const composing = useRef(false);
  /** The trigger Escape dismissed; typing the same token does not reopen it. */
  const dismissed = useRef<string | null>(null);
  const isPhone = useIsPhone();
  /**
   * The revision of the text this composer is showing: set when the draft is
   * loaded, when it is written, and when another tab's change is adopted.
   */
  const revision = useRef<DraftRevision | null>(null);
  /** The thread this component is showing now; a send started on another must not touch it. */
  const shownThread = useRef({ host: summary.host, threadId: summary.thread_id });
  /** -1 while the in-progress text is showing; otherwise an index into sent prompts. */
  const historyIndex = useRef(-1);
  /** The in-progress text captured before the first ArrowUp. */
  const historyDraft = useRef("");

  const terminalOpen = useApp((state) => state.ui.layout.terminalOpen);
  const view = useApp((state) => state.ui.layout.view);
  const commands = useMemo(
    () =>
      composerCommands(summary, {
        terminalOpen,
        view,
        phone: isPhone,
        openPhonePanel,
      }),
    [summary, terminalOpen, view, isPhone],
  );
  const workspaceTarget = useMemo(
    () => ({ host: summary.host, workspace: summary.workspace_path }),
    [summary.host, summary.workspace_path],
  );
  const fileIndex = useWorkspaceFiles(workspaceTarget, menu?.trigger.kind === "file");
  // Rank the latest token synchronously. Debouncing the query left the menu
  // showing (and accepting) rows from the previous query for ~120 ms.
  const menuQuery = menu?.trigger.query ?? "";
  const commandQuery = menu?.trigger.kind === "command" ? menu.trigger.query : "";
  const fileResults = useMemo(
    () => rankFilePaths(fileIndex.files, menuQuery, 50),
    [fileIndex.files, menuQuery],
  );
  const commandResults = useMemo(
    () => rankCommands(commands, commandQuery),
    [commands, commandQuery],
  );
  const rows: ComposerMenuRow[] = useMemo(() => {
    if (menu?.trigger.kind === "command") {
      return commandResults.map((command) => ({
        kind: "command" as const,
        id: command.id,
        label: command.label,
        ...(command.hint ? { hint: command.hint } : {}),
        icon: command.icon,
      }));
    }
    return fileResults.map((path) => ({ kind: "file" as const, id: path, path }));
  }, [menu?.trigger.kind, commandResults, fileResults]);
  const activeIndex = menu && rows.length > 0 ? Math.min(menu.activeIndex, rows.length - 1) : 0;
  const workspaceName = summary.workspace_path.split("/").filter(Boolean).pop() ?? summary.host;
  const menuListId = `composer-menu-${summary.thread_id}`;

  const online = connection === "online";
  const writable = online && hostOnline;
  const disabledReason = !online
    ? `Your reply is saved as a draft and sends when ${hostName} reconnects`
    : !hostOnline
      ? `${hostName} is unreachable. The draft stays here.`
      : null;
  const attachments = useAttachmentDrafts(summary.thread_id, {
    enabled: writable,
    disabledReason,
  });

  useEffect(() => {
    shownThread.current = { host: summary.host, threadId: summary.thread_id };
    const entry = readThreadDraftEntry(summary.host, summary.thread_id);
    setText(entry.text);
    revision.current = entry.revision;
    historyIndex.current = -1;
    historyDraft.current = "";
    setEditing(null);
    setUi({ queuedEdit: null });
    setMenu(null);
    dismissed.current = null;
    requestAnimationFrame(() => {
      const element = ref.current;
      if (!element) return;
      element.style.height = "auto";
      element.style.height = `${Math.min(element.scrollHeight, 132)}px`;
    });
  }, [summary.host, summary.thread_id]);

  // The popover points at the caret line. Its position follows edits, not the
  // highlighted row, so moving with arrows never makes it jump. Measure the
  // real selection: the token may extend past the caret when it sits mid-word.
  useLayoutEffect(() => {
    const element = ref.current;
    if (!menu || !element || isPhone) return;
    const caret = Math.min(element.selectionStart ?? element.value.length, element.value.length);
    setAnchor(caretAnchor(element, caret));
  }, [menu, isPhone]);

  useEffect(() => {
    if (focusNonce > 0) ref.current?.focus();
  }, [focusNonce]);

  // Another tab may edit this thread's draft. Adopt its text only while this
  // composer has no unsaved edit of its own.
  useEffect(() => {
    return subscribeDraftChanges((change) => {
      if (change.scope !== "thread") return;
      if (change.host !== summary.host || change.target !== summary.thread_id) return;
      historyIndex.current = -1;
      historyDraft.current = "";
      revision.current = change.revision;
      setText(change.text);
      setMenu(null);
    });
  }, [summary.host, summary.thread_id]);

  const newThreadOpen = useApp((state) => state.ui.newThreadOpen);
  useAttachShortcut(attachments.openPicker, !newThreadOpen);

  const autosize = () => {
    const element = ref.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, 132)}px`;
  };

  const update = (value: string) => {
    historyIndex.current = -1;
    historyDraft.current = "";
    setText(value);
    // While editing a queued message the box holds that message, not the
    // thread's draft; the draft stays in storage untouched for Cancel.
    if (!editing) {
      revision.current = writeThreadDraft(summary.host, summary.thread_id, value);
    }
  };

  const recall = (direction: "backward" | "forward"): boolean => {
    const element = ref.current;
    if (!element) return false;
    const step = stepPromptHistory({
      direction,
      prompts: sentPrompts(getState().logs[summary.thread_id]),
      state: { index: historyIndex.current, draft: historyDraft.current },
      current: element.value,
    });
    if (!step) return false;
    historyIndex.current = step.index;
    historyDraft.current = step.draft;
    // Recall never touches the stored draft: the user's in-progress text
    // stays there until they edit the recalled prompt or send something.
    // The caret starts at the top so another ArrowUp keeps walking back
    // through a multi-line prompt instead of moving inside it.
    setText(step.text);
    requestAnimationFrame(() => {
      element.focus();
      element.setSelectionRange(0, 0);
      autosize();
    });
    return true;
  };

  const submit = async (opposite = false) => {
    if (!writable || sending || attachments.blockedReason) return;
    if (editing) {
      await saveEdit();
      return;
    }
    if (!text.trim() && attachments.attachments.length === 0) return;
    setMenu(null);
    dismissed.current = null;
    // Send exactly what is on screen, and remember the revision it was loaded
    // or written under. A newer draft written elsewhere while this send waits
    // must not be cleared.
    const value = text;
    const submitted = revision.current;
    const submittedHost = summary.host;
    const submittedThread = summary.thread_id;
    // A recalled prompt is not the stored draft: recall left the user's
    // in-progress text in storage on purpose, so a successful send of the
    // recalled text must not clear it. Remember whether this send came from
    // history and what the box held before recall started.
    const recalled = historyIndex.current >= 0;
    const preservedDraft = historyDraft.current;
    /** Where in history this send started; a newer recall replaces the restore. */
    const submittedHistoryIndex = historyIndex.current;
    // The button and the send shortcut agree: a running turn on a harness with
    // mid-turn input now gets Steer when the client's default says so; the
    // opposite shortcut always means the other action.
    let steer = false;
    if (running) {
      const action = opposite ? (defaultAction === "steer" ? "queue" : "steer") : defaultAction;
      steer = action === "steer" && canSteer;
    }
    // Store the draft before the send waits so a second tab's edit during the
    // request is not hidden behind this tab's pending write.
    flushDrafts();
    setSending(true);
    const ok = await sendMessage(submittedThread, value, attachments.attachments, { steer });
    setSending(false);
    if (!ok) return;
    // Clear only the draft that was sent. If this tab typed again, or another
    // tab saved newer text, that text stays in the box and in storage.
    const cleared = recalled
      ? false
      : clearThreadDraftIfRevision(submittedHost, submittedThread, submitted);
    // The pane may have switched threads while the send waited. The new
    // thread's box belongs to that thread, so leave its text and files alone.
    const stillShowing =
      shownThread.current.host === submittedHost &&
      shownThread.current.threadId === submittedThread;
    if (!stillShowing) return;
    // A recall the person made while the send waited is their newer intent:
    // its text stays on screen instead of the draft recall had preserved.
    const stillRecalled =
      recalled && historyIndex.current >= 0 && historyIndex.current === submittedHistoryIndex;
    historyIndex.current = -1;
    historyDraft.current = "";
    if (stillRecalled) {
      // Put the preserved draft back: the recalled prompt was sent, but the
      // unsent text from before recall is still unsent.
      setText(preservedDraft);
    } else if (cleared) {
      revision.current = null;
      setText("");
    }
    attachments.clear();
    requestAnimationFrame(autosize);
  };

  const beginEdit = (message: QueuedMessage) => {
    if (sending) return;
    setEditing({
      messageId: message.message_id,
      draftText: text,
      draftRevision: revision.current,
    });
    setText(message.text);
    setUi({
      queuedEdit: { threadId: summary.thread_id, messageId: message.message_id },
    });
    requestAnimationFrame(() => {
      const element = ref.current;
      if (!element) return;
      element.focus();
      element.setSelectionRange(message.text.length, message.text.length);
      autosize();
    });
  };
  beginEditRef.current = beginEdit;

  /** Leaves edit mode and either restores the draft or hands the text back. */
  const endEdit = useCallback(
    (restoreDraft: boolean) => {
      const current = editing;
      if (!current) return;
      setEditing(null);
      setUi({ queuedEdit: null });
      if (restoreDraft) {
        setText(current.draftText);
        revision.current = current.draftRevision;
        return;
      }
      // The queued message vanished while it was being edited. The edited text
      // moves into the composer only when that composer had no draft of its own
      // to restore; otherwise the draft wins and the edit is dropped.
      if (current.draftText.trim().length > 0) {
        setText(current.draftText);
        revision.current = current.draftRevision;
        return;
      }
      revision.current = writeThreadDraft(summary.host, summary.thread_id, text);
    },
    [editing, text, summary.host, summary.thread_id],
  );

  const saveEdit = async () => {
    if (!editing) return;
    const message = queued.find((item) => item.message_id === editing.messageId);
    if (!message) {
      endEdit(false);
      return;
    }
    const ok = await editQueuedMessage(
      summary.thread_id,
      editing.messageId,
      text,
      message.attachments,
    );
    if (!ok) return;
    endEdit(true);
    requestAnimationFrame(autosize);
  };

  const cancelEdit = () => {
    endEdit(true);
    requestAnimationFrame(autosize);
  };

  // Another pane (or the queue at the end of the thread) asks for a message to
  // be edited here.
  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ threadId: string; message: QueuedMessage }>).detail;
      if (!detail || detail.threadId !== summary.thread_id) return;
      beginEditRef.current(detail.message);
    };
    window.addEventListener("h4rn:edit-queued", handler);
    return () => window.removeEventListener("h4rn:edit-queued", handler);
  }, [summary.thread_id]);

  // A message can start (or be removed elsewhere) while it is open here. The
  // edit ends; changed text stays only when there was no draft to restore.
  useEffect(() => {
    if (!editing) return;
    if (queued.some((item) => item.message_id === editing.messageId)) return;
    endEdit(false);
  }, [queued, editing, endEdit]);

  const canSend =
    writable &&
    !sending &&
    !attachments.blockedReason &&
    (text.trim().length > 0 || attachments.attachments.length > 0);

  const replyTo = isPhone
    ? `Reply to ${harnessLabel(summary.harness)}`
    : `Reply to ${harnessLabel(summary.harness)} on ${summary.host}`;

  const syncMenu = (value: string, caret: number | null) => {
    if (!writable) {
      setMenu(null);
      return;
    }
    const trigger = detectComposerTrigger(value, caret ?? value.length);
    if (!trigger) {
      setMenu(null);
      dismissed.current = null;
      return;
    }
    const key = triggerKey(trigger);
    if (dismissed.current === key) {
      setMenu(null);
      return;
    }
    if (dismissed.current && dismissed.current !== key) dismissed.current = null;
    setMenu((current) => {
      if (
        current &&
        current.trigger.kind === trigger.kind &&
        current.trigger.start === trigger.start &&
        current.trigger.query === trigger.query &&
        current.trigger.end === trigger.end
      ) {
        return { ...current, trigger };
      }
      return { trigger, activeIndex: 0 };
    });
  };

  const focusCaret = (caret: number) => {
    const element = ref.current;
    if (!element) return;
    requestAnimationFrame(() => {
      if (!ref.current) return;
      element.focus();
      element.setSelectionRange(caret, caret);
      autosize();
    });
  };

  const closeMenu = (dismiss: boolean) => {
    if (dismiss && menu) dismissed.current = triggerKey(menu.trigger);
    setMenu(null);
  };

  const choose = (index: number) => {
    const row = rows[index];
    if (!menu || !row) return;
    // Read the textarea instead of render state: an accept key can arrive in
    // the same batch as the change that typed the query, before React has
    // re-rendered the rows. Ranking the old query then inserts the wrong row.
    const element = ref.current;
    const visibleText = element?.value ?? text;
    // The live trigger is the only one an accept may use. A key can land in the
    // same batch as text that replaced the trigger (`/` → `@`), before the rows
    // follow; accepting a row from the old menu then runs the wrong thing.
    const trigger = element
      ? detectComposerTrigger(element.value, element.selectionStart ?? element.value.length)
      : menu.trigger;
    if (!trigger || trigger.kind !== row.kind) return;
    if (row.kind === "file") {
      // Rank again for the query on screen right now. A key that lands before
      // the render that would have refreshed `rows` must never accept a row
      // from the previous query.
      const path = rankFilePaths(fileIndex.files, trigger.query, 50)[index];
      if (!path) return;
      const applied = applyCompletion(visibleText, trigger, fileReference(path));
      update(applied.value);
      dismissed.current = null;
      setMenu(null);
      focusCaret(applied.caret);
      return;
    }
    const command = rankCommands(commands, trigger.query)[index];
    // Nothing is ranked for this query: keep the typed text rather than run a
    // row the previous query left behind.
    if (!command) return;
    const applied = applyCompletion(visibleText, trigger, "");
    update(applied.value);
    dismissed.current = null;
    setMenu(null);
    command?.run();
    // A command that opened a dialog or sheet now owns focus; pulling the
    // caret back into the textarea would fight it.
    if (!command?.opensDialogOrSheet) focusCaret(applied.caret);
  };

  const insertMention = () => {
    const element = ref.current;
    if (!element || !writable) return;
    const start = element.selectionStart ?? text.length;
    const end = element.selectionEnd ?? start;
    const next = insertMentionToken(text, start, end);
    update(next.value);
    dismissed.current = null;
    // Run the same trigger detection the keyboard uses, so the menu opens for
    // exactly the token the next keystroke will keep filtering.
    syncMenu(next.value, next.caret);
    focusCaret(next.caret);
  };

  const input = (
    <textarea
      ref={ref}
      value={text}
      rows={1}
      aria-label={replyTo}
      aria-autocomplete="list"
      aria-controls={menu ? menuListId : undefined}
      aria-activedescendant={
        menu && rows.length > 0 ? `${menuListId}-option-${activeIndex}` : undefined
      }
      placeholder={disabledReason ?? replyTo}
      disabled={!writable}
      onChange={(event) => {
        const next = event.target.value;
        update(next);
        if (!composing.current) syncMenu(next, event.target.selectionStart);
      }}
      onSelect={(event) => {
        if (!composing.current) {
          syncMenu(event.currentTarget.value, event.currentTarget.selectionStart);
        }
      }}
      onCompositionStart={() => {
        composing.current = true;
        setMenu(null);
      }}
      onCompositionEnd={(event) => {
        composing.current = false;
        syncMenu(event.currentTarget.value, event.currentTarget.selectionStart);
      }}
      onInput={autosize}
      onPaste={(event) => {
        if (!writable) return;
        const files = Array.from(event.clipboardData.files);
        if (files.length > 0) {
          event.preventDefault();
          attachments.addFiles(files);
          return;
        }
        const pasted = event.clipboardData.getData("text/plain");
        if (attachments.addPastedText(pasted)) event.preventDefault();
      }}
      onKeyDown={(event) => {
        if (event.nativeEvent.isComposing) return;
        const element = event.currentTarget;
        // While the picker is open its arrows and accept keys belong to it,
        // not to history recall or send.
        if (menu) {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            if (rows.length === 0) return;
            const delta = event.key === "ArrowDown" ? 1 : -1;
            setMenu((current) =>
              current
                ? {
                    ...current,
                    activeIndex: stepSelection(current.activeIndex, rows.length, delta),
                  }
                : current,
            );
            return;
          }
          if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey)) {
            event.preventDefault();
            if (rows.length > 0) choose(activeIndex);
            return;
          }
          // Shift+↵ is a newline even with the menu open; it falls through to
          // the textarea's own default because the send key needs no shift.
          if (event.key === "Escape") {
            event.preventDefault();
            closeMenu(true);
            return;
          }
        }
        if (event.key === "Enter") {
          const mod = event.metaKey || event.ctrlKey;
          if (editing) {
            if (!event.shiftKey) {
              event.preventDefault();
              void saveEdit();
            }
            return;
          }
          if (multiline) {
            if (mod) {
              // Cmd/Ctrl+Enter sends with the default action; adding Shift is
              // the opposite action. With only a queued message to act on, let
              // the global "steer the oldest" shortcut have it.
              if (event.shiftKey && !text.trim()) return;
              event.preventDefault();
              void submit(event.shiftKey);
            }
            return;
          }
          if (mod && !event.shiftKey) {
            event.preventDefault();
            void submit(true);
            return;
          }
          if (!mod && !event.shiftKey) {
            event.preventDefault();
            void submit(false);
          }
          return;
        }
        // Alt/Option+Up with the caret at the very start edits the newest
        // queued message; the chord is rebindable in Settings → Shortcuts.
        if (!editing && queued.length > 0) {
          const chord = chordFromEvent(event);
          if (
            chord &&
            matchCommand(chord) === "thread.editQueuedMessage" &&
            (element.selectionStart ?? 0) === 0 &&
            (element.selectionEnd ?? 0) === 0
          ) {
            event.preventDefault();
            const newest = queued[queued.length - 1];
            if (newest) beginEdit(newest);
            return;
          }
        }
        if (
          (event.key !== "ArrowUp" && event.key !== "ArrowDown") ||
          event.metaKey ||
          event.ctrlKey ||
          event.altKey ||
          event.shiftKey
        ) {
          return;
        }
        if (event.key === "ArrowUp") {
          // The first-line rule decides whether recall starts; once history
          // mode is open the caret sits at the top of the recalled text and
          // further ArrowUps keep walking back even through several lines.
          if (
            historyIndex.current < 0 &&
            !caretOnFirstLine(element.value, element.selectionStart ?? 0, element.selectionEnd ?? 0)
          ) {
            return;
          }
          if (recall("backward")) event.preventDefault();
          return;
        }
        if (historyIndex.current >= 0 && recall("forward")) event.preventDefault();
      }}
    />
  );

  const inputBlock = (
    <div className="q-comp-in">
      {input}
      {!isPhone && menu ? (
        <ComposerMenu
          kind={menu.trigger.kind}
          rows={rows}
          activeIndex={activeIndex}
          anchor={anchor}
          lineHeight={anchor.lineHeight}
          loading={fileIndex.loading}
          error={fileIndex.error}
          workspaceName={workspaceName}
          query={menuQuery}
          isPhone={false}
          indexedCount={fileIndex.files.length}
          truncated={fileIndex.truncated}
          listId={menuListId}
          onSelect={choose}
          onHighlight={(index) =>
            setMenu((current) => (current ? { ...current, activeIndex: index } : current))
          }
          onClose={() => closeMenu(false)}
        />
      ) : null}
    </div>
  );

  const hiddenInput = (
    <input
      ref={attachments.inputRef}
      type="file"
      multiple
      accept={FILE_ACCEPT}
      aria-label="Attach files"
      className="sr-only"
      tabIndex={-1}
      onChange={(event) => attachments.onInputChange(event.target.files)}
    />
  );

  const sendButton = (
    <button
      type="button"
      className={cn("q-send", canSend && "ready")}
      disabled={!canSend}
      onClick={() => void (editing ? saveEdit() : submit())}
      title={
        editing
          ? "Save the queued message"
          : (attachments.blockedReason ??
            disabledReason ??
            (defaultAction === "steer" && running && canSteer
              ? "Steer into the running turn (Enter)"
              : running
                ? "Queue for the next turn (Enter)"
                : "Send (Enter)"))
      }
      aria-label={editing ? "Save queued message" : "Send reply"}
      data-action={editing ? "save" : defaultAction}
    >
      {editing ? (
        <CheckIcon className="q-icon" strokeWidth={2} aria-hidden />
      ) : defaultAction === "steer" && running && canSteer ? (
        <CornerDownRightIcon className="q-icon" strokeWidth={2} aria-hidden />
      ) : running ? (
        <ListEndIcon className="q-icon" strokeWidth={2} aria-hidden />
      ) : (
        <ArrowUpIcon className="q-icon" strokeWidth={2} aria-hidden />
      )}
    </button>
  );

  const editBar = editing ? (
    <div className="q-edit-bar">
      <span className="t">
        Editing a queued message
        {running && !canSteer ? " · this agent cannot take it mid-turn" : ""}
      </span>
      <span className="grow" />
      <button type="button" className="q-btn ghost" onClick={cancelEdit}>
        <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        Cancel
      </button>
      <button type="button" className="q-btn ghost" onClick={() => void saveEdit()}>
        <CheckIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        Save
      </button>
    </div>
  ) : null;

  if (isPhone) {
    return (
      <fieldset
        aria-label="Message composer"
        className="q-ph-composer"
        style={{ margin: 0, minWidth: 0, position: "relative" }}
        onBlur={(event) => {
          if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
          setMenu(null);
        }}
      >
        <AttachmentTray
          tasks={attachments.tasks}
          onRemove={attachments.remove}
          onRetry={attachments.retry}
        />
        <div className="q-ph-comp">
          <button
            type="button"
            disabled={!writable}
            onClick={attachments.openPicker}
            aria-label="Attach files"
            title={disabledReason ?? "Attach files"}
            className="q-ibtn"
            style={{ width: 40, height: 40, flex: "none" }}
          >
            <PlusIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
          </button>
          <div className="in">
            {editBar}
            {inputBlock}
            {canSend ? sendButton : null}
          </div>
        </div>
        {menu ? (
          <ComposerMenu
            kind={menu.trigger.kind}
            rows={rows}
            activeIndex={activeIndex}
            anchor={anchor}
            lineHeight={anchor.lineHeight}
            loading={fileIndex.loading}
            error={fileIndex.error}
            workspaceName={workspaceName}
            query={menuQuery}
            isPhone
            indexedCount={fileIndex.files.length}
            truncated={fileIndex.truncated}
            listId={menuListId}
            onSelect={choose}
            onHighlight={(index) =>
              setMenu((current) => (current ? { ...current, activeIndex: index } : current))
            }
            onClose={() => closeMenu(true)}
          />
        ) : null}
        {hiddenInput}
      </fieldset>
    );
  }

  return (
    <>
      <fieldset
        aria-label="Message composer"
        className="q-composer"
        style={{ margin: 0, minWidth: 0, position: "relative" }}
        onBlur={(event) => {
          if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
          setMenu(null);
        }}
        onDragEnter={(event) => {
          if (!writable || !event.dataTransfer.types.includes("Files")) return;
          event.preventDefault();
          setDragging(true);
        }}
        onDragOver={(event) => {
          if (!writable) return;
          event.preventDefault();
          event.dataTransfer.dropEffect = "copy";
        }}
        onDragLeave={(event) => {
          if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
          setDragging(false);
        }}
        onDrop={(event) => {
          event.preventDefault();
          setDragging(false);
          if (!writable) return;
          const files = Array.from(event.dataTransfer.files);
          if (files.length > 0) attachments.addFiles(files);
        }}
      >
        <AttachmentTray
          tasks={attachments.tasks}
          onRemove={attachments.remove}
          onRetry={attachments.retry}
        />

        {editBar}

        {inputBlock}

        {dragging ? (
          <div
            style={{
              position: "absolute",
              inset: 0,
              zIndex: 10,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              borderRadius: 12,
              background: "var(--bg-raised)",
            }}
          >
            <p className="q-badge you" style={{ height: 26, padding: "0 10px" }}>
              Drop files to attach
            </p>
          </div>
        ) : null}

        {hiddenInput}

        <div className="q-comp-f">
          <button
            type="button"
            disabled={!writable}
            onClick={attachments.openPicker}
            aria-label="Attach files"
            title={disabledReason ?? "Attach files (Ctrl/Cmd U)"}
            className="q-ibtn"
          >
            <PaperclipIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
          <button
            type="button"
            disabled={!writable}
            onClick={insertMention}
            aria-label="Mention a file"
            title="Mention a file"
            className="q-ibtn"
          >
            <AtSignIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
          <span className="grow" />

          <ThreadControls summary={summary} />

          {sendButton}
        </div>
      </fieldset>

      {attachments.attachments.length > 0 ? (
        <div className="q-usage-foot">
          <span className="grow" />
          <span className="q-num">{attachments.attachments.length} attached</span>
        </div>
      ) : null}
    </>
  );
}
