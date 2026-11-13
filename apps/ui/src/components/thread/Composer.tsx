import { ArrowUpIcon, AtSignIcon, ChevronDownIcon, PaperclipIcon, PlusIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";

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
import { useAttachmentDrafts } from "@/features/attachments/use-attachments";
import type { ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessLabel } from "@/lib/entities";
import { sendMessage, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const drafts = new Map<string, string>();

export function Composer({ summary }: { summary: ThreadSummary }) {
  const focusNonce = useApp((state) => state.ui.focusComposerNonce);
  const connection = useApp((state) => state.connection.status);
  const hostOnline = useApp((state) => state.hosts[summary.host]?.online ?? false);
  const hostName = useApp((state) => state.hosts[summary.host]?.name ?? summary.host);
  const [text, setText] = useState(() => drafts.get(summary.thread_id) ?? "");
  const [sending, setSending] = useState(false);
  const [dragging, setDragging] = useState(false);
  const ref = useRef<HTMLTextAreaElement | null>(null);

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
    setText(drafts.get(summary.thread_id) ?? "");
    requestAnimationFrame(() => {
      const element = ref.current;
      if (!element) return;
      element.style.height = "auto";
      element.style.height = `${Math.min(element.scrollHeight, 132)}px`;
    });
  }, [summary.thread_id]);

  useEffect(() => {
    if (focusNonce > 0) ref.current?.focus();
  }, [focusNonce]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "u") {
        event.preventDefault();
        attachments.openPicker();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [attachments.openPicker]);

  const autosize = () => {
    const element = ref.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, 132)}px`;
  };

  const update = (value: string) => {
    setText(value);
    drafts.set(summary.thread_id, value);
  };

  const insertMention = () => {
    const element = ref.current;
    if (!element || !writable) return;
    const start = element.selectionStart ?? text.length;
    const next = `${text.slice(0, start)}@${text.slice(start)}`;
    update(next);
    requestAnimationFrame(() => {
      element.focus();
      element.setSelectionRange(start + 1, start + 1);
      autosize();
    });
  };

  const submit = async () => {
    if (!writable || sending || attachments.blockedReason) return;
    if (!text.trim() && attachments.attachments.length === 0) return;
    const value = text;
    setSending(true);
    const ok = await sendMessage(summary.thread_id, value, attachments.attachments);
    setSending(false);
    if (ok) {
      drafts.delete(summary.thread_id);
      setText("");
      attachments.clear();
      requestAnimationFrame(autosize);
    }
  };

  const canSend =
    writable &&
    !sending &&
    !attachments.blockedReason &&
    (text.trim().length > 0 || attachments.attachments.length > 0);

  const isPhone = useIsPhone();
  const replyTo = isPhone
    ? `Reply to ${harnessLabel(summary.harness)}`
    : `Reply to ${harnessLabel(summary.harness)} on ${summary.host}`;

  const input = (
    <textarea
      ref={ref}
      value={text}
      rows={1}
      aria-label={replyTo}
      placeholder={disabledReason ?? replyTo}
      disabled={!writable}
      onChange={(event) => update(event.target.value)}
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
        if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
          event.preventDefault();
          void submit();
        }
      }}
    />
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
        style={{ margin: 0, minWidth: 0 }}
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
            {input}
            {canSend ? sendButton : null}
          </div>
        </div>
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

        {input}

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

          <DropdownMenu>
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
