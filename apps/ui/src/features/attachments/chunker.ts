import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex } from "@noble/hashes/utils.js";

/** Largest attachment the daemon accepts by default (50 MiB). */
export const MAX_ATTACHMENT_BYTES = 50 * 1024 * 1024;

/** A pasted block larger than this becomes a text-file attachment. */
export const TEXT_ATTACHMENT_THRESHOLD = 2_000;

/** Upper bound on one message's attachment tray. */
export const MAX_ATTACHMENTS = 10;

/** Splits bytes into frames no larger than `chunkSize`. */
export function chunkBytes(bytes: Uint8Array, chunkSize: number): Uint8Array[] {
  if (!Number.isInteger(chunkSize) || chunkSize <= 0) {
    // i18n-ignore-next-line: the chunk size is a constant, so a bad one is a bug
    throw new Error(`chunk size must be a positive integer, got ${chunkSize}`);
  }
  const chunks: Uint8Array[] = [];
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    chunks.push(bytes.subarray(offset, Math.min(offset + chunkSize, bytes.length)));
  }
  return chunks;
}

/** BLAKE3 hex digest, matching the daemon's content address. */
export function blake3Hex(bytes: Uint8Array): string {
  return bytesToHex(blake3(bytes));
}

/** Human name for a pasted block that is too large for the textarea. */
export function pastedTextName(now = new Date()): string {
  const pad = (value: number) => value.toString().padStart(2, "0");
  return `pasted-text-${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(
    now.getHours(),
  )}${pad(now.getMinutes())}${pad(now.getSeconds())}.txt`;
}
