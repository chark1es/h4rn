import { describe, expect, it } from "vitest";

import {
  AckTracker,
  BINARY_HEADER_LEN,
  BinaryKind,
  classifyFrame,
  decodeBinaryFrame,
  encodeBinaryFrame,
  ptyInputReplySeq,
} from "./binary";

const payload = new TextEncoder().encode("hello\r\n");

describe("binary frames", () => {
  it("pins the reply flag word to a literal golden vector", () => {
    // The Rust constant `pty_input_reply_seq(1, 1)` must produce the same
    // number: position 1 is shifted one bit and ordinal 1 starts at bit 33.
    expect(ptyInputReplySeq(1, 1)).toBe(2 ** 33 + 3);
  });

  it("round-trips a frame through the shared header", () => {
    const bytes = encodeBinaryFrame({
      kind: BinaryKind.PtyOutput,
      streamId: 7,
      seq: 42,
      payload,
    });
    expect(bytes.length).toBe(BINARY_HEADER_LEN + payload.length);
    expect([...bytes.subarray(0, 4)]).toEqual([0x48, 0x34, 1, BinaryKind.PtyOutput]);

    const decoded = decodeBinaryFrame(bytes);
    expect(decoded).not.toBeNull();
    expect(decoded?.kind).toBe(BinaryKind.PtyOutput);
    expect(decoded?.streamId).toBe(7);
    expect(decoded?.seq).toBe(42);
    expect([...(decoded?.payload ?? [])]).toEqual([...payload]);
  });

  it("preserves chunk sequences above 32 bits", () => {
    const seq = 0x1_0000_0005;
    const decoded = decodeBinaryFrame(
      encodeBinaryFrame({ kind: BinaryKind.PtyOutput, streamId: 1, seq, payload }),
    );
    expect(decoded?.seq).toBe(seq);
  });

  it("decodes an ArrayBuffer not owned from byte zero", () => {
    const frame = encodeBinaryFrame({
      kind: BinaryKind.PtySnapshot,
      streamId: 2,
      seq: 3,
      payload,
    });
    const sibling = new Uint8Array(frame.length + 4);
    sibling.set(frame, 4);
    const decoded = decodeBinaryFrame(sibling.subarray(4));
    expect(decoded?.kind).toBe(BinaryKind.PtySnapshot);
    expect(decoded?.streamId).toBe(2);
  });

  it("rejects bad magic, version, kind and short buffers", () => {
    const good = encodeBinaryFrame({
      kind: BinaryKind.PtyOutput,
      streamId: 1,
      seq: 1,
      payload,
    });
    expect(decodeBinaryFrame(good.subarray(0, 8))).toBeNull();

    const magic = good.slice();
    magic[0] = 0;
    expect(decodeBinaryFrame(magic)).toBeNull();

    const version = good.slice();
    version[2] = 2;
    expect(decodeBinaryFrame(version)).toBeNull();

    const kind = good.slice();
    kind[3] = 99;
    expect(decodeBinaryFrame(kind)).toBeNull();
  });
});

describe("classifyFrame", () => {
  it("returns output bytes for the requested stream", () => {
    const frame = decodeBinaryFrame(
      encodeBinaryFrame({ kind: BinaryKind.PtyOutput, streamId: 4, seq: 9, payload }),
    );
    if (!frame) throw new Error("frame did not decode");
    const classified = classifyFrame(frame, 4);
    expect(classified.kind).toBe("output");
    if (classified.kind !== "output") return;
    expect(classified.seq).toBe(9);
    expect(new TextDecoder().decode(classified.data)).toBe("hello\r\n");
  });

  it("decodes snapshots as ANSI text", () => {
    const frame = decodeBinaryFrame(
      encodeBinaryFrame({ kind: BinaryKind.PtySnapshot, streamId: 4, seq: 10, payload }),
    );
    if (!frame) throw new Error("frame did not decode");
    const classified = classifyFrame(frame, 4);
    expect(classified).toEqual({ kind: "snapshot", seq: 10, ansi: "hello\r\n" });
  });

  it("ignores frames for another terminal stream", () => {
    const frame = decodeBinaryFrame(
      encodeBinaryFrame({ kind: BinaryKind.PtyOutput, streamId: 8, seq: 1, payload }),
    );
    if (!frame) throw new Error("frame did not decode");
    expect(classifyFrame(frame, 4)).toEqual({ kind: "unknown" });
  });
});

describe("AckTracker", () => {
  it("acknowledges only chunks that advanced, in order", () => {
    const tracker = new AckTracker(10);
    expect(tracker.takeAck()).toBeNull();

    tracker.applied(9);
    expect(tracker.takeAck()).toBeNull();

    tracker.applied(11);
    expect(tracker.takeAck()).toBe(11);
    expect(tracker.takeAck()).toBeNull();

    tracker.applied(12);
    tracker.applied(13);
    expect(tracker.takeAck()).toBe(13);
  });

  it("resets the position after a resync snapshot", () => {
    const tracker = new AckTracker(5);
    tracker.reset(20);
    expect(tracker.position).toBe(20);
    expect(tracker.takeAck()).toBe(20);

    tracker.applied(18);
    expect(tracker.takeAck()).toBeNull();
    expect(tracker.position).toBe(20);
  });
});
