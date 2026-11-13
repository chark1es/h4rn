/**
 * Binary frame codec for attachment uploads and downloads (ADR-007/008).
 *
 * Every binary WebSocket message starts with the protocol's fixed 16-byte
 * little-endian header. Keeping the codec here means `H4rnClient` only moves
 * bytes and the attachments feature owns the meaning of a stream.
 */

export const BINARY_HEADER_LEN = 16;
export const BINARY_MAGIC = [0x48, 0x34] as const; // "H4"

/** Binary kind discriminants from `h4rn-protocol::binary`. */
export const BinaryKind = {
  PtyOutput: 1,
  PtySnapshot: 2,
  PtyInput: 3,
  BlobChunk: 4,
  Ack: 5,
  Resize: 6,
} as const;

export type DecodedFrame = {
  kind: number;
  streamId: number;
  seq: number;
  payload: Uint8Array;
};

/** Encodes one frame. `payload` is copied into the returned buffer. */
export function encodeFrame(
  kind: number,
  streamId: number,
  seq: number,
  payload: Uint8Array = new Uint8Array(0),
): Uint8Array {
  const bytes = new Uint8Array(BINARY_HEADER_LEN + payload.length);
  bytes[0] = BINARY_MAGIC[0];
  bytes[1] = BINARY_MAGIC[1];
  bytes[2] = 1; // framing version
  bytes[3] = kind;
  const view = new DataView(bytes.buffer);
  view.setUint32(4, streamId, true);
  view.setBigUint64(8, BigInt(seq), true);
  bytes.set(payload, BINARY_HEADER_LEN);
  return bytes;
}

/** Decodes one WebSocket binary message, or returns null if it is not ours. */
export function decodeFrame(data: ArrayBuffer | Uint8Array): DecodedFrame | null {
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  if (bytes.length < BINARY_HEADER_LEN) return null;
  if (bytes[0] !== BINARY_MAGIC[0] || bytes[1] !== BINARY_MAGIC[1]) return null;
  if (bytes[2] !== 1) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    kind: bytes[3] ?? 0,
    streamId: view.getUint32(4, true),
    seq: Number(view.getBigUint64(8, true)),
    payload: bytes.subarray(BINARY_HEADER_LEN),
  };
}

type FrameListener = (frame: DecodedFrame) => void;

/**
 * Routes decoded binary frames to the feature that opened a stream.
 *
 * Binary replies can arrive before the `await` that learns the stream id
 * finishes, so frames for a stream with no listener are buffered and drained
 * by the first `listen`. The per-stream cap keeps a misbehaving daemon from
 * growing memory without bound.
 */
class BinaryRouter {
  private readonly listeners = new Map<number, Set<FrameListener>>();
  private readonly buffered = new Map<number, DecodedFrame[]>();
  private detach: (() => void) | null = null;

  attach(client: { onBinary: (listener: (data: ArrayBuffer) => void) => () => void }): void {
    if (this.detach) return;
    this.detach = client.onBinary((data) => this.push(data));
  }

  push(data: ArrayBuffer | Uint8Array): void {
    const frame = decodeFrame(data);
    if (!frame) return;
    const listeners = this.listeners.get(frame.streamId);
    if (!listeners || listeners.size === 0) {
      const queue = this.buffered.get(frame.streamId) ?? [];
      queue.push(frame);
      if (queue.length > 1024) queue.splice(0, queue.length - 1024);
      this.buffered.set(frame.streamId, queue);
      return;
    }
    this.dispatch(frame, listeners);
  }

  listen(streamId: number, listener: FrameListener): () => void {
    const listeners = this.listeners.get(streamId) ?? new Set<FrameListener>();
    listeners.add(listener);
    this.listeners.set(streamId, listeners);

    const queued = this.buffered.get(streamId);
    if (queued) {
      this.buffered.delete(streamId);
      for (const frame of queued) this.dispatch(frame, listeners);
    }

    return () => {
      const current = this.listeners.get(streamId);
      if (!current) return;
      current.delete(listener);
      if (current.size === 0) this.listeners.delete(streamId);
    };
  }

  private dispatch(frame: DecodedFrame, listeners: Set<FrameListener>): void {
    for (const listener of [...listeners]) listener(frame);
  }
}

export const binaryRouter = new BinaryRouter();
