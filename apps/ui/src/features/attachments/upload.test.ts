import { describe, expect, it } from "vitest";

import type { BlobInfo } from "@/gen";
import { MAX_ATTACHMENT_BYTES } from "./chunker";
import {
  createUploadSnapshot,
  formatBytes,
  reduceUpload,
  toAttachment,
  type UploadSnapshot,
  validateFile,
} from "./upload";

function blob(overrides: Partial<BlobInfo> = {}): BlobInfo {
  return {
    id: "00ff",
    name: "diagram.png",
    mime: "image/png",
    size: 2048,
    created_at: "2026-11-12T00:00:00Z",
    has_thumbnail: false,
    ...overrides,
  };
}

function snapshot(overrides: Partial<UploadSnapshot> = {}): UploadSnapshot {
  return { ...createUploadSnapshot(2048), ...overrides };
}

describe("upload state machine", () => {
  it("moves from hashing to uploading to done", () => {
    let state = createUploadSnapshot(2048);
    expect(state.phase).toBe("queued");

    state = reduceUpload(state, { type: "hash_started" });
    expect(state.phase).toBe("hashing");

    state = reduceUpload(state, { type: "hashed", digest: "abc123" });
    expect(state).toMatchObject({ phase: "queued", digest: "abc123", failure: null });

    state = reduceUpload(state, {
      type: "begun",
      uploadId: "up_1",
      streamId: 72,
      chunkSize: 1024,
      sentBytes: 0,
      nextSeq: 0,
    });
    expect(state).toMatchObject({
      phase: "uploading",
      uploadId: "up_1",
      streamId: 72,
      chunkSize: 1024,
    });

    state = reduceUpload(state, { type: "chunk_acked", sentBytes: 1024, nextSeq: 1 });
    expect(state).toMatchObject({ sentBytes: 1024, nextSeq: 1, phase: "uploading" });

    state = reduceUpload(state, { type: "commit_started" });
    expect(state.phase).toBe("committing");

    const committed = blob();
    state = reduceUpload(state, { type: "committed", blob: committed });
    expect(state).toMatchObject({ phase: "done", blob: committed, failure: null });
  });

  it("keeps progress when a chunk fails so retry can resume", () => {
    const failed = reduceUpload(
      snapshot({
        phase: "uploading",
        uploadId: "up_1",
        streamId: 72,
        chunkSize: 1024,
        sentBytes: 1024,
        nextSeq: 1,
        digest: "abc123",
      }),
      { type: "failed", failure: { message: "Connection dropped.", hint: "Retry." } },
    );

    expect(failed.phase).toBe("failed");
    expect(failed.failure).toEqual({ message: "Connection dropped.", hint: "Retry." });
    expect(failed).toMatchObject({
      uploadId: "up_1",
      chunkSize: 1024,
      sentBytes: 1024,
      nextSeq: 1,
      digest: "abc123",
    });

    const retried = reduceUpload(failed, { type: "retrying" });
    expect(retried).toMatchObject({ phase: "queued", failure: null, uploadId: "up_1" });
    expect(retried.sentBytes).toBe(1024);
  });

  it("resets to the daemon's resume point on a new begun event", () => {
    const state = reduceUpload(
      snapshot({ phase: "failed", uploadId: "up_1", streamId: 72, chunkSize: 1024 }),
      {
        type: "begun",
        uploadId: "up_1",
        streamId: 72,
        chunkSize: 1024,
        sentBytes: 2048,
        nextSeq: 2,
      },
    );

    expect(state).toMatchObject({
      phase: "uploading",
      sentBytes: 2048,
      nextSeq: 2,
      failure: null,
    });
  });

  it("clears the failure when cancelled", () => {
    const state = reduceUpload(
      snapshot({ phase: "failed", failure: { message: "No.", hint: null } }),
      { type: "cancelled" },
    );
    expect(state).toMatchObject({ phase: "cancelled", failure: null });
  });
});

describe("validateFile", () => {
  it("accepts a normal file", () => {
    expect(validateFile(new File(["hello"], "notes.txt", { type: "text/plain" }))).toBeNull();
  });

  it("rejects an empty file with an action", () => {
    const failure = validateFile(new File([], "empty.txt"));
    expect(failure?.message).toMatch(/empty/i);
    expect(failure?.hint).toMatch(/remove/i);
  });

  it("rejects a file over the daemon limit and names both sizes", () => {
    const failure = validateFile(new File([new Uint8Array(MAX_ATTACHMENT_BYTES + 1)], "huge.bin"));
    expect(failure?.message).toContain("huge.bin");
    expect(failure?.message).toContain("50.0 MiB");
    expect(failure?.hint).toMatch(/compress|split/i);
  });

  it("rejects a filename longer than 255 characters", () => {
    const failure = validateFile(new File(["x"], `${"a".repeat(256)}.txt`));
    expect(failure?.message).toMatch(/name is too long/i);
  });
});

describe("toAttachment", () => {
  it("drops daemon-only blob fields and sets an empty local path", () => {
    expect(toAttachment(blob())).toEqual({
      id: "00ff",
      name: "diagram.png",
      mime: "image/png",
      size: 2048,
      local_path: "",
    });
  });
});

describe("formatBytes", () => {
  it("uses bytes, KiB and MiB at their boundaries", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1024)).toBe("1.0 KiB");
    expect(formatBytes(1024 * 1024)).toBe("1.0 MiB");
  });
});
