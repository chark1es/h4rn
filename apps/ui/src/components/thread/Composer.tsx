import { CpuIcon, PaperclipIcon, SendIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { ENTER, Kbd, SHIFT } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
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
import { harnessLabel } from "@/lib/entities";
import { sendMessage, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const drafts = new Map<string, string>();

export function Composer({ summary }: { summary: ThreadSummary }) {
  const focusNonce = useApp((state) => state.ui.focusComposerNonce);
  const connection = useApp((state) => state.connection.status);
  const hostOnline = useApp((state) => state.hosts[summary.host]?.online ?? false);
  const [text, setText] = useState(() => drafts.get(summary.thread_id) ?? "");
  const [sending, setSending] = useState(false);
  const [dragging, setDragging] = useState(false);
  const ref = useRef<HTMLTextAreaElement | null>(null);

  const online = connection === "online";
  const writable = online && hostOnline;
  const disabledReason = !online
    ? "Reconnect to reply. The draft stays here."
    : !hostOnline
      ? `${summary.host} is unreachable. The draft stays here.`
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

  return (
    <div className="shrink-0 border-t border-line bg-paper px-3 pt-2 pb-2 sm:px-6">
      <div className="mx-auto w-full max-w-[760px]">
        <fieldset
          aria-label="Message composer"
          className={cn(
            "relative m-0 min-w-0 rounded-md border border-control-border bg-raised p-0 transition-[border-color,box-shadow] duration-[var(--dur-fast)] focus-within:border-cue focus-within:ring-2 focus-within:ring-cue/30",
            disabledReason && "bg-panel",
            dragging && "border-cue ring-2 ring-cue/30",
          )}
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

          <textarea
            ref={ref}
            value={text}
            rows={1}
            aria-label={`Reply to ${harnessLabel(summary.harness)} on ${summary.host}`}
            placeholder={
              disabledReason ?? `Reply to ${harnessLabel(summary.harness)} on ${summary.host}…`
            }
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
            className="block max-h-[132px] min-h-[38px] w-full resize-none bg-transparent px-3 py-2 text-compact leading-relaxed text-ink outline-none placeholder:text-ink-faint disabled:cursor-not-allowed"
          />

          {dragging ? (
            <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center rounded-md bg-raised/90">
              <p className="rounded-sm border border-dashed border-cue bg-cue-tint/60 px-3 py-2 text-micro font-medium text-cue-ink">
                Drop files to attach
              </p>
            </div>
          ) : null}

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

          <div className="flex items-center gap-1.5 border-t border-line/70 px-2 py-1.5">
            <span className="hidden text-micro text-ink-muted sm:inline">
              Reply to {harnessLabel(summary.harness)} on {summary.host}
            </span>

            <div className="ml-auto flex items-center gap-1">
              <button
                type="button"
                disabled={!writable}
                onClick={attachments.openPicker}
                aria-label="Attach files"
                title={disabledReason ?? "Attach files (Ctrl/Cmd U)"}
                className="flex size-10 items-center justify-center rounded-sm text-ink-faint transition-colors duration-[var(--dur-instant)] hover:bg-panel hover:text-ink focus-visible:ring-2 focus-visible:ring-cue/40 focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-50 sm:size-7"
              >
                <PaperclipIcon className="size-3.5" aria-hidden />
              </button>

              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    className="flex h-7 max-w-[210px] items-center gap-1.5 rounded-sm border border-line px-1.5 font-mono text-micro text-ink-muted transition-colors duration-[var(--dur-instant)] hover:bg-panel hover:text-ink"
                    aria-label="Harness and model"
                  >
                    <CpuIcon className="size-3 shrink-0" aria-hidden />
                    <span className="truncate">{summary.model}</span>
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
                  <p className="px-2 py-1.5 text-micro leading-relaxed text-ink-faint">
                    Harness and model are fixed for a thread. Start a new thread to change them.
                  </p>
                </DropdownMenuContent>
              </DropdownMenu>

              <Button
                size="sm"
                className="h-7 px-2"
                disabled={
                  !writable ||
                  sending ||
                  Boolean(attachments.blockedReason) ||
                  (!text.trim() && attachments.attachments.length === 0)
                }
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
                <SendIcon aria-hidden />
                <span className="hidden sm:inline">Send</span>
              </Button>
            </div>
          </div>
        </fieldset>

        <div className="mt-1 flex items-center gap-2 text-micro text-ink-faint">
          {disabledReason ? (
            <span className="text-strike-ink/80">{disabledReason}</span>
          ) : attachments.blockedReason ? (
            <span className="text-strike-ink/80">{attachments.blockedReason}</span>
          ) : (
            <span className="hidden sm:inline">
              <Kbd>{ENTER}</Kbd> send ·{" "}
              <Kbd>
                {SHIFT} {ENTER}
              </Kbd>{" "}
              newline · <Kbd>R</Kbd> focus
            </span>
          )}
          {attachments.attachments.length > 0 ? (
            <span className="font-mono tabular">{attachments.attachments.length} attached</span>
          ) : null}
          <span className="ml-auto hidden sm:inline">
            @ files · / commands · paste or drop images
          </span>
        </div>
      </div>
    </div>
  );
}
