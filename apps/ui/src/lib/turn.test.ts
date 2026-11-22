import { describe, expect, it } from "vitest";

import { stoppedNotice } from "./turn";

describe("stoppedNotice", () => {
  it("names the limit that stopped the turn", () => {
    expect(stoppedNotice("token_limit")).toBe("Stopped: the agent hit its token limit.");
    expect(stoppedNotice("turn_limit")).toBe("Stopped: the agent hit its turn-request limit.");
  });

  it("stays quiet for every other status", () => {
    expect(stoppedNotice("completed")).toBeNull();
    expect(stoppedNotice("failed")).toBeNull();
    expect(stoppedNotice("interrupted")).toBeNull();
    expect(stoppedNotice("running")).toBeNull();
  });
});
