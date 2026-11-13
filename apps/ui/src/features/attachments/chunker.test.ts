import { describe, expect, it } from "vitest";

import {
  blake3Hex,
  chunkBytes,
  MAX_ATTACHMENT_BYTES,
  pastedTextName,
  TEXT_ATTACHMENT_THRESHOLD,
} from "./chunker";

describe("chunkBytes", () => {
  it("splits an exact multiple into equal chunks", () => {
    const chunks = chunkBytes(new Uint8Array(12), 4);
    expect(chunks).toHaveLength(3);
    expect(chunks.map((chunk) => chunk.byteLength)).toEqual([4, 4, 4]);
  });

  it("keeps a short final chunk", () => {
    const chunks = chunkBytes(new Uint8Array(10), 4);
    expect(chunks.map((chunk) => chunk.byteLength)).toEqual([4, 4, 2]);
  });

  it("returns nothing for empty bytes", () => {
    expect(chunkBytes(new Uint8Array(0), 1024)).toEqual([]);
  });

  it("covers the input without gaps or duplication", () => {
    const bytes = Uint8Array.from({ length: 37 }, (_, index) => index);
    const chunks = chunkBytes(bytes, 8);
    const joined = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0));
    let offset = 0;
    for (const chunk of chunks) {
      joined.set(chunk, offset);
      offset += chunk.byteLength;
    }
    expect(Array.from(joined)).toEqual(Array.from(bytes));
  });

  it("rejects a non-positive or fractional chunk size", () => {
    expect(() => chunkBytes(new Uint8Array(1), 0)).toThrow(/positive integer/);
    expect(() => chunkBytes(new Uint8Array(1), -8)).toThrow(/positive integer/);
    expect(() => chunkBytes(new Uint8Array(1), 1.5)).toThrow(/positive integer/);
  });
});

describe("blake3Hex", () => {
  it("matches the published BLAKE3 vector for empty input", () => {
    expect(blake3Hex(new Uint8Array(0))).toBe(
      "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
    );
  });

  it("matches the published BLAKE3 vector for abc", () => {
    expect(blake3Hex(new TextEncoder().encode("abc"))).toBe(
      "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
    );
  });
});

describe("pastedTextName", () => {
  it("stamps a sortable, zero-padded txt name", () => {
    expect(pastedTextName(new Date(2026, 0, 5, 7, 8, 9))).toBe("pasted-text-20260105-070809.txt");
  });
});

describe("constants", () => {
  it("keeps the paste threshold below the daemon limit", () => {
    expect(TEXT_ATTACHMENT_THRESHOLD).toBe(2_000);
    expect(MAX_ATTACHMENT_BYTES).toBe(50 * 1024 * 1024);
  });
});
