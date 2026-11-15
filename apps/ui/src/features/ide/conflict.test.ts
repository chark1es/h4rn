import { describe, expect, it } from "vitest";
import { conflictFromWrite, keepMineBaseEtag } from "./conflict";

describe("conflictFromWrite", () => {
  it("marks a file deleted mid-edit and retries keep-mine as a create", () => {
    const state = conflictFromWrite("a.md", "mine", {
      expected_etag: "e1",
      current_etag: null,
      current_content: null,
      current_encoding: "utf-8",
    });
    expect(state.deleted).toBe(true);
    expect(state.theirs).toBe("");
    expect(keepMineBaseEtag(state)).toBeNull();
  });

  it("keeps the etag handshake for an ordinary conflict", () => {
    const state = conflictFromWrite("a.md", "mine", {
      expected_etag: "e1",
      current_etag: "e2",
      current_content: "theirs",
      current_encoding: "utf-8",
    });
    expect(state.deleted).toBe(false);
    expect(keepMineBaseEtag(state)).toBe("e2");
  });
});
