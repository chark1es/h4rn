import { ArrowUpIcon, AtSignIcon, ChevronDownIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import { useDebouncedValue } from "@/features/composer/useDebouncedValue";
import { useWorkspaceFiles } from "@/features/composer/workspaceFiles";
import {
  proxyModelsFor,
  proxyOverrideFor,
  useProxy,
  useProxyCatalog,
} from "@/features/proxy/proxyStore";
import type { ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { composerCommands } from "@/lib/composerCommands";
import { caretOnFirstLine, sentPrompts, stepPromptHistory } from "@/lib/composerHistory";
import {
  applyCompletion,
  type ComposerTrigger,
  detectComposerTrigger,
  fileReference,
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
import { getState, sendMessage, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { ComposerMenu, type ComposerMenuRow } from "./ComposerMenu";

type OpenMenu = { trigger: ComposerTrigger; activeIndex: number };

/** Where the desktop popover sits, relative to the input wrapper. */
function caretAnchor(
  element: HTMLTextAreaElement,
  position: number,
): { left: number; bottom: number } {
  const wrapper = element.parentElement;
  if (!wrapper) return { left: 0, bottom: 0 };
  const width = wrapper.clientWidth || element.clientWidth;
  if (typeof window === "undefined" || !window.getComputedStyle) {
    return { left: 0, bottom: 0 };
  }
  const computed = window.getComputedStyle(element);
  const mirror = document.createElement("div");
  for (const property of [
    "fontFamily",
    "fontSize",
    "fontStyle",
    "fontWeight",
    "letterSpacing",
    "lineHeight",
    "paddingTop",
    "paddingRight",
    "paddingBottom",
    "paddingLeft",
    "borderTopWidth",
    "borderRightWidth",
    "borderBottomWidth",
    "borderLeftWidth",
    "borderStyle",
    "boxSizing",
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
  const above = wrapper.clientHeight - (element.offsetTop + markerTop);
  return {
    left: Math.max(0, Math.min(markerLeft + element.offsetLeft, Math.max(0, width - 240))),
    bottom: Math.max(lineHeight, above + 6),
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
  const [menuOpen, setMenuOpen] = useState(false);
  const [menu, setMenu] = useState<OpenMenu | null>(null);
  const [anchor, setAnchor] = useState({ left: 0, bottom: 0 });
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

  useProxyCatalog();
  useProxy((state) => state.profiles);
  const proxyActive = proxyOverrideFor(summary.harness);
  const proxyModels = proxyModelsFor(summary.harness);

  const terminalOpen = useApp((state) => state.ui.layout.terminalOpen);
  const view = useApp((state) => state.ui.layout.view);
  const commands = useMemo(
    () => composerCommands(summary, { terminalOpen, view }),
    [summary, terminalOpen, view],
  );
  const workspaceTarget = useMemo(
    () => ({ host: summary.host, workspace: summary.workspace_path }),
    [summary.host, summary.workspace_path],
  );
  const fileIndex = useWorkspaceFiles(workspaceTarget, menu?.trigger.kind === "file");
  const menuQuery = menu?.trigger.query ?? "";
  const settledQuery = useDebouncedValue(menuQuery, 120);
  const commandQuery = menu?.trigger.kind === "command" ? menu.trigger.query : "";
  const fileResults = useMemo(
    () => rankFilePaths(fileIndex.files, settledQuery, 50),
    [fileIndex.files, settledQuery],
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
  // highlighted row, so moving with arrows never makes it jump.
  useLayoutEffect(() => {
    const element = ref.current;
    if (!menu || !element || isPhone) return;
    setAnchor(caretAnchor(element, menu.trigger.end));
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
    revision.current = writeThreadDraft(summary.host, summary.thread_id, value);
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

  const submit = async () => {
    if (!writable || sending || attachments.blockedReason) return;
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
    // Store the draft before the send waits so a second tab's edit during the
    // request is not hidden behind this tab's pending write.
    flushDrafts();
    setSending(true);
    const ok = await sendMessage(submittedThread, value, attachments.attachments);
    setSending(false);
    if (!ok) return;
    // Clear only the draft that was sent. If this tab typed again, or another
    // tab saved newer text, that text stays in the box and in storage.
    const cleared = clearThreadDraftIfRevision(submittedHost, submittedThread, submitted);
    // The pane may have switched threads while the send waited. The new
    // thread's box belongs to that thread, so leave its text and files alone.
    const stillShowing =
      shownThread.current.host === submittedHost &&
      shownThread.current.threadId === submittedThread;
    if (!stillShowing) return;
    historyIndex.current = -1;
    historyDraft.current = "";
    if (cleared) {
      revision.current = null;
      setText("");
    }
    attachments.clear();
    requestAnimationFrame(autosize);
  };

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
    if (row.kind === "file") {
      const applied = applyCompletion(text, menu.trigger, fileReference(row.path));
      update(applied.value);
      dismissed.current = null;
      setMenu(null);
      focusCaret(applied.caret);
      return;
    }
    const command = commandResults.find((entry) => entry.id === row.id);
    const applied = applyCompletion(text, menu.trigger, "");
    update(applied.value);
    dismissed.current = null;
    setMenu(null);
    command?.run();
    focusCaret(applied.caret);
  };

  const insertMention = () => {
    const element = ref.current;
    if (!element || !writable) return;
    const start = element.selectionStart ?? text.length;
    const end = element.selectionEnd ?? start;
    const next = `${text.slice(0, start)}@${text.slice(end)}`;
    update(next);
    dismissed.current = null;
    setMenu({ trigger: { kind: "file", query: "", start, end: start + 1 }, activeIndex: 0 });
    requestAnimationFrame(() => {
      element.focus();
      element.setSelectionRange(start + 1, start + 1);
      autosize();
    });
  };

  const input = (
    <textarea
      ref={ref}
      value={text}
      rows={1}
      aria-label={replyTo}
      aria-autocomplete="list"
      aria-controls={menu ? `composer-menu-${summary.thread_id}` : undefined}
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
          if (event.key === "Enter" || event.key === "Tab") {
            event.preventDefault();
            if (rows.length > 0) choose(activeIndex);
            return;
          }
          if (event.key === "Escape") {
            event.preventDefault();
            closeMenu(true);
            return;
          }
        }
        if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
          event.preventDefault();
          void submit();
          return;
        }
        if (
          (event.key !== "ArrowUp" && event.key !== "ArrowDown") ||
          event.nativeEvent.isComposing ||
          menuOpen ||
          event.metaKey ||
          event.ctrlKey ||
          event.altKey ||
          event.shiftKey
        ) {
          return;
        }
        const element = event.currentTarget;
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
          loading={fileIndex.loading}
          error={fileIndex.error}
          workspaceName={workspaceName}
          query={menu.trigger.kind === "file" ? settledQuery : menu.trigger.query}
          isPhone={false}
          listId={`composer-menu-${summary.thread_id}`}
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
      onClick={() => void submit()}
      title={
        attachments.blockedReason ??
        disabledReason ??
        (attachments.attachments.length > 0
          ? `Send with ${attachments.attachments.length} attachment${
              attachments.attachments.length === 1 ? "" : "s"
            } (Enter)`
          : "Send (Enter)")
      }
      aria-label="Send reply"
    >
      <ArrowUpIcon className="q-icon" strokeWidth={2} aria-hidden />
    </button>
  );

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
            loading={fileIndex.loading}
            error={fileIndex.error}
            workspaceName={workspaceName}
            query={menu.trigger.kind === "file" ? settledQuery : menu.trigger.query}
            isPhone
            listId={`composer-menu-${summary.thread_id}`}
            onSelect={choose}
            onHighlight={(index) =>
              setMenu((current) => (current ? { ...current, activeIndex: index } : current))
            }
            onClose={() => closeMenu(false)}
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

          <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="q-model"
                aria-label="Harness and model"
                title={`${harnessLabel(summary.harness)} · ${summary.model}`}
              >
                <span className="truncate" style={{ maxWidth: 210 }}>
                  {summary.model}
                </span>
                <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-60">
              <DropdownMenuLabel>Harness</DropdownMenuLabel>
              <DropdownMenuItem disabled>{harnessLabel(summary.harness)}</DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>Model</DropdownMenuLabel>
              <DropdownMenuItem disabled>
                <span className="font-mono">{summary.model}</span>
              </DropdownMenuItem>
              {proxyActive && proxyModels.length > 0 ? (
                <>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>Proxy models · {proxyActive.profile.name}</DropdownMenuLabel>
                  {proxyModels.slice(0, 8).map((model) => (
                    <DropdownMenuItem key={model.id} disabled>
                      <span className="font-mono">{model.id}</span>
                      <span className="ml-auto text-small text-fg-3">{model.source}</span>
                    </DropdownMenuItem>
                  ))}
                </>
              ) : null}
              <DropdownMenuSeparator />
              <p className="px-2 py-1.5 text-small leading-relaxed text-fg-3">
                Harness and model are fixed for a thread. Start a new thread to change them.
              </p>
            </DropdownMenuContent>
          </DropdownMenu>

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
