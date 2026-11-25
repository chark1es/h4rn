import { type RefObject, useCallback, useEffect, useRef, useState } from "react";

import type { Attachment } from "@/gen";
import { getClient } from "@/lib/store/appStore";

import { MAX_ATTACHMENTS, pastedTextName, TEXT_ATTACHMENT_THRESHOLD } from "./chunker";
import { pickWithDesktop } from "./picker";
import {
  AttachmentUploader,
  toAttachment,
  type UploadFailure,
  type UploadSnapshot,
  validateFile,
} from "./upload";

type DraftTask = {
  key: string;
  file: File;
  uploader: AttachmentUploader;
  previewUrl: string | null;
  invalid: UploadFailure | null;
  /**
   * An attachment a rewind returned: already committed on the daemon, so it
   * has no upload to run and is sent by id exactly like an uploaded one.
   */
  restored?: Attachment;
};

export type AttachmentDraft = DraftTask & {
  snapshot: UploadSnapshot;
  failure: UploadFailure | null;
};

export type AttachmentDrafts = {
  tasks: AttachmentDraft[];
  attachments: Attachment[];
  uploading: boolean;
  failed: boolean;
  blockedReason: string | null;
  inputRef: RefObject<HTMLInputElement | null>;
  addFiles: (files: Iterable<File>) => void;
  addPastedText: (text: string) => boolean;
  openPicker: () => void;
  onInputChange: (files: FileList | null) => void;
  remove: (key: string) => void;
  retry: (key: string) => void;
  clear: () => void;
  /** Puts already-committed attachments back in the tray, without re-uploading. */
  restore: (attachments: Attachment[]) => void;
};

/** Drafts survive thread switches and reconnects; uploads keep running. */
const draftsByThread = new Map<string, DraftTask[]>();
let taskCounter = 0;

function isActive(snapshot: UploadSnapshot): boolean {
  return (
    snapshot.phase === "queued" ||
    snapshot.phase === "hashing" ||
    snapshot.phase === "uploading" ||
    snapshot.phase === "committing"
  );
}

/**
 * Attachment tray state for one composer.
 *
 * `enabled` mirrors the composer's writable state: when the host is offline
 * the tray refuses files with the same reason the textarea shows, instead of
 * letting an upload fail later.
 */
export function useAttachmentDrafts(
  threadId: string,
  options: { enabled: boolean; disabledReason: string | null },
): AttachmentDrafts {
  const [, setRevision] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const tasks = draftsByThread.get(threadId) ?? [];
  const refresh = useCallback(() => setRevision((value) => value + 1), []);

  const setTasks = useCallback(
    (next: DraftTask[] | ((current: DraftTask[]) => DraftTask[])) => {
      const current = draftsByThread.get(threadId) ?? [];
      draftsByThread.set(threadId, typeof next === "function" ? next(current) : next);
      refresh();
    },
    [threadId, refresh],
  );

  // Snapshots live on the uploader; a subscription turns them into renders.
  useEffect(() => {
    const offs = tasks.map((task) => task.uploader.subscribe(refresh));
    return () => {
      for (const off of offs) off();
    };
    // `tasks` is the per-thread array identity; snapshot changes call refresh.
  }, [tasks, refresh]);

  const addFiles = useCallback(
    (files: Iterable<File>) => {
      if (!options.enabled) {
        return;
      }
      const additions: DraftTask[] = [];
      for (const file of files) {
        const invalid = validateFile(file);
        const uploader = new AttachmentUploader(getClient(), file);
        taskCounter += 1;
        additions.push({
          key: `att-${taskCounter}`,
          file,
          uploader,
          previewUrl: file.type.startsWith("image/") ? URL.createObjectURL(file) : null,
          invalid,
        });
      }
      if (additions.length === 0) return;

      setTasks((current) => {
        const room = Math.max(0, MAX_ATTACHMENTS - current.length);
        const accepted = additions.slice(0, room);
        for (const task of additions.slice(room)) {
          if (task.previewUrl) URL.revokeObjectURL(task.previewUrl);
        }
        for (const task of accepted) {
          if (!task.invalid) void task.uploader.start();
        }
        return [...current, ...accepted];
      });
    },
    [options.enabled, setTasks],
  );

  const addPastedText = useCallback(
    (text: string): boolean => {
      if (text.length <= TEXT_ATTACHMENT_THRESHOLD) return false;
      const file = new File([text], pastedTextName(), { type: "text/plain" });
      addFiles([file]);
      return true;
    },
    [addFiles],
  );

  const openPicker = useCallback(() => {
    if (!options.enabled) return;
    void pickWithDesktop()
      .then((files) => {
        if (files === null) inputRef.current?.click();
        else if (files.length > 0) addFiles(files);
      })
      .catch(() => {
        // A native dialog failure should not strand the composer; the input is
        // the dependable path in every webview.
        inputRef.current?.click();
      });
  }, [addFiles, options.enabled]);

  const onInputChange = useCallback(
    (files: FileList | null) => {
      if (files && files.length > 0) addFiles(Array.from(files));
      if (inputRef.current) inputRef.current.value = "";
    },
    [addFiles],
  );

  const remove = useCallback(
    (key: string) => {
      setTasks((current) => {
        const task = current.find((candidate) => candidate.key === key);
        task?.uploader.cancel();
        if (task?.previewUrl) URL.revokeObjectURL(task.previewUrl);
        return current.filter((candidate) => candidate.key !== key);
      });
    },
    [setTasks],
  );

  const retry = useCallback(
    (key: string) => {
      const task = draftsByThread.get(threadId)?.find((candidate) => candidate.key === key);
      if (!task) return;
      if (task.invalid) return;
      void task.uploader.retry();
      refresh();
    },
    [threadId, refresh],
  );

  const restore = useCallback(
    (attachments: Attachment[]) => {
      if (attachments.length === 0) return;
      setTasks((current) => {
        const known = new Set(
          current.flatMap((task) => {
            if (task.restored) return [task.restored.id];
            const blob = task.uploader.getSnapshot().blob;
            return blob ? [blob.id] : [];
          }),
        );
        const additions: DraftTask[] = [];
        for (const attachment of attachments) {
          if (known.has(attachment.id)) continue;
          known.add(attachment.id);
          taskCounter += 1;
          const file = new File([], attachment.name, { type: attachment.mime });
          additions.push({
            key: `restored-${taskCounter}`,
            file,
            uploader: new AttachmentUploader(getClient(), file),
            previewUrl: null,
            invalid: null,
            restored: attachment,
          });
        }
        if (additions.length === 0) return current;
        // A rewind must never silently drop part of the prompt's attachments.
        // Put every one back, even past the send limit: the person resolves
        // the limit before sending, and the composer says how many to remove.
        return [...current, ...additions];
      });
    },
    [setTasks],
  );

  const clear = useCallback(() => {
    setTasks((current) => {
      for (const task of current) {
        if (task.previewUrl) URL.revokeObjectURL(task.previewUrl);
      }
      return [];
    });
  }, [setTasks]);

  const drafts: AttachmentDraft[] = tasks.map((task) => {
    const snapshot = task.uploader.getSnapshot();
    if (task.restored) {
      return {
        ...task,
        snapshot: {
          ...snapshot,
          phase: "done",
          totalBytes: task.restored.size,
          sentBytes: task.restored.size,
          blob: {
            id: task.restored.id,
            name: task.restored.name,
            mime: task.restored.mime,
            size: task.restored.size,
            created_at: new Date().toISOString(),
            has_thumbnail: false,
          },
          failure: null,
        },
        failure: null,
      };
    }
    return {
      ...task,
      snapshot,
      failure: task.invalid ?? snapshot.failure,
    };
  });

  const uploading = drafts.some((task) => !task.invalid && isActive(task.snapshot));
  const failed = drafts.some(
    (task) => Boolean(task.failure) && (task.invalid || task.snapshot.phase === "failed"),
  );
  const attachments: Attachment[] = [];
  for (const task of drafts) {
    if (task.snapshot.phase === "done" && task.snapshot.blob) {
      attachments.push(toAttachment(task.snapshot.blob));
    }
  }

  const blockedReason = !options.enabled
    ? options.disabledReason
    : uploading
      ? "Wait for the attachments to finish uploading."
      : failed
        ? "Retry or remove the failed attachment."
        : drafts.length > MAX_ATTACHMENTS
          ? `A message can carry ${MAX_ATTACHMENTS} attachments. Remove ${
              drafts.length - MAX_ATTACHMENTS
            } before sending.`
          : null;

  return {
    tasks: drafts,
    attachments,
    uploading,
    failed,
    blockedReason,
    inputRef,
    addFiles,
    addPastedText,
    openPicker,
    onInputChange,
    remove,
    retry,
    clear,
    restore,
  };
}

/**
 * Opens this composer's file picker on the attach shortcut. Only the
 * composer that owns the screen listens: the New thread dialog while it is
 * open, otherwise the thread or start-a-thread composer underneath.
 */
export function useAttachShortcut(openPicker: () => void, active: boolean): void {
  useEffect(() => {
    if (!active) return;
    const handler = () => openPicker();
    window.addEventListener("h4rn:attach", handler);
    return () => window.removeEventListener("h4rn:attach", handler);
  }, [openPicker, active]);
}
