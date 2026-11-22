/**
 * Binary frame header shared with `h4rn-protocol` (ADR-007).
 *
 * ```text
 * 0  2  magic    b"H4"
 * 2  1  version  1
 * 3  1  kind     1 pty_output, 2 pty_snapshot, 3 pty_input, ...
 * 4  4  stream   u32 little-endian
 * 8  8  seq      u64 little-endian
 * 16 …  payload
 * ```
 *
 * The UI only needs the low 32 bits of `seq` in practice (a terminal never
 * produces four billion chunks in one run), but it decodes the full value so
 * the ack it sends back matches what the daemon recorded.
 */

export const BINARY_HEADER_LEN = 16;
export const BINARY_MAGIC = [0x48, 0x34] as const;

export const BinaryKind = {
  PtyOutput: 1,
  PtySnapshot: 2,
  PtyInput: 3,
  BlobChunk: 4,
  Ack: 5,
  Resize: 6,
} as const;

export type BinaryKindValue = (typeof BinaryKind)[keyof typeof BinaryKind];

/**
 * Flag in the `seq` field of a `PtyInput` frame.
 *
 * Input frames have no sequence, so `seq` carries flags. Bit 0 marks xterm's
 * own answer to a terminal query; an older daemon ignores `seq` for input and
 * writes the answer as ordinary input, exactly as every viewer used to.
 */
export const PTY_INPUT_REPLY = 1;

/** Shift of the output position in a reply input frame's `seq` flag word. */
export const PTY_INPUT_POSITION_SHIFT = 1;

/**
 * Packs a marked reply's output position into an input frame's `seq` field.
 *
 * `position` is the output chunk whose bytes xterm was parsing when it saw the
 * query; a query split across chunks uses the chunk that contained its end.
 */
export function ptyInputReplySeq(position: number): number {
  return position * 2 ** PTY_INPUT_POSITION_SHIFT + PTY_INPUT_REPLY;
}

export type BinaryFrame = {
  kind: BinaryKindValue;
  streamId: number;
  /** Safe integer; sequences stay below `Number.MAX_SAFE_INTEGER`. */
  seq: number;
  payload: Uint8Array;
};

export function encodeBinaryFrame(frame: BinaryFrame): Uint8Array {
  const bytes = new Uint8Array(BINARY_HEADER_LEN + frame.payload.length);
  bytes[0] = BINARY_MAGIC[0];
  bytes[1] = BINARY_MAGIC[1];
  bytes[2] = 1;
  bytes[3] = frame.kind;
  new DataView(bytes.buffer).setUint32(4, frame.streamId, true);
  const view = new DataView(bytes.buffer);
  view.setUint32(8, frame.seq >>> 0, true);
  view.setUint32(12, Math.floor(frame.seq / 0x1_0000_0000), true);
  bytes.set(frame.payload, BINARY_HEADER_LEN);
  return bytes;
}

export function decodeBinaryFrame(input: ArrayBuffer | Uint8Array): BinaryFrame | null {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (bytes.length < BINARY_HEADER_LEN) return null;
  if (bytes[0] !== BINARY_MAGIC[0] || bytes[1] !== BINARY_MAGIC[1]) return null;
  if (bytes[2] !== 1) return null;
  const kind = bytes[3];
  if (kind === undefined || kind < 1 || kind > 6) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const streamId = view.getUint32(4, true);
  const low = view.getUint32(8, true);
  const high = view.getUint32(12, true);
  const seq = high * 0x1_0000_0000 + low;
  return {
    kind: kind as BinaryKindValue,
    streamId,
    seq,
    payload: bytes.subarray(BINARY_HEADER_LEN),
  };
}

export type IncomingFrame =
  | { kind: "output"; seq: number; data: Uint8Array }
  | { kind: "snapshot"; seq: number; ansi: string }
  | { kind: "unknown" };

/**
 * Classifies a binary frame for one terminal stream.
 *
 * Frames for other streams are ignored: one socket may carry several
 * terminals, each with its own sequence space.
 */
export function classifyFrame(frame: BinaryFrame, streamId: number): IncomingFrame {
  if (frame.streamId !== streamId) return { kind: "unknown" };
  if (frame.kind === BinaryKind.PtyOutput) {
    return { kind: "output", seq: frame.seq, data: frame.payload };
  }
  if (frame.kind === BinaryKind.PtySnapshot) {
    return {
      kind: "snapshot",
      seq: frame.seq,
      ansi: new TextDecoder().decode(frame.payload),
    };
  }
  return { kind: "unknown" };
}

/**
 * Tracks the highest contiguous chunk the terminal has finished writing and
 * returns the sequence to acknowledge. Chunks must be applied in order; a gap
 * waits for the missing chunk rather than acknowledging past it.
 */
export class AckTracker {
  private lastApplied: number;
  private ackPending = false;

  constructor(startSeq: number) {
    this.lastApplied = startSeq;
  }

  /** Records a chunk that xterm finished writing, in stream order. */
  applied(seq: number): void {
    if (seq <= this.lastApplied) return;
    this.lastApplied = seq;
    this.ackPending = true;
  }

  /** Replaces the applied position after a resync snapshot. */
  reset(seq: number): void {
    this.lastApplied = Math.max(this.lastApplied, seq);
    this.ackPending = true;
  }

  /** Returns an ack value only when it advanced since the previous call. */
  takeAck(): number | null {
    if (!this.ackPending) return null;
    this.ackPending = false;
    return this.lastApplied;
  }

  get position(): number {
    return this.lastApplied;
  }
}
