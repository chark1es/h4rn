import { describe, expect, it } from "vitest";

import {
  formatShortCode,
  isCompleteShortCode,
  maskShortCode,
  orderedScopes,
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
