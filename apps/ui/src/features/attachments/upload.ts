import type { Attachment, BlobInfo } from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { toProtoError } from "@/lib/client/errors";
import { formatNumber, i18n } from "@/lib/i18n";

import { blake3Hex, chunkBytes, MAX_ATTACHMENT_BYTES } from "./chunker";
import { BinaryKind, binaryRouter, encodeFrame } from "./frames";

export type UploadPhase =
  | "queued"
  | "hashing"
  | "uploading"
  | "committing"
  | "done"
  | "failed"
  | "cancelled";

/**
 * Why a file was not attached. A check the client makes keeps a kind and the
 * tray words it in the active language; a failed upload carries the text it
 * failed with (the daemon's own, or the client's, already translated).
 */
export type UploadFailure =
  | { kind: "empty" }
  | { kind: "tooLarge"; size: number; limit: number }
  | { kind: "nameTooLong" }
  | { kind: "error"; message: string; hint: string | null };

export type UploadSnapshot = {
  phase: UploadPhase;
  uploadId: string | null;
  streamId: number | null;
  chunkSize: number;
  nextSeq: number;
  sentBytes: number;
  totalBytes: number;
  digest: string | null;
  blob: BlobInfo | null;
  failure: UploadFailure | null;
};

export type UploadEvent =
  | { type: "hash_started" }
  | { type: "hashed"; digest: string }
  | {
      type: "begun";
      uploadId: string;
      streamId: number;
      chunkSize: number;
      sentBytes: number;
      nextSeq: number;
    }
  | { type: "chunk_acked"; sentBytes: number; nextSeq: number }
  | { type: "commit_started" }
  | { type: "committed"; blob: BlobInfo }
  | { type: "failed"; failure: UploadFailure }
  | { type: "cancelled" }
  | { type: "retrying" };

export function createUploadSnapshot(totalBytes: number): UploadSnapshot {
  return {
    phase: "queued",
    uploadId: null,
    streamId: null,
    chunkSize: 0,
    nextSeq: 0,
    sentBytes: 0,
    totalBytes,
    digest: null,
    blob: null,
    failure: null,
  };
}

/** Pure reducer so the upload lifecycle is testable without a socket. */
export function reduceUpload(state: UploadSnapshot, event: UploadEvent): UploadSnapshot {
  switch (event.type) {
    case "hash_started":
      return { ...state, phase: "hashing", failure: null };
    case "hashed":
      return { ...state, phase: "queued", digest: event.digest, failure: null };
    case "begun":
      return {
        ...state,
        phase: "uploading",
        uploadId: event.uploadId,
        streamId: event.streamId,
        chunkSize: event.chunkSize,
        sentBytes: event.sentBytes,
        nextSeq: event.nextSeq,
        failure: null,
      };
    case "chunk_acked":
      return { ...state, sentBytes: event.sentBytes, nextSeq: event.nextSeq };
    case "commit_started":
      return { ...state, phase: "committing" };
    case "committed":
      return { ...state, phase: "done", blob: event.blob, failure: null };
    case "failed":
      return { ...state, phase: "failed", failure: event.failure };
    case "cancelled":
      return { ...state, phase: "cancelled", failure: null };
    case "retrying":
      return { ...state, phase: "queued", failure: null };
  }
}

/** Converts a committed blob into the message attachment shape. */
export function toAttachment(blob: BlobInfo): Attachment {
  return {
    id: blob.id,
    name: blob.name,
    mime: blob.mime,
    size: blob.size,
    local_path: "",
  };
}

/** Reasons a file never reaches the daemon. */
export function validateFile(file: File): UploadFailure | null {
  if (file.size === 0) return { kind: "empty" };
  if (file.size > MAX_ATTACHMENT_BYTES) {
    return { kind: "tooLarge", size: file.size, limit: MAX_ATTACHMENT_BYTES };
  }
  if (file.name.length > 255) return { kind: "nameTooLong" };
  return null;
}

/** A failure as copy a person can act on, in the active language. */
export function describeUploadFailure(
  failure: UploadFailure,
  name: string,
): { message: string; hint: string | null } {
  switch (failure.kind) {
    case "empty":
      return {
        message: i18n.t("composer:attachments.errors.empty"),
        hint: i18n.t("composer:attachments.errors.emptyHint"),
      };
    case "tooLarge":
      return {
        message: i18n.t("composer:attachments.errors.tooLarge", {
          name,
          size: formatBytes(failure.size),
          limit: formatBytes(failure.limit),
        }),
        hint: i18n.t("composer:attachments.errors.tooLargeHint"),
      };
    case "nameTooLong":
      return {
        message: i18n.t("composer:attachments.errors.nameTooLong"),
        hint: i18n.t("composer:attachments.errors.nameTooLongHint", {
          max: formatNumber(255),
        }),
      };
    case "error":
      return { message: failure.message, hint: failure.hint };
  }
}

/** A size in binary units, with the number in the active language's format. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${formatNumber(bytes)} B`;
  const tenth = { minimumFractionDigits: 1, maximumFractionDigits: 1 };
  if (bytes < 1024 * 1024) return `${formatNumber(bytes / 1024, tenth)} KiB`;
  return `${formatNumber(bytes / (1024 * 1024), tenth)} MiB`;
}

type Listener = () => void;

/**
 * Drives one chunked upload: hash, stream chunks with per-frame acks, commit,
 * and resume from the daemon's byte count after a failure.
 */
export class AttachmentUploader {
  private snapshot: UploadSnapshot;
  private readonly listeners = new Set<Listener>();
  private bytes: Uint8Array | null = null;
  private promise: Promise<BlobInfo | null> | null = null;
  private cancelled = false;
  private readonly ackWaiters = new Map<string, () => void>();

  constructor(
    private readonly client: H4rnClient,
    readonly file: File,
  ) {
    this.snapshot = createUploadSnapshot(file.size);
    binaryRouter.attach(client);
  }

  get name(): string {
    return this.file.name;
  }

  get mime(): string {
    return this.file.type || "application/octet-stream";
  }

  getSnapshot(): UploadSnapshot {
    return this.snapshot;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** Resolves with the committed blob, or null when cancelled/failed. */
  start(): Promise<BlobInfo | null> {
    if (!this.promise) this.promise = this.run();
    return this.promise;
  }

  /** Sends `AbortUpload` and stops reporting progress. */
  cancel(): void {
    if (this.snapshot.phase === "done" || this.snapshot.phase === "cancelled") return;
    this.cancelled = true;
    for (const resolve of this.ackWaiters.values()) resolve();
    this.ackWaiters.clear();
    if (this.snapshot.uploadId) {
      void this.client
        .request({ type: "abort_upload", upload_id: this.snapshot.uploadId })
        .catch(() => undefined);
    }
    this.transition({ type: "cancelled" });
  }

  /** Resumes a failed upload from the bytes the daemon already accepted. */
  retry(): Promise<BlobInfo | null> {
    if (this.snapshot.phase !== "failed") return this.start();
    this.cancelled = false;
    this.promise = null;
    this.transition({ type: "retrying" });
    return this.start();
  }

  private transition(event: UploadEvent): void {
    this.snapshot = reduceUpload(this.snapshot, event);
    for (const listener of this.listeners) listener();
  }

  private async run(): Promise<BlobInfo | null> {
    try {
      if (this.cancelled) {
        this.transition({ type: "cancelled" });
        return null;
      }

      if (!this.snapshot.digest) {
        if (!this.bytes) {
          this.transition({ type: "hash_started" });
          const buffer = await this.file.arrayBuffer();
          this.bytes = new Uint8Array(buffer);
        }
        if (this.cancelled) {
          this.transition({ type: "cancelled" });
          return null;
        }
        if (this.bytes.byteLength !== this.file.size) {
          throw new Error(i18n.t("composer:attachments.errors.changed"));
        }
        this.transition({ type: "hashed", digest: blake3Hex(this.bytes) });
      }

      if (!this.bytes) {
        throw new Error(i18n.t("composer:attachments.errors.released"));
      }
      const digest = this.snapshot.digest;
      if (!digest) throw new Error(i18n.t("composer:attachments.errors.hashMissing"));

      // Resume only when the daemon still knows this upload; a restart drops
      // the session and a fresh `BeginUpload` is the honest recovery.
      if (this.snapshot.uploadId && this.snapshot.chunkSize > 0) {
        try {
          const status = await this.client.request({
            type: "query_upload",
            upload_id: this.snapshot.uploadId,
          });
          if (status.type === "upload_status") {
            const received = status.upload.received_bytes;
            const startSeq = Math.floor(received / this.snapshot.chunkSize);
            if (!this.cancelled) {
              this.transition({
                type: "begun",
                uploadId: status.upload.upload_id,
                streamId: status.upload.stream_id,
                chunkSize: this.snapshot.chunkSize,
                sentBytes: received,
                nextSeq: startSeq,
              });
            }
          }
        } catch {
          this.snapshot = {
            ...createUploadSnapshot(this.file.size),
            digest: this.snapshot.digest,
          };
        }
      }

      if (!this.snapshot.uploadId) {
        const started = await this.client.request({
          type: "begin_upload",
          name: this.file.name || "attachment",
          size: this.file.size,
          mime_hint: this.mime,
        });
        if (started.type !== "upload_started") {
          throw new Error(i18n.t("composer:attachments.errors.noStream"));
        }
        this.transition({
          type: "begun",
          uploadId: started.upload.upload_id,
          streamId: started.upload.stream_id,
          chunkSize: started.upload.chunk_size,
          sentBytes: 0,
          nextSeq: 0,
        });
      }

      const { uploadId, streamId, chunkSize } = this.snapshot;
      if (!uploadId || streamId === null || chunkSize <= 0) {
        throw new Error(i18n.t("composer:attachments.errors.incomplete"));
      }

      const startSeq = this.snapshot.nextSeq;
      const chunks = chunkBytes(this.bytes, chunkSize);
      const off = binaryRouter.listen(streamId, (frame) => {
        if (frame.kind !== BinaryKind.Ack) return;
        const waiter = this.ackWaiters.get(`${frame.streamId}:${frame.seq}`);
        if (waiter) waiter();
      });

      try {
        for (let seq = startSeq; seq < chunks.length; seq += 1) {
          if (this.cancelled) {
            this.transition({ type: "cancelled" });
            return null;
          }
          const payload = chunks[seq];
          if (!payload) continue;
          const wait = this.waitForAck(streamId, seq);
          this.client.sendBinary(encodeFrame(BinaryKind.BlobChunk, streamId, seq, payload));
          await wait;
          if (this.cancelled) {
            this.transition({ type: "cancelled" });
            return null;
          }
          const sentBytes = Math.min(this.file.size, (seq + 1) * chunkSize);
          this.transition({ type: "chunk_acked", sentBytes, nextSeq: seq + 1 });
        }
      } finally {
        off();
      }

      this.transition({ type: "commit_started" });
      const committed = await this.client.request({
        type: "commit_upload",
        upload_id: uploadId,
        blake3: digest,
      });
      if (committed.type !== "upload_committed") {
        throw new Error(i18n.t("composer:attachments.errors.notConfirmed"));
      }
      this.transition({ type: "committed", blob: committed.blob });
      return committed.blob;
    } catch (error) {
      if (this.cancelled || this.snapshot.phase === "cancelled") {
        this.transition({ type: "cancelled" });
        return null;
      }
      const proto = toProtoError(error);
      this.transition({
        type: "failed",
        failure: {
          kind: "error",
          message: proto.message,
          hint: proto.hint,
        },
      });
      return null;
    }
  }

  private waitForAck(streamId: number, seq: number): Promise<void> {
    return new Promise((resolve, reject) => {
      const key = `${streamId}:${seq}`;
      const timer = setTimeout(() => {
        this.ackWaiters.delete(key);
        const percent = Math.min(
          99,
          Math.round((seq * this.snapshot.chunkSize * 100) / Math.max(1, this.file.size)),
        );
        reject(
          new Error(
            i18n.t("composer:attachments.errors.stalled", {
              name: this.file.name,
              percent: formatNumber(percent / 100, { style: "percent" }),
            }),
          ),
        );
      }, 20_000);
      this.ackWaiters.set(key, () => {
        clearTimeout(timer);
        this.ackWaiters.delete(key);
        resolve();
      });
    });
  }
}
