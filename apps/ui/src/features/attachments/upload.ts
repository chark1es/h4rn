import type { Attachment, BlobInfo } from "@/gen";
import type { H4rnClient } from "@/lib/client/client";
import { toProtoError } from "@/lib/client/errors";

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

export type UploadFailure = {
  message: string;
  hint: string | null;
};

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

/** Reasons a file never reaches the daemon, with copy a person can act on. */
export function validateFile(file: File): UploadFailure | null {
  if (file.size === 0) {
    return { message: "This file is empty.", hint: "Pick a file with content, or remove it." };
  }
  if (file.size > MAX_ATTACHMENT_BYTES) {
    return {
      message: `${file.name} is ${formatBytes(file.size)}; the limit is ${formatBytes(
        MAX_ATTACHMENT_BYTES,
      )}.`,
      hint: "Compress it or split it into smaller files.",
    };
  }
  if (file.name.length > 255) {
    return {
      message: "The file name is too long.",
      hint: "Rename it to 255 characters or fewer, then attach it again.",
    };
  }
  return null;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
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
          throw new Error("The file changed while it was being attached.");
        }
        this.transition({ type: "hashed", digest: blake3Hex(this.bytes) });
      }

      if (!this.bytes) {
        throw new Error("The attachment bytes were released before the upload finished.");
      }
      const digest = this.snapshot.digest;
      if (!digest) throw new Error("The attachment hash is missing.");

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
          throw new Error("The daemon did not open an upload stream.");
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
        throw new Error("The upload stream is incomplete.");
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
        throw new Error("The daemon did not confirm the upload.");
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
        reject(new Error(`The daemon stopped acknowledging ${this.file.name} at ${percent}%.`));
      }, 20_000);
      this.ackWaiters.set(key, () => {
        clearTimeout(timer);
        this.ackWaiters.delete(key);
        resolve();
      });
    });
  }
}
