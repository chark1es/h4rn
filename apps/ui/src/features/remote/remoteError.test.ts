import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { type RemoteError, RemoteFailure, remoteErrorText, toRemoteError } from "./remoteError";

afterEach(() => setLocalePreference("system"));

const KINDS: RemoteError[] = [
  { kind: "noSignInWindow" },
  { kind: "callbackState" },
  { kind: "signInDenied" },
  { kind: "signInExpired" },
  { kind: "noNoiseKey", params: { name: "atlas" } },
  { kind: "notPairingLink" },
  { kind: "pairingExpired" },
  { kind: "signInToScan" },
  { kind: "signInToType" },
  { kind: "otherConnect", params: { url: "https://relay.test" } },
  { kind: "notApproved", params: { name: "atlas" } },
  { kind: "codesDiffer" },
];

describe("remote errors", () => {
  it("word every client kind in the active language", async () => {
    const english = KINDS.map(remoteErrorText);
    await setLocalePreference("zh-CN");
    const chinese = KINDS.map(remoteErrorText);
    for (const [index, text] of chinese.entries()) {
      expect(text, KINDS[index]?.kind).not.toBe(english[index]);
      expect(text, KINDS[index]?.kind).toMatch(/[一-鿿]/);
    }
    expect(remoteErrorText({ kind: "noNoiseKey", params: { name: "atlas" } })).toContain(
      "h4rnd login",
    );
  });

  it("keep the relay's own words as sent", async () => {
    await setLocalePreference("es");
    expect(toRemoteError({ message: "rate limited", hint: "wait a minute" })).toEqual({
      kind: "sent",
      text: "wait a minute",
    });
    expect(remoteErrorText({ kind: "sent", text: "rate limited" })).toBe("rate limited");
  });

  it("carry the kind of a failure the client raised itself", () => {
    const failure = new RemoteFailure({ kind: "noNoiseKey", params: { name: "atlas" } });
    expect(failure.message).toContain("atlas");
    expect(toRemoteError(failure)).toEqual({ kind: "noNoiseKey", params: { name: "atlas" } });
  });
});
