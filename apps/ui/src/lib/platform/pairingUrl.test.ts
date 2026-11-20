import { describe, expect, it } from "vitest";

import { isDirectPairingUrl, parsePairingUrl } from "./pairingUrl";

const ENDPOINT = "192.168.1.20:47810";
const CANDIDATE = "100.101.7.42:47810";

function directUrl(): string {
  return (
    `h4rn://pair?e=${encodeURIComponent(ENDPOINT)}` +
    `&d=${"ab".repeat(32)}&p=${"cd".repeat(32)}` +
    `&a=${encodeURIComponent(CANDIDATE)}&l=${encodeURIComponent("Atlas")}`
  );
}

describe("parsePairingUrl", () => {
  it("accepts the direct pairing QR", () => {
    // This used to throw "missing daemon or token"; the direct QR has neither.
    const raw = directUrl();
    expect(parsePairingUrl(raw)).toEqual({ kind: "direct", raw });
    expect(isDirectPairingUrl(raw)).toBe(true);
  });

  it("keeps a connect pairing on the relay path", () => {
    const raw = `h4rn://pair?c=${encodeURIComponent("https://connect.test")}&m=machine-1&k=${"aa".repeat(32)}&p=view`;
    expect(parsePairingUrl(raw)).toEqual({ kind: "connect", raw });
    expect(isDirectPairingUrl(raw)).toBe(false);
  });

  it("reads a local daemon link", () => {
    expect(
      parsePairingUrl("h4rn://connect?daemon=ws%3A%2F%2F127.0.0.1%3A47800%2Fws&token=abc"),
    ).toEqual({ kind: "daemon", daemon: "ws://127.0.0.1:47800/ws", token: "abc" });
  });

  it("hands OIDC callbacks to the app untouched", () => {
    expect(parsePairingUrl("h4rn://callback?code=one&state=two")).toEqual({
      kind: "oidc",
      detail: { code: "one", state: "two" },
    });
  });

  it("still refuses a link that has neither shape", () => {
    expect(() => parsePairingUrl("h4rn://pair?e=host%3A47810")).toThrow(
      "The pairing link is missing daemon or token.",
    );
  });

  it("ignores other schemes and targets", () => {
    expect(parsePairingUrl("https://example.test/pair?e=x&d=y&p=z")).toBeNull();
    expect(parsePairingUrl("h4rn://something?e=x&d=y&p=z")).toBeNull();
    expect(isDirectPairingUrl("h4rn://connect?daemon=x&token=y")).toBe(false);
  });
});
