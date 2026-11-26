import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import {
  formatShortCode,
  isCompleteShortCode,
  maskShortCode,
  orderedScopes,
  scopeSub,
  scopesLabel,
  toggleScope,
} from "./pairingScopes";

describe("pairing scopes", () => {
  it("always holds view and keeps a stable order", () => {
    expect(orderedScopes(["files", "approve"])).toEqual(["view", "approve", "files"]);
    expect(orderedScopes([])).toEqual(["view"]);
  });

  it("toggles scopes on and off but never removes view", () => {
    let scopes = toggleScope(["view", "approve"], "terminal");
    expect(scopes).toEqual(["view", "approve", "terminal"]);
    scopes = toggleScope(scopes, "approve");
    expect(scopes).toEqual(["view", "terminal"]);
    expect(toggleScope(scopes, "view")).toEqual(["view", "terminal"]);
  });

  it("labels a device's scopes", () => {
    expect(scopesLabel(undefined)).toBe("All");
    expect(scopesLabel([])).toBe("All");
    expect(scopesLabel(["view", "approve", "terminal", "files", "admin"])).toBe("All");
    expect(scopesLabel(["view"])).toBe("View only");
    expect(scopesLabel(["view", "approve", "files"])).toBe("Approve and reply, Files and edits");
  });

  describe("in another language", () => {
    afterEach(() => setLocalePreference("system"));

    it("joins the names the way the language does", async () => {
      await setLocalePreference("ja");
      expect(scopesLabel(["view", "approve", "files"])).toBe("承認と返信、ファイルと編集");
      await setLocalePreference("es");
      expect(scopesLabel(["view", "approve", "terminal", "files"])).toMatch(/, .+ y /);
    });

    it("gives a scope with no second line an empty one, never its key", async () => {
      for (const locale of ["system", "zh-CN", "es", "ja"] as const) {
        await setLocalePreference(locale);
        expect(scopeSub("approve"), locale).toBe("");
        expect(scopeSub("files"), locale).toBe("");
        expect(scopeSub("view"), locale).not.toBe("");
      }
    });
  });
});

describe("short code formatting", () => {
  it("formats and masks what is typed", () => {
    expect(formatShortCode("abcdefgh")).toBe("ABCD-EFGH");
    expect(formatShortCode("abc")).toBe("ABC");
    expect(maskShortCode("ab cd-ef gh  jj")).toBe("ABCD-EFGH");
    expect(maskShortCode("")).toBe("");
  });

  it("knows when eight characters are present", () => {
    expect(isCompleteShortCode("abcd-efgh")).toBe(true);
    expect(isCompleteShortCode("abcd-efg")).toBe(false);
  });
});
